//! Main menu — port of `cmd/mu/cli/tui.go`.
//!
//! Shows the mu banner, a health snapshot, and a list of menu items.
//! Keys: ↑/↓ or j/k navigate, 1-6 select by number, Enter/Space confirms,
//! q/ctrl+c quits. After a subcommand runs, the menu re-opens with a
//! transient done-summary banner (or error banner + continue hint).

use std::time::Duration;

use crossterm::event::KeyModifiers;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Text};
use ratatui::widgets::Paragraph;

use super::styles;
use crate::size;
use crate::status;

/// Menu item — mirrors Go's `menuItem` struct.
#[derive(Clone, Debug)]
pub struct MenuItem {
    pub label: &'static str,
    pub desc: &'static str,
    pub command: &'static str,
}

/// The six menu items — exact order from Go's `menuItems`.
pub const MENU_ITEMS: &[MenuItem] = &[
    MenuItem {
        label: "Audit",
        desc: "Diagnose cleanup issues and apply recommended fixes",
        command: "audit",
    },
    MenuItem {
        label: "Clean",
        desc: "Free disk space (cache, apt, snap, journal, kernels)",
        command: "clean",
    },
    MenuItem {
        label: "Uninstall",
        desc: "Remove apps and all their remnants",
        command: "uninstall",
    },
    MenuItem {
        label: "Optimize",
        desc: "Run system maintenance tasks",
        command: "optimize",
    },
    MenuItem {
        label: "Status",
        desc: "Live system dashboard",
        command: "status",
    },
    MenuItem {
        label: "Quit",
        desc: "Exit mu",
        command: "quit",
    },
];

/// The ASCII banner — Go's `banner` const.
const BANNER: &str = "░█▄▒▄█░█▒█░░\n░█▒▀▒█░▀▄█▒░";

/// Health snapshot — mirrors Go's `healthMsg`.
#[derive(Clone, Debug, Default)]
pub struct HealthSnapshot {
    pub cpu: f64,
    pub mem_used: u64,
    pub mem_total: u64,
    pub disk_free: f64,
}

impl HealthSnapshot {
    /// Whether the snapshot is empty (all zeros — Go's `memTotal == 0`).
    fn is_empty(&self) -> bool {
        self.mem_total == 0
    }
}

/// Main menu state — mirrors Go's `mainMenuModel`.
#[derive(Clone, Debug, Default)]
pub struct MainMenu {
    pub cursor: usize,
    pub chosen: Option<&'static str>,
    pub snapshot: Option<HealthSnapshot>,
    pub banner: String,
    pub banner_err: bool,
}

impl MainMenu {
    /// Create a new menu with the given persisted cursor and snapshot.
    pub fn new(cursor: usize, snapshot: Option<HealthSnapshot>) -> Self {
        Self {
            cursor,
            snapshot,
            ..Default::default()
        }
    }

    /// Set the transient banner (shown after a subcommand completes).
    pub fn with_banner(mut self, banner: String, err: bool) -> Self {
        self.banner = banner;
        self.banner_err = err;
        self
    }

    /// Set the health snapshot (after async collection).
    pub fn set_snapshot(&mut self, snap: HealthSnapshot) {
        self.snapshot = Some(snap);
    }

    /// Collect a health snapshot — port of Go's `mainMenuModel.Init` cmd.
    pub fn collect_health() -> HealthSnapshot {
        let mut readers = status::Readers::real();
        let s1 = match (readers.cpu)() {
            Ok(s) => s,
            Err(_) => return HealthSnapshot::default(),
        };
        std::thread::sleep(Duration::from_secs(1));
        let s2 = match (readers.cpu)() {
            Ok(s) => s,
            Err(_) => return HealthSnapshot::default(),
        };
        let cpu = status::cpu_percent(s1, s2);
        let mem = (readers.memory)().unwrap_or_default();
        let (disks, _disk_err) = (readers.disk)();

        let disk_free = disks
            .iter()
            .find(|d| d.mount == "/" && d.total_bytes > 0)
            .map(|d| d.free_bytes as f64 / d.total_bytes as f64 * 100.0)
            .unwrap_or(100.0);

        HealthSnapshot {
            cpu,
            mem_used: mem.total_kb.saturating_sub(mem.available_kb),
            mem_total: mem.total_kb,
            disk_free,
        }
    }

    /// Handle a key. Returns true if the menu is done (quit or chosen).
    pub fn handle_key(&mut self, key: crossterm::event::KeyCode) -> bool {
        use crossterm::event::KeyCode;

        // If a banner is showing, any key clears it first.
        if !self.banner.is_empty() {
            self.banner.clear();
            self.banner_err = false;
            return false;
        }

        match key {
            KeyCode::Char('q') => {
                self.chosen = Some("quit");
                true
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                }
                false
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.cursor < MENU_ITEMS.len() - 1 {
                    self.cursor += 1;
                }
                false
            }
            KeyCode::Char(c @ '1'..='6') => {
                let idx = (c as u8 - b'1') as usize;
                if idx < MENU_ITEMS.len() {
                    self.chosen = Some(MENU_ITEMS[idx].command);
                    return true;
                }
                false
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.chosen = Some(MENU_ITEMS[self.cursor].command);
                true
            }
            _ => false,
        }
    }

    /// Handle a key event (with modifiers). Returns true if done.
    /// Go: ctrl+c and q quit. Esc is NOT a quit key in the menu (M6 fix).
    pub fn handle_key_event(&mut self, key: crossterm::event::KeyEvent) -> bool {
        // Go: a showing banner consumes ANY key — clears it before ctrl+c
        // can quit (Update checks `m.banner != ""` first).
        if !self.banner.is_empty() {
            self.banner.clear();
            self.banner_err = false;
            return false;
        }
        // Ctrl+C quits (C3/M5 fix).
        if super::is_ctrl_c(&key) {
            self.chosen = Some("quit");
            return true;
        }
        // Modified keys map to tea names like "ctrl+q"/"alt+x" — no arm
        // matches them, so they are ignored like Go's default case.
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return false;
        }
        self.handle_key(key.code)
    }

    /// Render the menu into the frame area — port of Go's `mainMenuModel.View()`.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let mut lines: Vec<Line> = Vec::new();

        // Top padding: \n\n
        lines.push(Line::raw(""));
        lines.push(Line::raw(""));

        // Banner — styled with primary color, indented by 2 spaces.
        for banner_line in BANNER.lines() {
            lines.push(Line::styled(
                format!("  {banner_line}"),
                styles::bold_primary(),
            ));
        }

        // Title
        lines.push(Line::styled(
            "  Mole Ubuntu — safe system cleaner",
            styles::bold_primary(),
        ));

        // Biometric hint
        let sudo_status = crate::sudo::get_status(&crate::sudo::resolve_pam_path(None));
        if sudo_status.enabled {
            lines.push(Line::styled(
                "  [Biometrics: Enabled (pam_fprintd)]",
                styles::faint(),
            ));
        } else if sudo_status.module_installed {
            lines.push(Line::styled(
                "  [Biometrics: Available (run 'mu sudo enable')]",
                styles::faint(),
            ));
        }

        // Update notice if cached check shows newer version
        let cache_check = crate::xdg::cache_home().join("mu/last_version_check.json");
        if let Ok(content) = std::fs::read_to_string(&cache_check)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&content)
            && let Some(latest) = v.get("latest").and_then(|s| s.as_str())
            && crate::update::is_newer_version(latest, crate::update::current_version())
        {
            lines.push(Line::styled(
                format!("  ★ Update available: {latest} (run 'mu update')"),
                styles::bold_primary(),
            ));
        }

        // Banner (transient done/error summary)
        if !self.banner.is_empty() {
            let style = if self.banner_err {
                ratatui::style::Style::default()
                    .add_modifier(ratatui::style::Modifier::BOLD)
                    .fg(styles::DANGER)
            } else {
                styles::bold_primary()
            };
            lines.push(Line::styled(format!("  {}", self.banner), style));
            if self.banner_err {
                lines.push(Line::styled("  press any key to continue", styles::faint()));
            }
            lines.push(Line::raw(""));
        }

        // Health snapshot
        match &self.snapshot {
            None => {
                lines.push(Line::styled("  Loading...", styles::faint()));
                lines.push(Line::raw(""));
            }
            Some(snap) if snap.is_empty() => {
                lines.push(Line::styled("  ---  ---  ---", styles::faint()));
                lines.push(Line::raw(""));
            }
            Some(snap) => {
                let cpu_str = format!("CPU: {:.0}%", snap.cpu);
                let ram_str = format!(
                    "RAM: {}/{}",
                    size::human_kb(snap.mem_used),
                    size::human_kb(snap.mem_total)
                );
                let disk_str = format!("Disk /: {:.0}% free", snap.disk_free);
                lines.push(Line::styled(
                    format!("  {cpu_str}  {ram_str}  {disk_str}"),
                    styles::faint(),
                ));
                lines.push(Line::raw(""));
            }
        }

        // Menu items
        for (i, item) in MENU_ITEMS.iter().enumerate() {
            let prefix = if i == self.cursor { "▶ " } else { "  " };
            let text = format!("{prefix}{:<14} {}", item.label, item.desc);
            let style = if i == self.cursor {
                styles::bold_primary()
            } else {
                ratatui::style::Style::default()
            };
            lines.push(Line::styled(format!("  {text}"), style));
        }

        // Footer hint: \n\n\n + hint line
        lines.push(Line::raw(""));
        lines.push(Line::raw(""));
        lines.push(Line::raw(""));
        lines.push(Line::raw(
            "  ↑/↓ or j/k to navigate  •  Enter to select  •  q to quit",
        ));

        frame.render_widget(Paragraph::new(Text::from(lines)), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn menu_items_order() {
        assert_eq!(MENU_ITEMS.len(), 6);
        assert_eq!(MENU_ITEMS[0].command, "audit");
        assert_eq!(MENU_ITEMS[5].command, "quit");
    }

    #[test]
    fn navigate_down_up() {
        let mut m = MainMenu::default();
        assert_eq!(m.cursor, 0);
        m.handle_key(KeyCode::Down);
        assert_eq!(m.cursor, 1);
        m.handle_key(KeyCode::Down);
        assert_eq!(m.cursor, 2);
        m.handle_key(KeyCode::Up);
        assert_eq!(m.cursor, 1);
    }

    #[test]
    fn j_k_navigation() {
        let mut m = MainMenu::default();
        m.handle_key(KeyCode::Char('j'));
        assert_eq!(m.cursor, 1);
        m.handle_key(KeyCode::Char('k'));
        assert_eq!(m.cursor, 0);
    }

    #[test]
    fn numeric_selection() {
        let mut m = MainMenu::default();
        m.handle_key(KeyCode::Char('3'));
        assert_eq!(m.chosen, Some("uninstall"));
    }

    #[test]
    fn enter_selects() {
        let mut m = MainMenu {
            cursor: 2,
            ..Default::default()
        };
        m.handle_key(KeyCode::Enter);
        assert_eq!(m.chosen, Some("uninstall"));
    }

    #[test]
    fn quit_key() {
        let mut m = MainMenu::default();
        assert!(m.handle_key(KeyCode::Char('q')));
        assert_eq!(m.chosen, Some("quit"));
    }

    #[test]
    fn banner_clears_on_key() {
        let mut m = MainMenu::default().with_banner("✅ Done".to_string(), false);
        assert!(!m.handle_key(KeyCode::Down)); // key clears banner, doesn't navigate
        assert!(m.banner.is_empty());
        assert_eq!(m.cursor, 0); // cursor didn't move
    }

    #[test]
    fn error_banner_shows_hint() {
        let m = MainMenu::default().with_banner("❌ Error".to_string(), true);
        assert!(m.banner_err);
        assert_eq!(m.banner, "❌ Error");
    }

    #[test]
    fn render_snapshot_loading() {
        let m = MainMenu::default(); // snapshot = None
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| m.render(f, f.area())).unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(content.contains("Loading..."));
        assert!(content.contains("Mole Ubuntu"));
    }

    #[test]
    fn render_snapshot_with_data() {
        let snap = HealthSnapshot {
            cpu: 42.0,
            mem_used: 8000000,
            mem_total: 16000000,
            disk_free: 55.0,
        };
        let m = MainMenu::new(0, Some(snap));
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| m.render(f, f.area())).unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(content.contains("CPU: 42%"));
        assert!(content.contains("Disk /: 55% free"));
    }

    #[test]
    fn render_cursor_highlight() {
        let m = MainMenu::new(1, None); // cursor on "Clean"
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| m.render(f, f.area())).unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(content.contains("▶ Clean"));
    }
}
