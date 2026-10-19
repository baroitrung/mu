//! TUI module — ratatui+crossterm port of the Go bubbletea/lipgloss UI.
//!
//! Provides the main menu loop, confirm dialog, and run-shell renderer.
//! The actual subcommand logic (clean/optimize/audit/uninstall/status) is
//! called from the business-logic modules; this module only handles the
//! interactive TUI surface (alt-screen, key handling, rendering).
//!
//! Architecture mirrors Go's `runTUI()`:
//! 1. Main menu (alt-screen) → user selects a command
//! 2. Menu exits alt-screen → subcommand runs (confirm → run-shell → summary)
//! 3. Menu re-opens with a transient banner (done/error)
//! 4. Repeat until quit

#![allow(dead_code)]

pub mod analyze;
pub mod audit;
pub mod clean;
pub mod confirm;
pub mod menu;
pub mod run;
pub mod select;
pub mod status;
pub mod styles;
pub mod uninstall;

use std::io::IsTerminal;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use menu::MainMenu;

/// RAII guard that disables raw mode on drop — ensures the terminal is
/// restored even on error/panic paths (C2 fix). Go's bubbletea handles
/// this automatically; ratatui requires manual cleanup.
struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

/// RAII guard that leaves the alternate screen on drop.
struct AltScreenGuard;

impl Drop for AltScreenGuard {
    fn drop(&mut self) {
        let mut stdout = std::io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen);
    }
}

/// Run the main TUI loop — port of Go's `runTUI()`.
///
/// Returns `Ok(())` on clean quit, `Err(...)` on terminal setup failure.
/// Each iteration: show menu → run chosen subcommand → re-show menu with
/// result banner.
pub fn run_tui(debug: bool) -> anyhow::Result<()> {
    let mut cursor = 0usize;
    let mut snapshot: Option<menu::HealthSnapshot> = None;
    let mut banner = String::new();
    let mut banner_err = false;

    loop {
        // Show the menu (alt-screen).
        let final_menu = run_menu_loop(cursor, snapshot.take(), banner.clone(), banner_err)?;

        let chosen = match final_menu.chosen {
            Some(c) if c != "quit" => c,
            _ => return Ok(()),
        };

        cursor = final_menu.cursor;
        banner.clear();
        banner_err = false;

        // Run the chosen subcommand (outside alt-screen).
        let run_result = dispatch_subcommand(chosen, debug);

        // Invalidate the health snapshot so the next menu visit re-reads /proc.
        snapshot = None;

        match run_result {
            Ok(Some(summary)) => {
                // Go: only shows ✅ when summary != "Aborted."; otherwise
                // shows "Clean cancelled." / "Optimize cancelled." (M3 fix).
                if summary != "Aborted." {
                    banner = format!("✅ {summary}");
                } else {
                    banner = format!("{} cancelled.", capitalize(chosen));
                }
            }
            Ok(None) => {
                // Subcommand handled its own output (status, uninstall, audit).
            }
            Err(e) => {
                eprintln!("\nerror: {e}");
                banner = format!("❌ {e}");
                banner_err = true;
            }
        }
    }
}

/// Run the menu loop in alt-screen until the user picks a command or quits.
fn run_menu_loop(
    cursor: usize,
    snapshot: Option<menu::HealthSnapshot>,
    banner: String,
    banner_err: bool,
) -> anyhow::Result<MainMenu> {
    // Alt-screen guard: only enter if stdout is a TTY.
    if !std::io::stdout().is_terminal() {
        return Err(anyhow::anyhow!(
            "could not open a new TTY: open /dev/tty: no such device or address"
        ));
    }

    enable_raw_mode()?;
    // C2 fix: guard ensures raw mode is disabled on any exit path.
    let _raw_guard = RawModeGuard;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    // C2 fix: guard ensures alt-screen is left on any exit path.
    let _alt_guard = AltScreenGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // M4 fix: enter alt-screen with snapshot=None first (shows "Loading…"),
    // then collect health asynchronously. The first frame renders "Loading…"
    // immediately; after a short poll timeout or key, health is collected and
    // the menu re-renders with real data.
    let mut menu = MainMenu::new(cursor, snapshot.clone()).with_banner(banner, banner_err);

    // If no cached snapshot, show "Loading…" then collect.
    let needs_health = snapshot.is_none();
    let mut health_collected = !needs_health;

    loop {
        terminal.draw(|f| menu.render(f, f.area()))?;

        // If we need health and haven't collected yet, poll briefly for a key
        // (non-blocking), then collect health on the first iteration.
        if needs_health && !health_collected {
            // Short poll so the "Loading…" frame is visible briefly.
            if event::poll(std::time::Duration::from_millis(100))? {
                #[allow(clippy::collapsible_if)]
                if let Event::Key(key) = event::read()? {
                    if menu.handle_key_event(key) {
                        break;
                    }
                }
            }
            // Collect health now (1s CPU sample). The "Loading…" frame was
            // already drawn above.
            let snap = menu::MainMenu::collect_health();
            menu.set_snapshot(snap);
            health_collected = true;
            continue;
        }

        if let Event::Key(key) = event::read()? {
            #[allow(clippy::collapsible_if)]
            if menu.handle_key_event(key) {
                break;
            }
        }
        // Resize events are handled by ratatui's Terminal::draw automatically
        // (it reads the current terminal size on each draw). No explicit
        // resize handling needed for the menu since it doesn't cache width.
    }

    Ok(menu)
}

/// Run the status dashboard in its own alt-screen loop (C1 fix).
/// Returns Ok(()) on quit, Err on terminal failure.
pub fn run_status_dashboard() -> anyhow::Result<()> {
    if !std::io::stdout().is_terminal() {
        return Err(anyhow::anyhow!(
            "could not open a new TTY: open /dev/tty: no such device or address"
        ));
    }

    enable_raw_mode()?;
    let _raw_guard = RawModeGuard;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let _alt_guard = AltScreenGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut dash = status::StatusDashboard::new();

    // Initial tick to populate the first frame.
    dash.tick();

    loop {
        terminal.draw(|f| dash.render(f, f.area()))?;

        // Poll for events with a 1s timeout; if no event, tick and redraw.
        if event::poll(std::time::Duration::from_secs(1))? {
            match event::read()? {
                Event::Key(key) => {
                    if dash.handle_key_event(key) {
                        break;
                    }
                }
                Event::Resize(w, h) => {
                    dash.resize(w, h);
                }
                _ => {}
            }
        } else {
            // Timeout — tick the dashboard for live refresh.
            dash.tick();
        }
    }

    Ok(())
}

/// Run the interactive disk analyzer in its own alt-screen loop.
pub fn run_analyze_tui(
    opts: &crate::analyze::Options,
    target: &std::path::Path,
) -> anyhow::Result<()> {
    if !std::io::stdout().is_terminal() {
        return Err(anyhow::anyhow!(
            "could not open a new TTY: open /dev/tty: no such device or address"
        ));
    }

    if let Err(e) = std::fs::metadata(target).or_else(|_| std::fs::symlink_metadata(target)) {
        return Err(anyhow::anyhow!("cannot inspect {}: {e}", target.display()));
    }

    enable_raw_mode()?;
    let _raw_guard = RawModeGuard;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let _alt_guard = AltScreenGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut tui = analyze::AnalyzeTui::new(opts.dry_run);

    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancel_for_worker = cancel.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let target_buf = target.to_path_buf();

    std::thread::spawn(move || {
        let _ = std::panic::catch_unwind(move || {
            let _ = crate::analyze::walker::scan_path(
                &target_buf,
                crate::analyze::walker::WalkerOptions::default(),
                Some(tx),
                cancel_for_worker,
            );
        });
    });

    loop {
        terminal.draw(|f| tui.render(f, f.area()))?;

        while let Ok(msg) = rx.try_recv() {
            tui.apply_message(msg);
        }

        if event::poll(std::time::Duration::from_millis(60))? {
            match event::read()? {
                Event::Key(key) => {
                    if tui.handle_key_event(key) {
                        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                }
                Event::Resize(w, h) => {
                    tui.resize(w, h);
                }
                _ => {}
            }
        } else {
            tui.tick();
        }
    }

    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// Run the uninstall TUI in its own alt-screen loop (C1 fix).
/// Returns Ok(Some(all_items)) — the FULL discovered item list with
/// selected flags, Go's post-`phaseDone` model state — if the user
/// confirmed; Ok(None) if they cancelled/quit; Err on terminal failure.
/// Callers need all_items (not just the selection) because Go passes every
/// discovered package as `installed` to `RemoveSelected` — shared-remnant
/// protection reads the non-selected owners.
pub fn run_uninstall_tui() -> anyhow::Result<Option<Vec<crate::uninstall::model::PkgItem>>> {
    if !std::io::stdout().is_terminal() {
        return Err(anyhow::anyhow!(
            "could not open a new TTY: open /dev/tty: no such device or address"
        ));
    }

    enable_raw_mode()?;
    let _raw_guard = RawModeGuard;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let _alt_guard = AltScreenGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut tui = uninstall::UninstallTui::new();

    // Load packages asynchronously — show spinner while loading (like Go's
    // Init() cmd). Use a background thread; the main loop polls for events
    // and advances the spinner via tick() until loading completes.
    let (tx, rx) = std::sync::mpsc::channel::<uninstall::LoadResult>();
    std::thread::spawn(move || {
        // A panicking worker must still send — else the spinner runs
        // forever with the real error invisible.
        let result = std::panic::catch_unwind(uninstall::UninstallTui::load_packages)
            .unwrap_or_else(|_| uninstall::LoadResult {
                items: Vec::new(),
                discovery_err: Some("internal error: package discovery panicked".into()),
            });
        let _ = tx.send(result);
    });

    loop {
        terminal.draw(|f| tui.render(f, f.area()))?;

        // Check if background loading completed.
        if let Ok(result) = rx.try_recv() {
            tui.apply_load_result(result);
        }

        if event::poll(std::time::Duration::from_millis(80))? {
            match event::read()? {
                Event::Key(key) => {
                    if tui.handle_key_event(key) {
                        break;
                    }
                }
                Event::Resize(w, h) => {
                    tui.resize(w, h);
                }
                _ => {}
            }
        } else {
            // Timeout — advance the spinner.
            tui.tick();
        }
    }

    if tui.phase == uninstall::Phase::Done {
        // The widget's PkgItem mirrors the model's — convert so callers get
        // the business type (full list with selected flags).
        let items = tui
            .all_items
            .into_iter()
            .map(|it| crate::uninstall::model::PkgItem {
                pkg: it.pkg,
                selected: it.selected,
            })
            .collect();
        Ok(Some(items))
    } else {
        Ok(None)
    }
}

/// Dispatch a subcommand by name — runs outside the TUI.
/// Returns `Ok(Some(summary))` for clean/optimize (with a summary string),
/// `Ok(None)` for status/uninstall/audit (which handle their own output),
/// `Err(...)` on failure.
fn dispatch_subcommand(command: &str, debug: bool) -> anyhow::Result<Option<String>> {
    match command {
        "audit" => {
            // Audit: the interactive wizard (scan → select → confirm →
            // apply → rescore) — the same path direct `mu audit` takes.
            let opts = crate::audit::Options {
                report: false,
                json: false,
                dry_run: false,
                debug,
                include: Vec::new(),
            };
            let code = crate::audit::run(&opts);
            // Wizard exits 0 on success, 1 on apply failures (the error was
            // already printed); a nonzero code maps to the ❌ banner like
            // Go's RunE error return.
            if code != 0 {
                anyhow::bail!("audit exited with code {code}");
            }
            Ok(None)
        }
        "clean" => {
            // Clean TTY: show confirm dialog, then run with auto_yes if confirmed.
            let opts = crate::clean::Options {
                dry_run: false,
                debug,
                include: Vec::new(),
                auto_yes: false,
                external: None,
            };
            crate::clean::run_tui(&opts)
                .map(Some)
                .map_err(|e| anyhow::anyhow!("{e}"))
        }
        "optimize" => {
            // Optimize TTY: show confirm dialog, then run with auto_yes if confirmed.
            let opts = crate::optimize::Options {
                dry_run: false,
                debug,
                skip: Vec::new(),
                auto_yes: false,
            };
            crate::optimize::run_tui(&opts)
                .map(Some)
                .map_err(|e| anyhow::anyhow!("{e}"))
        }
        "uninstall" => {
            // Uninstall TTY: Go validates config BEFORE the TUI
            // (LoadWhitelist fail-closed) — abort-fast ordering preserved.
            if let Err(e) = crate::uninstall::preflight() {
                anyhow::bail!("{e}");
            }
            // Run the interactive TUI, then if confirmed, call the
            // business-logic remove with the FULL item list — Go's finish()
            // reads all discovered packages for shared-remnant protection,
            // not just the selection.
            let items = run_uninstall_tui()?;
            if let Some(items) = items {
                if !items.iter().any(|it| it.selected) {
                    println!("Nothing to remove.");
                    return Ok(None);
                }
                let opts = crate::uninstall::Options {
                    dry_run: false,
                    debug,
                    ..Default::default()
                };
                let code = crate::uninstall::run_with_items(&opts, &items);
                if code != 0 {
                    anyhow::bail!("uninstall exited with code {code}");
                }
            }
            Ok(None)
        }
        "status" => {
            // Status TTY: run the live dashboard.
            run_status_dashboard()?;
            Ok(None)
        }
        "purge" => {
            // Purge TTY: the interactive multi-select drives the deletion
            // (review F4); every removal still goes through SafeDelete.
            let opts = crate::purge::Options {
                paths: Vec::new(),
                dry_run: false,
                auto_yes: false,
                include_empty: false,
                debug,
            };
            let code = crate::purge::run(&opts);
            if code != 0 {
                anyhow::bail!("purge exited with code {code}");
            }
            Ok(None)
        }
        "installer" => {
            // Installer TTY: same interactive selection flow as purge.
            let opts = crate::installer::Options {
                paths: Vec::new(),
                dry_run: false,
                auto_yes: false,
                debug,
            };
            let code = crate::installer::run(&opts);
            if code != 0 {
                anyhow::bail!("installer exited with code {code}");
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

/// Capitalize the first letter of a command name for the cancelled banner.
fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Check if a key event is Ctrl+C.
pub fn is_ctrl_c(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// Inline YES/NO confirm dialog (default NO) — used by clean/optimize TUI
/// paths. Enters alt-screen, shows the prompt, returns true if YES selected.
/// On non-TTY, returns false (matching Go's non-interactive default).
pub fn confirm_inline(prompt: &str) -> bool {
    if !std::io::stdout().is_terminal() {
        return false;
    }

    if enable_raw_mode().is_err() {
        return false;
    }
    let _raw_guard = RawModeGuard;
    let mut stdout = std::io::stdout();
    if execute!(stdout, EnterAlternateScreen).is_err() {
        return false;
    }
    let _alt_guard = AltScreenGuard;
    let backend = CrosstermBackend::new(stdout);
    let Ok(mut terminal) = Terminal::new(backend) else {
        return false;
    };

    let mut c = confirm::Confirm::new(prompt);
    loop {
        if terminal.draw(|f| c.render(f, f.area())).is_err() {
            return false;
        }
        let Ok(ev) = event::read() else {
            return false;
        };
        #[allow(clippy::collapsible_if)]
        if let Event::Key(key) = ev {
            if c.handle_key_event(key) {
                break;
            }
        }
    }

    c.result.unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use menu::MENU_ITEMS;

    #[test]
    fn menu_items_count() {
        assert_eq!(MENU_ITEMS.len(), 8);
    }

    #[test]
    fn dispatch_unknown_returns_none() {
        let result = dispatch_subcommand("unknown", false).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn capitalize_works() {
        assert_eq!(capitalize("clean"), "Clean");
        assert_eq!(capitalize("optimize"), "Optimize");
        assert_eq!(capitalize(""), "");
    }

    #[test]
    fn is_ctrl_c_detects() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(is_ctrl_c(&key));
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(!is_ctrl_c(&key));
    }
}
