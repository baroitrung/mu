//! Multi-select list — used by `mu purge`/`mu installer` to let the user pick
//! which discovered items to remove (Phase 4 review F4: the spec's
//! "interactive selection in TUI or headless removal with `--yes`").
//!
//! Keys: ↑/↓ or j/k navigate, Space toggle, `a` select all, `n` select none,
//! Enter confirm, q/Esc/ctrl+c cancel. Nothing is selected by default — the
//! action is destructive.

use std::io::IsTerminal;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::text::{Line, Text};
use ratatui::widgets::Paragraph;

use super::{AltScreenGuard, RawModeGuard, is_ctrl_c};

/// A row is `(label, detail)` — the path and its size/kind.
pub type Row = (String, String);

/// Multi-select dialog state.
#[derive(Clone, Debug)]
pub struct SelectList {
    pub title: String,
    pub rows: Vec<Row>,
    pub cursor: usize,
    pub selected: Vec<bool>,
    pub done: bool,
    /// `Some(indices)` after Enter, `None` after cancel/quit.
    pub confirmed: Option<Vec<usize>>,
}

impl SelectList {
    /// Create a list with nothing selected (destructive default).
    pub fn new(title: impl Into<String>, rows: Vec<Row>) -> Self {
        let selected = vec![false; rows.len()];
        Self {
            title: title.into(),
            rows,
            cursor: 0,
            selected,
            done: false,
            confirmed: None,
        }
    }

    /// Indices currently selected, in list order.
    pub fn chosen(&self) -> Vec<usize> {
        self.selected
            .iter()
            .enumerate()
            .filter(|(_, on)| **on)
            .map(|(i, _)| i)
            .collect()
    }

    fn set_all(&mut self, on: bool) {
        self.selected.fill(on);
    }

    /// Handle a bare key. Returns true when the dialog is done.
    pub fn handle_key(&mut self, key: KeyCode) -> bool {
        if self.rows.is_empty() {
            self.confirmed = Some(Vec::new());
            self.done = true;
            return true;
        }
        match key {
            KeyCode::Up | KeyCode::Char('k') => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                }
                false
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.cursor + 1 < self.rows.len() {
                    self.cursor += 1;
                }
                false
            }
            KeyCode::Char(' ') => {
                self.selected[self.cursor] = !self.selected[self.cursor];
                false
            }
            KeyCode::Char('a') => {
                self.set_all(true);
                false
            }
            KeyCode::Char('n') => {
                self.set_all(false);
                false
            }
            KeyCode::Enter => {
                self.confirmed = Some(self.chosen());
                self.done = true;
                true
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                self.confirmed = None;
                self.done = true;
                true
            }
            _ => false,
        }
    }

    /// Handle a key event (ctrl+c quits; modified keys are ignored).
    pub fn handle_key_event(&mut self, key: crossterm::event::KeyEvent) -> bool {
        if is_ctrl_c(&key) {
            self.confirmed = None;
            self.done = true;
            return true;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return false;
        }
        self.handle_key(key.code)
    }

    /// Render the list into `area`.
    pub fn render(&self, frame: &mut ratatui::Frame, area: Rect) {
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::raw(""));
        lines.push(Line::raw(format!("  {}", self.title)));
        lines.push(Line::raw(""));

        // Keep the cursor visible in a window that fits the area.
        let visible = area.height.saturating_sub(6).max(3) as usize;
        let start = if self.cursor >= visible {
            self.cursor + 1 - visible
        } else {
            0
        };
        for (i, (label, detail)) in self.rows.iter().enumerate().skip(start).take(visible) {
            let mark = if self.selected[i] { "[x]" } else { "[ ]" };
            let pointer = if i == self.cursor { "▶" } else { " " };
            lines.push(Line::raw(format!("  {pointer} {mark} {label}  {detail}")));
        }
        lines.push(Line::raw(""));
        lines.push(Line::raw(
            "  ↑/↓ or j/k navigate  •  Space toggle  •  a: all  •  n: none",
        ));
        lines.push(Line::raw("  Enter: confirm  •  q/Esc: cancel"));

        frame.render_widget(Paragraph::new(Text::from(lines)), area);
    }
}

/// Interactive multi-select on a TTY. Returns `Some(indices)` on confirm
/// (possibly empty), `None` when the list is cancelled or stdout is not a
/// terminal (the caller then falls back to the headless `--yes` gate).
pub fn select_indices(title: &str, rows: Vec<Row>) -> Option<Vec<usize>> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    if rows.is_empty() {
        return Some(Vec::new());
    }
    if enable_raw_mode().is_err() {
        return None;
    }
    let _raw_guard = RawModeGuard;
    let mut stdout = std::io::stdout();
    if execute!(stdout, EnterAlternateScreen).is_err() {
        return None;
    }
    let _alt_guard = AltScreenGuard;
    let backend = CrosstermBackend::new(stdout);
    let Ok(mut terminal) = Terminal::new(backend) else {
        return None;
    };

    let mut list = SelectList::new(title, rows);
    loop {
        if terminal.draw(|f| list.render(f, f.area())).is_err() {
            return None;
        }
        let ev = match event::read() {
            Ok(ev) => ev,
            Err(_) => return None,
        };
        if let Event::Key(key) = ev
            && list.handle_key_event(key)
        {
            break;
        }
    }
    list.confirmed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(n: usize) -> Vec<Row> {
        (0..n)
            .map(|i| (format!("/tmp/item-{i}"), format!("{} B", i + 1)))
            .collect()
    }

    #[test]
    fn select_toggle_and_confirm() {
        let mut l = SelectList::new("pick", rows(3));
        assert_eq!(
            l.chosen(),
            Vec::<usize>::new(),
            "nothing selected initially"
        );
        l.handle_key(KeyCode::Char(' ')); // select row 0
        l.handle_key(KeyCode::Down);
        l.handle_key(KeyCode::Down);
        l.handle_key(KeyCode::Char(' ')); // select row 2
        assert_eq!(l.chosen(), vec![0, 2]);
        assert!(l.handle_key(KeyCode::Enter));
        assert_eq!(l.confirmed, Some(vec![0, 2]));
    }

    #[test]
    fn select_all_and_none() {
        let mut l = SelectList::new("pick", rows(4));
        l.handle_key(KeyCode::Char('a'));
        assert_eq!(l.chosen(), vec![0, 1, 2, 3]);
        l.handle_key(KeyCode::Char('n'));
        assert_eq!(l.chosen(), Vec::<usize>::new());
    }

    #[test]
    fn select_cancel_returns_none() {
        let mut l = SelectList::new("pick", rows(2));
        l.handle_key(KeyCode::Char(' '));
        assert!(l.handle_key(KeyCode::Char('q')));
        assert_eq!(l.confirmed, None);
        assert!(l.done);
    }

    #[test]
    fn select_moves_cursor_within_bounds() {
        let mut l = SelectList::new("pick", rows(2));
        l.handle_key(KeyCode::Up); // no-op at the top
        assert_eq!(l.cursor, 0);
        l.handle_key(KeyCode::Down);
        l.handle_key(KeyCode::Down); // no-op at the bottom
        assert_eq!(l.cursor, 1);
    }

    #[test]
    fn select_empty_list_confirms_empty() {
        let mut l = SelectList::new("pick", Vec::new());
        assert!(l.handle_key(KeyCode::Enter));
        assert_eq!(l.confirmed, Some(Vec::new()));
    }
}
