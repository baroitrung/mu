//! Uninstall TUI — port of `internal/uninstall/model.go`.
//!
//! Type-to-search package list with multi-select, remnant preview, and
//! YES/NO confirm. Phases: search → confirm → done.
//!
//! Keys (search phase): type to search, ↑/↓ navigate, Space select,
//! Esc/Ctrl+C quit, Enter → confirm (if any selected).
//! Keys (confirm phase): ←/→/h/l/tab toggle, Enter confirms, q/Esc quit.

use std::collections::BTreeMap;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use super::styles;
use crate::size;
use crate::uninstall::{self, Package};

/// Spinner frames — Go's `spinnerFrames`.
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Phase — mirrors Go's `phase` type.
#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Search,
    Confirm,
    ReviewRemnants,
    Done,
}

/// Result of background package loading — sent from a worker thread.
pub struct LoadResult {
    pub items: Vec<PkgItem>,
    pub discovery_err: Option<String>,
}

/// Uninstall TUI state — mirrors Go's `uninstallModel`.
#[derive(Clone, Debug)]
pub struct UninstallTui {
    pub phase: Phase,
    pub spinner_idx: usize,
    pub all_items: Vec<PkgItem>,
    pub items: Vec<PkgItem>,
    pub query: String,
    pub loaded: bool,
    pub discovery_err: Option<String>,
    pub cursor: usize,
    pub selected: BTreeMap<String, bool>,
    pub window_width: u16,
    pub window_h: u16,
    pub scroll_off: usize,
    pub confirm_cursor: usize, // 0=YES 1=NO
    pub remnant_items: Vec<(String, String, bool)>,
    pub remnant_cursor: usize,
}
/// Package item — mirrors Go's `pkgItem`.
#[derive(Clone, Debug)]
pub struct PkgItem {
    pub pkg: Package,
    pub selected: bool,
}

impl UninstallTui {
    /// Create a new uninstall TUI — port of Go's `newModel`.
    pub fn new() -> Self {
        Self {
            phase: Phase::Search,
            spinner_idx: 0,
            all_items: Vec::new(),
            items: Vec::new(),
            query: String::new(),
            loaded: false,
            discovery_err: None,
            cursor: 0,
            selected: BTreeMap::new(),
            window_width: 80,
            window_h: 24,
            scroll_off: 0,
            confirm_cursor: 1, // default to NO
            remnant_items: Vec::new(),
            remnant_cursor: 0,
        }
    }

    /// Load packages — port of Go's `Init` cmd (Discover + FindRemnants).
    pub fn load(&mut self) {
        let result = Self::load_packages();
        self.apply_load_result(result);
    }

    /// Load packages in a way that can run on a background thread (no `&mut self`).
    /// Returns the discovered items + any discovery error.
    pub fn load_packages() -> LoadResult {
        use crate::runner::ProcessRunner;
        let runner = ProcessRunner;
        let (pkgs, err) = uninstall::discover(&runner);
        let discovery_err = err.map(|e| e.to_string());
        let mut items = Vec::new();
        for mut pkg in pkgs {
            pkg.remnants_found = uninstall::find_remnants(&pkg.name);
            pkg.remnants_kb = uninstall::remnant_size(&pkg.remnants_found) / 1024;
            items.push(PkgItem {
                pkg,
                selected: false,
            });
        }
        LoadResult {
            items,
            discovery_err,
        }
    }

    /// Apply the result of background loading.
    pub fn apply_load_result(&mut self, result: LoadResult) {
        self.discovery_err = result.discovery_err;
        self.all_items = result.items;
        self.loaded = true;
        self.items = filter_items(&self.all_items, &self.query);
        self.cursor = 0;
        self.scroll_off = 0;
    }

    /// Advance the spinner — port of Go's `tickMsg` handler.
    pub fn tick(&mut self) {
        if !self.loaded {
            self.spinner_idx = (self.spinner_idx + 1) % SPINNER_FRAMES.len();
        }
    }

    /// Handle a window resize — port of Go's `WindowSizeMsg`.
    pub fn resize(&mut self, width: u16, height: u16) {
        self.window_width = width;
        self.window_h = height;
    }

    /// List height — port of Go's `listHeight`.
    fn list_height(&self) -> usize {
        // saturating_sub: a <7-row terminal yields 3 like Go's `max(h,3)`.
        (self.window_h as usize).saturating_sub(7).max(3)
    }

    /// Handle a key — port of Go's `Update` KeyMsg handler.
    /// Returns true if the TUI is done (quit or confirmed).
    pub fn handle_key(&mut self, key: crossterm::event::KeyCode) -> bool {
        use crossterm::event::KeyCode;

        match self.phase {
            Phase::Search => {
                match key {
                    // Esc quits the search (Go: "esc" → quit).
                    KeyCode::Esc => return true,
                    KeyCode::Backspace => {
                        if !self.query.is_empty() {
                            // Remove last char (UTF-8 safe)
                            self.query.pop();
                            self.items = filter_items(&self.all_items, &self.query);
                            self.cursor = 0;
                            self.scroll_off = 0;
                        }
                    }
                    KeyCode::Up => {
                        if self.cursor > 0 {
                            self.cursor -= 1;
                            if self.cursor < self.scroll_off {
                                self.scroll_off = self.cursor;
                            }
                        }
                    }
                    KeyCode::Down => {
                        if self.cursor < self.items.len().saturating_sub(1) {
                            self.cursor += 1;
                            let vis_h = self.list_height();
                            if self.cursor >= self.scroll_off + vis_h {
                                self.scroll_off = self.cursor - vis_h + 1;
                            }
                        }
                    }
                    KeyCode::Char(' ') => {
                        if self.cursor < self.items.len() {
                            let key = self.items[self.cursor].pkg.key();
                            let cur = *self.selected.get(&key).unwrap_or(&false);
                            self.selected.insert(key.clone(), !cur);
                            for item in &mut self.all_items {
                                if item.pkg.key() == key {
                                    item.selected = !cur;
                                    break;
                                }
                            }
                            self.items = filter_items(&self.all_items, &self.query);
                        }
                    }
                    KeyCode::Enter => {
                        let n_selected = self.selected.values().filter(|&&v| v).count();
                        if n_selected > 0 {
                            self.phase = Phase::Confirm;
                            self.confirm_cursor = 1; // default to NO
                        }
                    }
                    // Printable runes (including q, j, k) append to the query.
                    // C3 fix: Ctrl+C is handled in handle_key_event, not here,
                    // so plain 'c' reaches this arm and is a valid search char.
                    KeyCode::Char(c) if !c.is_control() => {
                        self.query.push(c);
                        self.items = filter_items(&self.all_items, &self.query);
                        self.cursor = 0;
                        self.scroll_off = 0;
                    }
                    _ => {}
                }
            }
            Phase::Confirm => {
                match key {
                    // Go: only q and ctrl+c quit the confirm (not Esc).
                    KeyCode::Char('q') => return true,
                    KeyCode::Char('r') => {
                        self.remnant_items.clear();
                        for (key, sel) in &self.selected {
                            if !sel {
                                continue;
                            }
                            for it in &self.all_items {
                                if it.pkg.key() == *key {
                                    for r in &it.pkg.remnants_found {
                                        self.remnant_items.push((key.clone(), r.clone(), true));
                                    }
                                }
                            }
                        }
                        if !self.remnant_items.is_empty() {
                            self.remnant_cursor = 0;
                            self.phase = Phase::ReviewRemnants;
                        }
                    }
                    KeyCode::Left
                    | KeyCode::Char('h')
                    | KeyCode::Right
                    | KeyCode::Char('l')
                    | KeyCode::Tab => {
                        self.confirm_cursor = 1 - self.confirm_cursor;
                    }
                    KeyCode::Enter => {
                        if self.confirm_cursor == 0 {
                            self.phase = Phase::Done;
                            return true;
                        }
                        // NO: return to search
                        self.phase = Phase::Search;
                    }
                    _ => {}
                }
            }
            Phase::ReviewRemnants => {
                match key {
                    KeyCode::Up => {
                        if self.remnant_cursor > 0 {
                            self.remnant_cursor -= 1;
                        }
                    }
                    KeyCode::Down => {
                        if self.remnant_cursor + 1 < self.remnant_items.len() {
                            self.remnant_cursor += 1;
                        }
                    }
                    KeyCode::Char(' ') => {
                        if self.remnant_cursor < self.remnant_items.len() {
                            self.remnant_items[self.remnant_cursor].2 =
                                !self.remnant_items[self.remnant_cursor].2;
                        }
                    }
                    KeyCode::Enter | KeyCode::Esc => {
                        // Apply remnant selections back to all_items
                        for (pkg_key, path, keep) in &self.remnant_items {
                            if !keep {
                                for it in &mut self.all_items {
                                    if it.pkg.key() == *pkg_key {
                                        it.pkg.remnants_found.retain(|r| r != path);
                                    }
                                }
                            }
                        }
                        self.phase = Phase::Confirm;
                    }
                    _ => {}
                }
            }
            Phase::Done => {}
        }
        false
    }

    /// Handle a key event (with modifiers). Returns true if done.
    /// Go: ctrl+c quits in both search and confirm phases.
    pub fn handle_key_event(&mut self, key: crossterm::event::KeyEvent) -> bool {
        // C3 fix: real Ctrl+C check (not the tautological guard).
        if super::is_ctrl_c(&key) {
            return true;
        }
        // Modified keys map to tea names like "ctrl+q"/"alt+x" — no model
        // arm matches them, so they are ignored like Go's default case.
        if key.modifiers.intersects(
            crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT,
        ) {
            return false;
        }
        self.handle_key(key.code)
    }

    /// Get the selected packages — port of Go's `selectedPackages`.
    pub fn selected_packages(&self) -> Vec<Package> {
        self.all_items
            .iter()
            .filter(|it| it.selected)
            .map(|it| it.pkg.clone())
            .collect()
    }

    /// Render the uninstall TUI — port of Go's `View()`.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let mut lines: Vec<Line> = Vec::new();

        match self.phase {
            Phase::Search => {
                let n_selected = self.selected.values().filter(|&&v| v).count();
                let header = format!("  mu uninstall  ({n_selected} selected)");
                lines.push(Line::raw(""));
                lines.push(Line::raw(""));
                lines.push(Line::styled(header, styles::bold_primary()));
                lines.push(Line::raw(""));

                // Search line with spinner
                let spin = if !self.loaded {
                    format!("  {}", SPINNER_FRAMES[self.spinner_idx])
                } else {
                    String::new()
                };
                lines.push(Line::styled(
                    format!("  Search: {}█{spin}", self.query),
                    styles::bold_primary(),
                ));
                lines.push(Line::raw(""));

                if let Some(err) = &self.discovery_err {
                    lines.push(Line::styled(
                        format!("  Discovery warning: {err}"),
                        ratatui::style::Style::default().fg(styles::AMBER),
                    ));
                    lines.push(Line::raw(""));
                }

                // List content
                if self.loaded && self.all_items.is_empty() && self.discovery_err.is_some() {
                    lines.push(Line::styled(
                        "  No package source could be loaded.",
                        styles::faint(),
                    ));
                } else if self.query.is_empty() {
                    lines.push(Line::styled(
                        "  Type to search installed packages...",
                        styles::faint(),
                    ));
                } else if self.items.is_empty() {
                    lines.push(Line::styled("  No packages found.", styles::faint()));
                } else {
                    let vis_h = self.list_height();
                    let end = (self.scroll_off + vis_h).min(self.items.len());
                    for i in self.scroll_off..end {
                        let it = &self.items[i];
                        let total_kb = it.pkg.installed_kb + it.pkg.remnants_kb;
                        let size_str = size::human_size(total_kb * 1024);
                        let check = if it.selected { "✓ " } else { "  " };
                        let line = format!(
                            "{check}{} [{}] {size_str}  {}",
                            it.pkg.name, it.pkg.source, it.pkg.version
                        );
                        if i == self.cursor {
                            lines.push(Line::styled(
                                format!("  {}", line.trim_start()),
                                ratatui::style::Style::default()
                                    .bg(ratatui::style::Color::Rgb(0x1F, 0x29, 0x37))
                                    .fg(ratatui::style::Color::Rgb(0xF9, 0xFA, 0xFB)),
                            ));
                        } else {
                            lines.push(Line::raw(format!("  {line}")));
                        }
                    }
                    if self.items.len() > vis_h {
                        lines.push(Line::styled(
                            format!(
                                "  [{}-{} of {}]",
                                self.scroll_off + 1,
                                end,
                                self.items.len()
                            ),
                            styles::faint(),
                        ));
                    }
                }

                lines.push(Line::raw(""));
                lines.push(Line::raw(""));
                lines.push(Line::raw(
                    "  Type: search  •  ↑/↓: navigate  •  Space: select  •  Esc: quit",
                ));
            }
            Phase::Confirm => {
                lines.push(Line::raw(""));
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "  Will remove:",
                    ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::BOLD),
                ));

                for (key, sel) in &self.selected {
                    if !sel {
                        continue;
                    }
                    for it in &self.all_items {
                        if it.pkg.key() != *key {
                            continue;
                        }
                        lines.push(Line::raw(format!(
                            "    • {} ({})",
                            it.pkg.name, it.pkg.source
                        )));
                        for r in &it.pkg.remnants_found {
                            lines.push(Line::raw(format!("      - {r}")));
                        }
                        break;
                    }
                }

                // M1 fix: render YES/NO buttons as styled spans so the active
                // button is visible (Go's StyleButtonOn/Off).
                let yes_style = if self.confirm_cursor == 0 {
                    styles::button_on()
                } else {
                    styles::button_off()
                };
                let no_style = if self.confirm_cursor == 1 {
                    styles::button_on()
                } else {
                    styles::button_off()
                };
                // m7 fix: 1 blank line before buttons (Go: \n).
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(" YES ", yes_style),
                    Span::raw("   "),
                    Span::styled(" NO ", no_style),
                ]));
                // m7 fix: 3 blank lines after buttons (Go: \n\n\n).
                lines.push(Line::raw(""));
                lines.push(Line::raw(""));
                lines.push(Line::raw(""));
                lines.push(Line::raw(
                    "  ←/→: toggle  •  r: review remnants  •  Enter: confirm  •  q: quit",
                ));
            }
            Phase::ReviewRemnants => {
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "  Review Remnants (Space to toggle, Enter to save):",
                    styles::bold_primary(),
                ));
                lines.push(Line::raw(""));
                for (idx, (pkg_key, path, sel)) in self.remnant_items.iter().enumerate() {
                    let check = if *sel { "[x]" } else { "[ ]" };
                    let text = format!("  {check} {path} ({pkg_key})");
                    if idx == self.remnant_cursor {
                        lines.push(Line::styled(
                            text,
                            ratatui::style::Style::default()
                                .bg(ratatui::style::Color::Rgb(0x1F, 0x29, 0x37))
                                .fg(ratatui::style::Color::Rgb(0xF9, 0xFA, 0xFB)),
                        ));
                    } else {
                        lines.push(Line::raw(text));
                    }
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "  ↑/↓: navigate  •  Space: toggle  •  Enter/Esc: back to confirm",
                    styles::faint(),
                ));
            }
            Phase::Done => {
                lines.push(Line::raw(""));
                lines.push(Line::raw(""));
                lines.push(Line::raw("  Confirmed. Removing packages..."));
                lines.push(Line::raw(""));
            }
        }

        frame.render_widget(Paragraph::new(Text::from(lines)), area);
    }
}

impl Default for UninstallTui {
    fn default() -> Self {
        Self::new()
    }
}

/// Filter items by query — port of Go's `filterItems`.
fn filter_items(all: &[PkgItem], query: &str) -> Vec<PkgItem> {
    if query.is_empty() {
        return Vec::new();
    }
    let q = query.to_lowercase();
    all.iter()
        .filter(|it| it.pkg.name.to_lowercase().contains(&q))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn defaults_to_search_no() {
        let tui = UninstallTui::new();
        assert_eq!(tui.phase, Phase::Search);
        assert_eq!(tui.confirm_cursor, 1); // default NO
        assert!(!tui.loaded);
    }

    #[test]
    fn filter_items_empty_query() {
        let items = vec![PkgItem {
            pkg: Package {
                name: "vim".into(),
                source: "apt".into(),
                ..Default::default()
            },
            selected: false,
        }];
        assert!(filter_items(&items, "").is_empty());
    }

    #[test]
    fn filter_items_match() {
        let items = vec![
            PkgItem {
                pkg: Package {
                    name: "vim".into(),
                    source: "apt".into(),
                    ..Default::default()
                },
                selected: false,
            },
            PkgItem {
                pkg: Package {
                    name: "emacs".into(),
                    source: "apt".into(),
                    ..Default::default()
                },
                selected: false,
            },
        ];
        let result = filter_items(&items, "vi");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].pkg.name, "vim");
    }

    #[test]
    fn search_type_appends_query() {
        let mut tui = UninstallTui::new();
        tui.handle_key(KeyCode::Char('v'));
        tui.handle_key(KeyCode::Char('i'));
        tui.handle_key(KeyCode::Char('m'));
        assert_eq!(tui.query, "vim");
    }

    #[test]
    fn backspace_removes_last_char() {
        let mut tui = UninstallTui::new();
        tui.handle_key(KeyCode::Char('v'));
        tui.handle_key(KeyCode::Char('i'));
        tui.handle_key(KeyCode::Backspace);
        assert_eq!(tui.query, "v");
    }

    #[test]
    fn esc_quits_search() {
        let mut tui = UninstallTui::new();
        assert!(tui.handle_key(KeyCode::Esc));
    }

    #[test]
    fn enter_without_selection_stays_in_search() {
        let mut tui = UninstallTui::new();
        tui.handle_key(KeyCode::Enter);
        assert_eq!(tui.phase, Phase::Search);
    }

    #[test]
    fn space_selects_item() {
        let mut tui = UninstallTui::new();
        tui.all_items = vec![PkgItem {
            pkg: Package {
                name: "vim".into(),
                source: "apt".into(),
                ..Default::default()
            },
            selected: false,
        }];
        tui.items = tui.all_items.clone();
        tui.cursor = 0;
        tui.handle_key(KeyCode::Char(' '));
        assert!(tui.selected.get("apt:vim").unwrap_or(&false));
    }

    #[test]
    fn enter_with_selection_goes_to_confirm() {
        let mut tui = UninstallTui::new();
        tui.selected.insert("apt:vim".into(), true);
        tui.handle_key(KeyCode::Enter);
        assert_eq!(tui.phase, Phase::Confirm);
        assert_eq!(tui.confirm_cursor, 1); // default NO
    }

    #[test]
    fn confirm_no_returns_to_search() {
        let mut tui = UninstallTui::new();
        tui.phase = Phase::Confirm;
        tui.confirm_cursor = 1; // NO
        tui.handle_key(KeyCode::Enter);
        assert_eq!(tui.phase, Phase::Search);
    }

    #[test]
    fn confirm_yes_goes_to_done() {
        let mut tui = UninstallTui::new();
        tui.phase = Phase::Confirm;
        tui.confirm_cursor = 0; // YES
        assert!(tui.handle_key(KeyCode::Enter));
        assert_eq!(tui.phase, Phase::Done);
    }

    #[test]
    fn confirm_toggle() {
        let mut tui = UninstallTui::new();
        tui.phase = Phase::Confirm;
        assert_eq!(tui.confirm_cursor, 1);
        tui.handle_key(KeyCode::Left);
        assert_eq!(tui.confirm_cursor, 0);
        tui.handle_key(KeyCode::Right);
        assert_eq!(tui.confirm_cursor, 1);
    }

    #[test]
    fn render_search_snapshot() {
        let tui = UninstallTui::new();
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| tui.render(f, f.area())).unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(content.contains("mu uninstall"));
        assert!(content.contains("Search:"));
        assert!(content.contains("Type to search"));
    }

    #[test]
    fn render_confirm_snapshot() {
        let mut tui = UninstallTui::new();
        tui.phase = Phase::Confirm;
        tui.selected.insert("apt:vim".into(), true);
        tui.all_items = vec![PkgItem {
            pkg: Package {
                name: "vim".into(),
                source: "apt".into(),
                ..Default::default()
            },
            selected: true,
        }];
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| tui.render(f, f.area())).unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(content.contains("Will remove:"));
        assert!(content.contains("vim"));
    }

    #[test]
    fn review_remnants_checklist_toggles_and_saves() {
        let mut tui = UninstallTui::new();
        tui.phase = Phase::Confirm;
        tui.selected.insert("apt:vim".into(), true);
        tui.all_items = vec![PkgItem {
            pkg: Package {
                name: "vim".into(),
                source: "apt".into(),
                remnants_found: vec![
                    "/home/user/.config/vim".into(),
                    "/home/user/.cache/vim".into(),
                ],
                ..Default::default()
            },
            selected: true,
        }];

        // Press 'r' to enter remnant review
        tui.handle_key(KeyCode::Char('r'));
        assert_eq!(tui.phase, Phase::ReviewRemnants);
        assert_eq!(tui.remnant_items.len(), 2);
        assert!(tui.remnant_items[0].2); // initially selected

        // Toggle first remnant off with Space
        tui.handle_key(KeyCode::Char(' '));
        assert!(!tui.remnant_items[0].2);

        // Press Enter to save and return to Confirm
        tui.handle_key(KeyCode::Enter);
        assert_eq!(tui.phase, Phase::Confirm);
        // The first remnant should have been deselected and removed from remnants_found
        assert_eq!(
            tui.all_items[0].pkg.remnants_found,
            vec!["/home/user/.cache/vim"]
        );
    }
}
