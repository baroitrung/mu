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

/// Why the interactive selector is not showing a list — the three outcomes a
/// caller must be able to tell apart (review p4r1#1: a user's own cancel was
/// reported as "Non-interactive").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectOutcome {
    /// Enter pressed: the chosen indices, in list order (possibly empty).
    Confirmed(Vec<usize>),
    /// q, Esc or ctrl+c — the user cancelled.
    Cancelled,
    /// No terminal to draw on (stdout or stdin is not a TTY): the user was
    /// never asked, so the caller falls back to its headless `--yes` gate.
    NonInteractive,
    /// A terminal was attached but could not be initialised.
    Unavailable(String),
}

/// What a [`SelectOutcome`] means to the caller that asked for a selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    /// Act on these indices (never empty).
    Proceed(Vec<usize>),
    /// Print `message` (on stderr when `is_error`) and exit with `code`.
    Stop {
        code: i32,
        message: String,
        is_error: bool,
    },
}

/// Resolve a selector outcome into "delete these" or "stop, with this text".
///
/// One home for the decision so a user's cancel, a caller with no terminal and
/// an empty pick can never be reported as one another — the defect in review
/// p4r1#1, where every `None` printed the non-TTY hint.
pub fn resolve(outcome: SelectOutcome) -> Selection {
    match outcome {
        SelectOutcome::Confirmed(indices) if !indices.is_empty() => Selection::Proceed(indices),
        SelectOutcome::Confirmed(_) => Selection::Stop {
            code: 0,
            message: "Nothing selected. Aborted.".to_string(),
            is_error: false,
        },
        SelectOutcome::Cancelled => Selection::Stop {
            code: 0,
            message: "Cancelled. Nothing was deleted.".to_string(),
            is_error: false,
        },
        SelectOutcome::NonInteractive => Selection::Stop {
            code: 0,
            message: "Non-interactive: pass --yes to confirm, or --dry-run to preview.\nAborted."
                .to_string(),
            is_error: false,
        },
        SelectOutcome::Unavailable(reason) => Selection::Stop {
            code: 1,
            message: format!("error: interactive selection unavailable: {reason}"),
            is_error: true,
        },
    }
}

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

/// Interactive multi-select on a TTY. [`SelectOutcome::Confirmed`] carries the
/// chosen indices (possibly empty); a cancel and a missing terminal are
/// distinct outcomes so the caller never mislabels one as the other.
pub fn select_indices(title: &str, rows: Vec<Row>) -> SelectOutcome {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        return SelectOutcome::NonInteractive;
    }
    if rows.is_empty() {
        return SelectOutcome::Confirmed(Vec::new());
    }
    if let Err(e) = enable_raw_mode() {
        return SelectOutcome::Unavailable(format!("raw mode: {e}"));
    }
    let _raw_guard = RawModeGuard;
    let mut stdout = std::io::stdout();
    if let Err(e) = execute!(stdout, EnterAlternateScreen) {
        return SelectOutcome::Unavailable(format!("alternate screen: {e}"));
    }
    let _alt_guard = AltScreenGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(e) => return SelectOutcome::Unavailable(format!("terminal: {e}")),
    };

    let mut list = SelectList::new(title, rows);
    loop {
        if let Err(e) = terminal.draw(|f| list.render(f, f.area())) {
            return SelectOutcome::Unavailable(format!("draw: {e}"));
        }
        let ev = match event::read() {
            Ok(ev) => ev,
            Err(e) => return SelectOutcome::Unavailable(format!("read event: {e}")),
        };
        if let Event::Key(key) = ev
            && list.handle_key_event(key)
        {
            break;
        }
    }
    match list.confirmed {
        Some(indices) => SelectOutcome::Confirmed(indices),
        None => SelectOutcome::Cancelled,
    }
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

    // Review p4r1#1: each "nothing to delete" outcome gets its own message and
    // exit code — a cancel must never read "Non-interactive".
    #[test]
    fn resolve_keeps_cancel_and_non_interactive_apart() {
        let cancel = resolve(SelectOutcome::Cancelled);
        let non_tty = resolve(SelectOutcome::NonInteractive);

        assert_eq!(stop_code(&cancel), 0, "a cancel is not an error");
        assert_eq!(stop_code(&non_tty), 0);

        let cancel_text = resolved_text(&cancel);
        let non_tty_text = resolved_text(&non_tty);
        assert!(cancel_text.contains("Cancelled"), "{cancel_text}");
        assert!(non_tty_text.contains("Non-interactive"), "{non_tty_text}");
        assert!(
            !cancel_text.contains("Non-interactive"),
            "a user's cancel must not be reported as non-interactive: {cancel_text}"
        );
    }

    fn stop_code(s: &Selection) -> i32 {
        match s {
            Selection::Stop { code, .. } => *code,
            Selection::Proceed(_) => panic!("expected a stop"),
        }
    }

    fn resolved_text(s: &Selection) -> String {
        match s {
            Selection::Stop { message, .. } => message.clone(),
            Selection::Proceed(_) => String::new(),
        }
    }

    #[test]
    fn resolve_proceeds_only_on_a_non_empty_pick() {
        assert_eq!(
            resolve(SelectOutcome::Confirmed(vec![2, 0])),
            Selection::Proceed(vec![2, 0])
        );
        match resolve(SelectOutcome::Confirmed(Vec::new())) {
            Selection::Stop { code, message, .. } => {
                assert_eq!(code, 0);
                assert!(message.contains("Nothing selected"), "{message}");
            }
            Selection::Proceed(_) => panic!("an empty pick must not proceed"),
        }
    }

    // A terminal that fails to initialise is a real error: nonzero exit and
    // the reason on stderr, not the non-TTY hint.
    #[test]
    fn resolve_reports_an_unavailable_terminal_as_an_error() {
        match resolve(SelectOutcome::Unavailable("raw mode: nope".into())) {
            Selection::Stop {
                code,
                message,
                is_error,
            } => {
                assert_eq!(code, 1);
                assert!(is_error);
                assert!(message.contains("raw mode: nope"), "{message}");
            }
            Selection::Proceed(_) => panic!("unavailable terminal must not proceed"),
        }
    }
}
