//! Ratatui ncdu-like interactive disk usage explorer.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::analyze::model::{FileNode, LargestFile, ScanMessage, SortMode, ViewMode};
use crate::paths;
use crate::trash;
use crate::tui::styles;

fn selected_style() -> Style {
    Style::default()
        .bg(Color::Rgb(0x1F, 0x29, 0x37))
        .fg(Color::Rgb(0xF9, 0xFA, 0xFB))
}

fn normal_style() -> Style {
    Style::default()
}
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Interactive disk analysis TUI state.
#[derive(Clone, Debug)]
pub struct AnalyzeTui {
    pub root: Option<FileNode>,
    /// Indices from root to current displayed directory node.
    pub path_indices: Vec<usize>,
    pub cursor: usize,
    pub scroll_off: usize,
    pub sort_mode: SortMode,
    pub view_mode: ViewMode,
    pub dry_run: bool,
    /// If Some, modal confirmation is open for this target path.
    pub confirm_delete: Option<PathBuf>,
    pub confirm_size: u64,
    /// 0 = NO, 1 = YES (default 0).
    pub confirm_cursor: usize,
    pub message: Option<(String, bool)>,
    pub scanning: bool,
    pub spinner_idx: usize,
    pub scan_progress_text: String,
    pub largest_files: Vec<LargestFile>,
    pub window_width: u16,
    pub window_h: u16,
}

impl AnalyzeTui {
    pub fn new(dry_run: bool) -> Self {
        Self {
            root: None,
            path_indices: Vec::new(),
            cursor: 0,
            scroll_off: 0,
            sort_mode: SortMode::SizeDesc,
            view_mode: ViewMode::Tree,
            dry_run,
            confirm_delete: None,
            confirm_size: 0,
            confirm_cursor: 0,
            message: None,
            scanning: true,
            spinner_idx: 0,
            scan_progress_text: "Starting scan...".into(),
            largest_files: Vec::new(),
            window_width: 80,
            window_h: 24,
        }
    }

    /// Advance spinner animation.
    pub fn tick(&mut self) {
        self.spinner_idx = (self.spinner_idx + 1) % SPINNER_FRAMES.len();
    }

    pub fn resize(&mut self, w: u16, h: u16) {
        self.window_width = w;
        self.window_h = h;
    }

    /// Update state from background scan message.
    pub fn apply_message(&mut self, msg: ScanMessage) {
        match msg {
            ScanMessage::Progress {
                items_scanned,
                bytes_scanned,
                current_dir,
            } => {
                let bytes_human = crate::size::human_size(bytes_scanned as i64);
                let name = current_dir
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| current_dir.display().to_string());
                self.scan_progress_text = format!("{items_scanned} items ({bytes_human}) · {name}");
            }
            ScanMessage::Complete { root, stats } => {
                self.root = Some(root);
                self.largest_files = stats.largest_files;
                self.scanning = false;
                self.apply_sort();
            }
            ScanMessage::Error(err) => {
                self.scanning = false;
                self.message = Some((format!("Scan error: {err}"), true));
            }
        }
    }

    /// Resolve mutable reference to current directory node.
    pub fn current_node_mut(&mut self) -> Option<&mut FileNode> {
        let mut cur = self.root.as_mut()?;
        for &idx in &self.path_indices {
            if idx < cur.children.len() {
                cur = &mut cur.children[idx];
            } else {
                return None;
            }
        }
        Some(cur)
    }

    /// Resolve immutable reference to current directory node.
    pub fn current_node(&self) -> Option<&FileNode> {
        let mut cur = self.root.as_ref()?;
        for &idx in &self.path_indices {
            if idx < cur.children.len() {
                cur = &cur.children[idx];
            } else {
                return None;
            }
        }
        Some(cur)
    }

    /// Re-sort the current directory's children according to `self.sort_mode`.
    pub fn apply_sort(&mut self) {
        let mode = self.sort_mode;
        if let Some(node) = self.current_node_mut() {
            node.sort_children(mode);
        }
    }

    /// Handle keyboard input. Returns true if the TUI should exit.
    pub fn handle_key_event(&mut self, key: KeyEvent) -> bool {
        // Clear non-persistent message on next key press
        if self.confirm_delete.is_none() {
            self.message = None;
        }

        // Global abort: Ctrl+C
        if crate::tui::is_ctrl_c(&key) {
            return true;
        }

        // Modal confirmation dialog handling
        if self.confirm_delete.is_some() {
            return self.handle_confirm_key(key);
        }

        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Esc => {
                if self.view_mode == ViewMode::LargestFiles {
                    self.view_mode = ViewMode::Tree;
                } else if !self.path_indices.is_empty() {
                    self.navigate_up();
                } else {
                    return true;
                }
            }
            KeyCode::Tab => {
                self.view_mode = match self.view_mode {
                    ViewMode::Tree => ViewMode::LargestFiles,
                    ViewMode::LargestFiles => ViewMode::Tree,
                };
                self.cursor = 0;
                self.scroll_off = 0;
            }
            KeyCode::Char('s') => {
                self.sort_mode = self.sort_mode.next();
                self.apply_sort();
                self.cursor = 0;
                self.scroll_off = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.navigate_down();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.navigate_up_cursor();
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if self.view_mode == ViewMode::Tree {
                    self.navigate_into();
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                if self.view_mode == ViewMode::Tree {
                    self.navigate_up();
                }
            }
            KeyCode::Char('d') => {
                self.trigger_delete();
            }
            _ => {}
        }

        false
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Char('h')
            | KeyCode::Char('l')
            | KeyCode::Tab => {
                self.confirm_cursor = if self.confirm_cursor == 0 { 1 } else { 0 };
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.confirm_delete = None;
            }
            KeyCode::Enter => {
                if self.confirm_cursor == 1 {
                    self.execute_delete();
                } else {
                    self.confirm_delete = None;
                }
            }
            _ => {}
        }
        false
    }

    fn navigate_down(&mut self) {
        let max_items = match self.view_mode {
            ViewMode::Tree => self.current_node().map(|n| n.children.len()).unwrap_or(0),
            ViewMode::LargestFiles => self.largest_files.len(),
        };

        if max_items > 0 && self.cursor < max_items - 1 {
            self.cursor += 1;
            let visible_height = self.window_h.saturating_sub(6) as usize;
            if visible_height > 0 && self.cursor >= self.scroll_off + visible_height {
                self.scroll_off = self.cursor - visible_height + 1;
            }
        }
    }

    fn navigate_up_cursor(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            if self.cursor < self.scroll_off {
                self.scroll_off = self.cursor;
            }
        }
    }

    fn navigate_into(&mut self) {
        let Some(node) = self.current_node() else {
            return;
        };
        if self.cursor < node.children.len() && node.children[self.cursor].is_dir {
            self.path_indices.push(self.cursor);
            self.cursor = 0;
            self.scroll_off = 0;
            self.apply_sort();
        }
    }

    fn navigate_up(&mut self) {
        if let Some(prev_idx) = self.path_indices.pop() {
            self.cursor = prev_idx;
            self.scroll_off = 0;
            self.apply_sort();
        }
    }

    fn trigger_delete(&mut self) {
        if self.dry_run {
            self.message = Some(("Deletion disabled in --dry-run mode".into(), true));
            return;
        }

        let Some(node) = self.current_node() else {
            return;
        };
        if self.cursor >= node.children.len() {
            return;
        }

        let target_path = node.children[self.cursor].path.clone();
        let target_size = node.children[self.cursor].size;

        if paths::is_protected(&target_path) {
            self.message = Some((
                format!("Refused: {} is a protected path", target_path.display()),
                true,
            ));
            return;
        }

        self.confirm_delete = Some(target_path);
        self.confirm_size = target_size;
        self.confirm_cursor = 0; // Default NO
    }

    fn execute_delete(&mut self) {
        let Some(target_path) = self.confirm_delete.take() else {
            return;
        };

        match trash::safe_delete(&target_path, self.dry_run) {
            Ok(()) => {
                let name = target_path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| target_path.display().to_string());

                // Remove child from memory
                if let Some(cur) = self.current_node_mut()
                    && let Some(pos) = cur.children.iter().position(|c| c.path == target_path)
                {
                    cur.children.remove(pos);
                }

                // Recalculate tree sizes
                if let Some(ref mut root) = self.root {
                    root.recalculate_sizes();
                }

                // Re-adjust cursor if needed
                let max_len = self.current_node().map(|n| n.children.len()).unwrap_or(0);
                if self.cursor >= max_len && self.cursor > 0 {
                    self.cursor = max_len.saturating_sub(1);
                }

                self.message = Some((format!("Moved to trash: {name}"), false));
            }
            Err(e) => {
                self.message = Some((format!("Delete failed: {e}"), true));
            }
        }
    }

    /// Render full TUI frame.
    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        self.window_width = area.width;
        self.window_h = area.height;

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Header
                Constraint::Min(5),    // Body
                Constraint::Length(1), // Footer / Keybindings
            ])
            .split(area);

        self.render_header(f, chunks[0]);

        match self.view_mode {
            ViewMode::Tree => self.render_tree(f, chunks[1]),
            ViewMode::LargestFiles => self.render_largest(f, chunks[1]),
        }

        self.render_footer(f, chunks[2]);

        if self.confirm_delete.is_some() {
            self.render_confirm_dialog(f, area);
        }
    }

    fn render_header(&self, f: &mut Frame, area: Rect) {
        let current_path = self
            .current_node()
            .map(|n| n.path.display().to_string())
            .unwrap_or_else(|| "...".into());

        let total_size = self
            .current_node()
            .map(|n| n.size_human.clone())
            .unwrap_or_else(|| "0 B".into());

        let total_items = self.current_node().map(|n| n.item_count).unwrap_or(0);

        let mut status_spans = Vec::new();
        if self.scanning {
            let spinner = SPINNER_FRAMES[self.spinner_idx];
            status_spans.push(Span::styled(
                format!("{spinner} Scanning: {} ", self.scan_progress_text),
                Style::default().fg(Color::Yellow),
            ));
        } else {
            status_spans.push(Span::styled(
                "✔ Scan ready  ",
                Style::default().fg(Color::Green),
            ));
        }

        if self.dry_run {
            status_spans.push(Span::styled(
                "[DRY RUN] ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
        }

        status_spans.push(Span::styled(
            format!("[Sort: {}]  ", self.sort_mode.label()),
            Style::default().fg(Color::Cyan),
        ));

        let view_label = match self.view_mode {
            ViewMode::Tree => "[View: Tree]",
            ViewMode::LargestFiles => "[View: Largest Files]",
        };
        status_spans.push(Span::styled(
            view_label,
            Style::default().fg(Color::Magenta),
        ));

        let lines = vec![
            Line::from(vec![
                Span::styled("Path: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    current_path,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  ({total_size}, {total_items} items)"),
                    Style::default().fg(Color::Green),
                ),
            ]),
            Line::from(status_spans),
        ];

        let block = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(Color::DarkGray));
        let paragraph = Paragraph::new(lines).block(block);
        f.render_widget(paragraph, area);
    }

    fn render_tree(&self, f: &mut Frame, area: Rect) {
        let Some(node) = self.current_node() else {
            let p = Paragraph::new(Line::from("Loading..."))
                .style(Style::default().fg(Color::DarkGray));
            f.render_widget(p, area);
            return;
        };

        if node.children.is_empty() {
            let msg = if self.scanning {
                "Scanning directory..."
            } else {
                "(empty directory)"
            };
            let p = Paragraph::new(Line::from(msg)).style(Style::default().fg(Color::DarkGray));
            f.render_widget(p, area);
            return;
        }

        let visible_height = area.height as usize;
        let parent_size = node.size.max(1);

        let mut lines = Vec::new();
        for (i, child) in node
            .children
            .iter()
            .enumerate()
            .skip(self.scroll_off)
            .take(visible_height)
        {
            let is_selected = i == self.cursor;
            let prefix = if is_selected { "▶ " } else { "  " };

            // Percentage bar (width 10)
            let ratio = child.size as f64 / parent_size as f64;
            let filled = (ratio * 10.0).round().min(10.0) as usize;
            let empty = 10usize.saturating_sub(filled);
            let bar = format!("[{}{}]", "#".repeat(filled), " ".repeat(empty));

            let dir_icon = if child.is_dir { "[dir] " } else { "      " };
            let name_display = if child.is_dir {
                format!("{}/", child.name)
            } else {
                child.name.clone()
            };

            let item_count_str = if child.is_dir {
                format!("({} items)", child.item_count)
            } else {
                String::new()
            };

            let mut style = if is_selected {
                selected_style()
            } else if child.is_dir {
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD)
            } else {
                normal_style()
            };

            if child.err.is_some() {
                style = style.fg(Color::Red);
            }

            let line = Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(
                    format!("{:<10} ", child.size_human),
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled(format!("{bar} "), Style::default().fg(Color::DarkGray)),
                Span::styled(dir_icon, Style::default().fg(Color::Cyan)),
                Span::styled(format!("{:<30} ", name_display), style),
                Span::styled(item_count_str, Style::default().fg(Color::DarkGray)),
            ]);

            lines.push(line);
        }

        let p = Paragraph::new(lines);
        f.render_widget(p, area);
    }

    fn render_largest(&self, f: &mut Frame, area: Rect) {
        if self.largest_files.is_empty() {
            let p = Paragraph::new(Line::from("No large files discovered yet."))
                .style(Style::default().fg(Color::DarkGray));
            f.render_widget(p, area);
            return;
        }

        let visible_height = area.height as usize;
        let mut lines = Vec::new();

        for (i, file) in self
            .largest_files
            .iter()
            .enumerate()
            .skip(self.scroll_off)
            .take(visible_height)
        {
            let is_selected = i == self.cursor;
            let prefix = if is_selected { "▶ " } else { "  " };
            let style = if is_selected {
                selected_style()
            } else {
                normal_style()
            };
            let line = Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(
                    format!("{:<10} ", file.size_human),
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled(file.path.display().to_string(), style),
            ]);
            lines.push(line);
        }

        let p = Paragraph::new(lines);
        f.render_widget(p, area);
    }

    fn render_footer(&self, f: &mut Frame, area: Rect) {
        if let Some((ref text, is_err)) = self.message {
            let color = if is_err { Color::Red } else { Color::Green };
            let p = Paragraph::new(Line::from(vec![Span::styled(
                text.as_str(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )]));
            f.render_widget(p, area);
            return;
        }

        let help = "↑/↓/j/k navigate · Enter/→ open · Esc/← back · d delete · s sort · Tab largest · q quit";
        let p = Paragraph::new(Line::from(vec![Span::styled(
            help,
            Style::default().fg(Color::DarkGray),
        )]));
        f.render_widget(p, area);
    }

    fn render_confirm_dialog(&self, f: &mut Frame, area: Rect) {
        let Some(ref target) = self.confirm_delete else {
            return;
        };

        let dialog_area = centered_rect(60, 25, area);
        f.render_widget(Clear, dialog_area);

        let size_str = crate::size::human_size(self.confirm_size as i64);
        let block = Block::default()
            .title(" Move to Trash ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Red));

        let prompt = format!("Move to trash: {} ({size_str})?", target.display());

        let no_style = if self.confirm_cursor == 0 {
            styles::button_on()
        } else {
            styles::button_off()
        };

        let yes_style = if self.confirm_cursor == 1 {
            styles::button_on()
        } else {
            styles::button_off()
        };

        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(prompt, Style::default().fg(Color::White))),
            Line::from(""),
            Line::from(vec![
                Span::raw("    "),
                Span::styled(" [ NO ] ", no_style),
                Span::raw("        "),
                Span::styled(" [ YES ] ", yes_style),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "←/→ toggle · Enter confirm · Esc cancel",
                Style::default().fg(Color::DarkGray),
            )),
        ];

        let paragraph = Paragraph::new(lines)
            .block(block)
            .alignment(Alignment::Center);
        f.render_widget(paragraph, dialog_area);
    }
}

/// Helper to center a rectangular dialog overlay.
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::empty())
    }

    #[test]
    fn test_navigation_and_sorting() {
        let mut tui = AnalyzeTui::new(false);
        let mut root = FileNode::new("root".into(), PathBuf::from("/tmp/root"), 300, true, 1);
        let c1 = FileNode::new(
            "small".into(),
            PathBuf::from("/tmp/root/small"),
            100,
            false,
            1,
        );
        let c2 = FileNode::new(
            "large".into(),
            PathBuf::from("/tmp/root/large"),
            200,
            false,
            1,
        );
        root.children = vec![c1, c2];
        tui.root = Some(root);
        tui.apply_sort();

        // SizeDesc -> large should be index 0
        assert_eq!(tui.current_node().unwrap().children[0].name, "large");

        // Navigate down
        tui.handle_key_event(dummy_key(KeyCode::Char('j')));
        assert_eq!(tui.cursor, 1);

        // Sort toggle -> SizeAsc
        tui.handle_key_event(dummy_key(KeyCode::Char('s')));
        assert_eq!(tui.sort_mode, SortMode::SizeAsc);
        assert_eq!(tui.current_node().unwrap().children[0].name, "small");
    }

    #[test]
    fn test_dry_run_prevents_delete() {
        let mut tui = AnalyzeTui::new(true); // dry_run = true
        let mut root = FileNode::new("root".into(), PathBuf::from("/tmp/root"), 100, true, 1);
        let c = FileNode::new(
            "file".into(),
            PathBuf::from("/tmp/root/file"),
            100,
            false,
            1,
        );
        root.children = vec![c];
        tui.root = Some(root);

        // Press 'd'
        tui.handle_key_event(dummy_key(KeyCode::Char('d')));
        assert!(tui.confirm_delete.is_none());
        assert!(tui.message.unwrap().0.contains("disabled in --dry-run"));
    }

    #[test]
    fn test_protected_path_prevents_delete() {
        let mut tui = AnalyzeTui::new(false);
        let mut root = FileNode::new("root".into(), PathBuf::from("/"), 100, true, 1);
        let etc = FileNode::new("etc".into(), PathBuf::from("/etc"), 100, true, 1);
        root.children = vec![etc];
        tui.root = Some(root);

        tui.handle_key_event(dummy_key(KeyCode::Char('d')));
        assert!(tui.confirm_delete.is_none());
        assert!(tui.message.unwrap().0.contains("protected path"));
    }
}
