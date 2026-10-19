//! Interactive Whitelist Editor — Ratatui widget to view and manage
//! protected paths, cache skip patterns, and optimize skip steps.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Text};
use ratatui::widgets::Paragraph;

use super::styles;
use crate::config::{self, Whitelist};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WhitelistTab {
    ProtectedPaths,
    CacheSkip,
    OptimizeSkip,
}

pub struct WhitelistTui {
    pub tab: WhitelistTab,
    pub cursor: usize,
    pub config: Whitelist,
    pub modified: bool,
    pub status_msg: Option<String>,
    pub load_failed: bool,
    pub config_home: Option<std::path::PathBuf>,
}

impl WhitelistTui {
    pub fn new() -> Self {
        Self::new_from(None)
    }

    pub fn new_from(config_home: Option<&std::path::Path>) -> Self {
        let res = match config_home {
            Some(p) => config::load_user_whitelist_from(p),
            None => config::load_user_whitelist(),
        };
        let (config, status_msg, load_failed) = match res {
            Ok(Some(cfg)) => (cfg, None, false),
            Ok(None) => (
                Whitelist::default(),
                Some("User whitelist (~/.config/mu/config.toml)".to_string()),
                false,
            ),
            Err(e) => (
                Whitelist::default(),
                Some(format!("Error loading user config: {e}. Saving disabled.")),
                true,
            ),
        };
        Self {
            tab: WhitelistTab::ProtectedPaths,
            cursor: 0,
            config,
            modified: false,
            status_msg,
            load_failed,
            config_home: config_home.map(|p| p.to_path_buf()),
        }
    }
    pub fn current_items(&self) -> &[String] {
        match self.tab {
            WhitelistTab::ProtectedPaths => &self.config.protected_paths.system,
            WhitelistTab::CacheSkip => &self.config.cache_skip.dirs,
            WhitelistTab::OptimizeSkip => &self.config.optimize_skip.steps,
        }
    }
    pub fn current_items_for(tab: WhitelistTab, config: &mut Whitelist) -> &mut Vec<String> {
        match tab {
            WhitelistTab::ProtectedPaths => &mut config.protected_paths.system,
            WhitelistTab::CacheSkip => &mut config.cache_skip.dirs,
            WhitelistTab::OptimizeSkip => &mut config.optimize_skip.steps,
        }
    }

    pub fn handle_key_event(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => true,
            KeyCode::Tab => {
                self.tab = match self.tab {
                    WhitelistTab::ProtectedPaths => WhitelistTab::CacheSkip,
                    WhitelistTab::CacheSkip => WhitelistTab::OptimizeSkip,
                    WhitelistTab::OptimizeSkip => WhitelistTab::ProtectedPaths,
                };
                self.cursor = 0;
                false
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                }
                false
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let len = self.current_items().len();
                if len > 0 && self.cursor < len - 1 {
                    self.cursor += 1;
                }
                false
            }
            KeyCode::Char(' ') => {
                let cursor = self.cursor;
                let items = Self::current_items_for(self.tab, &mut self.config);
                if !items.is_empty() && cursor < items.len() {
                    let item = &items[cursor];
                    let toggled = if item.starts_with("# ") {
                        item.trim_start_matches("# ").to_string()
                    } else {
                        format!("# {item}")
                    };
                    items[cursor] = toggled;
                    self.modified = true;
                    self.status_msg = Some("Entry toggled (press 's' to save)".to_string());
                }
                false
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                let cursor = self.cursor;
                let items = Self::current_items_for(self.tab, &mut self.config);
                if !items.is_empty() && cursor < items.len() {
                    items.remove(cursor);
                    if self.cursor >= items.len() && self.cursor > 0 {
                        self.cursor -= 1;
                    }
                    self.modified = true;
                    self.status_msg = Some("Entry deleted (press 's' to save)".to_string());
                }
                false
            }
            KeyCode::Char('s') => {
                if self.load_failed {
                    self.status_msg = Some(
                        "Refusing to save: user config file was malformed on startup. Fix syntax manually to avoid data loss."
                            .to_string(),
                    );
                    return false;
                }
                let res = match &self.config_home {
                    Some(p) => config::save_whitelist_to(p, &self.config),
                    None => config::save_whitelist(&self.config),
                };
                match res {
                    Ok(path) => {
                        self.status_msg = Some(format!("Saved to {}", path.display()));
                        self.modified = false;
                    }
                    Err(e) => {
                        self.status_msg = Some(format!("Error saving: {e}"));
                    }
                }
                false
            }
            _ => false,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let mut lines = Vec::new();

        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "  Whitelist Configuration Editor",
            styles::bold_primary(),
        ));

        // Tab bar
        let tab_prot = if self.tab == WhitelistTab::ProtectedPaths {
            "[ 1. Protected Paths ]"
        } else {
            "  1. Protected Paths  "
        };
        let tab_cache = if self.tab == WhitelistTab::CacheSkip {
            "[ 2. Cache Skip ]"
        } else {
            "  2. Cache Skip  "
        };
        let tab_opt = if self.tab == WhitelistTab::OptimizeSkip {
            "[ 3. Optimize Skip ]"
        } else {
            "  3. Optimize Skip  "
        };

        lines.push(Line::styled(
            format!("  {tab_prot}   {tab_cache}   {tab_opt}"),
            Style::default(),
        ));
        lines.push(Line::raw(""));

        // Status / notice
        if let Some(msg) = &self.status_msg {
            lines.push(Line::styled(format!("  {msg}"), styles::bold_primary()));
            lines.push(Line::raw(""));
        }

        // List
        let items = self.current_items();
        if items.is_empty() {
            lines.push(Line::styled(
                "  (no entries in this category)",
                styles::faint(),
            ));
        } else {
            for (idx, item) in items.iter().enumerate() {
                let prefix = if idx == self.cursor { "▶ " } else { "  " };
                let style = if idx == self.cursor {
                    styles::bold_primary()
                } else {
                    Style::default()
                };
                lines.push(Line::styled(format!("  {prefix}{item}"), style));
            }
        }

        // Footer
        lines.push(Line::raw(""));
        lines.push(Line::raw(""));
        let mod_indicator = if self.modified { "  [Modified]" } else { "" };
        lines.push(Line::styled(
            format!(
                "  Tab category • ↑/↓ move • Space toggle • d delete • s save • q quit{mod_indicator}"
            ),
            styles::faint(),
        ));

        frame.render_widget(Paragraph::new(Text::from(lines)), area);
    }
}

pub fn run_whitelist_tui() -> crate::error::Result<()> {
    let mut terminal = ratatui::init();
    let mut app = WhitelistTui::new();

    loop {
        terminal
            .draw(|f| app.render(f, f.area()))
            .map_err(crate::error::Error::Io)?;

        if let crossterm::event::Event::Key(key) =
            crossterm::event::read().map_err(crate::error::Error::Io)?
            && app.handle_key_event(key)
        {
            break;
        }
    }

    ratatui::restore();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_whitelist_tui_tab_switch() {
        let tmp = std::env::temp_dir().join(format!("mu-tui-tab-{}", std::process::id()));
        let mut tui = WhitelistTui::new_from(Some(&tmp));
        assert_eq!(tui.tab, WhitelistTab::ProtectedPaths);

        let tab_key = KeyEvent::from(KeyCode::Tab);
        assert!(!tui.handle_key_event(tab_key));
        assert_eq!(tui.tab, WhitelistTab::CacheSkip);

        assert!(!tui.handle_key_event(tab_key));
        assert_eq!(tui.tab, WhitelistTab::OptimizeSkip);

        assert!(!tui.handle_key_event(tab_key));
        assert_eq!(tui.tab, WhitelistTab::ProtectedPaths);
    }

    #[test]
    fn test_whitelist_tui_quit() {
        let tmp = std::env::temp_dir().join(format!("mu-tui-quit-{}", std::process::id()));
        let mut tui = WhitelistTui::new_from(Some(&tmp));
        let q_key = KeyEvent::from(KeyCode::Char('q'));
        assert!(tui.handle_key_event(q_key));
    }

    #[test]
    fn test_whitelist_tui_toggle_and_delete() {
        let tmp = std::env::temp_dir().join(format!("mu-tui-toggle-{}", std::process::id()));
        let mut tui = WhitelistTui::new_from(Some(&tmp));
        tui.config
            .protected_paths
            .system
            .push("/opt/test-path".to_string());
        assert_eq!(tui.current_items().len(), 1);
        assert!(!tui.modified);

        // Space toggles commented state
        let space_key = KeyEvent::from(KeyCode::Char(' '));
        assert!(!tui.handle_key_event(space_key));
        assert!(tui.modified);
        assert_eq!(tui.current_items()[0], "# /opt/test-path");

        // Space toggles back
        assert!(!tui.handle_key_event(space_key));
        assert_eq!(tui.current_items()[0], "/opt/test-path");

        // 'd' deletes the item
        let d_key = KeyEvent::from(KeyCode::Char('d'));
        assert!(!tui.handle_key_event(d_key));
        assert!(tui.current_items().is_empty());
    }

    #[test]
    fn test_whitelist_tui_refuses_save_on_load_error() {
        let tmp = std::env::temp_dir().join(format!("mu-tui-err-{}", std::process::id()));
        let mu_dir = tmp.join("mu");
        let _ = std::fs::create_dir_all(&mu_dir);
        let bad_file = mu_dir.join("config.toml");
        std::fs::write(&bad_file, b"broken = [toml").unwrap();

        let mut tui = WhitelistTui::new_from(Some(&tmp));
        assert!(tui.load_failed);

        // Pressing 's' must refuse to save
        let s_key = KeyEvent::from(KeyCode::Char('s'));
        assert!(!tui.handle_key_event(s_key));
        assert!(
            tui.status_msg
                .as_deref()
                .unwrap()
                .contains("Refusing to save")
        );

        // File content was NOT overwritten
        assert_eq!(std::fs::read(&bad_file).unwrap(), b"broken = [toml");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
