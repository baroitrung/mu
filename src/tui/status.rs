//! Status dashboard TUI — port of `internal/status/view.go`.
//!
//! Tick-driven live CPU/RAM/disk/network refresh with alt-screen,
//! health score display, and scan_errors surfacing. Uses the already-
//! ported `Dashboard` state machine from `src/status/mod.rs`.

use crossterm::event::KeyModifiers;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::styles;
use crate::status::{self, Dashboard, human_bytes, human_kb};

/// Default terminal width — Go's `defaultTerm`.
const DEFAULT_TERM: u16 = 80;
const LABEL_WIDTH: usize = 8;
const DISK_LABEL_W: usize = 14;

/// Status dashboard TUI state.
pub struct StatusDashboard {
    pub dashboard: Dashboard,
    pub width: u16,
    pub readers: status::Readers,
    pub show_cores: bool,
    pub show_processes: bool,
}

impl StatusDashboard {
    /// Create a new status dashboard — port of Go's `NewModel`.
    pub fn new() -> Self {
        Self {
            dashboard: Dashboard::new(),
            width: DEFAULT_TERM,
            readers: status::Readers::real(),
            show_cores: false,
            show_processes: false,
        }
    }

    /// Handle a window resize.
    pub fn resize(&mut self, width: u16, _height: u16) {
        self.width = width;
    }

    /// One dashboard tick — delegates to the ported `Dashboard::tick`.
    pub fn tick(&mut self) {
        self.dashboard.tick(&mut self.readers);
    }

    /// Handle a key — q/Q quits, c toggles CPU cores, p toggles top processes.
    pub fn handle_key(&mut self, key: crossterm::event::KeyCode) -> bool {
        use crossterm::event::KeyCode;
        match key {
            KeyCode::Char('q') | KeyCode::Char('Q') => true,
            KeyCode::Char('c') | KeyCode::Char('C') => {
                self.show_cores = !self.show_cores;
                false
            }
            KeyCode::Char('p') | KeyCode::Char('P') => {
                self.show_processes = !self.show_processes;
                false
            }
            _ => false,
        }
    }

    /// Handle a key event (with modifiers). Returns true if done.
    /// Go: q/Q/ctrl+c quit.
    pub fn handle_key_event(&mut self, key: crossterm::event::KeyEvent) -> bool {
        if super::is_ctrl_c(&key) {
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

    /// Render the dashboard — port of Go's `renderDashboard`.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let width = if self.width > 0 {
            self.width as usize
        } else {
            DEFAULT_TERM as usize
        };
        let m = &self.dashboard;
        let mut lines: Vec<Line> = Vec::new();

        // Header
        lines.push(Line::raw(""));
        lines.push(Line::raw(""));
        let header = Line::from(vec![
            Span::styled("  mu status", styles::bold_primary()),
            Span::styled("  ·  live · 1s", styles::faint()),
        ]);
        lines.push(header);
        lines.push(Line::raw(""));

        // Health — Go pre-styles the health text with hColor + bold.
        let h_color = styles::health_color(m.health);
        let health_text = format!("{}/100  {}", m.health, styles::health_label(m.health));
        let health_line = metric_line_styled(
            "Health",
            LABEL_WIDTH,
            m.health as f64,
            h_color,
            &health_text,
            &health_text,
            width,
            Some(h_color),
        );
        lines.push(health_line);
        lines.push(Line::raw(""));

        // CPU
        if !m.cpu_ready {
            lines.push(Line::raw(format!("  {:<8} {}", "CPU", "sampling…")));
        } else {
            let primary = format!("{:.1}%", m.cpu);
            lines.push(metric_line(
                "CPU",
                LABEL_WIDTH,
                m.cpu,
                styles::usage_color(m.cpu),
                &primary,
                &primary,
                width,
            ));
        }

        // CPU Cores (toggle with 'c')
        if self.show_cores && !m.cpu_cores.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::styled("  CPU Cores", styles::faint()));
            for (i, &core_pct) in m.cpu_cores.iter().enumerate() {
                let label = format!("Core {i}");
                let text = format!("{core_pct:.1}%");
                lines.push(metric_line(
                    &label,
                    LABEL_WIDTH,
                    core_pct,
                    styles::usage_color(core_pct),
                    &text,
                    &text,
                    width,
                ));
            }
        }

        // RAM
        if m.mem.total_kb == 0 {
            lines.push(Line::raw(format!("  {:<8} {}", "RAM", "unavailable")));
        } else {
            let used_kb = m.mem.total_kb.saturating_sub(m.mem.available_kb);
            let ram_pct = used_kb as f64 / m.mem.total_kb as f64 * 100.0;
            let primary = format!("{:.0}%", ram_pct);
            let wide = format!(
                "{} / {}  ({})",
                human_kb(used_kb),
                human_kb(m.mem.total_kb),
                primary
            );
            lines.push(metric_line(
                "RAM",
                LABEL_WIDTH,
                ram_pct,
                styles::usage_color(ram_pct),
                &wide,
                &primary,
                width,
            ));
        }

        // Swap
        if m.mem.swap_total_kb == 0 {
            lines.push(Line::raw(format!("  {:<8} {}", "Swap", "none")));
        } else {
            let swap_used = m.mem.swap_total_kb.saturating_sub(m.mem.swap_free_kb);
            let swap_pct = swap_used as f64 / m.mem.swap_total_kb as f64 * 100.0;
            let primary = format!("{:.0}%", swap_pct);
            let wide = format!(
                "{} / {}  ({})",
                human_kb(swap_used),
                human_kb(m.mem.swap_total_kb),
                primary
            );
            lines.push(metric_line(
                "Swap",
                LABEL_WIDTH,
                swap_pct,
                styles::usage_color(swap_pct),
                &wide,
                &primary,
                width,
            ));
        }

        // Disks
        if !m.disks.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::styled("  Disks", styles::faint()));
            for d in &m.disks {
                let used_pct = if d.total_bytes > 0 {
                    (d.total_bytes - d.free_bytes) as f64 / d.total_bytes as f64 * 100.0
                } else {
                    0.0
                };
                let primary = format!("used {:.0}%", used_pct);
                let wide = format!("{}  ·  {} free", primary, human_bytes(d.free_bytes));
                lines.push(metric_line(
                    &d.mount,
                    DISK_LABEL_W,
                    used_pct,
                    styles::usage_color(used_pct),
                    &wide,
                    &primary,
                    width,
                ));
            }
        }

        // Network (active only)
        let mut net_lines: Vec<String> = Vec::new();
        for (iface, rate) in &m.net_rates {
            if rate.rx_bytes_per_sec == 0 && rate.tx_bytes_per_sec == 0 {
                continue;
            }
            net_lines.push(format!(
                "  {:<10}  ↑ {}/s  ↓ {}/s",
                iface,
                human_bytes(rate.tx_bytes_per_sec),
                human_bytes(rate.rx_bytes_per_sec),
            ));
        }
        net_lines.sort();
        if !net_lines.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::styled("  Network", styles::faint()));
            for nl in net_lines {
                lines.push(Line::raw(nl));
            }
        }

        // Top Processes (toggle with 'p')
        if self.show_processes && !m.processes.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::styled("  Top Processes", styles::faint()));
            lines.push(Line::raw(format!(
                "  {:<7} {:<24} {:>10}",
                "PID", "NAME", "MEM"
            )));
            for p in &m.processes {
                lines.push(Line::raw(format!(
                    "  {:<7} {:<24} {:>10}",
                    p.pid,
                    truncate_width(&p.name, 24),
                    human_bytes(p.memory_bytes),
                )));
            }
        }

        // Scan errors (m4 fix: truncate to terminal width like Go's truncateWidth)
        for scan_err in &m.scan_errors {
            lines.push(Line::styled(
                truncate_width(&format!("  Unavailable: {scan_err}"), width),
                styles::faint(),
            ));
        }

        // Footer
        lines.push(Line::raw(""));
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "  q to quit · c cores · p processes",
            styles::faint(),
        ));

        frame.render_widget(Paragraph::new(ratatui::text::Text::from(lines)), area);
    }
}

impl Default for StatusDashboard {
    fn default() -> Self {
        Self::new()
    }
}

/// `metricBar` — renders a solid usage bar at pct (0–100) as styled spans.
/// M2 fix: filled segments use the stress color, empty segments use the
/// empty color — matching Go's `fullStyle`/`emptyStyle` foreground colors.
fn metric_bar_spans(
    pct: f64,
    width: usize,
    full_color: ratatui::style::Color,
) -> Vec<Span<'static>> {
    let width = if width < 1 { 1 } else { width };
    let pct = pct.clamp(0.0, 100.0);
    let mut filled = (pct / 100.0 * width as f64 + 0.5) as usize;
    if filled > width {
        filled = width;
    }
    if pct > 0.0 && filled == 0 {
        filled = 1;
    }
    let empty = width - filled;
    let full_style = ratatui::style::Style::default().fg(full_color);
    let empty_style = ratatui::style::Style::default().fg(styles::EMPTY);
    vec![
        Span::styled("█".repeat(filled), full_style),
        Span::styled("░".repeat(empty), empty_style),
    ]
}

/// `padLabel` — right-pad label to width.
fn pad_label(label: &str, width: usize) -> String {
    let label = truncate_width(label, width);
    let display_width = label.chars().count();
    if display_width >= width {
        label
    } else {
        format!("{}{}", label, " ".repeat(width - display_width))
    }
}

/// `truncateWidth` — truncate string to max display width.
fn truncate_width(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        format!(
            "{}…",
            chars[..max.saturating_sub(1)].iter().collect::<String>()
        )
    }
}

/// `metricLine` — renders a labeled metric line with bar and text as a
/// styled `Line` (M2 fix: colored bar segments).
fn metric_line(
    label: &str,
    preferred_label_width: usize,
    pct: f64,
    color: ratatui::style::Color,
    wide_text: &str,
    primary_text: &str,
    width: usize,
) -> Line<'static> {
    metric_line_styled(
        label,
        preferred_label_width,
        pct,
        color,
        wide_text,
        primary_text,
        width,
        None,
    )
}

/// `metricLine` with optional text styling (M2 fix: health text colored).
/// `text_color` applies the given color+bold to the wide/primary text spans.
#[allow(clippy::too_many_arguments)]
fn metric_line_styled(
    label: &str,
    preferred_label_width: usize,
    pct: f64,
    color: ratatui::style::Color,
    wide_text: &str,
    primary_text: &str,
    width: usize,
    text_color: Option<ratatui::style::Color>,
) -> Line<'static> {
    let width = if width == 0 {
        DEFAULT_TERM as usize
    } else {
        width
    };
    let text_style = text_color.map(|c| {
        ratatui::style::Style::default()
            .fg(c)
            .add_modifier(ratatui::style::Modifier::BOLD)
    });
    let mut label_w = preferred_label_width;
    let max_label_w = width.saturating_sub(3 + primary_text.chars().count());
    if label_w > max_label_w {
        label_w = max_label_w;
    }
    if label_w < 1 {
        label_w = 1;
    }
    let prefix = format!("  {}", pad_label(label, label_w));
    let bar_width = width.saturating_sub(prefix.chars().count() + 3 + wide_text.chars().count());
    let bar_width = if bar_width > 40 { 40 } else { bar_width };
    if bar_width >= 1 {
        let mut spans: Vec<Span<'static>> = Vec::new();
        spans.push(Span::raw(prefix));
        spans.push(Span::raw(" "));
        spans.extend(metric_bar_spans(pct, bar_width, color));
        let text_span = if let Some(style) = text_style {
            Span::styled(format!("  {wide_text}"), style)
        } else {
            Span::raw(format!("  {wide_text}"))
        };
        spans.push(text_span);
        Line::from(spans)
    } else if wide_text != primary_text {
        let bar_width =
            width.saturating_sub(prefix.chars().count() + 3 + primary_text.chars().count());
        let bar_width = if bar_width > 40 { 40 } else { bar_width };
        if bar_width >= 1 {
            let mut spans: Vec<Span<'static>> = Vec::new();
            spans.push(Span::raw(prefix));
            spans.push(Span::raw(" "));
            spans.extend(metric_bar_spans(pct, bar_width, color));
            let text_span = if let Some(style) = text_style {
                Span::styled(format!("  {primary_text}"), style)
            } else {
                Span::raw(format!("  {primary_text}"))
            };
            spans.push(text_span);
            Line::from(spans)
        } else {
            Line::raw(truncate_width(&format!("{prefix} {primary_text}"), width))
        }
    } else {
        Line::raw(truncate_width(&format!("{prefix} {primary_text}"), width))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn metric_bar_basic() {
        let spans = metric_bar_spans(50.0, 10, styles::SUCCESS);
        let s: String = spans.iter().map(|s| s.content.clone()).collect();
        assert!(s.contains("█"));
        assert!(s.contains("░"));
    }

    #[test]
    fn metric_bar_full() {
        let spans = metric_bar_spans(100.0, 10, styles::SUCCESS);
        let s: String = spans.iter().map(|s| s.content.clone()).collect();
        assert_eq!(s, "██████████");
    }

    #[test]
    fn metric_bar_empty() {
        let spans = metric_bar_spans(0.0, 10, styles::SUCCESS);
        let s: String = spans.iter().map(|s| s.content.clone()).collect();
        assert_eq!(s, "░░░░░░░░░░");
    }

    #[test]
    fn pad_label_short() {
        assert_eq!(pad_label("CPU", 8), "CPU     ");
    }

    #[test]
    fn pad_label_exact() {
        assert_eq!(pad_label("Network", 7), "Network");
    }

    #[test]
    fn truncate_width_short() {
        assert_eq!(truncate_width("hello", 10), "hello");
    }

    #[test]
    fn truncate_width_long() {
        let result = truncate_width("hello world", 5);
        assert!(result.ends_with("…"));
        assert!(result.chars().count() <= 5);
    }

    #[test]
    fn render_dashboard_snapshot() {
        let dash = StatusDashboard::new();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| dash.render(f, f.area())).unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(content.contains("mu status"));
        assert!(content.contains("q to quit"));
    }

    #[test]
    fn quit_key() {
        let mut dash = StatusDashboard::new();
        assert!(dash.handle_key(crossterm::event::KeyCode::Char('q')));
    }

    #[test]
    fn non_quit_key() {
        let mut dash = StatusDashboard::new();
        assert!(!dash.handle_key(crossterm::event::KeyCode::Char('x')));
    }

    #[test]
    fn toggle_cores_key() {
        let mut dash = StatusDashboard::new();
        assert!(!dash.show_cores);
        assert!(!dash.handle_key(crossterm::event::KeyCode::Char('c')));
        assert!(dash.show_cores);
        assert!(!dash.handle_key(crossterm::event::KeyCode::Char('c')));
        assert!(!dash.show_cores);
    }

    #[test]
    fn toggle_processes_key() {
        let mut dash = StatusDashboard::new();
        assert!(!dash.show_processes);
        assert!(!dash.handle_key(crossterm::event::KeyCode::Char('p')));
        assert!(dash.show_processes);
        assert!(!dash.handle_key(crossterm::event::KeyCode::Char('p')));
        assert!(!dash.show_processes);
    }
}
