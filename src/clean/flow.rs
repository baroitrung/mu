//! `flow.go` — the clean workflow's state machine: scanning → summary →
//! confirm → running → done. What lives here is the transition and
//! accounting logic the Go suite drives through `Update`: message handling,
//! scan-error aggregation, the YES/NO confirm gate (default NO), per-row
//! run status, abort/failure accounting, and the summary strings.
//!
//! The ratatui driver is [`crate::tui::run_clean_flow`]: it owns the real
//! terminal, interprets [`Cmd`]s (sequential `scan_cmd`/`run_cmd` calls —
//! sudo targets suspend the terminal, Go's `tea.Exec`/`ui.ExecTerminal`
//! equivalent, so sudo's password prompt doesn't fight raw mode), feeds
//! key/resize/tick messages, and renders [`FlowModel::view_lines`] — the
//! ported `View` helpers.

use std::io::IsTerminal;
use std::time::Instant;

use ratatui::text::{Line, Span};

use crate::error::Error;
use crate::tui::styles;
use crate::{oplog, size};

use super::{CleanTarget, Options, join_errors};

/// `interactive` — both stdin and stdout are real terminals. When false
/// (pipes, CI, editors), `run` falls back to plain line output.
pub(crate) fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// `flowState` — the workflow's current screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FlowState {
    Scanning,
    Summary,
    Confirm,
    Running,
    Done,
}

/// `rowStatus`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RowStatus {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

/// `runRow`.
#[derive(Debug)]
pub(crate) struct RunRow {
    pub label: &'static str,
    pub size: i64,
    pub status: RowStatus,
    pub err: Option<Error>,
}

/// `scanTargetMsg` — one target's scan completion. Go keeps `items` even
/// when `previewErr` is set (the error is recorded, the partial preview is
/// shown); `runPlain` does the opposite and drops errored previews.
#[derive(Default)]
pub(crate) struct ScanTargetMsg {
    pub idx: usize,
    pub size: i64,
    pub items: Vec<String>,
    pub err: Option<Error>,
    pub preview_err: Option<Error>,
}

/// `itemDoneMsg` — one target's execute completion.
pub(crate) struct ItemDoneMsg {
    pub idx: usize,
    pub err: Option<Error>,
}

/// The `tea.KeyMsg` strings the model switches on (`msg.String()`),
/// pre-resolved: `"ctrl+c"`, `"q"`/`"esc"`, `"enter"`/`" "`, and the
/// cursor-flip keys `"left"`/`"h"`/`"right"`/`"l"`/`"tab"`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Key {
    CtrlC,
    Quit,
    Select,
    Flip,
}

/// `tea.Msg` — the messages `Update` switches on. `Noop` stands in for the
/// message types the Go `Update` ignores via its default arm.
pub(crate) enum Msg {
    /// `tea.WindowSizeMsg`.
    WindowSize {
        width: usize,
    },
    /// `spinner.TickMsg`.
    SpinnerTick,
    ScanTarget(ScanTargetMsg),
    ItemDone(ItemDoneMsg),
    /// `tea.KeyMsg`.
    Key(Key),
    /// Anything `Update` doesn't handle (Go's default arm → nil cmd).
    Noop,
}

/// `tea.Cmd` — what `update` asks the runtime to do next, in `tea.Batch`
/// argument order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Cmd {
    /// `m.spinner.Tick` — keep the animation alive.
    SpinnerTick,
    /// `m.scanCmd(i)`.
    Scan(usize),
    /// `m.runCmd(i)`.
    Run(usize),
    /// `tea.Quit`.
    Quit,
}

/// `runCmd`'s dispatch decision — the regression guard for the Unikey
/// sudo-password bug. A `RequiresSudo` target in a real (non-dry-run) run
/// goes through `ui.ExecTerminal` (`tea.Exec` releases the terminal so
/// sudo's password prompt doesn't race bubbletea's raw-mode readLoop);
/// non-sudo and dry-run targets keep the plain path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RunDispatch {
    ExecTerminal,
    Plain,
}

/// `flowModel` — `results` drops Go's `scanResult.target` copy —
/// `results[i]` pairs with `targets[i]` because scan cmds run strictly in
/// order. `spinner_idx`/`started` are the driver-facing pieces of Go's
/// `spinner.Model`/`started`: the spinner frame index advances on
/// `SpinnerTick`, and `started` feeds the done view's "Took" line.
pub(crate) struct FlowModel {
    opts: Options,
    targets: Vec<CleanTarget>,
    /// Display labels captured once at construction (`count_fn` may read the
    /// filesystem) so the render path is free of filesystem reads.
    display_labels: Vec<String>,
    /// `state` — pub(crate) so the driver can cheaply test Scanning/Running.
    pub(crate) state: FlowState,
    /// Terminal width from `WindowSize`; default 80.
    width: usize,

    /// `m.spinner` frame index — Go's animated spinner reduces to a frame
    /// counter the view reads.
    spinner_idx: usize,
    /// `started` — set at construction for the done view's "Took Xs".
    started: Instant,

    results: Vec<Scanned>,
    total: i64,
    scan_errs: Vec<Error>,

    /// 0=YES 1=NO; defaults to NO.
    confirm_cursor: usize,
    rows: Vec<RunRow>,
    freed: i64,
    failed: usize,
    /// ctrl+c mid-run: finish current item, skip the rest.
    aborted: bool,

    /// "Freed X" / "Dry run — X reclaimable" / "Aborted." / ...
    /// pub(crate): the driver returns it as `runFlow`'s summary string.
    pub(crate) summary_out: String,
    /// pub(crate): the driver turns it into `runFlow`'s error return.
    pub(crate) run_err: Option<Error>,
}

/// `scanResult` minus the `target` field (see [`FlowModel`]).
struct Scanned {
    size: i64,
    items: Vec<String>,
}

impl FlowModel {
    /// `newFlowModel`.
    pub(crate) fn new(opts: Options, targets: Vec<CleanTarget>) -> Self {
        // Compute the display labels once, not on every render tick (finding 7
        // r1: `display_label` may read the filesystem for the trash count).
        let display_labels = targets.iter().map(CleanTarget::display_label).collect();
        Self {
            opts,
            targets,
            display_labels,
            state: FlowState::Scanning,
            width: 80,
            spinner_idx: 0,
            started: Instant::now(),
            results: Vec::new(),
            total: 0,
            scan_errs: Vec::new(),
            confirm_cursor: 1,
            rows: Vec::new(),
            freed: 0,
            failed: 0,
            aborted: false,
            summary_out: String::new(),
            run_err: None,
        }
    }

    /// `Init` — empty targets short-circuit to Done; otherwise batch the
    /// spinner tick with the first scan.
    pub(crate) fn init(&mut self) -> Vec<Cmd> {
        if self.targets.is_empty() {
            self.state = FlowState::Done;
            self.summary_out = "Nothing to clean.".to_string();
            return Vec::new();
        }
        vec![Cmd::SpinnerTick, Cmd::Scan(0)]
    }

    /// `scanCmd` — scan one target and produce its `ScanTarget` msg. Go
    /// runs this in a command goroutine; here it's a plain call. Go records
    /// `size: sz` unconditionally — the partial total on error.
    pub(crate) fn scan_cmd(&self, i: usize) -> Msg {
        let t = &self.targets[i];
        let mut m = ScanTargetMsg {
            idx: i,
            ..Default::default()
        };
        let (sz, err) = (t.scan)();
        m.size = sz;
        m.err = err;
        if m.err.is_none()
            && let Some(preview) = &t.preview
        {
            let (items, preview_err) = preview();
            m.items = items;
            m.preview_err = preview_err;
        }
        Msg::ScanTarget(m)
    }

    /// `runCmd`'s dispatch decision — see [`RunDispatch`]. Go reads
    /// `m.results[i].target`; `results[i]` pairs with `targets[i]`.
    pub(crate) fn run_dispatch(&self, i: usize) -> RunDispatch {
        let t = &self.targets[i];
        if !self.opts.dry_run && t.requires_sudo {
            RunDispatch::ExecTerminal
        } else {
            RunDispatch::Plain
        }
    }

    /// `runCmd`'s body — execute target `i` and wrap the outcome as the
    /// `ItemDone` msg the goroutine (or `tea.Exec`) would deliver. The
    /// caller consults [`FlowModel::run_dispatch`] for HOW to run it.
    pub(crate) fn run_cmd(&self, i: usize) -> Msg {
        let t = &self.targets[i];
        Msg::ItemDone(ItemDoneMsg {
            idx: i,
            err: (t.execute)(self.opts.dry_run).err(),
        })
    }

    /// `Update`.
    pub(crate) fn update(&mut self, msg: Msg) -> Vec<Cmd> {
        match msg {
            Msg::WindowSize { width } => self.width = width,
            // Keep the spinner animating only while it's visible (scanning
            // and running). On static screens (summary/confirm/done) drop
            // the tick so the view stops re-rendering ~13×/s.
            Msg::SpinnerTick => {
                if matches!(self.state, FlowState::Scanning | FlowState::Running) {
                    self.spinner_idx = (self.spinner_idx + 1) % SPINNER_FRAMES.len();
                    return vec![Cmd::SpinnerTick];
                }
            }
            Msg::ScanTarget(m) => return self.on_scan_target(m),
            Msg::ItemDone(m) => return self.on_item_done(m),
            Msg::Key(k) => return self.handle_key(k),
            Msg::Noop => {}
        }
        Vec::new()
    }

    fn on_scan_target(&mut self, m: ScanTargetMsg) -> Vec<Cmd> {
        debug_assert_eq!(m.idx, self.results.len(), "scan cmds run in order");
        self.results.push(Scanned {
            size: m.size,
            items: m.items,
        });
        let id = self.targets[m.idx].id;
        match m.err {
            Some(e) => self.scan_errs.push(Error::Msg(format!("scan {id}: {e}"))),
            None => {
                self.total += m.size;
                if let Some(e) = m.preview_err {
                    self.scan_errs
                        .push(Error::Msg(format!("preview {id}: {e}")));
                }
            }
        }
        if m.idx + 1 < self.targets.len() {
            return vec![Cmd::Scan(m.idx + 1)];
        }
        self.state = FlowState::Summary;
        Vec::new()
    }

    fn on_item_done(&mut self, m: ItemDoneMsg) -> Vec<Cmd> {
        let target_id = self.targets[m.idx].id;
        {
            let row = &mut self.rows[m.idx];
            match m.err {
                Some(e) => {
                    row.status = RowStatus::Failed;
                    row.err = Some(e);
                    self.failed += 1;
                    oplog::log_outcome("clean", target_id, "failure");
                }
                None => {
                    row.status = RowStatus::Done;
                    self.freed += row.size;
                    oplog::log_outcome(
                        "clean",
                        target_id,
                        if self.opts.dry_run {
                            "dry-run"
                        } else {
                            "success"
                        },
                    );
                }
            }
        }
        let next = m.idx + 1;
        if next < self.rows.len() && !self.aborted {
            self.rows[next].status = RowStatus::Running;
            return vec![Cmd::Run(next)];
        }
        self.finish_run(next);
        Vec::new()
    }

    fn finish_run(&mut self, next: usize) {
        for row in self.rows.iter_mut().skip(next) {
            row.status = RowStatus::Skipped;
        }
        self.state = FlowState::Done;
        if self.aborted {
            self.summary_out = "Aborted.".to_string();
        } else if self.failed > 0 {
            self.run_err = Some(Error::Msg(format!(
                "{} clean target(s) failed",
                self.failed
            )));
            self.summary_out = if self.opts.dry_run {
                format!(
                    "Dry run — {} reclaimable ({} failed)",
                    size::human_size(self.total),
                    self.failed
                )
            } else {
                format!(
                    "Freed {} — {} item(s) failed",
                    size::human_size(self.freed),
                    self.failed
                )
            };
        } else if self.opts.dry_run {
            self.summary_out = format!("Dry run — {} reclaimable", size::human_size(self.total));
        } else {
            self.summary_out = format!("Freed {}", size::human_size(self.freed));
        }
    }

    fn handle_key(&mut self, key: Key) -> Vec<Cmd> {
        match key {
            Key::CtrlC => match self.state {
                // finish the in-flight item, skip the rest
                FlowState::Running => self.aborted = true,
                FlowState::Done => return vec![Cmd::Quit],
                _ => return self.quit_abort(),
            },
            Key::Quit => match self.state {
                // ignore: only ctrl+c cancels a run in progress
                FlowState::Running => {}
                FlowState::Done => return vec![Cmd::Quit],
                _ => return self.quit_abort(),
            },
            Key::Select => match self.state {
                FlowState::Summary => {
                    if !self.scan_errs.is_empty() {
                        self.run_err = Some(join_errors(&self.scan_errs));
                        return vec![Cmd::Quit];
                    }
                    if self.opts.dry_run || self.opts.auto_yes {
                        self.start_run();
                        return vec![Cmd::SpinnerTick, Cmd::Run(0)];
                    }
                    self.state = FlowState::Confirm;
                }
                FlowState::Confirm => {
                    if self.confirm_cursor == 1 {
                        // NO — declined
                        self.summary_out = "Aborted.".to_string();
                        return vec![Cmd::Quit];
                    }
                    self.start_run();
                    return vec![Cmd::SpinnerTick, Cmd::Run(0)];
                }
                FlowState::Done => return vec![Cmd::Quit],
                _ => {}
            },
            Key::Flip => {
                if self.state == FlowState::Confirm {
                    self.confirm_cursor = 1 - self.confirm_cursor;
                }
            }
        }
        Vec::new()
    }

    /// `quitAbort` — exits with a scan-error report when the scan finished
    /// with errors, matching the Enter key's behavior on the error summary;
    /// otherwise records a plain "Aborted." summary.
    fn quit_abort(&mut self) -> Vec<Cmd> {
        if self.state == FlowState::Summary && !self.scan_errs.is_empty() {
            self.run_err = Some(join_errors(&self.scan_errs));
        } else {
            self.summary_out = "Aborted.".to_string();
        }
        vec![Cmd::Quit]
    }

    fn start_run(&mut self) {
        self.rows = self
            .results
            .iter()
            .enumerate()
            .map(|(i, res)| RunRow {
                label: self.targets[i].label,
                size: res.size,
                status: RowStatus::Pending,
                err: None,
            })
            .collect();
        self.rows[0].status = RowStatus::Running;
        self.state = FlowState::Running;
    }

    /// `runCount` — rows that finished cleanly (the aborted view's
    /// "items cleaned" count).
    fn run_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.status == RowStatus::Done)
            .count()
    }

    /// `View` — the ported lipgloss views as styled ratatui lines. Layout
    /// and text match Go byte-for-byte where the styling allows it;
    /// `m.spinner.View()` becomes the advancing frame glyph.
    pub(crate) fn view_lines(&self) -> Vec<Line<'static>> {
        match self.state {
            FlowState::Scanning => self.view_scanning(),
            FlowState::Summary => self.view_summary(),
            FlowState::Confirm => self.view_confirm(),
            FlowState::Running => self.view_running(),
            FlowState::Done => self.view_done(),
        }
    }

    /// `viewScanning` — spinner + "Scanning system (n of m): label".
    fn view_scanning(&self) -> Vec<Line<'static>> {
        let mut s = format!("  {} Scanning system", SPINNER_FRAMES[self.spinner_idx]);
        let done = self.results.len();
        if done > 0 {
            let label = if done < self.display_labels.len() {
                format!(": {}", self.display_labels[done])
            } else {
                String::new()
            };
            s.push_str(&format!("  ({done} of {}){label}", self.targets.len()));
        }
        vec![
            Line::raw(""),
            Line::raw(""),
            Line::styled(s, styles::bold_primary()),
            Line::raw(""),
            Line::raw(""),
            Line::styled("  q or ctrl+c: cancel", styles::faint()),
        ]
    }

    /// `viewSummary` — the CATEGORY/SIZE table + "Potential space to free"
    /// total, then scan errors or the continue hint.
    fn view_summary(&self) -> Vec<Line<'static>> {
        let mut size_w = size::human_size(self.total).chars().count();
        for res in &self.results {
            size_w = size_w.max(size::human_size(res.size).chars().count());
        }
        let mut label_w = self
            .display_labels
            .iter()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0);
        let max = self.width.saturating_sub(size_w + 6);
        if label_w > max && max > 4 {
            label_w = max;
        }
        if label_w < 1 {
            label_w = 1;
        }
        let pad = |display: &str| " ".repeat(label_w.saturating_sub(display.chars().count()) + 2);

        let mut lines = vec![
            Line::raw(""),
            Line::raw(""),
            Line::styled("  Scan complete", styles::bold_primary()),
            Line::raw(""),
            Line::styled(
                format!("  CATEGORY{}{:>size_w$}", pad("CATEGORY"), "SIZE"),
                styles::faint(),
            ),
        ];
        for (i, res) in self.results.iter().enumerate() {
            let label = truncate(&self.display_labels[i], label_w as isize);
            lines.push(Line::raw(format!(
                "  {label}{}{:>size_w$}",
                pad(&label),
                size::human_size(res.size)
            )));
            for item in &res.items {
                lines.push(Line::styled(
                    format!("    - {}", truncate(item, self.width as isize - 8)),
                    styles::faint(),
                ));
            }
        }
        lines.push(Line::raw(format!("  {}", "─".repeat(label_w + size_w + 2))));
        let prefix = "Potential space to free:";
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(prefix, styles::bold_primary()),
            Span::raw(pad(prefix)),
            Span::raw(format!("{:>size_w$}", size::human_size(self.total))),
        ]));
        if self.opts.dry_run {
            lines.push(Line::styled(
                "  DRY RUN — no files will be deleted",
                styles::faint(),
            ));
        }
        if !self.scan_errs.is_empty() {
            for e in &self.scan_errs {
                lines.push(Line::styled(
                    format!(
                        "  {}",
                        styles::mark_error(&truncate(&e.to_string(), self.width as isize - 6))
                    ),
                    styles::faint(),
                ));
            }
            lines.push(Line::raw(""));
            lines.push(Line::raw(""));
            lines.push(Line::raw(""));
            lines.push(Line::styled("  Enter or q: exit", styles::faint()));
            return lines;
        }
        lines.push(Line::raw(""));
        lines.push(Line::raw(""));
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "  Enter: continue  •  q: quit",
            styles::faint(),
        ));
        lines
    }

    /// `viewConfirm` — "Proceed to clean?" + item count + YES/NO buttons.
    fn view_confirm(&self) -> Vec<Line<'static>> {
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
        vec![
            Line::raw(""),
            Line::raw(""),
            Line::styled("  Proceed to clean?", styles::bold_primary()),
            Line::raw(""),
            Line::styled(
                format!(
                    "  {} item(s) — {}",
                    self.results.len(),
                    size::human_size(self.total)
                ),
                styles::faint(),
            ),
            Line::raw(""),
            Line::from(vec![
                Span::raw("  "),
                Span::styled(" YES ", yes_style),
                Span::raw("  "),
                Span::styled(" NO ", no_style),
            ]),
            Line::raw(""),
            Line::raw(""),
            Line::raw(""),
            Line::styled(
                "  ←/→ navigate  •  Enter: confirm  •  q: quit",
                styles::faint(),
            ),
        ]
    }

    /// `viewRunning` — "Cleaning   n of m" + one row per target status.
    fn view_running(&self) -> Vec<Line<'static>> {
        let started = self
            .rows
            .iter()
            .filter(|r| {
                matches!(
                    r.status,
                    RowStatus::Running | RowStatus::Done | RowStatus::Failed
                )
            })
            .count();
        let mut lines = vec![
            Line::raw(""),
            Line::raw(""),
            Line::from(vec![
                Span::styled("  Cleaning", styles::bold_primary()),
                Span::styled(
                    format!("   {started} of {}", self.rows.len()),
                    styles::faint(),
                ),
            ]),
            Line::raw(""),
        ];
        for row in &self.rows {
            let label = truncate(row.label, self.width as isize - 14);
            let sz = size::human_size(row.size);
            let line = match row.status {
                RowStatus::Pending => Line::styled(format!("  {label}  {sz}"), styles::faint()),
                RowStatus::Running => Line::raw(format!(
                    "  {} Cleaning {label}  {sz}",
                    SPINNER_FRAMES[self.spinner_idx]
                )),
                RowStatus::Done => Line::raw(format!(
                    "  {}",
                    styles::mark_success(&format!("Cleaned {label}  {sz}"))
                )),
                RowStatus::Failed => Line::raw(format!(
                    "  {}",
                    styles::mark_error(&format!(
                        "Failed {label} — {}",
                        truncate(
                            &row.err.as_ref().map(|e| e.to_string()).unwrap_or_default(),
                            self.width as isize - 8
                        )
                    ))
                )),
                RowStatus::Skipped => {
                    Line::styled(format!("  Skipped {label} (cancelled)"), styles::faint())
                }
            };
            lines.push(line);
        }
        lines.push(Line::raw(""));
        lines.push(Line::raw(""));
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "  ctrl+c: cancel after current item",
            styles::faint(),
        ));
        lines
    }

    /// `viewDone` — summary + "Took Xs" + per-failure details.
    fn view_done(&self) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::raw(""),
            Line::raw(""),
            Line::styled("  Done", styles::bold_primary()),
            Line::raw(""),
        ];
        if self.aborted {
            lines.push(Line::styled(
                format!(
                    "  Cancelled — {} of {} items cleaned",
                    self.run_count(),
                    self.rows.len()
                ),
                styles::faint(),
            ));
        } else if self.failed > 0 {
            lines.push(Line::raw(format!(
                "  {}",
                styles::mark_error(&self.summary_out)
            )));
        } else {
            lines.push(Line::raw(format!(
                "  {}",
                styles::mark_success(&self.summary_out)
            )));
        }
        lines.push(Line::styled(
            format!("  Took {}", took(self.started)),
            styles::faint(),
        ));
        for row in &self.rows {
            if row.status == RowStatus::Failed {
                lines.push(Line::raw(format!(
                    "  {}",
                    styles::mark_error(&format!(
                        "Failed {} — {}",
                        row.label,
                        row.err.as_ref().map(|e| e.to_string()).unwrap_or_default()
                    ))
                )));
            }
        }
        lines.push(Line::raw(""));
        lines.push(Line::raw(""));
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "  Enter or q: exit  •  rerun: choose Clean in the menu (or `mu clean`)",
            styles::faint(),
        ));
        lines
    }
}

/// `spinnerFrames` — Go's `spinner.Dot` frames (same as the uninstall TUI).
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// `time.Since(started).Round(time.Second).String()` — `<1s` under a
/// second, `Ns` under a minute, `Mm Ss` beyond.
fn took(started: Instant) -> String {
    let secs = started.elapsed().as_secs();
    if secs < 1 {
        "<1s".to_string()
    } else if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{}s", secs / 60, secs % 60)
    }
}

/// `m.truncate` — `ansi.Truncate(s, width, "…")`; `width` < 1 leaves `s`
/// untouched (Go's `width int` goes negative on narrow terminals). Only the
/// views call it; the char-count approximation stands in for cell-width
/// truncation until the tui-port wave brings a real width table.
fn truncate(s: &str, width: isize) -> String {
    if width < 1 {
        return s.to_string();
    }
    let width = width as usize;
    if s.chars().count() <= width {
        return s.to_string();
    }
    let kept: String = s.chars().take(width - 1).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    //! Ports of `flow_test.go`. Pure-view assertions (the `m.View()` calls)
    //! are skipped — views are tui-port work; each ported test notes the
    //! drop. `TestViewScanningProgress` is skipped entirely (view-only).

    use super::*;

    const SIZE_A: i64 = 1 << 20;
    const SIZE_B: i64 = 2 << 20;
    const SIZE_C: i64 = 4 << 20;

    /// `fakeTargets` — three deterministic targets; index >= 0 makes that
    /// target's Scan or Execute fail.
    fn fake_targets(fail_scan: i32, fail_exec: i32) -> Vec<CleanTarget> {
        let sizes = [SIZE_A, SIZE_B, SIZE_C];
        let labels = ["Alpha Cache", "Beta Logs", "Gamma Temp"];
        let ids = ["a", "b", "c"];
        (0..3)
            .map(|i| {
                let scan_fails = i as i32 == fail_scan;
                let exec_fails = i as i32 == fail_exec;
                let sz = sizes[i];
                CleanTarget {
                    id: ids[i],
                    label: labels[i],
                    requires_sudo: false,
                    opt_in: false,
                    scan: Box::new(move || {
                        if scan_fails {
                            (0, Some(Error::Msg("scan boom".to_string())))
                        } else {
                            (sz, None)
                        }
                    }),
                    preview: None,
                    execute: Box::new(move |_dry| {
                        if exec_fails {
                            Err(Error::Msg("exec boom".to_string()))
                        } else {
                            Ok(())
                        }
                    }),
                    count_fn: None,
                }
            })
            .collect()
    }

    fn new_test_flow(opts: Options, fail_scan: i32, fail_exec: i32) -> FlowModel {
        FlowModel::new(opts, fake_targets(fail_scan, fail_exec))
    }

    /// Go test helper `update` — feed msgs, discard cmds (the Go helper
    /// does `mm, _ := m.Update(msg)`). Call `m.update` directly where a
    /// test inspects the returned cmd.
    fn drive(m: &mut FlowModel, msgs: Vec<Msg>) {
        for msg in msgs {
            m.update(msg);
        }
    }

    fn scan_msg(idx: usize, size: i64) -> Msg {
        Msg::ScanTarget(ScanTargetMsg {
            idx,
            size,
            ..Default::default()
        })
    }

    fn item_done(idx: usize) -> Msg {
        Msg::ItemDone(ItemDoneMsg { idx, err: None })
    }

    /// `scanAll` — drive the scan phase with real sizes, ending at Summary.
    fn scan_all(m: &mut FlowModel) {
        for (i, size) in [SIZE_A, SIZE_B, SIZE_C].iter().enumerate() {
            drive(m, vec![scan_msg(i, *size)]);
        }
        assert_eq!(m.state, FlowState::Summary, "after scan");
    }

    /// `confirmYes` — walk summary → confirm → YES.
    fn confirm_yes(m: &mut FlowModel) {
        drive(
            m,
            vec![
                Msg::Key(Key::Select), // summary → confirm
                Msg::Key(Key::Flip),   // default is NO; move to YES
                Msg::Key(Key::Select), // confirm
            ],
        );
    }

    // TestFlowHappyPath — the `m.View()` contains-assertions are Go
    // bubbletea views (tui-port); the state/row/summary assertions port.
    #[test]
    fn flow_happy_path() {
        let mut m = new_test_flow(Options::default(), -1, -1);
        scan_all(&mut m);
        confirm_yes(&mut m);
        assert_eq!(m.state, FlowState::Running, "after YES");
        assert_eq!(m.rows[0].status, RowStatus::Running);

        drive(&mut m, vec![item_done(0)]);
        assert_eq!(m.rows[0].status, RowStatus::Done, "after item 0");
        assert_eq!(m.rows[1].status, RowStatus::Running, "after item 0");

        drive(&mut m, vec![item_done(1), item_done(2)]);
        assert_eq!(m.state, FlowState::Done);
        assert_eq!(m.summary_out, "Freed 7.0 MB");
    }

    // TestFlowDecline.
    #[test]
    fn flow_decline() {
        let mut m = new_test_flow(Options::default(), -1, -1);
        scan_all(&mut m);
        drive(&mut m, vec![Msg::Key(Key::Select)]); // summary → confirm
        let cmds = m.update(Msg::Key(Key::Select)); // confirm with default NO
        assert_eq!(m.summary_out, "Aborted.");
        assert_eq!(cmds, vec![Cmd::Quit]);
    }

    // TestFlowDryRunSkipsConfirm.
    #[test]
    fn flow_dry_run_skips_confirm() {
        let mut m = new_test_flow(
            Options {
                dry_run: true,
                ..Options::default()
            },
            -1,
            -1,
        );
        scan_all(&mut m);
        let cmds = m.update(Msg::Key(Key::Select)); // → running directly
        assert_eq!(m.state, FlowState::Running, "dry-run skips confirm");
        assert_eq!(cmds, vec![Cmd::SpinnerTick, Cmd::Run(0)]);
        drive(&mut m, vec![item_done(0), item_done(1), item_done(2)]);
        assert_eq!(m.summary_out, "Dry run — 7.0 MB reclaimable");
    }

    // TestFlowAutoYesSkipsConfirm.
    #[test]
    fn flow_auto_yes_skips_confirm() {
        let mut m = new_test_flow(
            Options {
                auto_yes: true,
                ..Options::default()
            },
            -1,
            -1,
        );
        scan_all(&mut m);
        let cmds = m.update(Msg::Key(Key::Select)); // → running directly
        assert_eq!(m.state, FlowState::Running, "--yes skips confirm");
        assert_eq!(cmds, vec![Cmd::SpinnerTick, Cmd::Run(0)]);
        drive(&mut m, vec![item_done(0), item_done(1), item_done(2)]);
        assert_eq!(m.summary_out, "Freed 7.0 MB");
    }

    // TestFlowCtrlCCancelsAfterCurrent — the "Cancelled" done-view check is
    // a view assertion (tui-port).
    #[test]
    fn flow_ctrl_c_cancels_after_current() {
        let mut m = new_test_flow(Options::default(), -1, -1);
        scan_all(&mut m);
        confirm_yes(&mut m);
        drive(&mut m, vec![item_done(0)]);
        drive(&mut m, vec![Msg::Key(Key::CtrlC)]); // while item 1 is running
        assert!(m.aborted, "expected aborted after ctrl+c");
        drive(&mut m, vec![item_done(1)]); // the in-flight item finishes
        assert_eq!(m.state, FlowState::Done);
        assert_eq!(m.rows[2].status, RowStatus::Skipped);
        assert_eq!(m.summary_out, "Aborted.");
    }

    // TestFlowFailedItem — the "Failed Beta Logs" done-view check is a view
    // assertion (tui-port).
    #[test]
    fn flow_failed_item() {
        let mut m = new_test_flow(Options::default(), -1, 1);
        scan_all(&mut m);
        confirm_yes(&mut m);
        drive(&mut m, vec![item_done(0)]);
        drive(
            &mut m,
            vec![Msg::ItemDone(ItemDoneMsg {
                idx: 1,
                err: Some(Error::Msg("exec boom".to_string())),
            })],
        );
        assert_eq!(m.rows[1].status, RowStatus::Failed);
        drive(&mut m, vec![item_done(2)]);
        assert_eq!(m.state, FlowState::Done);
        let err = m.run_err.as_ref().expect("expected runErr");
        assert!(
            err.to_string().contains("1 clean target(s) failed"),
            "runErr = {err}, want failure count"
        );
    }

    // TestFlowScanErrorAborts.
    #[test]
    fn flow_scan_error_aborts() {
        let mut m = new_test_flow(Options::default(), 0, -1);
        drive(
            &mut m,
            vec![
                Msg::ScanTarget(ScanTargetMsg {
                    idx: 0,
                    err: Some(Error::Msg("scan boom".to_string())),
                    ..Default::default()
                }),
                scan_msg(1, SIZE_B),
                scan_msg(2, SIZE_C),
            ],
        );
        assert_eq!(m.state, FlowState::Summary);
        assert_eq!(m.scan_errs.len(), 1);
        drive(&mut m, vec![Msg::Key(Key::Select)]);
        assert!(m.run_err.is_some(), "expected runErr after scan failure");
    }

    // TestFlowNarrowWidth — the Go test asserts views still render at
    // width 20 (tui-port). What ports: WindowSize updates the width and the
    // flow completes undisturbed.
    #[test]
    fn flow_narrow_width() {
        let mut m = new_test_flow(Options::default(), -1, -1);
        scan_all(&mut m);
        drive(&mut m, vec![Msg::WindowSize { width: 20 }]);
        assert_eq!(m.width, 20);
        confirm_yes(&mut m);
        drive(&mut m, vec![item_done(0), item_done(1), item_done(2)]);
        assert_eq!(m.state, FlowState::Done);
    }

    // TestFlowInitEmptyTargets.
    #[test]
    fn flow_init_empty_targets() {
        let mut m = new_test_flow(Options::default(), -1, -1);
        m.targets = Vec::new();
        assert!(m.init().is_empty(), "empty targets must not start a scan");
        assert_eq!(m.state, FlowState::Done);
        assert_eq!(m.summary_out, "Nothing to clean.");
    }

    // TestFlowInitStartsScan.
    #[test]
    fn flow_init_starts_scan() {
        let mut m = new_test_flow(Options::default(), -1, -1);
        assert_eq!(
            m.init(),
            vec![Cmd::SpinnerTick, Cmd::Scan(0)],
            "expected the spinner+scan batch cmd for non-empty targets"
        );
    }

    // TestScanCmdRunsTarget.
    #[test]
    fn scan_cmd_runs_target() {
        let m = new_test_flow(Options::default(), -1, -1);
        let Msg::ScanTarget(sm) = m.scan_cmd(1) else {
            panic!("scan_cmd = want ScanTarget msg")
        };
        assert_eq!(sm.idx, 1);
        assert_eq!(sm.size, SIZE_B);
        assert!(sm.err.is_none());
    }

    // TestScanCmdCollectsPreview — Go's Preview returns items AND error;
    // the flow keeps both (runPlain drops errored previews instead).
    #[test]
    fn scan_cmd_collects_preview() {
        let target = CleanTarget {
            id: "x",
            label: "X Cache",
            requires_sudo: false,
            opt_in: false,
            scan: Box::new(|| (SIZE_A, None)),
            preview: Some(Box::new(|| {
                (
                    vec!["/tmp/a".to_string()],
                    Some(Error::Msg("preview boom".to_string())),
                )
            })),
            execute: Box::new(|_| Ok(())),
            count_fn: None,
        };
        let m = FlowModel::new(Options::default(), vec![target]);
        let Msg::ScanTarget(sm) = m.scan_cmd(0) else {
            panic!("scan_cmd = want ScanTarget msg")
        };
        assert_eq!(sm.items, vec!["/tmp/a".to_string()]);
        assert_eq!(
            sm.preview_err.as_ref().map(ToString::to_string).as_deref(),
            Some("preview boom")
        );
    }

    // TestQuitAbortReportsScanErrors.
    #[test]
    fn quit_abort_reports_scan_errors() {
        let mut m = new_test_flow(Options::default(), 0, -1);
        drive(
            &mut m,
            vec![
                Msg::ScanTarget(ScanTargetMsg {
                    idx: 0,
                    err: Some(Error::Msg("scan boom".to_string())),
                    ..Default::default()
                }),
                scan_msg(1, SIZE_B),
                scan_msg(2, SIZE_C),
            ],
        );
        let cmds = m.quit_abort();
        assert!(cmds.contains(&Cmd::Quit), "quitAbort must return tea.Quit");
        let err = m.run_err.as_ref().expect("expected joined scan error");
        assert!(err.to_string().contains("scan boom"), "runErr = {err}");
    }

    // TestQuitAbortPlain.
    #[test]
    fn quit_abort_plain() {
        let mut m = new_test_flow(Options::default(), -1, -1);
        let cmds = m.quit_abort();
        assert!(cmds.contains(&Cmd::Quit), "quitAbort must return tea.Quit");
        assert_eq!(m.summary_out, "Aborted.");
    }

    // TestViewScanningProgress — SKIPPED: pure view test (the "(1 of 3):
    // Beta Logs" progress line is `viewScanning`, tui-port work).

    // TestViewConfirm — the view text is tui-port; the state transition
    // (summary → confirm on Enter) ports.
    #[test]
    fn view_confirm_state() {
        let mut m = new_test_flow(Options::default(), -1, -1);
        scan_all(&mut m);
        drive(&mut m, vec![Msg::Key(Key::Select)]);
        assert_eq!(m.state, FlowState::Confirm);
    }

    // TestTruncateUnclamped.
    #[test]
    fn truncate_unclamped() {
        assert_eq!(truncate("abcdef", 0), "abcdef");
    }

    // TestUpdateSpinnerTickAndUnknownMsgs.
    #[test]
    fn update_spinner_tick_and_unknown_msgs() {
        let mut m = new_test_flow(Options::default(), -1, -1); // scanning: spinner visible
        let cmds = m.update(Msg::SpinnerTick);
        assert_eq!(
            cmds,
            vec![Cmd::SpinnerTick],
            "visible spinner keeps animating"
        );
        let cmds = m.update(Msg::Noop);
        assert!(cmds.is_empty(), "unknown messages are ignored");
        scan_all(&mut m);
        drive(&mut m, vec![Msg::Key(Key::Select)]); // → confirm (static screen)
        let cmds = m.update(Msg::SpinnerTick);
        assert!(cmds.is_empty(), "tick dropped on static screens");
    }

    // TestRunCmdReleasesTerminalForSudo — regression guard for the Unikey
    // sudo-password bug: a RequiresSudo target in a real run dispatches via
    // tea.Exec (terminal released so sudo's prompt doesn't race the
    // raw-mode readLoop). The actual ExecTerminal call is tui-port; the
    // dispatch decision is what the test asserts.
    #[test]
    fn run_cmd_releases_terminal_for_sudo() {
        fn sudo_target() -> CleanTarget {
            CleanTarget {
                id: "apt",
                label: "APT Cache",
                requires_sudo: true,
                opt_in: false,
                scan: Box::new(|| (0, None)),
                preview: None,
                execute: Box::new(|_| Ok(())),
                count_fn: None,
            }
        }
        fn plain_target() -> CleanTarget {
            CleanTarget {
                id: "user-cache",
                label: "User cache",
                requires_sudo: false,
                opt_in: false,
                scan: Box::new(|| (0, None)),
                preview: None,
                execute: Box::new(|_| Ok(())),
                count_fn: None,
            }
        }

        let mut m = FlowModel::new(Options::default(), vec![sudo_target(), plain_target()]);
        m.results = vec![
            Scanned {
                size: SIZE_A,
                items: Vec::new(),
            },
            Scanned {
                size: SIZE_B,
                items: Vec::new(),
            },
        ];

        assert_eq!(
            m.run_dispatch(0),
            RunDispatch::ExecTerminal,
            "sudo target must release the terminal (tea.Exec)"
        );
        assert_eq!(m.run_dispatch(1), RunDispatch::Plain);
        let Msg::ItemDone(done) = m.run_cmd(1) else {
            panic!("non-sudo target run_cmd = want ItemDone msg")
        };
        assert_eq!(done.idx, 1);
        assert!(done.err.is_none());

        // Dry-run never executes sudo, so it must not take the tea.Exec path.
        let mut dry = FlowModel::new(
            Options {
                dry_run: true,
                ..Options::default()
            },
            vec![sudo_target()],
        );
        dry.results = vec![Scanned {
            size: SIZE_A,
            items: Vec::new(),
        }];
        assert_eq!(dry.run_dispatch(0), RunDispatch::Plain);
        let Msg::ItemDone(done) = dry.run_cmd(0) else {
            panic!("dry-run sudo target run_cmd = want ItemDone msg")
        };
        assert!(done.err.is_none());
    }

    // Finding 6: the interactive summary must render the dynamic
    // system-trash item count (`display_label`), not the static label.
    #[test]
    fn summary_shows_system_trash_item_count() {
        let target = CleanTarget {
            id: "system-trash",
            label: "System Trash",
            requires_sudo: false,
            opt_in: true,
            scan: Box::new(|| (SIZE_A, None)),
            preview: None,
            execute: Box::new(|_| Ok(())),
            count_fn: Some(Box::new(|| "System Trash (2 items)".to_string())),
        };
        let mut m = FlowModel::new(Options::default(), vec![target]);
        m.results = vec![Scanned {
            size: SIZE_A,
            items: Vec::new(),
        }];
        m.total = SIZE_A;
        m.state = FlowState::Summary;
        let text: String = m
            .view_lines()
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("System Trash (2 items)"),
            "summary missing dynamic count label: {text:?}"
        );
    }
}
