//! Ratatui ncdu-like interactive disk usage explorer.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent};
use nix::sys::statvfs::statvfs;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::analyze::model::{FileNode, LargestFile, ScanMessage, SortMode, ViewMode};
use crate::paths;
use crate::runner::{CommandSpec, Runner};
use crate::trash;

/// Confirmation state for deletion in the Analyze TUI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DeleteConfirm {
    #[default]
    None,
    Single(PathBuf, u64),
    Batch(Vec<PathBuf>, u64),
}

impl DeleteConfirm {
    pub fn is_none(&self) -> bool {
        matches!(self, DeleteConfirm::None)
    }

    pub fn is_some(&self) -> bool {
        !self.is_none()
    }
}

/// Message emitted by background deletion worker.
#[derive(Debug)]
pub enum DeleteMsg {
    Progress {
        removed: usize,
    },
    Done {
        successful_paths: Vec<PathBuf>,
        fail_count: usize,
        first_error: Option<String>,
    },
}
/// Check if a node qualifies for a [cleanable] badge:
/// 1. Must NOT be covered by any clean root (roots covered by `mu clean`, derived from clean target root list).
/// 2. Must be named `node_modules`, `target`, `.venv`, `dist`, or `CACHEDIR.TAG`,
///    or be a directory containing a `CACHEDIR.TAG` child.
pub fn is_cleanable(node: &FileNode, clean_roots: &[PathBuf]) -> bool {
    for root in clean_roots {
        if node.path == *root || node.path.starts_with(root) {
            return false;
        }
    }

    let name = node.name.as_str();
    if matches!(
        name,
        "node_modules" | "target" | ".venv" | "dist" | "CACHEDIR.TAG"
    ) {
        return true;
    }

    if node.is_dir && node.children.iter().any(|c| c.name == "CACHEDIR.TAG") {
        return true;
    }

    false
}

/// Recursively count cleanable nodes and total size in bytes.
pub fn count_cleanable_nodes(node: &FileNode, clean_roots: &[PathBuf]) -> (u64, usize) {
    let mut bytes = 0u64;
    let mut count = 0usize;
    if is_cleanable(node, clean_roots) {
        bytes += node.size;
        count += 1;
    } else {
        for child in &node.children {
            let (b, c) = count_cleanable_nodes(child, clean_roots);
            bytes += b;
            count += c;
        }
    }
    (bytes, count)
}

/// Scan a directory for regular files older than `older_than_secs` (e.g. 90 days).
pub fn scan_old_files(dir: &Path, older_than_secs: u64) -> (u64, usize) {
    let mut total_bytes = 0u64;
    let mut count = 0usize;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    let now = std::time::SystemTime::now();
    let threshold = std::time::Duration::from_secs(older_than_secs);

    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_file()
            && let Ok(mtime) = meta.modified()
            && let Ok(elapsed) = now.duration_since(mtime)
            && elapsed >= threshold
        {
            total_bytes += meta.len();
            count += 1;
        }
    }
    (total_bytes, count)
}

fn read_preview_lines(
    path: &Path,
    max_lines: usize,
    max_bytes: usize,
) -> std::io::Result<Vec<String>> {
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; max_bytes];
    let n = std::io::Read::read(&mut file, &mut buf)?;
    buf.truncate(n);
    if buf.contains(&0) {
        return Ok(vec![
            "(binary file cannot be previewed as text)".to_string(),
        ]);
    }
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<String> = text
        .lines()
        .take(max_lines)
        .map(ToString::to_string)
        .collect();
    if lines.is_empty() {
        Ok(vec!["(empty file)".to_string()])
    } else {
        Ok(lines)
    }
}

#[derive(Clone, Debug)]
pub struct OverviewLocationItem {
    pub name: &'static str,
    pub path: PathBuf,
    pub size_human: String,
    pub item_count_str: String,
}
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
#[derive(Clone)]
pub struct AnalyzeTui {
    pub root: Option<FileNode>,
    /// Indices from root to current displayed directory node.
    pub path_indices: Vec<usize>,
    pub cursor: usize,
    pub scroll_off: usize,
    pub sort_mode: SortMode,
    pub view_mode: ViewMode,
    pub dry_run: bool,
    /// If not None, inline confirmation is open for target path(s).
    pub confirm_delete: DeleteConfirm,
    /// Whether an asynchronous deletion is currently in flight.
    pub deleting: bool,
    /// Live counter of successfully removed items during deletion.
    pub delete_count: u64,
    /// Total number of target items being deleted.
    pub delete_total: usize,
    /// Channel receiver for async delete progress / completion.
    pub delete_rx: Option<Arc<Mutex<mpsc::Receiver<DeleteMsg>>>>,
    /// Multi-selected paths for batch operations.
    pub selected_paths: HashSet<PathBuf>,
    /// Whether user is actively typing a name filter in Tree view.
    pub filter_mode: bool,
    /// Current search/filter string in Tree view.
    pub filter_text: String,
    /// Whether user is actively typing a filter query in LargestFiles view.
    pub large_filter_mode: bool,
    /// Current search/filter string in LargestFiles view.
    pub large_filter: String,
    pub message: Option<(String, bool)>,
    pub scanning: bool,
    pub spinner_idx: usize,
    pub scan_progress_text: String,
    pub largest_files: Vec<LargestFile>,
    pub window_width: u16,
    pub window_h: u16,
    pub has_overview: bool,
    pub runner: Arc<dyn Runner>,
    pub clean_roots: Vec<PathBuf>,
    pub old_downloads_bytes: u64,
    pub old_downloads_count: usize,
    pub cleanable_cache_bytes: u64,
    pub cleanable_cache_count: usize,
    pub preview_modal: Option<(String, Vec<String>)>,
    pub rescan_requested: bool,
    pub files_scanned: u64,
    pub dirs_scanned: u64,
    pub bytes_scanned: u64,
    pub current_scan_dir: Option<PathBuf>,
    pub last_total_files: Option<u64>,
    pub filesystem_free: Option<u64>,
    pub history: Vec<PathBuf>,
}

impl std::fmt::Debug for AnalyzeTui {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnalyzeTui")
            .field("cursor", &self.cursor)
            .field("view_mode", &self.view_mode)
            .field("has_overview", &self.has_overview)
            .field("scanning", &self.scanning)
            .finish()
    }
}

/// Width of the graphical usage bar in terminal cells.
pub const BAR_WIDTH: usize = 24;
/// Eighth-of-a-cell sub-block characters for the bar.
pub const SUB_CELL_BLOCKS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];

/// Return the terminal display width of a string in cells.
pub fn display_width(s: &str) -> usize {
    Span::raw(s).width()
}

/// Format a number with thousands separators (e.g. 1,234,567).
pub fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(b as char);
    }
    out
}

/// Query free filesystem space in bytes for a given path using statvfs.
pub fn query_filesystem_free(path: &Path) -> Option<u64> {
    match statvfs(path) {
        Ok(stat) => {
            let frsize = stat.fragment_size();
            let bavail = stat.blocks_available();
            Some(frsize.saturating_mul(bavail))
        }
        Err(_) => {
            if let Some(parent) = path.parent() {
                match statvfs(parent) {
                    Ok(stat) => {
                        let frsize = stat.fragment_size();
                        let bavail = stat.blocks_available();
                        Some(frsize.saturating_mul(bavail))
                    }
                    Err(_) => None,
                }
            } else {
                None
            }
        }
    }
}

/// Truncate middle of a string so display width does not exceed `max_width`.
pub fn truncate_middle(s: &str, max_width: usize) -> String {
    let current_width = display_width(s);
    if current_width <= max_width {
        return s.to_string();
    }
    if max_width <= 3 {
        let mut out = String::new();
        let mut w = 0;
        for c in s.chars() {
            let cw = display_width(&c.to_string());
            if w + cw > max_width {
                break;
            }
            out.push(c);
            w += cw;
        }
        return out;
    }
    let target_head_width = (max_width.saturating_sub(3)) / 3;
    let target_tail_width = max_width
        .saturating_sub(3)
        .saturating_sub(target_head_width);

    let mut head = String::new();
    let mut head_width = 0;
    for c in s.chars() {
        let cw = display_width(&c.to_string());
        if head_width + cw > target_head_width {
            break;
        }
        head.push(c);
        head_width += cw;
    }

    let mut tail_chars = Vec::new();
    let mut tail_width = 0;
    for c in s.chars().rev() {
        let cw = display_width(&c.to_string());
        if tail_width + cw > target_tail_width {
            break;
        }
        tail_chars.push(c);
        tail_width += cw;
    }
    tail_chars.reverse();
    let tail: String = tail_chars.into_iter().collect();

    format!("{head}...{tail}")
}

/// Truncate a name to fit `target_width` with ellipsis if necessary, then pad
/// with spaces so its display width is exactly `target_width`.
pub fn trim_and_pad_name(name: &str, target_width: usize) -> String {
    let current_width = display_width(name);
    if current_width == target_width {
        return name.to_string();
    }
    if current_width < target_width {
        let padding = " ".repeat(target_width - current_width);
        return format!("{name}{padding}");
    }

    const ELLIPSIS: &str = "...";
    const ELLIPSIS_WIDTH: usize = 3;
    if target_width <= ELLIPSIS_WIDTH {
        return ELLIPSIS.chars().take(target_width).collect();
    }

    let max_char_width = target_width - ELLIPSIS_WIDTH;
    let mut collected = String::new();
    let mut width = 0;
    let mut buf = [0u8; 4];

    for c in name.chars() {
        let c_str = c.encode_utf8(&mut buf);
        let c_w = display_width(c_str);
        if width + c_w > max_char_width {
            break;
        }
        collected.push(c);
        width += c_w;
    }

    let mut result = format!("{collected}{ELLIPSIS}");
    let final_width = display_width(&result);
    if final_width < target_width {
        result.push_str(&" ".repeat(target_width - final_width));
    }
    result
}

/// Compute the width of the name column from terminal width and index column width.
pub fn calculate_name_width(term_width: usize, index_width: usize) -> usize {
    const FIXED_WIDTH: usize = 65;
    let available = term_width.saturating_sub(FIXED_WIDTH);
    let name_width = available.clamp(24, 60);
    name_width.saturating_sub(index_width.saturating_sub(2))
}

/// Color for a given percentage share of parent:
/// >=50 red, >=20 yellow, >=5 blue, else green.
pub fn bar_color_for_percent(percent: f64) -> Color {
    if percent >= 50.0 {
        Color::Red
    } else if percent >= 20.0 {
        Color::Yellow
    } else if percent >= 5.0 {
        Color::Blue
    } else {
        Color::Green
    }
}

/// Render a 24-cell eighth-block bar colored by share ramp.
pub fn render_bar_span(value: u64, max_value: u64, percent: f64) -> Span<'static> {
    if value == 0 || max_value == 0 {
        return Span::raw(" ".repeat(BAR_WIDTH));
    }

    let bar_color = bar_color_for_percent(percent);
    let ratio = value as f64 / max_value as f64;
    let eighths = (ratio * BAR_WIDTH as f64 * 8.0).round().max(0.0) as usize;
    let full = eighths / 8;
    let remainder = eighths % 8;

    if full >= BAR_WIDTH {
        return Span::styled("█".repeat(BAR_WIDTH), Style::default().fg(bar_color));
    }

    if full == 0 && remainder == 0 {
        return Span::styled(
            format!("{}{}", SUB_CELL_BLOCKS[1], " ".repeat(BAR_WIDTH - 1)),
            Style::default().fg(Color::DarkGray),
        );
    }

    let rem_str = if remainder > 0 {
        SUB_CELL_BLOCKS[remainder]
    } else {
        ""
    };
    let drawn = full + if remainder > 0 { 1 } else { 0 };
    let empty = BAR_WIDTH.saturating_sub(drawn);

    Span::styled(
        format!("{}{}{}", "█".repeat(full), rem_str, " ".repeat(empty)),
        Style::default().fg(bar_color),
    )
}

/// Format a percentage into a 6-character right-aligned column:
/// `%.1f%%`, `< 0.1%` special case, or `  --  ` when unknown.
pub fn format_percent(percent: Option<f64>) -> String {
    const WIDTH: usize = 6;
    let Some(percent) = percent else {
        return "  --  ".to_string();
    };

    if percent > 0.0 && percent < 0.1 {
        "< 0.1%".to_string()
    } else {
        let label = format!("{:.1}%", percent);
        format!("{:>width$}", label, width = WIDTH)
    }
}

/// Format a human-readable size string right-aligned in 10 columns.
pub fn format_size_column(size_human: &str) -> String {
    if let Some(trimmed) = size_human.strip_suffix('+') {
        format!("{:>9}+", trimmed)
    } else {
        format!("{:>10}", size_human)
    }
}

/// Data needed to render a unified analyze row.
#[derive(Clone, Debug)]
pub struct RowItem<'a> {
    pub name: &'a str,
    pub is_dir: bool,
    pub size: u64,
    pub size_human: &'a str,
    pub is_cleanable: bool,
    pub is_checked: bool,
    pub is_cursor: bool,
    pub index: usize,
    pub index_width: usize,
    pub max_size: u64,
    pub parent_size: Option<u64>,
    pub name_width: usize,
    pub has_err: bool,
}

/// Build the styled spans for a row in tree or largest files view.
pub fn row_cells(item: &RowItem) -> Vec<Span<'static>> {
    let cursor_str = if item.is_cursor { "▶ " } else { "  " };
    let cursor_style = if item.is_cursor {
        selected_style()
    } else {
        normal_style()
    };

    let (select_str, select_style) = if item.is_checked {
        ("● ", Style::default().fg(Color::Green))
    } else {
        ("○ ", Style::default().fg(Color::DarkGray))
    };

    let index_str = format!("{:>width$}.", item.index, width = item.index_width);
    let num_style = if item.is_cursor {
        selected_style()
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let percent_val = item.parent_size.map(|ps| {
        if ps > 0 {
            (item.size as f64 / ps as f64) * 100.0
        } else {
            0.0
        }
    });
    let bar_percent = percent_val.unwrap_or(0.0);
    let bar_span = render_bar_span(item.size, item.max_size, bar_percent);

    let sep_span = Span::styled("  |  ", Style::default().fg(Color::DarkGray));

    let icon_str = if item.is_dir { "📁 " } else { "📄 " };
    let icon_style = if item.is_cursor {
        selected_style()
    } else if item.is_dir {
        Style::default().fg(Color::Cyan)
    } else {
        normal_style()
    };

    let padded_name = trim_and_pad_name(item.name, item.name_width);
    let name_style = if item.has_err {
        Style::default().fg(Color::Red)
    } else if item.is_cursor {
        selected_style()
    } else if item.is_checked {
        Style::default().fg(Color::Green)
    } else if item.is_dir {
        Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::BOLD)
    } else {
        normal_style()
    };

    let size_str = format_size_column(item.size_human);
    let size_style = if item.is_cursor {
        selected_style()
    } else {
        Style::default().fg(bar_color_for_percent(bar_percent))
    };

    let mut spans = Vec::with_capacity(12);
    spans.push(Span::styled(cursor_str, cursor_style));
    spans.push(Span::styled(select_str, select_style));
    spans.push(Span::styled(format!("{index_str} "), num_style));
    spans.push(bar_span);

    if let Some(pct) = percent_val {
        spans.push(Span::raw(" "));
        let percent_style = if item.is_cursor {
            selected_style()
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(format_percent(Some(pct)), percent_style));
    }

    spans.push(sep_span);
    spans.push(Span::styled(icon_str, icon_style));
    spans.push(Span::styled(padded_name, name_style));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(size_str, size_style));

    if item.is_cleanable {
        spans.push(Span::styled(
            "  🧹",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    spans
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenAction {
    OpenDir,
    RevealFile,
}

impl AnalyzeTui {
    pub fn new(dry_run: bool) -> Self {
        let clean_roots = crate::clean::clean_target_roots();
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let downloads = home.join("Downloads");
        let (old_bytes, old_count) = scan_old_files(&downloads, 90 * 86400);

        Self {
            root: None,
            path_indices: Vec::new(),
            cursor: 0,
            scroll_off: 0,
            sort_mode: SortMode::SizeDesc,
            view_mode: ViewMode::Tree,
            dry_run,
            confirm_delete: DeleteConfirm::None,
            deleting: false,
            delete_count: 0,
            delete_total: 0,
            delete_rx: None,
            selected_paths: HashSet::new(),
            filter_mode: false,
            filter_text: String::new(),
            large_filter_mode: false,
            large_filter: String::new(),
            message: None,
            scanning: true,
            spinner_idx: 0,
            scan_progress_text: "Starting scan...".into(),
            largest_files: Vec::new(),
            window_width: 80,
            window_h: 24,
            has_overview: false,
            runner: Arc::new(crate::runner::ProcessRunner),
            clean_roots,
            old_downloads_bytes: old_bytes,
            old_downloads_count: old_count,
            cleanable_cache_bytes: 0,
            cleanable_cache_count: 0,
            preview_modal: None,
            rescan_requested: false,
            files_scanned: 0,
            dirs_scanned: 0,
            bytes_scanned: 0,
            current_scan_dir: None,
            last_total_files: None,
            filesystem_free: None,
            history: Vec::new(),
        }
    }

    pub fn filesystem_free_human(&self) -> Option<String> {
        self.filesystem_free
            .map(|b| crate::size::human_size(b as i64))
    }

    pub fn refresh_filesystem_free(&mut self, path: &Path) {
        self.filesystem_free = query_filesystem_free(path);
    }

    pub fn with_runner(mut self, runner: Arc<dyn Runner>) -> Self {
        self.runner = runner;
        self
    }

    pub fn set_runner(&mut self, runner: Arc<dyn Runner>) {
        self.runner = runner;
    }

    pub fn set_overview_mode(&mut self, enabled: bool) {
        self.has_overview = enabled;
        if enabled {
            self.view_mode = ViewMode::Overview;
            self.cursor = 0;
            self.scroll_off = 0;
            if self.filesystem_free.is_none() {
                let home = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."));
                self.refresh_filesystem_free(&home);
            }
        } else {
            self.view_mode = ViewMode::Tree;
        }
    }

    pub fn set_old_downloads(&mut self, bytes: u64, count: usize) {
        self.old_downloads_bytes = bytes;
        self.old_downloads_count = count;
    }

    pub fn update_cleanable_stats(&mut self) {
        if let Some(root) = &self.root {
            let (c_bytes, c_count) = count_cleanable_nodes(root, &self.clean_roots);
            self.cleanable_cache_bytes = c_bytes;
            self.cleanable_cache_count = c_count;
        }
    }

    /// Advance spinner animation and poll async deletion.
    pub fn tick(&mut self) {
        self.spinner_idx = (self.spinner_idx + 1) % SPINNER_FRAMES.len();
        self.poll_delete();
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
                dirs_scanned,
                bytes_scanned,
                current_dir,
            } => {
                let files_scanned = items_scanned.saturating_sub(dirs_scanned);
                self.files_scanned = files_scanned;
                self.dirs_scanned = dirs_scanned;
                self.bytes_scanned = bytes_scanned;
                if self.filesystem_free.is_none() {
                    self.refresh_filesystem_free(&current_dir);
                }
                self.current_scan_dir = Some(current_dir.clone());
                let bytes_human = crate::size::human_size(bytes_scanned as i64);
                let name = current_dir
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| current_dir.display().to_string());
                self.scan_progress_text = format!("{items_scanned} items ({bytes_human}) · {name}");
            }
            ScanMessage::Complete { root, stats } => {
                let (c_bytes, c_count) = count_cleanable_nodes(&root, &self.clean_roots);
                self.cleanable_cache_bytes = c_bytes;
                self.cleanable_cache_count = c_count;
                self.last_total_files = Some(stats.total_files);
                self.refresh_filesystem_free(&root.path);
                self.current_scan_dir = None;
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
    /// Returns true if confirmation is open.
    pub fn is_confirm_open(&self) -> bool {
        self.confirm_delete.is_some()
    }

    /// Returns the children of the current node matching the filter (or all children if no filter).
    /// Each item is a pair of (child_index_in_node_children, &FileNode).
    pub fn filtered_children(&self) -> Vec<(usize, &FileNode)> {
        let Some(node) = self.current_node() else {
            return Vec::new();
        };
        let query = self.filter_text.trim().to_lowercase();
        if query.is_empty() {
            node.children.iter().enumerate().collect()
        } else {
            node.children
                .iter()
                .enumerate()
                .filter(|(_, child)| child.name.to_lowercase().contains(&query))
                .collect()
        }
    }

    /// Returns the largest files matching the large filter (or all largest files if no filter).
    pub fn filtered_largest_files(&self) -> Vec<&LargestFile> {
        let query = self.large_filter.trim().to_lowercase();
        if query.is_empty() {
            self.largest_files.iter().collect()
        } else {
            self.largest_files
                .iter()
                .filter(|file| {
                    let path_str = file.path.to_string_lossy().to_lowercase();
                    let name = file
                        .path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|s| s.to_lowercase())
                        .unwrap_or_default();
                    name.contains(&query) || path_str.contains(&query)
                })
                .collect()
        }
    }

    /// Clamps cursor and scroll_off to valid range for the current view.
    pub fn clamp_cursor(&mut self) {
        let max_items = match self.view_mode {
            ViewMode::Overview => self.overview_locations().len(),
            ViewMode::Tree => self.filtered_children().len(),
            ViewMode::LargestFiles => self.filtered_largest_files().len(),
        };
        if max_items == 0 {
            self.cursor = 0;
            self.scroll_off = 0;
        } else {
            if self.cursor >= max_items {
                self.cursor = max_items - 1;
            }
            let visible_height = self.window_h.saturating_sub(6) as usize;
            if visible_height > 0 && self.cursor >= self.scroll_off + visible_height {
                self.scroll_off = self.cursor.saturating_sub(visible_height) + 1;
            } else if self.cursor < self.scroll_off {
                self.scroll_off = self.cursor;
            }
        }
    }

    /// Returns the currently highlighted child under the cursor in the current view.
    pub fn current_child(&self) -> Option<(usize, &FileNode)> {
        if self.view_mode != ViewMode::Tree {
            return None;
        }
        let items = self.filtered_children();
        if self.cursor < items.len() {
            Some(items[self.cursor])
        } else {
            None
        }
    }

    /// Checks whether a path is protected by system prefixes or user whitelist.
    pub fn is_path_protected(&self, path: &Path) -> bool {
        Self::is_path_protected_static(path)
    }

    /// Standalone path protection check (prefixes + config whitelist).
    pub fn is_path_protected_static(path: &Path) -> bool {
        let abs_path = if path.is_relative() {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        } else {
            path.to_path_buf()
        };
        let clean_path = crate::xdg::clean(&abs_path);

        let as_root = if path.is_relative() {
            let stripped = path.strip_prefix(".").unwrap_or(path);
            let stripped = stripped.strip_prefix("/").unwrap_or(stripped);
            Path::new("/").join(stripped)
        } else {
            path.to_path_buf()
        };
        let clean_as_root = crate::xdg::clean(&as_root);

        if paths::is_protected(&clean_path) || paths::is_protected(&clean_as_root) {
            return true;
        }

        if let Ok(wl) = crate::config::load_whitelist() {
            if crate::whitelist::is_whitelisted(&clean_path, &wl)
                || crate::whitelist::is_whitelisted(&clean_as_root, &wl)
                || crate::whitelist::is_whitelisted(path, &wl)
            {
                return true;
            }
        } else {
            return true;
        }

        false
    }

    /// Handle keyboard input. Returns true if the TUI should exit.
    pub fn handle_key_event(&mut self, key: KeyEvent) -> bool {
        // While a delete is in flight, inputs are locked except the busy view's own quit (Ctrl+C).
        if self.deleting {
            if crate::tui::is_ctrl_c(&key) {
                return true;
            }
            return false;
        }

        // Clear non-persistent message on next key press
        if !self.is_confirm_open() {
            self.message = None;
        }

        // Global abort: Ctrl+C
        if crate::tui::is_ctrl_c(&key) {
            return true;
        }

        // Modal preview dialog handling
        if self.preview_modal.is_some() {
            match key.code {
                KeyCode::Esc
                | KeyCode::Char('q')
                | KeyCode::Char('Q')
                | KeyCode::Char('p')
                | KeyCode::Char('P')
                | KeyCode::Enter => {
                    self.preview_modal = None;
                }
                _ => {}
            }
            return false;
        }

        // Modal confirmation dialog handling
        if self.is_confirm_open() {
            return self.handle_confirm_key(key);
        }

        // Largest files filter typing mode handling
        if self.large_filter_mode {
            match key.code {
                KeyCode::Esc => {
                    self.large_filter.clear();
                    self.large_filter_mode = false;
                    self.clamp_cursor();
                }
                KeyCode::Enter => {
                    self.large_filter_mode = false;
                }
                KeyCode::Backspace => {
                    self.large_filter.pop();
                    self.clamp_cursor();
                }
                KeyCode::Down => {
                    self.navigate_down();
                }
                KeyCode::Up => {
                    self.navigate_up_cursor();
                }
                KeyCode::Char(c) if !c.is_control() => {
                    self.large_filter.push(c);
                    self.clamp_cursor();
                }
                _ => {}
            }
            return false;
        }

        // Filter typing mode handling
        if self.filter_mode {
            match key.code {
                KeyCode::Esc => {
                    self.filter_text.clear();
                    self.filter_mode = false;
                    self.cursor = 0;
                    self.scroll_off = 0;
                }
                KeyCode::Enter => {
                    self.filter_mode = false;
                }
                KeyCode::Backspace => {
                    self.filter_text.pop();
                    self.cursor = 0;
                    self.scroll_off = 0;
                }
                KeyCode::Down => {
                    self.navigate_down();
                }
                KeyCode::Up => {
                    self.navigate_up_cursor();
                }
                KeyCode::Char(c) if !c.is_control() => {
                    self.filter_text.push(c);
                    self.cursor = 0;
                    self.scroll_off = 0;
                }
                _ => {}
            }
            return false;
        }

        // Overview mode handling
        if self.view_mode == ViewMode::Overview {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => {
                    self.navigate_up_cursor();
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => {
                    self.navigate_down();
                }
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('L') => {
                    self.enter_overview_location();
                }
                KeyCode::Char('r') | KeyCode::Char('R') => {
                    self.trigger_rescan();
                }
                KeyCode::Char('o') | KeyCode::Char('O') => {
                    self.trigger_open_dir();
                }
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                    return true;
                }
                _ => {}
            }
            return false;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => {
                if !self.path_indices.is_empty() {
                    self.navigate_up();
                } else if self.has_overview {
                    self.view_mode = ViewMode::Overview;
                    self.cursor = 0;
                    self.scroll_off = 0;
                } else {
                    return true;
                }
            }
            KeyCode::Esc => {
                if self.view_mode == ViewMode::LargestFiles {
                    if !self.large_filter.is_empty() {
                        self.large_filter.clear();
                        self.clamp_cursor();
                    } else {
                        self.view_mode = ViewMode::Tree;
                        self.cursor = 0;
                        self.scroll_off = 0;
                    }
                } else if !self.filter_text.is_empty() {
                    self.filter_text.clear();
                    self.cursor = 0;
                    self.scroll_off = 0;
                } else if !self.path_indices.is_empty() {
                    self.navigate_up();
                } else if self.has_overview {
                    self.view_mode = ViewMode::Overview;
                    self.cursor = 0;
                    self.scroll_off = 0;
                } else {
                    return true;
                }
            }
            KeyCode::Tab | KeyCode::Char('t') | KeyCode::Char('T') => {
                self.view_mode = match self.view_mode {
                    ViewMode::Tree => ViewMode::LargestFiles,
                    ViewMode::LargestFiles => {
                        self.large_filter.clear();
                        self.large_filter_mode = false;
                        ViewMode::Tree
                    }
                    ViewMode::Overview => ViewMode::Overview,
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
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.trigger_rescan();
            }
            KeyCode::Char('o') | KeyCode::Char('O') => {
                self.trigger_open_dir();
            }
            KeyCode::Char('f') | KeyCode::Char('F') => {
                self.trigger_reveal_file();
            }
            KeyCode::Char('p') | KeyCode::Char('P') => {
                self.trigger_preview();
            }
            KeyCode::Char('/') => {
                if self.view_mode == ViewMode::Tree {
                    self.filter_mode = true;
                } else if self.view_mode == ViewMode::LargestFiles {
                    self.large_filter_mode = true;
                }
            }
            KeyCode::Char(' ')
                if self.view_mode == ViewMode::Tree || self.view_mode == ViewMode::LargestFiles =>
            {
                self.toggle_select_current();
            }
            KeyCode::Backspace | KeyCode::Delete if self.view_mode == ViewMode::Tree => {
                if !self.selected_paths.is_empty() {
                    self.trigger_batch_delete();
                } else {
                    self.trigger_delete();
                }
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => {
                self.navigate_down();
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => {
                self.navigate_up_cursor();
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('L') => {
                if self.view_mode == ViewMode::Tree {
                    self.navigate_into();
                }
            }
            KeyCode::Left
            | KeyCode::Char('h')
            | KeyCode::Char('H')
            | KeyCode::Char('b')
            | KeyCode::Char('B') => {
                if self.view_mode == ViewMode::LargestFiles {
                    self.large_filter.clear();
                    self.large_filter_mode = false;
                    self.view_mode = ViewMode::Tree;
                    self.cursor = 0;
                    self.scroll_off = 0;
                } else if self.view_mode == ViewMode::Tree {
                    if !self.path_indices.is_empty() {
                        self.navigate_up();
                    } else if self.has_overview {
                        self.view_mode = ViewMode::Overview;
                        self.cursor = 0;
                        self.scroll_off = 0;
                    }
                }
            }
            KeyCode::Char('d') if self.view_mode == ViewMode::Tree => {
                if !self.selected_paths.is_empty() {
                    self.trigger_batch_delete();
                } else {
                    self.trigger_delete();
                }
            }
            _ => {}
        }

        false
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Enter => {
                self.start_async_delete();
                false
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                self.confirm_delete = DeleteConfirm::None;
                false
            }
            _ => {
                if crate::tui::is_ctrl_c(&key) {
                    return true;
                }
                false
            }
        }
    }

    fn navigate_down(&mut self) {
        let max_items = match self.view_mode {
            ViewMode::Overview => self.overview_locations().len(),
            ViewMode::Tree => self.filtered_children().len(),
            ViewMode::LargestFiles => self.filtered_largest_files().len(),
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
        let Some((orig_idx, child)) = self.current_child() else {
            return;
        };
        if child.is_dir {
            self.path_indices.push(orig_idx);
            self.cursor = 0;
            self.scroll_off = 0;
            self.apply_sort();
        }
    }

    fn navigate_up(&mut self) {
        if let Some(prev_idx) = self.path_indices.pop() {
            self.apply_sort();
            let filtered = self.filtered_children();
            if let Some(display_idx) = filtered
                .iter()
                .position(|(orig_idx, _)| *orig_idx == prev_idx)
            {
                self.cursor = display_idx;
            } else {
                let max_len = filtered.len();
                if self.cursor >= max_len && max_len > 0 {
                    self.cursor = max_len - 1;
                }
            }
            self.scroll_off = 0;
            let visible_height = self.window_h.saturating_sub(6) as usize;
            if visible_height > 0 && self.cursor >= visible_height {
                self.scroll_off = self.cursor - visible_height + 1;
            }
        }
    }

    fn toggle_select_current(&mut self) {
        let target_path = match self.view_mode {
            ViewMode::Tree => {
                let Some((_, child)) = self.current_child() else {
                    return;
                };
                child.path.clone()
            }
            ViewMode::LargestFiles => {
                let filtered = self.filtered_largest_files();
                if self.cursor < filtered.len() {
                    filtered[self.cursor].path.clone()
                } else {
                    return;
                }
            }
            ViewMode::Overview => return,
        };
        if self.is_path_protected(&target_path) {
            self.message = Some((
                format!("Refused: {} is a protected path", target_path.display()),
                true,
            ));
            return;
        }

        if self.selected_paths.contains(&target_path) {
            self.selected_paths.remove(&target_path);
        } else {
            self.selected_paths.insert(target_path);
        }
    }

    fn trigger_delete(&mut self) {
        if self.dry_run {
            self.message = Some(("Deletion disabled in --dry-run mode".into(), true));
            return;
        }

        if self.view_mode != ViewMode::Tree {
            return;
        }

        let Some((_, child)) = self.current_child() else {
            return;
        };

        let target_path = child.path.clone();
        let target_size = child.size;

        if self.is_path_protected(&target_path) {
            self.message = Some((
                format!("Refused: {} is a protected path", target_path.display()),
                true,
            ));
            return;
        }

        self.confirm_delete = DeleteConfirm::Single(target_path, target_size);
    }

    fn trigger_batch_delete(&mut self) {
        if self.view_mode != ViewMode::Tree {
            return;
        }

        if self.selected_paths.is_empty() {
            return;
        }

        let selected: Vec<PathBuf> = self.selected_paths.iter().cloned().collect();
        let targets = drop_descendants(&selected);
        if targets.is_empty() {
            return;
        }

        let total_size: u64 = targets.iter().map(|p| self.find_path_size(p)).sum();

        self.confirm_delete = DeleteConfirm::Batch(targets, total_size);
    }

    fn find_path_size(&self, path: &Path) -> u64 {
        if let Some(root) = &self.root
            && let Some(sz) = find_node_size(root, path)
        {
            return sz;
        }
        std::fs::symlink_metadata(path)
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// Start asynchronous deletion of current confirmed target path(s).
    pub fn start_async_delete(&mut self) {
        let confirm = std::mem::take(&mut self.confirm_delete);
        let targets = match confirm {
            DeleteConfirm::Single(path, _) => vec![path],
            DeleteConfirm::Batch(paths, _) => paths,
            DeleteConfirm::None => return,
        };

        let targets = drop_descendants(&targets);
        if targets.is_empty() {
            self.message = Some(("Nothing to delete".to_string(), false));
            return;
        }

        self.deleting = true;
        self.delete_count = 0;
        self.delete_total = targets.len();

        let (tx, rx) = mpsc::channel();
        self.delete_rx = Some(Arc::new(Mutex::new(rx)));

        let dry_run = self.dry_run;
        std::thread::spawn(move || {
            let mut successful_paths = Vec::new();
            let mut fail_count = 0usize;
            let mut first_error: Option<String> = None;
            let mut removed_count = 0usize;

            for target_path in targets {
                // Re-lstat check
                if let Err(e) = std::fs::symlink_metadata(&target_path) {
                    fail_count += 1;
                    if first_error.is_none() {
                        first_error = Some(format!("{}: {e}", target_path.display()));
                    }
                    continue;
                }

                // Whitelist check
                if AnalyzeTui::is_path_protected_static(&target_path) {
                    fail_count += 1;
                    if first_error.is_none() {
                        first_error = Some(format!(
                            "Refused: {} is a protected path",
                            target_path.display()
                        ));
                    }
                    continue;
                }

                match trash::safe_delete(&target_path, dry_run) {
                    Ok(()) => {
                        successful_paths.push(target_path.clone());
                        removed_count += 1;
                        let _ = tx.send(DeleteMsg::Progress {
                            removed: removed_count,
                        });
                    }
                    Err(e) => {
                        fail_count += 1;
                        if first_error.is_none() {
                            first_error = Some(format!("{}: {e}", target_path.display()));
                        }
                    }
                }
            }

            let _ = tx.send(DeleteMsg::Done {
                successful_paths,
                fail_count,
                first_error,
            });
        });
    }

    /// Poll delete channel for progress or completion messages.
    pub fn poll_delete(&mut self) {
        if !self.deleting {
            return;
        }
        let Some(rx_lock) = self.delete_rx.clone() else {
            return;
        };
        let Ok(rx) = rx_lock.try_lock() else {
            return;
        };
        while let Ok(msg) = rx.try_recv() {
            self.apply_delete_message(msg);
            if !self.deleting {
                break;
            }
        }
    }

    /// Synchronously wait for in-flight deletion to complete.
    pub fn wait_for_delete(&mut self) {
        while self.deleting {
            let Some(rx_lock) = self.delete_rx.clone() else {
                break;
            };
            let Ok(rx) = rx_lock.lock() else {
                break;
            };
            match rx.recv() {
                Ok(msg) => self.apply_delete_message(msg),
                Err(_) => {
                    self.deleting = false;
                    self.delete_rx = None;
                    break;
                }
            }
        }
    }

    /// Apply a message from the background delete worker.
    pub fn apply_delete_message(&mut self, msg: DeleteMsg) {
        match msg {
            DeleteMsg::Progress { removed } => {
                self.delete_count = removed as u64;
            }
            DeleteMsg::Done {
                successful_paths,
                fail_count,
                first_error,
            } => {
                self.deleting = false;
                self.delete_rx = None;

                if !self.dry_run {
                    for path in &successful_paths {
                        if let Some(root) = &mut self.root {
                            remove_from_tree(root, path);
                        }
                        self.selected_paths.remove(path);
                        crate::analyze::cache::invalidate_cache(path);
                    }
                    if !successful_paths.is_empty() {
                        if let Some(root) = &self.root {
                            crate::analyze::cache::invalidate_cache(&root.path);
                        }
                        if let Some(root) = &mut self.root {
                            root.recalculate_sizes();
                        }
                        self.selected_paths.retain(|p| {
                            if let Some(root) = &self.root {
                                find_node(root, p).is_some()
                            } else {
                                false
                            }
                        });
                    }
                }

                let max_len = self.filtered_children().len();
                if self.cursor >= max_len && self.cursor > 0 {
                    self.cursor = max_len.saturating_sub(1);
                }

                let total_attempted = successful_paths.len() + fail_count;
                if fail_count == 0 {
                    let msg = if self.dry_run {
                        if successful_paths.len() == 1 {
                            let name = successful_paths[0]
                                .file_name()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_else(|| successful_paths[0].display().to_string());
                            format!("Moved to trash: {name} (dry run)")
                        } else {
                            format!("Moved {} items to trash (dry run)", successful_paths.len())
                        }
                    } else if successful_paths.len() == 1 {
                        let name = successful_paths[0]
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| successful_paths[0].display().to_string());
                        format!("Moved to trash: {name}")
                    } else {
                        format!("Moved {} items to trash", successful_paths.len())
                    };
                    self.message = Some((msg, false));
                } else if !successful_paths.is_empty() {
                    self.message = Some((
                        format!(
                            "Moved {} items to trash; {fail_count} failed: {}",
                            successful_paths.len(),
                            first_error.unwrap_or_default()
                        ),
                        true,
                    ));
                } else {
                    let msg = if total_attempted == 1 {
                        format!("Delete failed: {}", first_error.unwrap_or_default())
                    } else {
                        format!(
                            "Batch delete failed ({fail_count} errors): {}",
                            first_error.unwrap_or_default()
                        )
                    };
                    self.message = Some((msg, true));
                }
            }
        }
    }

    /// Execute delete synchronously (used in tests and non-interactive workflows).
    pub fn execute_delete(&mut self) {
        self.start_async_delete();
        self.wait_for_delete();
    }

    /// Execute batch delete synchronously (used in tests and non-interactive workflows).
    pub fn execute_batch_delete(&mut self) {
        self.start_async_delete();
        self.wait_for_delete();
    }

    fn calculate_footer_height(&self) -> u16 {
        match &self.confirm_delete {
            DeleteConfirm::Batch(targets, _) if targets.len() > 1 => {
                1 + targets.len().min(5) as u16
            }
            _ => 1,
        }
    }

    /// Render full TUI frame.
    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        self.poll_delete();

        self.window_width = area.width;
        self.window_h = area.height;

        let footer_height = self.calculate_footer_height();

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),             // Header
                Constraint::Min(5),                // Body
                Constraint::Length(footer_height), // Footer / Keybindings / Confirm
            ])
            .split(area);

        self.render_header(f, chunks[0]);

        if self.deleting {
            self.render_deleting(f, chunks[1]);
        } else {
            match self.view_mode {
                ViewMode::Overview => self.render_overview(f, chunks[1]),
                ViewMode::Tree => self.render_tree(f, chunks[1]),
                ViewMode::LargestFiles => self.render_largest(f, chunks[1]),
            }
        }

        self.render_footer(f, chunks[2]);

        if self.preview_modal.is_some() {
            self.render_preview_dialog(f, area);
        }
    }

    /// Contextual footer hint line representing the 8 states of the TUI.
    pub fn footer_hint(&self) -> String {
        if self.view_mode == ViewMode::Overview {
            if !self.history.is_empty() {
                // State 2: overview+history
                "↑↓←→ | Enter | R Refresh | O Open | P Preview | F File | Esc Back | q Quit"
                    .to_string()
            } else {
                // State 1: overview
                "↑↓→ | Enter | R Refresh | O Open | P Preview | F File | Esc/q Quit".to_string()
            }
        } else if self.view_mode == ViewMode::LargestFiles {
            if self.large_filter_mode || self.filter_mode {
                // State 8 (typing): largest-files filter typing
                "Type to filter  |  Enter Apply  |  Esc Clear  |  Ctrl+C Quit".to_string()
            } else if !self.large_filter.is_empty() || !self.filter_text.is_empty() {
                // State 8 (applied): largest-files filter applied
                let del_part = if !self.selected_paths.is_empty() {
                    format!("⌫/Del {}", self.selected_paths.len())
                } else {
                    "⌫/Del".to_string()
                };
                format!(
                    "↑↓← | Space Select | / Edit | Esc Clear filter | O Open | P Preview | F File | {del_part} | q Quit"
                )
            } else {
                // State 7: largest-files idle
                let del_part = if !self.selected_paths.is_empty() {
                    format!("⌫/Del {}", self.selected_paths.len())
                } else {
                    "⌫/Del".to_string()
                };
                format!(
                    "↑↓← | Space Select | / Filter | R Refresh | O Open | P Preview | F File | {del_part} | Esc Back | q Quit"
                )
            }
        } else if self.filter_mode {
            // State 5: tree filter typing
            "Type to filter  |  Enter Apply  |  Esc Clear  |  Ctrl+C Quit".to_string()
        } else if !self.filter_text.is_empty() {
            // State 6: tree filter applied
            let del_part = if !self.selected_paths.is_empty() {
                format!("⌫/Del {}", self.selected_paths.len())
            } else {
                "⌫/Del".to_string()
            };
            format!(
                "↑↓←→ | Enter | Space Select | / Edit | Esc Clear filter | O Open | P Preview | F File | {del_part} | q Quit"
            )
        } else {
            let del_part = if !self.selected_paths.is_empty() {
                format!("⌫/Del {}", self.selected_paths.len())
            } else {
                "⌫/Del".to_string()
            };
            let top_part = if !self.largest_files.is_empty() {
                format!(" | Tab Top {}", self.largest_files.len())
            } else {
                String::new()
            };
            // State 3 (tree idle) or State 4 (tree with a selection)
            format!(
                "↑↓←→ | Space Select | Enter | / Filter | R Refresh | O Open | P Preview | F File | {del_part}{top_part} | Esc Back | q Quit"
            )
        }
    }

    fn build_scan_status_spans(&self, area_width: usize) -> Vec<Span<'static>> {
        let mut status_spans = Vec::new();
        if self.scanning {
            let spinner = SPINNER_FRAMES[self.spinner_idx];
            status_spans.push(Span::styled(
                format!("{spinner} "),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));

            let progress_prefix = if let Some(total) = self.last_total_files {
                if total > 0 {
                    let mut percent =
                        (self.files_scanned as f64 / total as f64 * 100.0).round() as u64;
                    if percent > 100 {
                        percent = 100;
                    }
                    if self.scanning && percent >= 100 {
                        percent = 99;
                    }
                    format!(" {percent}%")
                } else {
                    String::new()
                }
            } else {
                String::new()
            };

            status_spans.push(Span::styled(
                format!("Scanning{progress_prefix}: "),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));

            status_spans.push(Span::styled(
                format!("{} files", format_number(self.files_scanned)),
                Style::default().fg(Color::Yellow),
            ));
            status_spans.push(Span::raw(", "));
            status_spans.push(Span::styled(
                format!("{} dirs", format_number(self.dirs_scanned)),
                Style::default().fg(Color::Yellow),
            ));
            status_spans.push(Span::raw(", "));
            status_spans.push(Span::styled(
                crate::size::human_size(self.bytes_scanned as i64),
                Style::default().fg(Color::Green),
            ));
            status_spans.push(Span::raw("  "));
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

        if self.view_mode != ViewMode::Overview {
            status_spans.push(Span::styled(
                format!("[Sort: {}]  ", self.sort_mode.label()),
                Style::default().fg(Color::Cyan),
            ));
        }

        let view_label = match self.view_mode {
            ViewMode::Overview => "[View: Overview]  ",
            ViewMode::Tree => "[View: Tree]  ",
            ViewMode::LargestFiles => "[View: Largest Files]  ",
        };
        status_spans.push(Span::styled(
            view_label,
            Style::default().fg(Color::Magenta),
        ));

        if self.view_mode == ViewMode::LargestFiles {
            if self.large_filter_mode || !self.large_filter.is_empty() {
                let matches = self.filtered_largest_files().len();
                let cursor_indicator = if self.large_filter_mode { "▌" } else { "" };
                status_spans.push(Span::styled(
                    format!(
                        "[Filter: \"{}\"{cursor_indicator} ({matches} matches)]  ",
                        self.large_filter
                    ),
                    Style::default().fg(Color::Yellow),
                ));
            }
        } else if self.filter_mode || !self.filter_text.is_empty() {
            let matches = self.filtered_children().len();
            let cursor_indicator = if self.filter_mode { "▌" } else { "" };
            status_spans.push(Span::styled(
                format!(
                    "[Filter: \"{}\"{cursor_indicator} ({matches} matches)]  ",
                    self.filter_text
                ),
                Style::default().fg(Color::Yellow),
            ));
        }

        if !self.selected_paths.is_empty() {
            status_spans.push(Span::styled(
                format!("[Selected: {}]  ", self.selected_paths.len()),
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        if self.scanning
            && let Some(dir) = &self.current_scan_dir
        {
            let status_w: usize = status_spans.iter().map(|s| display_width(&s.content)).sum();
            let short_path = dir.display().to_string();
            let remaining = area_width.saturating_sub(status_w + 2);
            if remaining >= 12 {
                let truncated = truncate_middle(&short_path, remaining);
                status_spans.push(Span::styled(
                    truncated,
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }

        status_spans
    }

    fn render_header(&self, f: &mut Frame, area: Rect) {
        if self.view_mode == ViewMode::Overview {
            let status_spans = self.build_scan_status_spans(area.width as usize);

            let mut title_spans = vec![Span::styled(
                "Disk Usage Overview",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )];
            if let Some(free_str) = self.filesystem_free_human() {
                title_spans.push(Span::styled(
                    format!("  ({free_str} free)"),
                    Style::default().fg(Color::DarkGray),
                ));
            }

            let lines = vec![Line::from(title_spans), Line::from(status_spans)];

            let block = Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(Color::DarkGray));
            let paragraph = Paragraph::new(lines).block(block);
            f.render_widget(paragraph, area);
            return;
        }

        let current_path = self
            .current_node()
            .map(|n| n.path.display().to_string())
            .unwrap_or_else(|| {
                self.current_scan_dir
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "...".into())
            });

        let status_spans = self.build_scan_status_spans(area.width as usize);

        let mut path_spans = vec![
            Span::styled("Path: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                current_path,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ];

        if (!self.scanning || self.root.is_some())
            && let Some(node) = self.current_node()
        {
            path_spans.push(Span::styled(
                format!("  ({}, {} items)", node.size_human, node.item_count),
                Style::default().fg(Color::Green),
            ));
        }

        if let Some(free_str) = self.filesystem_free_human() {
            path_spans.push(Span::styled(
                format!("  ({free_str} free)"),
                Style::default().fg(Color::DarkGray),
            ));
        }

        let lines = vec![Line::from(path_spans), Line::from(status_spans)];

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

        let items = self.filtered_children();
        if items.is_empty() {
            let msg = if !self.filter_text.trim().is_empty() {
                "No matching files or directories."
            } else {
                "(empty directory)"
            };
            let p = Paragraph::new(Line::from(msg)).style(Style::default().fg(Color::DarkGray));
            f.render_widget(p, area);
            return;
        }

        let visible_height = area.height as usize;
        let parent_size = node.size.max(1);
        let total_items = items.len();
        let index_width = total_items.to_string().len().max(2);
        let max_size = items.iter().map(|(_, c)| c.size).max().unwrap_or(1).max(1);
        let name_width = calculate_name_width(area.width as usize, index_width);

        let mut lines = Vec::new();
        for (display_idx, (_orig_idx, child)) in items
            .into_iter()
            .enumerate()
            .skip(self.scroll_off)
            .take(visible_height)
        {
            let is_cursor = display_idx == self.cursor;
            let is_checked = self.selected_paths.contains(&child.path);
            let name_display = if child.is_dir {
                format!("{}/", child.name)
            } else {
                child.name.clone()
            };
            let is_clean = is_cleanable(child, &self.clean_roots);

            let row_item = RowItem {
                name: &name_display,
                is_dir: child.is_dir,
                size: child.size,
                size_human: &child.size_human,
                is_cleanable: is_clean,
                is_checked,
                is_cursor,
                index: display_idx + 1,
                index_width,
                max_size,
                parent_size: Some(parent_size),
                name_width,
                has_err: child.err.is_some(),
            };

            lines.push(Line::from(row_cells(&row_item)));
        }

        let p = Paragraph::new(lines);
        f.render_widget(p, area);
    }

    fn render_largest(&self, f: &mut Frame, area: Rect) {
        let filtered = self.filtered_largest_files();
        if filtered.is_empty() {
            let msg = if !self.large_filter.is_empty() {
                format!("  No matches for \"{}\"", self.large_filter)
            } else {
                "No large files discovered yet.".to_string()
            };
            let p = Paragraph::new(Line::from(msg)).style(Style::default().fg(Color::DarkGray));
            f.render_widget(p, area);
            return;
        }

        let visible_height = area.height as usize;
        let total_items = filtered.len();
        let index_width = total_items.to_string().len().max(2);
        let max_size = filtered.first().map(|f| f.size).unwrap_or(1).max(1);
        let name_width = calculate_name_width(area.width as usize, index_width);

        let mut lines = Vec::new();

        for (i, file) in filtered
            .iter()
            .enumerate()
            .skip(self.scroll_off)
            .take(visible_height)
        {
            let is_cursor = i == self.cursor;
            let path_str = file.path.display().to_string();

            let row_item = RowItem {
                name: &path_str,
                is_dir: false,
                size: file.size,
                size_human: &file.size_human,
                is_cleanable: false,
                is_checked: self.selected_paths.contains(&file.path),
                is_cursor,
                index: i + 1,
                index_width,
                max_size,
                parent_size: None,
                name_width,
                has_err: false,
            };

            lines.push(Line::from(row_cells(&row_item)));
        }

        let p = Paragraph::new(lines);
        f.render_widget(p, area);
    }

    fn render_footer(&self, f: &mut Frame, area: Rect) {
        if self.confirm_delete.is_some() {
            self.render_confirm_footer(f, area);
            return;
        }

        if self.deleting {
            let p = Paragraph::new(Line::from(vec![Span::styled(
                "Ctrl+C Quit",
                Style::default().fg(Color::DarkGray),
            )]));
            f.render_widget(p, area);
            return;
        }

        if let Some((text, is_err)) = &self.message {
            let color = if *is_err { Color::Red } else { Color::Green };
            let p = Paragraph::new(Line::from(vec![Span::styled(
                text.as_str(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )]));
            f.render_widget(p, area);
            return;
        }

        let hint = self.footer_hint();
        let p = Paragraph::new(Line::from(vec![Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        )]));
        f.render_widget(p, area);
    }

    fn render_confirm_footer(&self, f: &mut Frame, area: Rect) {
        let max_w = area.width as usize;
        let mut lines = Vec::new();

        match &self.confirm_delete {
            DeleteConfirm::Single(target, size) => {
                let name = target
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| target.display().to_string());
                let size_str = crate::size::human_size(*size as i64);

                let prefix = "Delete: ";
                let suffix = "  Press Enter to confirm  |  Esc cancel";
                let fixed_w =
                    display_width(prefix) + display_width(suffix) + 2 + display_width(&size_str);

                let name_disp = if max_w > fixed_w {
                    truncate_middle(&name, max_w.saturating_sub(fixed_w))
                } else if max_w >= display_width(prefix) + 10 {
                    truncate_middle(&name, 8)
                } else {
                    name
                };

                let line = Line::from(vec![
                    Span::styled(
                        "Delete:",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        format!("{name_disp}, {size_str}"),
                        Style::default().fg(Color::White),
                    ),
                    Span::styled(suffix, Style::default().fg(Color::DarkGray)),
                ]);
                lines.push(line);
            }
            DeleteConfirm::Batch(targets, total_size) => {
                let count = targets.len();
                let size_str = crate::size::human_size(*total_size as i64);
                let suffix = "  Press Enter to confirm  |  Esc cancel";

                if count <= 1 {
                    let name = targets
                        .first()
                        .and_then(|p| p.file_name())
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| {
                            targets
                                .first()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default()
                        });

                    let prefix = "Delete: ";
                    let fixed_w = display_width(prefix)
                        + display_width(suffix)
                        + 2
                        + display_width(&size_str);
                    let name_disp = if max_w > fixed_w {
                        truncate_middle(&name, max_w.saturating_sub(fixed_w))
                    } else {
                        name
                    };

                    let line = Line::from(vec![
                        Span::styled(
                            "Delete:",
                            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" "),
                        Span::styled(
                            format!("{name_disp}, {size_str}"),
                            Style::default().fg(Color::White),
                        ),
                        Span::styled(suffix, Style::default().fg(Color::DarkGray)),
                    ]);
                    lines.push(line);
                } else {
                    let first_line = Line::from(vec![
                        Span::styled(
                            "Delete:",
                            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" "),
                        Span::styled(
                            format!("{count} items, {size_str}"),
                            Style::default().fg(Color::White),
                        ),
                        Span::styled(suffix, Style::default().fg(Color::DarkGray)),
                    ]);
                    lines.push(first_line);

                    for target in targets.iter().take(5) {
                        let name = target
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| target.display().to_string());
                        let path_display = target.display().to_string();

                        let bullet = "  • ";
                        let bullet_w = display_width(bullet);
                        let name_w = display_width(&name);
                        let paren_extra = 3; // " ()"
                        let budget = max_w.saturating_sub(bullet_w + name_w + paren_extra);

                        let path_span = if budget >= 10 {
                            let trunc_path = truncate_middle(&path_display, budget);
                            Span::styled(
                                format!(" ({trunc_path})"),
                                Style::default().fg(Color::DarkGray),
                            )
                        } else {
                            Span::raw("")
                        };

                        let item_line = Line::from(vec![
                            Span::raw(bullet),
                            Span::styled(name, Style::default().fg(Color::Yellow)),
                            path_span,
                        ]);
                        lines.push(item_line);
                    }
                }
            }
            DeleteConfirm::None => return,
        }

        let p = Paragraph::new(lines);
        f.render_widget(p, area);
    }

    fn render_deleting(&self, f: &mut Frame, area: Rect) {
        let spinner = SPINNER_FRAMES[self.spinner_idx];
        let mut spans = Vec::new();
        spans.push(Span::styled(
            format!("{spinner} "),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            "Deleting: ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));

        if self.delete_count > 0 {
            spans.push(Span::styled(
                format_number(self.delete_count),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                " items removed, please wait…",
                Style::default().fg(Color::White),
            ));
        } else {
            spans.push(Span::styled(
                "moving to Trash, please wait…",
                Style::default().fg(Color::White),
            ));
        }

        let lines = vec![Line::from(""), Line::from(spans)];
        let p = Paragraph::new(lines);
        f.render_widget(p, area);
    }

    fn enter_overview_location(&mut self) {
        let locs = self.overview_locations();
        if self.cursor >= locs.len() {
            return;
        }
        let loc_name = locs[self.cursor].name;
        let loc_path = locs[self.cursor].path.clone();
        self.history.push(loc_path.clone());
        self.refresh_filesystem_free(&loc_path);
        self.view_mode = ViewMode::Tree;
        self.cursor = 0;
        self.scroll_off = 0;
        self.path_indices.clear();

        match loc_name {
            "Home" => {}
            "Downloads" => {
                if let Some(root) = &self.root
                    && let Some(idx) = root
                        .children
                        .iter()
                        .position(|c| c.name == "Downloads" || c.path == loc_path)
                {
                    self.path_indices.push(idx);
                    self.apply_sort();
                }
            }
            "User Cache" => {
                if let Some(root) = &self.root
                    && let Some(idx) = root
                        .children
                        .iter()
                        .position(|c| c.name == ".cache" || c.path == loc_path)
                {
                    self.path_indices.push(idx);
                    self.apply_sort();
                }
            }
            "Root System" => {
                if let Some(root) = &self.root
                    && root.path != Path::new("/")
                {
                    self.trigger_rescan();
                }
            }
            _ => {}
        }
    }

    pub fn trigger_rescan(&mut self) {
        let target_opt = self.current_node().map(|n| n.path.clone());
        if let Some(target) = target_opt {
            crate::analyze::cache::invalidate_cache(&target);
            self.refresh_filesystem_free(&target);
        } else if let Some(root_path) = self.root.as_ref().map(|r| r.path.clone()) {
            crate::analyze::cache::invalidate_cache(&root_path);
            self.refresh_filesystem_free(&root_path);
        }
        if self.view_mode == ViewMode::Overview {
            let locs = self.overview_locations();
            if self.cursor < locs.len() {
                let loc_path = locs[self.cursor].path.clone();
                crate::analyze::cache::invalidate_cache(&loc_path);
                self.refresh_filesystem_free(&loc_path);
            }
        }
        self.files_scanned = 0;
        self.dirs_scanned = 0;
        self.bytes_scanned = 0;
        self.current_scan_dir = None;
        self.scanning = true;
        self.rescan_requested = true;
        self.scan_progress_text = "Rescanning...".to_string();
        self.message = Some(("Rescanning...".to_string(), false));
    }

    fn run_open_action(&mut self, action: OpenAction) {
        if self.view_mode == ViewMode::Overview {
            if action == OpenAction::RevealFile {
                return;
            }
            let locs = self.overview_locations();
            if self.cursor >= locs.len() {
                return;
            }
            let target_dir = locs[self.cursor].path.clone();

            if std::env::var_os("DISPLAY").is_none()
                && std::env::var_os("WAYLAND_DISPLAY").is_none()
            {
                self.message = Some((
                    "Cannot open GUI: neither DISPLAY nor WAYLAND_DISPLAY is set".to_string(),
                    true,
                ));
                return;
            }

            let spec = CommandSpec::new("xdg-open", [target_dir.as_os_str()]);
            match self.runner.run(&spec) {
                Ok(_) => {
                    self.message = Some((format!("Opened {}", target_dir.display()), false));
                }
                Err(e) => {
                    self.message = Some((
                        format!("Failed to open {}: {e}", target_dir.display()),
                        true,
                    ));
                }
            }
            return;
        }

        if !self.selected_paths.is_empty() {
            let count = self.selected_paths.len();
            if count > 20 {
                let verb = match action {
                    OpenAction::OpenDir => "open",
                    OpenAction::RevealFile => "reveal",
                };
                self.message = Some((
                    format!("Too many items to {verb}, max 20, selected {count}"),
                    true,
                ));
                return;
            }

            if std::env::var_os("DISPLAY").is_none()
                && std::env::var_os("WAYLAND_DISPLAY").is_none()
            {
                self.message = Some((
                    "Cannot open GUI: neither DISPLAY nor WAYLAND_DISPLAY is set".to_string(),
                    true,
                ));
                return;
            }

            let mut paths: Vec<PathBuf> = self.selected_paths.iter().cloned().collect();
            paths.sort();

            let mut errors = Vec::new();
            for path in paths {
                let target = match action {
                    OpenAction::OpenDir => {
                        if path.is_dir() {
                            path
                        } else {
                            path.parent().unwrap_or(&path).to_path_buf()
                        }
                    }
                    OpenAction::RevealFile => path,
                };
                let spec = CommandSpec::new("xdg-open", [target.as_os_str()]);
                if let Err(e) = self.runner.run(&spec) {
                    errors.push(format!("{}: {e}", target.display()));
                }
            }

            if errors.is_empty() {
                let verb = match action {
                    OpenAction::OpenDir => "Opened",
                    OpenAction::RevealFile => "Revealed",
                };
                self.message = Some((format!("{verb} {count} items"), false));
            } else {
                let verb = match action {
                    OpenAction::OpenDir => "open",
                    OpenAction::RevealFile => "reveal",
                };
                self.message = Some((
                    format!(
                        "Failed to {verb} {}/{count} items: {}",
                        errors.len(),
                        errors.join(", ")
                    ),
                    true,
                ));
            }
            return;
        }

        // Single cursor item
        let target = match self.view_mode {
            ViewMode::Overview => return,
            ViewMode::Tree => {
                if let Some((_, child)) = self.current_child() {
                    match action {
                        OpenAction::OpenDir => {
                            if child.is_dir {
                                child.path.clone()
                            } else {
                                child.path.parent().unwrap_or(&child.path).to_path_buf()
                            }
                        }
                        OpenAction::RevealFile => child.path.clone(),
                    }
                } else if let Some(node) = self.current_node() {
                    node.path.clone()
                } else {
                    return;
                }
            }
            ViewMode::LargestFiles => {
                let filtered = self.filtered_largest_files();
                if self.cursor < filtered.len() {
                    let file = &filtered[self.cursor];
                    match action {
                        OpenAction::OpenDir => {
                            file.path.parent().unwrap_or(&file.path).to_path_buf()
                        }
                        OpenAction::RevealFile => file.path.clone(),
                    }
                } else {
                    return;
                }
            }
        };

        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            self.message = Some((
                "Cannot open GUI: neither DISPLAY nor WAYLAND_DISPLAY is set".to_string(),
                true,
            ));
            return;
        }

        let spec = CommandSpec::new("xdg-open", [target.as_os_str()]);
        match self.runner.run(&spec) {
            Ok(_) => {
                let verb = match action {
                    OpenAction::OpenDir => "Opened",
                    OpenAction::RevealFile => "Revealed",
                };
                self.message = Some((format!("{verb} {}", target.display()), false));
            }
            Err(e) => {
                let verb = match action {
                    OpenAction::OpenDir => "open",
                    OpenAction::RevealFile => "reveal",
                };
                self.message = Some((format!("Failed to {verb} {}: {e}", target.display()), true));
            }
        }
    }

    fn trigger_open_dir(&mut self) {
        self.run_open_action(OpenAction::OpenDir);
    }

    fn trigger_reveal_file(&mut self) {
        self.run_open_action(OpenAction::RevealFile);
    }

    fn trigger_preview(&mut self) {
        let target_path = match self.view_mode {
            ViewMode::Overview => return,
            ViewMode::Tree => {
                if let Some((_, child)) = self.current_child() {
                    if child.is_dir {
                        self.message = Some(("Cannot preview a directory".to_string(), true));
                        return;
                    }
                    child.path.clone()
                } else {
                    return;
                }
            }
            ViewMode::LargestFiles => {
                let filtered = self.filtered_largest_files();
                if self.cursor < filtered.len() {
                    filtered[self.cursor].path.clone()
                } else {
                    return;
                }
            }
        };

        let filename = target_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| target_path.display().to_string());

        let lines = match read_preview_lines(&target_path, 50, 8192) {
            Ok(lines) => lines,
            Err(err) => vec![format!("(cannot preview: {err})")],
        };

        self.preview_modal = Some((filename, lines));
    }

    pub fn overview_locations(&self) -> Vec<OverviewLocationItem> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let downloads = home.join("Downloads");
        let cache = crate::xdg::cache_home();
        let root_dir = PathBuf::from("/");

        // Home size & items
        let (home_size, home_items) = if let Some(root) = &self.root {
            if root.path == home {
                (
                    root.size_human.clone(),
                    format!("{} items", root.item_count),
                )
            } else {
                ("—".into(), "—".into())
            }
        } else {
            ("—".into(), "—".into())
        };

        // Downloads size & items
        let (dl_size, dl_items) = if let Some(root) = &self.root {
            if let Some(dl) = root
                .children
                .iter()
                .find(|c| c.name == "Downloads" || c.path == downloads)
            {
                (dl.size_human.clone(), format!("{} items", dl.item_count))
            } else {
                ("—".into(), "—".into())
            }
        } else {
            ("—".into(), "—".into())
        };

        // Cache size & items
        let (cache_size, cache_items) = if let Some(root) = &self.root {
            if let Some(c) = root
                .children
                .iter()
                .find(|child| child.name == ".cache" || child.path == cache)
            {
                (c.size_human.clone(), format!("{} items", c.item_count))
            } else {
                ("—".into(), "—".into())
            }
        } else {
            ("—".into(), "—".into())
        };

        // Root filesystem size
        let (root_size, root_items) = {
            let (disks, _) = crate::status::proc::read_disk_full();
            if let Some(d) = disks.into_iter().find(|d| d.mount == "/") {
                let used = d.total_bytes.saturating_sub(d.free_bytes);
                (
                    crate::size::human_size(used as i64),
                    format!("Total: {}", crate::size::human_size(d.total_bytes as i64)),
                )
            } else {
                ("—".into(), "Root filesystem".into())
            }
        };

        vec![
            OverviewLocationItem {
                name: "Home",
                path: home,
                size_human: home_size,
                item_count_str: home_items,
            },
            OverviewLocationItem {
                name: "Downloads",
                path: downloads,
                size_human: dl_size,
                item_count_str: dl_items,
            },
            OverviewLocationItem {
                name: "User Cache",
                path: cache,
                size_human: cache_size,
                item_count_str: cache_items,
            },
            OverviewLocationItem {
                name: "Root System",
                path: root_dir,
                size_human: root_size,
                item_count_str: root_items,
            },
        ]
    }

    pub fn overview_insights(&self) -> Vec<String> {
        let mut insights = Vec::new();

        // 1. Old downloads (>90 days) pointer to mu installer
        if self.old_downloads_count > 0 {
            insights.push(format!(
                "• Downloads older than 90 days: {} ({} files) → run 'mu installer' to clean old installers",
                crate::size::human_size(self.old_downloads_bytes as i64),
                self.old_downloads_count
            ));
        } else {
            insights.push(
                "• Downloads older than 90 days: none (0 files) → run 'mu installer' to check for installers".to_string(),
            );
        }

        // 2. Cleanable build artifacts
        if self.cleanable_cache_count > 0 {
            insights.push(format!(
                "• Cleanable project artifacts: {} ({} directories: node_modules, target, .venv, dist)",
                crate::size::human_size(self.cleanable_cache_bytes as i64),
                self.cleanable_cache_count
            ));
        } else {
            insights.push("• Cleanable project artifacts: none discovered".to_string());
        }

        // 3. System trash
        let data_home = crate::xdg::data_home();
        let trash_count = crate::clean::targets::count_trash_items(&data_home);
        let trash_size = crate::clean::targets::system_trash_size(&data_home);
        if trash_count > 0 {
            insights.push(format!(
                "• System trash: {} ({} items) → run 'mu clean' to empty trash",
                crate::size::human_size(trash_size),
                trash_count
            ));
        }

        insights
    }

    fn render_overview(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(8), // Locations
                Constraint::Min(6),    // Insights
            ])
            .split(area);

        // 1. Locations
        let locs = self.overview_locations();
        let mut loc_lines = Vec::new();
        for (i, loc) in locs.iter().enumerate() {
            let is_cursor = i == self.cursor;
            let prefix = if is_cursor { "▶ " } else { "  " };
            let style = if is_cursor {
                selected_style()
            } else {
                normal_style()
            };

            let line = Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(
                    format!("{:<14} ", loc.name),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:<10} ", loc.size_human),
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled(format!("{:<30} ", loc.path.display()), style),
                Span::styled(
                    loc.item_count_str.as_str(),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            loc_lines.push(line);
        }

        let loc_block = Block::default()
            .title(" Locations ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        f.render_widget(Paragraph::new(loc_lines).block(loc_block), chunks[0]);

        // 2. Hidden-Space Insights
        let insights = self.overview_insights();
        let mut insight_lines = vec![Line::from("")];
        for ins in insights {
            insight_lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(ins, Style::default().fg(Color::White)),
            ]));
            insight_lines.push(Line::from(""));
        }

        let ins_block = Block::default()
            .title(" Hidden-Space Insights ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        f.render_widget(Paragraph::new(insight_lines).block(ins_block), chunks[1]);
    }

    fn render_preview_dialog(&self, f: &mut Frame, area: Rect) {
        let Some((filename, lines)) = &self.preview_modal else {
            return;
        };
        let dialog_area = centered_rect(70, 60, area);
        f.render_widget(Clear, dialog_area);

        let block = Block::default()
            .title(format!(" Preview: {filename} "))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));

        let mut content_lines: Vec<Line> = lines
            .iter()
            .map(|l| Line::from(Span::styled(l.as_str(), Style::default().fg(Color::White))))
            .collect();

        content_lines.push(Line::from(""));
        content_lines.push(Line::from(Span::styled(
            "Esc / q / p to close",
            Style::default().fg(Color::DarkGray),
        )));

        let paragraph = Paragraph::new(content_lines).block(block);
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

/// Drops any path that is a descendant of another path in the slice.
pub fn drop_descendants(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut result = Vec::new();
    for path in paths {
        let is_descendant = paths.iter().any(|other| {
            if other == path {
                return false;
            }
            let path_clean = crate::xdg::clean(path);
            let other_clean = crate::xdg::clean(other);
            paths::path_contains(&other_clean, &path_clean)
        });
        if !is_descendant {
            result.push(path.clone());
        }
    }
    result.sort();
    result.dedup();
    result
}

fn remove_from_tree(node: &mut FileNode, path: &Path) -> bool {
    let clean_target = crate::xdg::clean(path);
    if let Some(pos) = node
        .children
        .iter()
        .position(|c| c.path == path || crate::xdg::clean(&c.path) == clean_target)
    {
        node.children.remove(pos);
        return true;
    }
    for child in &mut node.children {
        if child.is_dir
            && (paths::path_contains(&child.path, path)
                || paths::path_contains(&crate::xdg::clean(&child.path), &clean_target))
            && remove_from_tree(child, path)
        {
            return true;
        }
    }
    false
}

fn find_node<'a>(node: &'a FileNode, path: &Path) -> Option<&'a FileNode> {
    if node.path == path || crate::xdg::clean(&node.path) == crate::xdg::clean(path) {
        return Some(node);
    }
    for child in &node.children {
        if child.path == path || crate::xdg::clean(&child.path) == crate::xdg::clean(path) {
            return Some(child);
        }
        if child.is_dir
            && (paths::path_contains(&child.path, path)
                || paths::path_contains(&crate::xdg::clean(&child.path), &crate::xdg::clean(path)))
            && let Some(n) = find_node(child, path)
        {
            return Some(n);
        }
    }
    None
}

fn find_node_size(node: &FileNode, path: &Path) -> Option<u64> {
    if node.path == path || crate::xdg::clean(&node.path) == crate::xdg::clean(path) {
        return Some(node.size);
    }
    for child in &node.children {
        if child.path == path || crate::xdg::clean(&child.path) == crate::xdg::clean(path) {
            return Some(child.size);
        }
        if child.is_dir
            && (paths::path_contains(&child.path, path)
                || paths::path_contains(&crate::xdg::clean(&child.path), &crate::xdg::clean(path)))
            && let Some(sz) = find_node_size(child, path)
        {
            return Some(sz);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::FakeRunner;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn dummy_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::empty())
    }

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mu-test-analyze-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
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

    #[test]
    fn test_relative_protected_path_prevents_delete() {
        let mut tui = AnalyzeTui::new(false);
        let mut root = FileNode::new("root".into(), PathBuf::from("."), 100, true, 1);
        let etc = FileNode::new("etc".into(), PathBuf::from("etc"), 100, true, 1);
        root.children = vec![etc];
        tui.root = Some(root);

        tui.handle_key_event(dummy_key(KeyCode::Char('d')));
        assert!(
            tui.confirm_delete.is_none(),
            "relative protected path should not trigger delete confirmation"
        );
        assert!(tui.message.unwrap().0.contains("protected path"));
    }

    #[test]
    fn test_delete_in_largest_files_view_is_prevented() {
        let mut tui = AnalyzeTui::new(false);
        let mut root = FileNode::new("root".into(), PathBuf::from("/tmp/root"), 100, true, 1);
        let c = FileNode::new(
            "unrelated.txt".into(),
            PathBuf::from("/tmp/root/unrelated.txt"),
            100,
            false,
            1,
        );
        root.children = vec![c];
        tui.root = Some(root);
        tui.largest_files = vec![crate::analyze::model::LargestFile::new(
            PathBuf::from("/tmp/other/big.dat"),
            1000,
        )];
        tui.view_mode = crate::analyze::model::ViewMode::LargestFiles;
        tui.cursor = 0;

        tui.handle_key_event(dummy_key(KeyCode::Char('d')));
        assert!(
            tui.confirm_delete.is_none(),
            "deletion must not be triggered from largest files view"
        );
    }

    #[test]
    fn test_select_toggle() {
        let mut tui = AnalyzeTui::new(false);
        let mut root = FileNode::new("root".into(), PathBuf::from("/tmp/root"), 200, true, 2);
        let f1 = FileNode::new(
            "f1.txt".into(),
            PathBuf::from("/tmp/root/f1.txt"),
            100,
            false,
            1,
        );
        let f2 = FileNode::new(
            "f2.txt".into(),
            PathBuf::from("/tmp/root/f2.txt"),
            100,
            false,
            1,
        );
        root.children = vec![f1, f2];
        tui.root = Some(root);

        assert!(tui.selected_paths.is_empty());

        // Select first row with Space
        tui.handle_key_event(dummy_key(KeyCode::Char(' ')));
        assert_eq!(tui.selected_paths.len(), 1);
        assert!(
            tui.selected_paths
                .contains(&PathBuf::from("/tmp/root/f1.txt"))
        );

        // Untoggle first row with Space
        tui.handle_key_event(dummy_key(KeyCode::Char(' ')));
        assert_eq!(tui.selected_paths.len(), 0);
        assert!(
            !tui.selected_paths
                .contains(&PathBuf::from("/tmp/root/f1.txt"))
        );
    }

    #[test]
    fn test_whitelisted_refusal() {
        let mut tui = AnalyzeTui::new(false);
        let mut root = FileNode::new("root".into(), PathBuf::from("/"), 200, true, 2);
        let etc = FileNode::new("etc".into(), PathBuf::from("/etc"), 100, true, 1);
        let safe = FileNode::new(
            "safe.txt".into(),
            PathBuf::from("/tmp/safe.txt"),
            100,
            false,
            1,
        );
        root.children = vec![etc, safe];
        tui.root = Some(root);

        // Attempt to select protected /etc with Space
        tui.cursor = 0;
        tui.handle_key_event(dummy_key(KeyCode::Char(' ')));
        assert!(
            tui.selected_paths.is_empty(),
            "whitelisted path must not be selected"
        );
        assert!(
            tui.message.as_ref().unwrap().0.contains("protected path"),
            "should display refusal message for protected path"
        );

        // Relative protected path (e.g. "etc")
        let mut tui_rel = AnalyzeTui::new(false);
        let mut root_rel = FileNode::new("root".into(), PathBuf::from("."), 100, true, 1);
        let rel_etc = FileNode::new("etc".into(), PathBuf::from("etc"), 100, true, 1);
        root_rel.children = vec![rel_etc];
        tui_rel.root = Some(root_rel);

        tui_rel.cursor = 0;
        tui_rel.handle_key_event(dummy_key(KeyCode::Char(' ')));
        assert!(
            tui_rel.selected_paths.is_empty(),
            "relative protected path must not be selected"
        );
        assert!(
            tui_rel
                .message
                .as_ref()
                .unwrap()
                .0
                .contains("protected path")
        );
    }

    #[test]
    fn test_ancestor_dedupe() {
        // Test drop_descendants helper directly
        let paths = vec![
            PathBuf::from("/tmp/root/dir"),
            PathBuf::from("/tmp/root/dir/file.txt"),
            PathBuf::from("/tmp/root/dir/sub/nested.txt"),
            PathBuf::from("/tmp/root/standalone.txt"),
        ];
        let deduped = drop_descendants(&paths);
        assert_eq!(
            deduped,
            vec![
                PathBuf::from("/tmp/root/dir"),
                PathBuf::from("/tmp/root/standalone.txt"),
            ]
        );

        // Test in AnalyzeTui execution
        let d = tempdir("dedupe");
        let sub = d.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let file1 = sub.join("f1.txt");
        let file2 = d.join("f2.txt");
        std::fs::write(&file1, b"hello").unwrap();
        std::fs::write(&file2, b"world").unwrap();

        let mut tui = AnalyzeTui::new(false);
        let mut root = FileNode::new("d".into(), d.clone(), 200, true, 2);
        let mut sub_node = FileNode::new("sub".into(), sub.clone(), 100, true, 1);
        let f1_node = FileNode::new("f1.txt".into(), file1.clone(), 5, false, 1);
        sub_node.children = vec![f1_node];
        let f2_node = FileNode::new("f2.txt".into(), file2.clone(), 5, false, 1);
        root.children = vec![sub_node, f2_node];
        tui.root = Some(root);

        // Select ancestor (sub) and descendant (sub/f1.txt)
        tui.selected_paths.insert(sub.clone());
        tui.selected_paths.insert(file1.clone());

        // Trigger batch delete
        tui.handle_key_event(dummy_key(KeyCode::Backspace));
        assert!(tui.confirm_delete.is_some());
        // Batch confirm targets must contain only sub
        if let DeleteConfirm::Batch(targets, _) = &tui.confirm_delete {
            assert_eq!(targets.len(), 1);
            assert_eq!(targets[0], sub);
        } else {
            panic!("expected DeleteConfirm::Batch");
        }

        // Confirm YES (direct Enter in D1)
        tui.handle_key_event(dummy_key(KeyCode::Enter));
        tui.wait_for_delete();

        assert!(!sub.exists(), "ancestor dir should be trashed");
        assert!(!file1.exists(), "child inside trashed dir should be gone");
        assert!(file2.exists(), "unrelated file should remain intact");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn test_esc_cancel_means_no_change() {
        let d = tempdir("no-change");
        let f1 = d.join("file1.txt");
        let f2 = d.join("file2.txt");
        std::fs::write(&f1, b"data1").unwrap();
        std::fs::write(&f2, b"data2").unwrap();

        let mut tui = AnalyzeTui::new(false);
        let mut root = FileNode::new("d".into(), d.clone(), 20, true, 2);
        let n1 = FileNode::new("file1.txt".into(), f1.clone(), 10, false, 1);
        let n2 = FileNode::new("file2.txt".into(), f2.clone(), 10, false, 1);
        root.children = vec![n1, n2];
        tui.root = Some(root);

        tui.selected_paths.insert(f1.clone());
        tui.selected_paths.insert(f2.clone());

        // Press Delete to open batch confirm
        tui.handle_key_event(dummy_key(KeyCode::Delete));
        assert!(tui.confirm_delete.is_some());

        // Press Esc to cancel
        tui.handle_key_event(dummy_key(KeyCode::Esc));
        assert!(tui.confirm_delete.is_none());
        assert_eq!(
            tui.selected_paths.len(),
            2,
            "selections must remain untouched on Esc cancel"
        );
        assert!(f1.exists(), "file1 must not be deleted");
        assert!(f2.exists(), "file2 must not be deleted");

        // Re-open confirm and press 'q' to cancel
        tui.handle_key_event(dummy_key(KeyCode::Backspace));
        assert!(tui.confirm_delete.is_some());
        tui.handle_key_event(dummy_key(KeyCode::Char('q')));
        assert!(tui.confirm_delete.is_none());
        assert_eq!(
            tui.selected_paths.len(),
            2,
            "selections must remain untouched on 'q' cancel"
        );
        assert!(f1.exists(), "file1 must not be deleted");
        assert!(f2.exists(), "file2 must not be deleted");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn test_partial_failure_continues() {
        let d = tempdir("partial-fail");
        let valid_file = d.join("valid.txt");
        let ghost_file = d.join("nonexistent_ghost.txt");
        std::fs::write(&valid_file, b"content").unwrap();

        let mut tui = AnalyzeTui::new(false);
        let mut root = FileNode::new("d".into(), d.clone(), 20, true, 2);
        let valid_node = FileNode::new("valid.txt".into(), valid_file.clone(), 10, false, 1);
        let ghost_node = FileNode::new(
            "nonexistent_ghost.txt".into(),
            ghost_file.clone(),
            10,
            false,
            1,
        );
        root.children = vec![valid_node, ghost_node];
        tui.root = Some(root);

        tui.selected_paths.insert(ghost_file.clone());
        tui.selected_paths.insert(valid_file.clone());

        // Trigger batch delete
        tui.handle_key_event(dummy_key(KeyCode::Backspace));
        assert!(tui.confirm_delete.is_some());

        // Confirm YES (direct Enter)
        tui.handle_key_event(dummy_key(KeyCode::Enter));
        tui.wait_for_delete();

        // The valid file must be deleted despite ghost failure
        assert!(!valid_file.exists(), "valid file must have been trashed");
        assert!(
            tui.message.as_ref().unwrap().0.contains("failed"),
            "summary must report partial failure"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn test_filter_backspace_does_not_delete() {
        let mut tui = AnalyzeTui::new(false);
        let mut root = FileNode::new("root".into(), PathBuf::from("/tmp/root"), 100, true, 1);
        let c = FileNode::new(
            "test.txt".into(),
            PathBuf::from("/tmp/root/test.txt"),
            100,
            false,
            1,
        );
        root.children = vec![c];
        tui.root = Some(root);

        // Enter filter mode
        tui.handle_key_event(dummy_key(KeyCode::Char('/')));
        assert!(tui.filter_mode);

        // Type characters
        tui.handle_key_event(dummy_key(KeyCode::Char('t')));
        tui.handle_key_event(dummy_key(KeyCode::Char('e')));
        assert_eq!(tui.filter_text, "te");

        // Backspace should only edit filter text
        tui.handle_key_event(dummy_key(KeyCode::Backspace));
        assert_eq!(tui.filter_text, "t");
        assert!(tui.confirm_delete.is_none());

        // Backspace again to empty
        tui.handle_key_event(dummy_key(KeyCode::Backspace));
        assert_eq!(tui.filter_text, "");
        assert!(tui.confirm_delete.is_none());

        // Backspace on empty filter text in filter mode must still NOT trigger delete
        tui.handle_key_event(dummy_key(KeyCode::Backspace));
        assert_eq!(tui.filter_text, "");
        assert!(tui.confirm_delete.is_none());
    }

    #[test]
    fn test_filter_case_insensitive_and_esc_clears() {
        let mut tui = AnalyzeTui::new(false);
        let mut root = FileNode::new("root".into(), PathBuf::from("/tmp/root"), 300, true, 3);
        let a = FileNode::new(
            "Apple.txt".into(),
            PathBuf::from("/tmp/root/Apple.txt"),
            100,
            false,
            1,
        );
        let b = FileNode::new(
            "banana.txt".into(),
            PathBuf::from("/tmp/root/banana.txt"),
            100,
            false,
            1,
        );
        let c = FileNode::new(
            "APRICOT.txt".into(),
            PathBuf::from("/tmp/root/APRICOT.txt"),
            100,
            false,
            1,
        );
        root.children = vec![a, b, c];
        tui.root = Some(root);

        // Enter filter mode and type lowercase "ap"
        tui.handle_key_event(dummy_key(KeyCode::Char('/')));
        tui.handle_key_event(dummy_key(KeyCode::Char('a')));
        tui.handle_key_event(dummy_key(KeyCode::Char('p')));
        assert_eq!(tui.filter_text, "ap");

        // Should match "Apple.txt" and "APRICOT.txt"
        assert_eq!(tui.filtered_children().len(), 2);
        assert_eq!(tui.filtered_children()[0].1.name, "Apple.txt");
        assert_eq!(tui.filtered_children()[1].1.name, "APRICOT.txt");

        // Esc clears filter and exits filter mode
        tui.handle_key_event(dummy_key(KeyCode::Esc));
        assert!(!tui.filter_mode);
        assert_eq!(tui.filter_text, "");
        assert_eq!(tui.filtered_children().len(), 3);
    }

    #[test]
    fn test_batch_delete_dry_run_leaves_files_intact() {
        let d = tempdir("dry-run-sentinel");
        let s1 = d.join("sentinel1.dat");
        let s2 = d.join("sentinel2.dat");
        std::fs::write(&s1, b"sentinel1").unwrap();
        std::fs::write(&s2, b"sentinel2").unwrap();

        let mut tui = AnalyzeTui::new(true); // dry_run = true
        let mut root = FileNode::new("d".into(), d.clone(), 20, true, 2);
        let n1 = FileNode::new("sentinel1.dat".into(), s1.clone(), 10, false, 1);
        let n2 = FileNode::new("sentinel2.dat".into(), s2.clone(), 10, false, 1);
        root.children = vec![n1, n2];
        tui.root = Some(root);

        tui.selected_paths.insert(s1.clone());
        tui.selected_paths.insert(s2.clone());

        // Trigger batch delete
        tui.handle_key_event(dummy_key(KeyCode::Backspace));
        assert!(tui.confirm_delete.is_some());

        // Confirm YES (direct Enter in D1)
        tui.handle_key_event(dummy_key(KeyCode::Enter));
        tui.wait_for_delete();

        // In dry run, files must remain intact
        assert!(s1.exists(), "sentinel1 must not be removed in dry-run");
        assert!(s2.exists(), "sentinel2 must not be removed in dry-run");
        assert!(
            tui.message.as_ref().unwrap().0.contains("dry run"),
            "message should mention dry run"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn test_navigate_up_restores_cursor_with_active_filter() {
        let mut tui = AnalyzeTui::new(false);
        let mut root = FileNode::new("root".into(), PathBuf::from("/tmp/root"), 600, true, 1);
        let f0 = FileNode::new(
            "file_zero.txt".into(),
            PathBuf::from("/tmp/root/file_zero.txt"),
            100,
            false,
            1,
        );
        let f1 = FileNode::new(
            "file_one.txt".into(),
            PathBuf::from("/tmp/root/file_one.txt"),
            100,
            false,
            1,
        );
        let mut d1 = FileNode::new(
            "target_dir_one".into(),
            PathBuf::from("/tmp/root/target_dir_one"),
            100,
            true,
            1,
        );
        d1.children = vec![FileNode::new(
            "sub1.txt".into(),
            PathBuf::from("/tmp/root/target_dir_one/sub1.txt"),
            100,
            false,
            1,
        )];
        let f2 = FileNode::new(
            "file_two.txt".into(),
            PathBuf::from("/tmp/root/file_two.txt"),
            100,
            false,
            1,
        );
        let mut d2 = FileNode::new(
            "target_dir_two".into(),
            PathBuf::from("/tmp/root/target_dir_two"),
            100,
            true,
            1,
        );
        d2.children = vec![FileNode::new(
            "sub2.txt".into(),
            PathBuf::from("/tmp/root/target_dir_two/sub2.txt"),
            100,
            false,
            1,
        )];
        let d3 = FileNode::new(
            "target_dir_three".into(),
            PathBuf::from("/tmp/root/target_dir_three"),
            100,
            true,
            1,
        );
        root.children = vec![f0, f1, d1, f2, d2, d3];
        tui.root = Some(root);

        // Filter by "target"
        tui.filter_text = "target".to_string();
        let filtered = tui.filtered_children();
        assert_eq!(filtered.len(), 3);
        assert_eq!(filtered[0].1.name, "target_dir_one"); // orig_idx 2, display 0
        assert_eq!(filtered[1].1.name, "target_dir_two"); // orig_idx 4, display 1
        assert_eq!(filtered[2].1.name, "target_dir_three"); // orig_idx 5, display 2

        // Test descending into display 0 (orig_idx 2)
        tui.cursor = 0;
        tui.handle_key_event(dummy_key(KeyCode::Enter));
        assert_eq!(tui.path_indices, vec![2]);

        // Navigate up: cursor must return to display index 0 (target_dir_one), not orig_idx 2
        tui.handle_key_event(dummy_key(KeyCode::Char('h')));
        assert!(tui.path_indices.is_empty());
        assert_eq!(tui.cursor, 0, "cursor should restore to display index 0");

        // Test descending into display 1 (orig_idx 4)
        tui.cursor = 1;
        tui.handle_key_event(dummy_key(KeyCode::Enter));
        assert_eq!(tui.path_indices, vec![4]);

        // Navigate up: cursor must return to display index 1 (target_dir_two), not clamped orig_idx
        tui.handle_key_event(dummy_key(KeyCode::Left));
        assert!(tui.path_indices.is_empty());
        assert_eq!(tui.cursor, 1, "cursor should restore to display index 1");
    }

    fn buffer_to_string(terminal: &Terminal<TestBackend>) -> String {
        let mut content = String::new();
        let buf = terminal.backend().buffer();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                content.push_str(buf[(x, y)].symbol());
            }
            content.push('\n');
        }
        content
    }

    #[test]
    fn test_overview_snapshot() {
        let mut tui = AnalyzeTui::new(false);
        tui.set_overview_mode(true);
        tui.set_old_downloads(2_500_000_000, 18);

        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let mut root = FileNode::new("home".into(), home.clone(), 10_000_000_000, true, 1);
        let dl = FileNode::new(
            "Downloads".into(),
            home.join("Downloads"),
            2_500_000_000,
            true,
            1,
        );
        let cache = FileNode::new(".cache".into(), home.join(".cache"), 1_000_000_000, true, 1);
        root.children = vec![dl, cache];
        tui.root = Some(root);
        tui.update_cleanable_stats();

        let backend = TestBackend::new(100, 25);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| tui.render(f, f.area())).unwrap();

        let text = buffer_to_string(&terminal);
        assert!(
            text.contains("Disk Usage Overview"),
            "missing header in:\n{text}"
        );
        assert!(
            text.contains("Locations"),
            "missing Locations block in:\n{text}"
        );
        assert!(text.contains("Home"), "missing Home in:\n{text}");
        assert!(text.contains("Downloads"), "missing Downloads in:\n{text}");
        assert!(
            text.contains("User Cache"),
            "missing User Cache in:\n{text}"
        );
        assert!(
            text.contains("Root System"),
            "missing Root System in:\n{text}"
        );
        assert!(
            text.contains("Hidden-Space Insights"),
            "missing Insights block in:\n{text}"
        );
        assert!(
            text.contains("Downloads older than 90 days"),
            "missing downloads insight in:\n{text}"
        );
        assert!(
            text.contains("mu installer"),
            "missing installer pointer in:\n{text}"
        );
        assert!(
            text.contains("Cleanable project artifacts"),
            "missing cleanable insight in:\n{text}"
        );
    }

    #[test]
    fn test_cleanable_badges_snapshot() {
        let mut tui = AnalyzeTui::new(false);
        let clean_root = PathBuf::from("/home/user/.cache");
        tui.clean_roots = vec![clean_root.clone()];

        let mut root = FileNode::new(
            "workspace".into(),
            PathBuf::from("/home/user/workspace"),
            5000,
            true,
            1,
        );
        let n1 = FileNode::new(
            "node_modules".into(),
            PathBuf::from("/home/user/workspace/node_modules"),
            1000,
            true,
            1,
        );
        let n2 = FileNode::new(
            "target".into(),
            PathBuf::from("/home/user/workspace/target"),
            1000,
            true,
            1,
        );
        let n3 = FileNode::new(
            ".venv".into(),
            PathBuf::from("/home/user/workspace/.venv"),
            1000,
            true,
            1,
        );
        let n4 = FileNode::new(
            "dist".into(),
            PathBuf::from("/home/user/workspace/dist"),
            1000,
            true,
            1,
        );
        let mut n5 = FileNode::new(
            "tagged_cache".into(),
            PathBuf::from("/home/user/workspace/tagged_cache"),
            500,
            true,
            1,
        );
        n5.children = vec![FileNode::new(
            "CACHEDIR.TAG".into(),
            PathBuf::from("/home/user/workspace/tagged_cache/CACHEDIR.TAG"),
            50,
            false,
            1,
        )];
        let regular = FileNode::new(
            "src".into(),
            PathBuf::from("/home/user/workspace/src"),
            500,
            true,
            1,
        );
        // Inside clean root -> excluded from badge
        let excluded = FileNode::new("target".into(), clean_root.join("target"), 1000, true, 1);

        root.children = vec![n1, n2, n3, n4, n5, regular, excluded];
        tui.root = Some(root);

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| tui.render(f, f.area())).unwrap();

        let text = buffer_to_string(&terminal);
        // Must display 🧹
        assert!(text.contains("🧹"), "missing cleanable badge in:\n{text}");

        // Separate lines check
        let lines: Vec<&str> = text.lines().collect();
        let node_modules_line = lines
            .iter()
            .find(|l| l.contains("node_modules/"))
            .expect("node_modules line");
        assert!(
            node_modules_line.contains("🧹"),
            "node_modules must have badge: {node_modules_line}"
        );

        let target_line = lines
            .iter()
            .find(|l| l.contains("target/"))
            .expect("target line");
        assert!(
            target_line.contains("🧹"),
            "target must have badge: {target_line}"
        );

        let venv_line = lines
            .iter()
            .find(|l| l.contains(".venv/"))
            .expect(".venv line");
        assert!(
            venv_line.contains("🧹"),
            ".venv must have badge: {venv_line}"
        );

        let dist_line = lines
            .iter()
            .find(|l| l.contains("dist/"))
            .expect("dist line");
        assert!(
            dist_line.contains("🧹"),
            "dist must have badge: {dist_line}"
        );

        let tagged_line = lines
            .iter()
            .find(|l| l.contains("tagged_cache/"))
            .expect("tagged line");
        assert!(
            tagged_line.contains("🧹"),
            "tagged_cache must have badge: {tagged_line}"
        );

        let src_line = lines.iter().find(|l| l.contains("src/")).expect("src line");
        assert!(
            !src_line.contains("🧹"),
            "src must NOT have badge: {src_line}"
        );
        // Check excluded in clean root does not have badge
        assert!(!is_cleanable(
            &FileNode::new("target".into(), clean_root.join("target"), 100, true, 1),
            &tui.clean_roots
        ));
    }

    #[test]
    fn test_insights_snapshot() {
        let mut tui = AnalyzeTui::new(false);
        tui.set_overview_mode(true);
        tui.set_old_downloads(3_500_000_000, 25);
        tui.cleanable_cache_bytes = 800_000_000;
        tui.cleanable_cache_count = 5;

        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| tui.render(f, f.area())).unwrap();

        let text = buffer_to_string(&terminal);
        assert!(text.contains("Downloads older than 90 days"));
        assert!(text.contains("25 files"));
        assert!(text.contains("run 'mu installer' to clean old installers"));
        assert!(text.contains("Cleanable project artifacts"));
        assert!(text.contains("5 directories"));
    }

    #[test]
    fn test_fake_runner_xdg_open_argv() {
        let runner = Arc::new(FakeRunner::new());
        let mut tui = AnalyzeTui::new(false).with_runner(runner.clone() as Arc<dyn Runner>);

        let dir_path = PathBuf::from("/tmp/test_dir");
        let file_path = PathBuf::from("/tmp/test_dir/file.txt");
        let mut root = FileNode::new("test_dir".into(), dir_path.clone(), 200, true, 1);
        let child_file = FileNode::new("file.txt".into(), file_path.clone(), 200, false, 1);
        root.children = vec![child_file];
        tui.root = Some(root);

        // Save and unset GUI display variables
        let orig_disp = std::env::var_os("DISPLAY");
        let orig_wayland = std::env::var_os("WAYLAND_DISPLAY");
        unsafe {
            std::env::remove_var("DISPLAY");
            std::env::remove_var("WAYLAND_DISPLAY");
        }

        // Trigger O and F without GUI display: must refuse and not invoke runner
        tui.cursor = 0;
        tui.handle_key_event(dummy_key(KeyCode::Char('o')));
        assert!(
            runner.invocations().is_empty(),
            "runner must not be called without GUI display"
        );
        assert!(tui.message.as_ref().unwrap().0.contains("DISPLAY"));

        tui.handle_key_event(dummy_key(KeyCode::Char('f')));
        assert!(
            runner.invocations().is_empty(),
            "runner must not be called without GUI display"
        );
        assert!(tui.message.as_ref().unwrap().0.contains("DISPLAY"));

        // Set DISPLAY and test again
        unsafe {
            std::env::set_var("DISPLAY", ":0");
        }

        // O on file -> opens its directory
        tui.handle_key_event(dummy_key(KeyCode::Char('o')));
        let calls = runner.invocations();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "xdg-open");
        assert_eq!(calls[0].args, vec![dir_path.as_os_str()]);

        // F on file -> reveals the file itself
        tui.handle_key_event(dummy_key(KeyCode::Char('f')));
        let calls = runner.invocations();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].program, "xdg-open");
        assert_eq!(calls[1].args, vec![file_path.as_os_str()]);

        // Restore env
        unsafe {
            if let Some(v) = orig_disp {
                std::env::set_var("DISPLAY", v);
            } else {
                std::env::remove_var("DISPLAY");
            }
            if let Some(v) = orig_wayland {
                std::env::set_var("WAYLAND_DISPLAY", v);
            } else {
                std::env::remove_var("WAYLAND_DISPLAY");
            }
        }
    }

    #[test]
    fn test_overview_navigation_and_tree_return() {
        let mut tui = AnalyzeTui::new(false);
        tui.set_overview_mode(true);

        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let mut root = FileNode::new("home".into(), home.clone(), 1000, true, 1);
        let mut dl = FileNode::new("Downloads".into(), home.join("Downloads"), 500, true, 1);
        dl.children = vec![FileNode::new(
            "item.zip".into(),
            home.join("Downloads/item.zip"),
            500,
            false,
            1,
        )];
        root.children = vec![dl];
        tui.root = Some(root);

        // Starts at Home (cursor 0)
        assert_eq!(tui.cursor, 0);
        assert_eq!(tui.view_mode, ViewMode::Overview);

        // Move down to Downloads (cursor 1)
        tui.handle_key_event(dummy_key(KeyCode::Char('j')));
        assert_eq!(tui.cursor, 1);

        // Enter Downloads -> enters tree inside Downloads
        tui.handle_key_event(dummy_key(KeyCode::Enter));
        assert_eq!(tui.view_mode, ViewMode::Tree);
        assert_eq!(tui.path_indices, vec![0]);

        // Leave Downloads with 'h' -> returns to root tree
        tui.handle_key_event(dummy_key(KeyCode::Char('h')));
        assert!(tui.path_indices.is_empty());
        assert_eq!(tui.view_mode, ViewMode::Tree);

        // At root tree, 'h' or 'q' returns to Overview
        tui.handle_key_event(dummy_key(KeyCode::Char('h')));
        assert_eq!(tui.view_mode, ViewMode::Overview);

        // In Overview, 'q' exits
        assert!(tui.handle_key_event(dummy_key(KeyCode::Char('q'))));
    }

    #[test]
    fn test_actions_rescan_and_preview() {
        let mut tui = AnalyzeTui::new(false);

        // Test R triggers rescan
        assert!(!tui.rescan_requested);
        tui.handle_key_event(dummy_key(KeyCode::Char('r')));
        assert!(tui.rescan_requested);
        assert!(tui.scanning);

        // Test P preview
        let d = tempdir("preview");
        let file = d.join("sample.txt");
        std::fs::write(&file, b"Line 1\nLine 2\nLine 3").unwrap();

        let mut root = FileNode::new("d".into(), d.clone(), 100, true, 1);
        let child = FileNode::new("sample.txt".into(), file.clone(), 100, false, 1);
        root.children = vec![child];
        tui.root = Some(root);
        tui.cursor = 0;

        // Press P on sample.txt
        tui.handle_key_event(dummy_key(KeyCode::Char('p')));
        assert!(tui.preview_modal.is_some());
        let (title, lines) = tui.preview_modal.as_ref().unwrap();
        assert_eq!(title, "sample.txt");
        assert_eq!(lines, &["Line 1", "Line 2", "Line 3"]);

        // Press Esc -> closes preview modal
        tui.handle_key_event(dummy_key(KeyCode::Esc));
        assert!(tui.preview_modal.is_none());

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn test_tui_rescan_invalidates_cache() {
        let d = tempdir("rescan-cache");
        let file = d.join("a.txt");
        std::fs::write(&file, b"test").unwrap();

        let mut root = FileNode::new("d".into(), d.clone(), 4, true, 1);
        let child = FileNode::new("a.txt".into(), file.clone(), 4, false, 1);
        root.children = vec![child];

        let stats = crate::analyze::model::ScanStats {
            total_bytes: 4,
            total_bytes_human: "4 B".into(),
            total_files: 1,
            total_dirs: 1,
            errors: 0,
            duration: std::time::Duration::from_millis(5),
            largest_files: vec![],
        };

        crate::analyze::cache::save_cache(&d, &root, &stats).unwrap();
        assert!(crate::analyze::cache::cache_path(&d).exists());

        let mut tui = AnalyzeTui::new(false);
        tui.root = Some(root);

        // Press 'r' to trigger rescan
        tui.handle_key_event(dummy_key(KeyCode::Char('r')));
        assert!(tui.rescan_requested);
        assert!(
            !crate::analyze::cache::cache_path(&d).exists(),
            "R rescan must invalidate cache"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn test_tui_delete_and_batch_delete_invalidate_cache() {
        let d = tempdir("delete-cache");
        let file1 = d.join("f1.txt");
        let file2 = d.join("f2.txt");
        std::fs::write(&file1, b"1").unwrap();
        std::fs::write(&file2, b"2").unwrap();

        let mut root = FileNode::new("d".into(), d.clone(), 2, true, 1);
        let c1 = FileNode::new("f1.txt".into(), file1.clone(), 1, false, 1);
        let c2 = FileNode::new("f2.txt".into(), file2.clone(), 1, false, 1);
        root.children = vec![c1, c2];

        let stats = crate::analyze::model::ScanStats {
            total_bytes: 2,
            total_bytes_human: "2 B".into(),
            total_files: 2,
            total_dirs: 1,
            errors: 0,
            duration: std::time::Duration::from_millis(5),
            largest_files: vec![],
        };

        // 1. Single delete invalidation
        crate::analyze::cache::save_cache(&d, &root, &stats).unwrap();
        assert!(crate::analyze::cache::cache_path(&d).exists());

        let mut tui = AnalyzeTui::new(false);
        tui.root = Some(root.clone());
        tui.confirm_delete = DeleteConfirm::Single(file1.clone(), 1);
        tui.execute_delete();
        assert!(
            !crate::analyze::cache::cache_path(&d).exists(),
            "single delete must invalidate cache"
        );

        // 2. Batch delete invalidation
        crate::analyze::cache::save_cache(&d, &root, &stats).unwrap();
        assert!(crate::analyze::cache::cache_path(&d).exists());

        tui.selected_paths.insert(file2.clone());
        tui.confirm_delete = DeleteConfirm::Batch(vec![file2.clone()], 1);
        tui.execute_batch_delete();
        assert!(
            !crate::analyze::cache::cache_path(&d).exists(),
            "batch delete must invalidate cache"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn test_bar_rendering_and_color_thresholds() {
        // Zero value or zero max_value gives 24 spaces
        let empty_bar1 = render_bar_span(0, 100, 50.0);
        assert_eq!(empty_bar1.content, " ".repeat(BAR_WIDTH));
        assert_eq!(display_width(&empty_bar1.content), BAR_WIDTH);

        let empty_bar2 = render_bar_span(100, 0, 50.0);
        assert_eq!(empty_bar2.content, " ".repeat(BAR_WIDTH));
        assert_eq!(display_width(&empty_bar2.content), BAR_WIDTH);

        // 100% share (full width 24 blocks)
        let full_bar = render_bar_span(1000, 1000, 100.0);
        assert_eq!(full_bar.content, "█".repeat(BAR_WIDTH));
        assert_eq!(full_bar.style.fg, Some(Color::Red));
        assert_eq!(display_width(&full_bar.content), BAR_WIDTH);

        // Very small value (>0 but rounded to 0 eighths) produces tick in DarkGray
        let tick_bar = render_bar_span(1, 10_000_000, 0.0001);
        assert!(tick_bar.content.starts_with(SUB_CELL_BLOCKS[1]));
        assert_eq!(tick_bar.style.fg, Some(Color::DarkGray));
        assert_eq!(display_width(&tick_bar.content), BAR_WIDTH);

        // Color thresholds (>=50 red, >=20 yellow, >=5 blue, else green)
        assert_eq!(bar_color_for_percent(50.0), Color::Red);
        assert_eq!(bar_color_for_percent(75.5), Color::Red);
        assert_eq!(bar_color_for_percent(100.0), Color::Red);

        assert_eq!(bar_color_for_percent(20.0), Color::Yellow);
        assert_eq!(bar_color_for_percent(35.0), Color::Yellow);
        assert_eq!(bar_color_for_percent(49.99), Color::Yellow);

        assert_eq!(bar_color_for_percent(5.0), Color::Blue);
        assert_eq!(bar_color_for_percent(12.5), Color::Blue);
        assert_eq!(bar_color_for_percent(19.99), Color::Blue);

        assert_eq!(bar_color_for_percent(4.99), Color::Green);
        assert_eq!(bar_color_for_percent(1.0), Color::Green);
        assert_eq!(bar_color_for_percent(0.0), Color::Green);

        // Sub-cell eighth-block progression
        // 1/8 of 1 cell -> value = 1, max_value = 192 -> ratio * 24 * 8 = 1.0 eighth
        for rem in 1..8 {
            // (rem as f64 / 192.0) gives exactly rem eighths
            let bar = render_bar_span(rem, 192, 10.0);
            assert!(
                bar.content.starts_with(SUB_CELL_BLOCKS[rem as usize]),
                "expected remainder {rem} to start with {}",
                SUB_CELL_BLOCKS[rem as usize]
            );
            assert_eq!(display_width(&bar.content), BAR_WIDTH);
        }
    }

    #[test]
    fn test_percent_formatting() {
        assert_eq!(format_percent(None), "  --  ");
        assert_eq!(format_percent(Some(0.0)), "  0.0%");
        assert_eq!(format_percent(Some(0.01)), "< 0.1%");
        assert_eq!(format_percent(Some(0.05)), "< 0.1%");
        assert_eq!(format_percent(Some(0.099)), "< 0.1%");
        assert_eq!(format_percent(Some(0.1)), "  0.1%");
        assert_eq!(format_percent(Some(1.5)), "  1.5%");
        assert_eq!(format_percent(Some(9.4)), "  9.4%");
        assert_eq!(format_percent(Some(20.0)), " 20.0%");
        assert_eq!(format_percent(Some(50.0)), " 50.0%");
        assert_eq!(format_percent(Some(100.0)), "100.0%");

        // Ensure all formatted percent strings have width exactly 6
        for p in [
            None,
            Some(0.0),
            Some(0.04),
            Some(0.1),
            Some(5.0),
            Some(50.0),
            Some(100.0),
        ] {
            assert_eq!(display_width(&format_percent(p)), 6);
        }
    }

    #[test]
    fn test_size_column_formatting() {
        assert_eq!(format_size_column("0 B"), "       0 B");
        assert_eq!(format_size_column("100 B"), "     100 B");
        assert_eq!(format_size_column("1.5 KB"), "    1.5 KB");
        assert_eq!(format_size_column("10.9 MB"), "   10.9 MB");
        assert_eq!(format_size_column("200.0 MB"), "  200.0 MB");
        assert_eq!(format_size_column("3.5 GB"), "    3.5 GB");
        assert_eq!(format_size_column("100 B+"), "    100 B+");

        for s in [
            "0 B", "100 B", "1.5 KB", "10.9 MB", "200.0 MB", "3.5 GB", "100 B+",
        ] {
            assert_eq!(display_width(&format_size_column(s)), 10);
        }
    }

    #[test]
    fn test_unicode_name_alignment() {
        let name_ascii = "ascii_file.txt";
        let name_cjk = "测试项目目录/"; // 4 Chinese chars (width 8) + 2 chars = width 10
        let name_emoji = "🦀_rust_code/"; // emoji (width 2) + 12 chars = width 14

        let target_width = 30;
        let padded_ascii = trim_and_pad_name(name_ascii, target_width);
        let padded_cjk = trim_and_pad_name(name_cjk, target_width);
        let padded_emoji = trim_and_pad_name(name_emoji, target_width);

        assert_eq!(display_width(&padded_ascii), target_width);
        assert_eq!(display_width(&padded_cjk), target_width);
        assert_eq!(display_width(&padded_emoji), target_width);

        // Render all three into a TestBackend buffer and check that the size column
        // starts at the exact same column index on all rows.
        let row1 = RowItem {
            name: name_ascii,
            is_dir: false,
            size: 1000,
            size_human: "1000 B",
            is_cleanable: false,
            is_checked: false,
            is_cursor: false,
            index: 1,
            index_width: 2,
            max_size: 1000,
            parent_size: Some(1000),
            name_width: target_width,
            has_err: false,
        };
        let row2 = RowItem {
            name: name_cjk,
            is_dir: true,
            size: 1000,
            size_human: "1000 B",
            is_cleanable: false,
            is_checked: false,
            is_cursor: false,
            index: 2,
            index_width: 2,
            max_size: 1000,
            parent_size: Some(1000),
            name_width: target_width,
            has_err: false,
        };
        let row3 = RowItem {
            name: name_emoji,
            is_dir: true,
            size: 1000,
            size_human: "1000 B",
            is_cleanable: false,
            is_checked: false,
            is_cursor: false,
            index: 3,
            index_width: 2,
            max_size: 1000,
            parent_size: Some(1000),
            name_width: target_width,
            has_err: false,
        };

        let lines = vec![
            Line::from(row_cells(&row1)),
            Line::from(row_cells(&row2)),
            Line::from(row_cells(&row3)),
        ];

        let backend = TestBackend::new(120, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let p = Paragraph::new(lines);
                f.render_widget(p, f.area());
            })
            .unwrap();

        let text = buffer_to_string(&terminal);
        let rendered_lines: Vec<&str> = text.lines().collect();
        assert!(rendered_lines.len() >= 3);

        // Check the actual terminal column (x-coordinate) in the TestBackend buffer
        let buf = terminal.backend().buffer();
        let find_size_col = |y: u16| -> u16 {
            for x in 0..(buf.area.width - 5) {
                if buf[(x, y)].symbol() == "1"
                    && buf[(x + 1, y)].symbol() == "0"
                    && buf[(x + 2, y)].symbol() == "0"
                    && buf[(x + 3, y)].symbol() == "0"
                    && buf[(x + 4, y)].symbol() == " "
                    && buf[(x + 5, y)].symbol() == "B"
                {
                    return x;
                }
            }
            panic!("could not find '1000 B' in row {y}");
        };

        let col1 = find_size_col(0);
        let col2 = find_size_col(1);
        let col3 = find_size_col(2);

        assert_eq!(
            col1, col2,
            "size column misaligned between ASCII and CJK: col {col1} vs col {col2}\nLine1: {}\nLine2: {}",
            rendered_lines[0], rendered_lines[1]
        );
        assert_eq!(
            col1, col3,
            "size column misaligned between ASCII and Emoji: col {col1} vs col {col3}\nLine1: {}\nLine3: {}",
            rendered_lines[0], rendered_lines[2]
        );
    }

    #[test]
    fn test_hint_presence_and_absence() {
        let item_clean = RowItem {
            name: "node_modules/",
            is_dir: true,
            size: 1000,
            size_human: "1000 B",
            is_cleanable: true,
            is_checked: false,
            is_cursor: false,
            index: 1,
            index_width: 2,
            max_size: 1000,
            parent_size: Some(1000),
            name_width: 30,
            has_err: false,
        };
        let spans_clean = row_cells(&item_clean);
        let clean_str: String = spans_clean.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            clean_str.contains('🧹'),
            "cleanable row must carry broom glyph"
        );

        let item_normal = RowItem {
            name: "src/",
            is_dir: true,
            size: 500,
            size_human: "500 B",
            is_cleanable: false,
            is_checked: false,
            is_cursor: false,
            index: 2,
            index_width: 2,
            max_size: 1000,
            parent_size: Some(1000),
            name_width: 30,
            has_err: false,
        };
        let spans_normal = row_cells(&item_normal);
        let normal_str: String = spans_normal.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !normal_str.contains('🧹'),
            "normal row must NOT carry broom glyph"
        );
    }

    #[test]
    fn test_tree_view_row_snapshot() {
        let mut tui = AnalyzeTui::new(false);
        let mut root = FileNode::new(
            "workspace".into(),
            PathBuf::from("/workspace"),
            2000,
            true,
            1,
        );
        let mut clean_node = FileNode::new(
            "node_modules".into(),
            PathBuf::from("/workspace/node_modules"),
            1000,
            true,
            1,
        );
        clean_node.children = vec![];
        let file_node = FileNode::new(
            "main.rs".into(),
            PathBuf::from("/workspace/main.rs"),
            500,
            false,
            1,
        );
        let small_node = FileNode::new(
            "readme.md".into(),
            PathBuf::from("/workspace/readme.md"),
            50,
            false,
            1,
        );
        root.children = vec![clean_node, file_node, small_node];
        tui.root = Some(root);

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| tui.render(f, f.area())).unwrap();

        let text = buffer_to_string(&terminal);
        let lines: Vec<&str> = text.lines().collect();

        // Find row lines
        let row1 = lines
            .iter()
            .find(|l| l.contains("node_modules/"))
            .expect("row1");
        let row2 = lines.iter().find(|l| l.contains("main.rs")).expect("row2");
        let row3 = lines
            .iter()
            .find(|l| l.contains("readme.md"))
            .expect("row3");

        // Row 1 (cursor, 50% share, cleanable)
        assert!(row1.contains('▶'), "cursor on row 1: {row1}");
        assert!(row1.contains('○'), "unselected circle on row 1: {row1}");
        assert!(row1.contains(" 1."), "index 1 on row 1: {row1}");
        assert!(row1.contains("50.0%"), "50.0% on row 1: {row1}");
        assert!(row1.contains('|'), "activity divider on row 1: {row1}");
        assert!(row1.contains('📁'), "dir icon on row 1: {row1}");
        assert!(row1.contains("1000 B"), "size on row 1: {row1}");
        assert!(row1.contains('🧹'), "cleanable badge on row 1: {row1}");

        // Row 2 (not cursor, 25% share, file)
        assert!(!row2.contains('▶'), "no cursor on row 2: {row2}");
        assert!(row2.contains('○'), "unselected circle on row 2: {row2}");
        assert!(row2.contains(" 2."), "index 2 on row 2: {row2}");
        assert!(row2.contains("25.0%"), "25.0% on row 2: {row2}");
        assert!(row2.contains('📄'), "file icon on row 2: {row2}");
        assert!(row2.contains("500 B"), "size on row 2: {row2}");
        assert!(!row2.contains('🧹'), "no cleanable badge on row 2: {row2}");

        // Row 3 (not cursor, 2.5% share, file)
        assert!(row3.contains(" 3."), "index 3 on row 3: {row3}");
        assert!(row3.contains(" 2.5%"), "2.5% on row 3: {row3}");
        assert!(row3.contains("50 B"), "size on row 3: {row3}");
    }

    #[test]
    fn test_footer_eight_states() {
        let mut tui = AnalyzeTui::new(false);

        // State 1: Overview idle (no history)
        tui.view_mode = ViewMode::Overview;
        tui.history.clear();
        assert_eq!(
            tui.footer_hint(),
            "↑↓→ | Enter | R Refresh | O Open | P Preview | F File | Esc/q Quit"
        );

        // State 2: Overview with history
        tui.history.push(PathBuf::from("/home/user"));
        assert_eq!(
            tui.footer_hint(),
            "↑↓←→ | Enter | R Refresh | O Open | P Preview | F File | Esc Back | q Quit"
        );

        // State 3: Tree idle (no selection, with and without top files)
        tui.view_mode = ViewMode::Tree;
        tui.selected_paths.clear();
        tui.largest_files.clear();
        assert_eq!(
            tui.footer_hint(),
            "↑↓←→ | Space Select | Enter | / Filter | R Refresh | O Open | P Preview | F File | ⌫/Del | Esc Back | q Quit"
        );

        tui.largest_files = (0..12)
            .map(|i| LargestFile {
                path: PathBuf::from(format!("/tmp/file_{i}.dat")),
                size: (12 - i) as u64 * 1024,
                size_human: format!("{} KB", 12 - i),
            })
            .collect();
        assert_eq!(
            tui.footer_hint(),
            "↑↓←→ | Space Select | Enter | / Filter | R Refresh | O Open | P Preview | F File | ⌫/Del | Tab Top 12 | Esc Back | q Quit"
        );

        // State 4: Tree with a selection
        tui.selected_paths.insert(PathBuf::from("/tmp/a"));
        tui.selected_paths.insert(PathBuf::from("/tmp/b"));
        tui.selected_paths.insert(PathBuf::from("/tmp/c"));
        assert_eq!(
            tui.footer_hint(),
            "↑↓←→ | Space Select | Enter | / Filter | R Refresh | O Open | P Preview | F File | ⌫/Del 3 | Tab Top 12 | Esc Back | q Quit"
        );

        // State 5: Tree filter typing
        tui.filter_mode = true;
        assert_eq!(
            tui.footer_hint(),
            "Type to filter  |  Enter Apply  |  Esc Clear  |  Ctrl+C Quit"
        );

        // State 6: Tree filter applied
        tui.filter_mode = false;
        tui.filter_text = "target".into();
        assert_eq!(
            tui.footer_hint(),
            "↑↓←→ | Enter | Space Select | / Edit | Esc Clear filter | O Open | P Preview | F File | ⌫/Del 3 | q Quit"
        );
        tui.selected_paths.clear();
        assert_eq!(
            tui.footer_hint(),
            "↑↓←→ | Enter | Space Select | / Edit | Esc Clear filter | O Open | P Preview | F File | ⌫/Del | q Quit"
        );

        // State 7: Largest files idle
        tui.view_mode = ViewMode::LargestFiles;
        tui.filter_text.clear();
        assert_eq!(
            tui.footer_hint(),
            "↑↓← | Space Select | / Filter | R Refresh | O Open | P Preview | F File | ⌫/Del | Esc Back | q Quit"
        );
        tui.selected_paths.insert(PathBuf::from("/tmp/file_0.dat"));
        tui.selected_paths.insert(PathBuf::from("/tmp/file_1.dat"));
        assert_eq!(
            tui.footer_hint(),
            "↑↓← | Space Select | / Filter | R Refresh | O Open | P Preview | F File | ⌫/Del 2 | Esc Back | q Quit"
        );

        // State 8: Largest files filter
        tui.filter_mode = true;
        assert_eq!(
            tui.footer_hint(),
            "Type to filter  |  Enter Apply  |  Esc Clear  |  Ctrl+C Quit"
        );
        tui.filter_mode = false;
        tui.filter_text = "dat".into();
        assert_eq!(
            tui.footer_hint(),
            "↑↓← | Space Select | / Edit | Esc Clear filter | O Open | P Preview | F File | ⌫/Del 2 | q Quit"
        );
    }

    #[test]
    fn test_header_free_space_rendering() {
        let mut tui = AnalyzeTui::new(false);
        let mut root = FileNode::new(
            "root".into(),
            PathBuf::from("/tmp/test_root"),
            1000,
            true,
            1,
        );
        root.children = vec![FileNode::new(
            "a.txt".into(),
            PathBuf::from("/tmp/test_root/a.txt"),
            1000,
            false,
            1,
        )];
        tui.root = Some(root);
        tui.scanning = false;

        // 1. With free space in Tree view
        tui.filesystem_free = Some(50 * 1024 * 1024 * 1024); // 50 GiB
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| tui.render(f, f.area())).unwrap();
        let text = buffer_to_string(&terminal);
        assert!(
            text.contains("50.0 GB free"),
            "expected free space in:\n{text}"
        );

        // 2. Without free space in Tree view
        tui.filesystem_free = None;
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| tui.render(f, f.area())).unwrap();
        let text = buffer_to_string(&terminal);
        assert!(
            !text.contains("free)"),
            "expected no free label in:\n{text}"
        );

        // 3. Overview mode with free space
        tui.set_overview_mode(true);
        tui.filesystem_free = Some(120 * 1024 * 1024 * 1024);
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| tui.render(f, f.area())).unwrap();
        let text = buffer_to_string(&terminal);
        assert!(
            text.contains("Disk Usage Overview  (120.0 GB free)"),
            "expected overview free in:\n{text}"
        );

        // 4. Overview mode without free space
        tui.filesystem_free = None;
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| tui.render(f, f.area())).unwrap();
        let text = buffer_to_string(&terminal);
        assert!(
            text.contains("Disk Usage Overview"),
            "expected overview title in:\n{text}"
        );
        assert!(
            !text.contains("free)"),
            "expected no free label in:\n{text}"
        );
    }

    #[test]
    fn test_header_scan_counters_and_percent() {
        let mut tui = AnalyzeTui::new(false);
        tui.scanning = true;
        tui.files_scanned = 1250;
        tui.dirs_scanned = 150;
        tui.bytes_scanned = 45 * 1024 * 1024;

        // Initial scan: last_total_files is None -> no percent prefix, no fake (0 B, 0 items)
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| tui.render(f, f.area())).unwrap();
        let text = buffer_to_string(&terminal);
        assert!(
            text.contains("Scanning: 1,250 files, 150 dirs, 45.0 MB"),
            "expected formatted counters in:\n{text}"
        );
        assert!(
            !text.contains('%'),
            "expected no percent prefix during initial scan in:\n{text}"
        );
        assert!(
            !text.contains("(0 B, 0 items)"),
            "must not display fake unmeasured total in:\n{text}"
        );

        // Rescan with known previous total: last_total_files = Some(2500) -> 50%
        tui.last_total_files = Some(2500);
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| tui.render(f, f.area())).unwrap();
        let text = buffer_to_string(&terminal);
        assert!(
            text.contains("Scanning 50%: 1,250 files, 150 dirs, 45.0 MB"),
            "expected 50% rescan progress in:\n{text}"
        );

        // Cap at 99% while scanning
        tui.files_scanned = 2500;
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| tui.render(f, f.area())).unwrap();
        let text = buffer_to_string(&terminal);
        assert!(
            text.contains("Scanning 99%: 2,500 files, 150 dirs, 45.0 MB"),
            "expected 99% capped rescan progress in:\n{text}"
        );
    }

    #[test]
    fn test_format_number_and_truncate_middle() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(999), "999");
        assert_eq!(format_number(1000), "1,000");
        assert_eq!(format_number(1234567), "1,234,567");

        let long_path = "/home/user/very/deeply/nested/directory/structure/file.txt";
        let truncated = truncate_middle(long_path, 25);
        assert!(display_width(&truncated) <= 25);
        assert!(truncated.contains("..."));
        assert_eq!(truncate_middle("short", 10), "short");
    }

    #[test]
    fn test_all_eight_states_backend_snapshots() {
        let render_bottom_line = |tui: &mut AnalyzeTui| -> String {
            let backend = TestBackend::new(120, 10);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| tui.render(f, f.area())).unwrap();
            let text = buffer_to_string(&terminal);
            text.lines().last().unwrap().to_string()
        };

        let mut tui = AnalyzeTui::new(false);
        let mut root = FileNode::new("root".into(), PathBuf::from("/tmp/root"), 100, true, 1);
        root.children = vec![FileNode::new(
            "a".into(),
            PathBuf::from("/tmp/root/a"),
            100,
            false,
            1,
        )];
        tui.root = Some(root);
        tui.scanning = false;

        // 1. Overview idle
        tui.set_overview_mode(true);
        tui.history.clear();
        let s1 = render_bottom_line(&mut tui);
        assert!(
            s1.contains("↑↓→ | Enter | R Refresh | O Open | P Preview | F File | Esc/q Quit"),
            "State 1: {s1}"
        );

        // 2. Overview + history
        tui.history.push(PathBuf::from("/tmp/root"));
        let s2 = render_bottom_line(&mut tui);
        assert!(
            s2.contains(
                "↑↓←→ | Enter | R Refresh | O Open | P Preview | F File | Esc Back | q Quit"
            ),
            "State 2: {s2}"
        );

        // 3. Tree idle
        tui.view_mode = ViewMode::Tree;
        tui.selected_paths.clear();
        tui.largest_files.clear();
        let s3 = render_bottom_line(&mut tui);
        assert!(
            s3.contains("↑↓←→ | Space Select | Enter | / Filter | R Refresh | O Open | P Preview | F File | ⌫/Del | Esc Back | q Quit"),
            "State 3: {s3}"
        );

        // 4. Tree with selection
        tui.selected_paths.insert(PathBuf::from("/tmp/root/a"));
        let s4 = render_bottom_line(&mut tui);
        assert!(
            s4.contains("↑↓←→ | Space Select | Enter | / Filter | R Refresh | O Open | P Preview | F File | ⌫/Del 1 | Esc Back | q Quit"),
            "State 4: {s4}"
        );

        // 5. Tree filter typing
        tui.filter_mode = true;
        let s5 = render_bottom_line(&mut tui);
        assert!(
            s5.contains("Type to filter  |  Enter Apply  |  Esc Clear  |  Ctrl+C Quit"),
            "State 5: {s5}"
        );

        // 6. Tree filter applied
        tui.filter_mode = false;
        tui.filter_text = "test".into();
        let s6 = render_bottom_line(&mut tui);
        assert!(
            s6.contains("↑↓←→ | Enter | Space Select | / Edit | Esc Clear filter | O Open | P Preview | F File | ⌫/Del 1 | q Quit"),
            "State 6: {s6}"
        );

        // 7. Largest files idle
        tui.view_mode = ViewMode::LargestFiles;
        tui.filter_text.clear();
        tui.selected_paths.clear();
        let s7 = render_bottom_line(&mut tui);
        assert!(
            s7.contains("↑↓← | Space Select | / Filter | R Refresh | O Open | P Preview | F File | ⌫/Del | Esc Back | q Quit"),
            "State 7: {s7}"
        );

        // 8. Largest files filter
        tui.filter_text = "bin".into();
        let s8 = render_bottom_line(&mut tui);
        assert!(
            s8.contains("↑↓← | Space Select | / Edit | Esc Clear filter | O Open | P Preview | F File | ⌫/Del | q Quit"),
            "State 8: {s8}"
        );
    }

    #[test]
    fn test_additive_keybindings() {
        let mut tui = AnalyzeTui::new(false);
        let mut root = FileNode::new(
            "root".into(),
            PathBuf::from("/tmp/test_root"),
            1000,
            true,
            2,
        );
        let sub = FileNode::new(
            "sub".into(),
            PathBuf::from("/tmp/test_root/sub"),
            600,
            true,
            1,
        );
        let f1 = FileNode::new(
            "f1.txt".into(),
            PathBuf::from("/tmp/test_root/f1.txt"),
            400,
            false,
            1,
        );
        root.children = vec![sub, f1];
        tui.root = Some(root);
        tui.largest_files = vec![crate::analyze::model::LargestFile::new(
            PathBuf::from("/tmp/test_root/f1.txt"),
            500,
        )];

        // 1. T toggles top-files view (alongside Tab)
        assert_eq!(tui.view_mode, ViewMode::Tree);
        tui.handle_key_event(dummy_key(KeyCode::Char('T')));
        assert_eq!(tui.view_mode, ViewMode::LargestFiles);
        tui.handle_key_event(dummy_key(KeyCode::Tab));
        assert_eq!(tui.view_mode, ViewMode::Tree);
        tui.handle_key_event(dummy_key(KeyCode::Char('t')));
        assert_eq!(tui.view_mode, ViewMode::LargestFiles);
        tui.handle_key_event(dummy_key(KeyCode::Char('T')));
        assert_eq!(tui.view_mode, ViewMode::Tree);

        // 2. r and R trigger rescan
        tui.rescan_requested = false;
        tui.handle_key_event(dummy_key(KeyCode::Char('r')));
        assert!(tui.rescan_requested);
        tui.rescan_requested = false;
        tui.handle_key_event(dummy_key(KeyCode::Char('R')));
        assert!(tui.rescan_requested);

        // 3. Movement variants: J / K
        assert_eq!(tui.cursor, 0);
        tui.handle_key_event(dummy_key(KeyCode::Char('J')));
        assert_eq!(tui.cursor, 1);
        tui.handle_key_event(dummy_key(KeyCode::Char('K')));
        assert_eq!(tui.cursor, 0);

        // 4. L enters directory, H / b goes back
        tui.handle_key_event(dummy_key(KeyCode::Char('L')));
        assert_eq!(tui.path_indices, vec![0]);
        tui.handle_key_event(dummy_key(KeyCode::Char('H')));
        assert!(tui.path_indices.is_empty());
        tui.handle_key_event(dummy_key(KeyCode::Char('L')));
        assert_eq!(tui.path_indices, vec![0]);
        tui.handle_key_event(dummy_key(KeyCode::Char('b')));
        assert!(tui.path_indices.is_empty());
        tui.handle_key_event(dummy_key(KeyCode::Char('L')));
        assert_eq!(tui.path_indices, vec![0]);
        tui.handle_key_event(dummy_key(KeyCode::Char('B')));
        assert!(tui.path_indices.is_empty());

        // In largest files view, b / B / H returns to tree
        tui.view_mode = ViewMode::LargestFiles;
        tui.handle_key_event(dummy_key(KeyCode::Char('b')));
        assert_eq!(tui.view_mode, ViewMode::Tree);
        tui.view_mode = ViewMode::LargestFiles;
        tui.handle_key_event(dummy_key(KeyCode::Char('H')));
        assert_eq!(tui.view_mode, ViewMode::Tree);

        // Q quits
        assert!(tui.handle_key_event(dummy_key(KeyCode::Char('Q'))));
    }

    #[test]
    fn test_batch_open_and_reveal_selection() {
        let runner = Arc::new(FakeRunner::new());
        let mut tui = AnalyzeTui::new(false).with_runner(runner.clone() as Arc<dyn Runner>);

        let orig_disp = std::env::var_os("DISPLAY");
        unsafe {
            std::env::set_var("DISPLAY", ":0");
        }

        let file1 = PathBuf::from("/tmp/test_dir/file1.txt");
        let file2 = PathBuf::from("/tmp/test_dir/file2.txt");
        tui.selected_paths.insert(file1.clone());
        tui.selected_paths.insert(file2.clone());

        // O on selection -> opens parent directories
        tui.handle_key_event(dummy_key(KeyCode::Char('O')));
        let calls = runner.invocations();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].program, "xdg-open");
        assert_eq!(calls[1].program, "xdg-open");
        assert_eq!(tui.message, Some(("Opened 2 items".to_string(), false)));

        // F on selection -> reveals the files
        tui.handle_key_event(dummy_key(KeyCode::Char('F')));
        let calls = runner.invocations();
        assert_eq!(calls.len(), 4); // 2 previous + 2 new
        assert_eq!(calls[2].program, "xdg-open");
        assert_eq!(calls[2].args, vec![file1.as_os_str()]);
        assert_eq!(calls[3].program, "xdg-open");
        assert_eq!(calls[3].args, vec![file2.as_os_str()]);
        assert_eq!(tui.message, Some(("Revealed 2 items".to_string(), false)));

        unsafe {
            if let Some(v) = orig_disp {
                std::env::set_var("DISPLAY", v);
            } else {
                std::env::remove_var("DISPLAY");
            }
        }
    }

    #[test]
    fn test_batch_open_and_reveal_cap_20() {
        let runner = Arc::new(FakeRunner::new());
        let mut tui = AnalyzeTui::new(false).with_runner(runner.clone() as Arc<dyn Runner>);

        let orig_disp = std::env::var_os("DISPLAY");
        unsafe {
            std::env::set_var("DISPLAY", ":0");
        }

        // Select 21 items -> over cap of 20
        for i in 0..21 {
            tui.selected_paths
                .insert(PathBuf::from(format!("/tmp/file_{i}.txt")));
        }

        // O refused over cap
        tui.handle_key_event(dummy_key(KeyCode::Char('O')));
        assert!(
            runner.invocations().is_empty(),
            "runner must not be called over cap"
        );
        assert_eq!(
            tui.message,
            Some((
                "Too many items to open, max 20, selected 21".to_string(),
                true
            ))
        );

        // F refused over cap
        tui.handle_key_event(dummy_key(KeyCode::Char('F')));
        assert!(
            runner.invocations().is_empty(),
            "runner must not be called over cap"
        );
        assert_eq!(
            tui.message,
            Some((
                "Too many items to reveal, max 20, selected 21".to_string(),
                true
            ))
        );

        // Exactly 20 items -> accepted
        tui.selected_paths
            .remove(&PathBuf::from("/tmp/file_20.txt"));
        assert_eq!(tui.selected_paths.len(), 20);

        tui.handle_key_event(dummy_key(KeyCode::Char('O')));
        assert_eq!(runner.invocations().len(), 20);
        assert_eq!(tui.message, Some(("Opened 20 items".to_string(), false)));

        unsafe {
            if let Some(v) = orig_disp {
                std::env::set_var("DISPLAY", v);
            } else {
                std::env::remove_var("DISPLAY");
            }
        }
    }

    #[test]
    fn test_batch_open_and_reveal_per_item_failure() {
        let runner = Arc::new(FakeRunner::new());
        let mut tui = AnalyzeTui::new(false).with_runner(runner.clone() as Arc<dyn Runner>);

        let orig_disp = std::env::var_os("DISPLAY");
        unsafe {
            std::env::set_var("DISPLAY", ":0");
        }

        let p1 = PathBuf::from("/tmp/a_good.txt");
        let p2 = PathBuf::from("/tmp/b_bad.txt");
        let p3 = PathBuf::from("/tmp/c_good.txt");
        tui.selected_paths.insert(p1);
        tui.selected_paths.insert(p2);
        tui.selected_paths.insert(p3);

        runner.set_handler(|spec| {
            if spec
                .args
                .iter()
                .any(|arg| arg.to_string_lossy().contains("b_bad"))
            {
                Err(crate::runner::RunError::Spawn(std::io::Error::other(
                    "disk failure",
                )))
            } else {
                Ok(crate::runner::Output {
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    exit_code: 0,
                    signal: None,
                })
            }
        });

        // Trigger F: all 3 items must be attempted (not aborted after failure on b_bad)
        tui.handle_key_event(dummy_key(KeyCode::Char('F')));
        assert_eq!(runner.invocations().len(), 3);
        let msg = tui.message.as_ref().unwrap();
        assert!(msg.1, "message must indicate error");
        assert!(
            msg.0.starts_with("Failed to reveal 1/3 items:"),
            "expected partial failure count in: {}",
            msg.0
        );
        assert!(msg.0.contains("/tmp/b_bad.txt") && msg.0.contains("disk failure"));

        unsafe {
            if let Some(v) = orig_disp {
                std::env::set_var("DISPLAY", v);
            } else {
                std::env::remove_var("DISPLAY");
            }
        }
    }

    #[test]
    fn test_largest_files_independent_filter() {
        let mut tui = AnalyzeTui::new(false);
        tui.largest_files = vec![
            crate::analyze::model::LargestFile::new(PathBuf::from("/tmp/alpha.log"), 1000),
            crate::analyze::model::LargestFile::new(PathBuf::from("/tmp/beta.txt"), 800),
            crate::analyze::model::LargestFile::new(PathBuf::from("/tmp/gamma.log"), 500),
        ];
        tui.view_mode = ViewMode::LargestFiles;

        // Enter filter mode with '/'
        tui.handle_key_event(dummy_key(KeyCode::Char('/')));
        assert!(tui.large_filter_mode);
        assert!(!tui.filter_mode, "tree filter mode must not be affected");

        // Type "log"
        tui.handle_key_event(dummy_key(KeyCode::Char('l')));
        tui.handle_key_event(dummy_key(KeyCode::Char('o')));
        tui.handle_key_event(dummy_key(KeyCode::Char('g')));
        assert_eq!(tui.large_filter, "log");
        assert_eq!(tui.filter_text, "", "tree filter text must not be affected");
        assert_eq!(tui.filtered_largest_files().len(), 2);

        // Render check: header contains match count and cursor
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| tui.render(f, f.area())).unwrap();
        let text = buffer_to_string(&terminal);
        assert!(
            text.contains("[Filter: \"log\"▌ (2 matches)]"),
            "expected filter with cursor in header:\n{text}"
        );

        // Enter to apply filter
        tui.handle_key_event(dummy_key(KeyCode::Enter));
        assert!(!tui.large_filter_mode);
        assert_eq!(tui.large_filter, "log");

        terminal.draw(|f| tui.render(f, f.area())).unwrap();
        let text = buffer_to_string(&terminal);
        assert!(
            text.contains("[Filter: \"log\" (2 matches)]"),
            "expected applied filter in header:\n{text}"
        );

        // Filter matching nothing renders "No matches for..."
        tui.large_filter = "zzz".into();
        terminal.draw(|f| tui.render(f, f.area())).unwrap();
        let text = buffer_to_string(&terminal);
        assert!(
            text.contains("No matches for \"zzz\""),
            "expected no matches message in:\n{text}"
        );
    }

    #[test]
    fn test_largest_files_filter_esc_ordering() {
        let mut tui = AnalyzeTui::new(false);
        tui.largest_files = vec![crate::analyze::model::LargestFile::new(
            PathBuf::from("/tmp/file.dat"),
            1000,
        )];
        tui.view_mode = ViewMode::LargestFiles;

        // 1. Esc while typing: clears query and exits typing, stays in LargestFiles
        tui.handle_key_event(dummy_key(KeyCode::Char('/')));
        tui.handle_key_event(dummy_key(KeyCode::Char('a')));
        assert!(tui.large_filter_mode);
        assert_eq!(tui.large_filter, "a");
        tui.handle_key_event(dummy_key(KeyCode::Esc));
        assert!(!tui.large_filter_mode);
        assert_eq!(tui.large_filter, "");
        assert_eq!(tui.view_mode, ViewMode::LargestFiles);

        // 2. Esc when filter is applied: clears filter, stays in LargestFiles
        tui.large_filter = "applied".into();
        tui.large_filter_mode = false;
        tui.handle_key_event(dummy_key(KeyCode::Esc));
        assert_eq!(tui.large_filter, "");
        assert_eq!(tui.view_mode, ViewMode::LargestFiles);

        // 3. Esc when filter is empty: leaves LargestFiles, returns to Tree
        tui.handle_key_event(dummy_key(KeyCode::Esc));
        assert_eq!(tui.view_mode, ViewMode::Tree);
    }

    #[test]
    fn test_largest_files_filter_cursor_clamp() {
        let mut tui = AnalyzeTui::new(false);
        tui.largest_files = vec![
            crate::analyze::model::LargestFile::new(PathBuf::from("/tmp/alpha.dat"), 1000),
            crate::analyze::model::LargestFile::new(PathBuf::from("/tmp/beta.dat"), 800),
            crate::analyze::model::LargestFile::new(PathBuf::from("/tmp/gamma.dat"), 600),
        ];
        tui.view_mode = ViewMode::LargestFiles;
        tui.cursor = 2; // at gamma

        // Filter to "alpha": only 1 match -> cursor clamped to 0
        tui.handle_key_event(dummy_key(KeyCode::Char('/')));
        for c in "alpha".chars() {
            tui.handle_key_event(dummy_key(KeyCode::Char(c)));
        }
        assert_eq!(tui.filtered_largest_files().len(), 1);
        assert_eq!(tui.cursor, 0, "cursor must clamp to available match index");

        // Filter matching nothing -> cursor is 0
        tui.handle_key_event(dummy_key(KeyCode::Char('x')));
        assert_eq!(tui.filtered_largest_files().len(), 0);
        assert_eq!(tui.cursor, 0);
    }

    #[test]
    fn test_largest_files_space_selection_and_no_delete() {
        let mut tui = AnalyzeTui::new(false);
        let p1 = PathBuf::from("/tmp/big1.dat");
        let p2 = PathBuf::from("/tmp/big2.dat");
        tui.largest_files = vec![
            crate::analyze::model::LargestFile::new(p1.clone(), 1000),
            crate::analyze::model::LargestFile::new(p2.clone(), 800),
        ];
        tui.view_mode = ViewMode::LargestFiles;
        tui.cursor = 0;

        // Space toggles selection
        tui.handle_key_event(dummy_key(KeyCode::Char(' ')));
        assert!(tui.selected_paths.contains(&p1));
        assert_eq!(tui.selected_paths.len(), 1);

        // Render shows checked marker
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| tui.render(f, f.area())).unwrap();
        let text = buffer_to_string(&terminal);
        assert!(
            text.contains('●'),
            "expected checked marker for selected item"
        );

        // Space toggles off
        tui.handle_key_event(dummy_key(KeyCode::Char(' ')));
        assert!(!tui.selected_paths.contains(&p1));
        assert_eq!(tui.selected_paths.len(), 0);

        // Select again and ensure delete is prevented in LargestFiles view
        tui.handle_key_event(dummy_key(KeyCode::Char(' ')));
        assert_eq!(tui.selected_paths.len(), 1);

        tui.handle_key_event(dummy_key(KeyCode::Backspace));
        assert!(
            tui.confirm_delete.is_none(),
            "batch delete must be prevented in largest files"
        );
        tui.handle_key_event(dummy_key(KeyCode::Delete));
        assert!(
            tui.confirm_delete.is_none(),
            "batch delete must be prevented in largest files"
        );
        tui.handle_key_event(dummy_key(KeyCode::Char('d')));
        assert!(
            tui.confirm_delete.is_none(),
            "single delete must be prevented in largest files"
        );
    }

    #[test]
    fn test_inline_confirm_rendered_single() {
        let mut tui = AnalyzeTui::new(false);
        let mut root = FileNode::new("root".into(), PathBuf::from("/tmp/root"), 1024, true, 1);
        let victim = FileNode::new(
            "victim.txt".into(),
            PathBuf::from("/tmp/root/victim.txt"),
            1024,
            false,
            1,
        );
        root.children = vec![victim];
        tui.root = Some(root);

        // Press 'd' to trigger single delete confirm
        tui.handle_key_event(dummy_key(KeyCode::Char('d')));
        assert!(matches!(tui.confirm_delete, DeleteConfirm::Single(..)));

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| tui.render(f, f.area())).unwrap();
        let text = buffer_to_string(&terminal);

        assert!(
            text.contains("Delete: victim.txt, 1.0 KB  Press Enter to confirm  |  Esc cancel"),
            "missing single inline confirm line in:\n{text}"
        );

        // Press Esc to cancel
        tui.handle_key_event(dummy_key(KeyCode::Esc));
        assert!(tui.confirm_delete.is_none());

        terminal.draw(|f| tui.render(f, f.area())).unwrap();
        let text_after = buffer_to_string(&terminal);
        assert!(
            !text_after.contains("Press Enter to confirm"),
            "confirm line should disappear after Esc in:\n{text_after}"
        );
    }

    #[test]
    fn test_inline_confirm_rendered_multi() {
        let mut tui = AnalyzeTui::new(false);
        let mut root = FileNode::new("root".into(), PathBuf::from("/tmp/root"), 3072, true, 3);
        let f1 = FileNode::new(
            "alpha.txt".into(),
            PathBuf::from("/tmp/root/alpha.txt"),
            1024,
            false,
            1,
        );
        let f2 = FileNode::new(
            "beta.txt".into(),
            PathBuf::from("/tmp/root/beta.txt"),
            1024,
            false,
            1,
        );
        let f3 = FileNode::new(
            "gamma.txt".into(),
            PathBuf::from("/tmp/root/gamma.txt"),
            1024,
            false,
            1,
        );
        root.children = vec![f1, f2, f3];
        tui.root = Some(root);

        tui.selected_paths
            .insert(PathBuf::from("/tmp/root/alpha.txt"));
        tui.selected_paths
            .insert(PathBuf::from("/tmp/root/beta.txt"));
        tui.selected_paths
            .insert(PathBuf::from("/tmp/root/gamma.txt"));

        // Press Backspace to trigger batch confirm
        tui.handle_key_event(dummy_key(KeyCode::Backspace));
        assert!(matches!(tui.confirm_delete, DeleteConfirm::Batch(..)));

        let backend = TestBackend::new(120, 15);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| tui.render(f, f.area())).unwrap();
        let text = buffer_to_string(&terminal);

        assert!(
            text.contains("Delete: 3 items, 3.0 KB  Press Enter to confirm  |  Esc cancel"),
            "missing multi inline confirm header line in:\n{text}"
        );
        assert!(text.contains("• alpha.txt") || text.contains("alpha.txt"));
        assert!(text.contains("• beta.txt") || text.contains("beta.txt"));
        assert!(text.contains("• gamma.txt") || text.contains("gamma.txt"));

        // Press 'q' to cancel
        tui.handle_key_event(dummy_key(KeyCode::Char('q')));
        assert!(tui.confirm_delete.is_none());
        assert_eq!(
            tui.selected_paths.len(),
            3,
            "selections preserved on cancel"
        );

        terminal.draw(|f| tui.render(f, f.area())).unwrap();
        let text_after = buffer_to_string(&terminal);
        assert!(
            !text_after.contains("Press Enter to confirm"),
            "confirm line should disappear after q in:\n{text_after}"
        );
    }

    #[test]
    fn test_busy_view_live_counter_rendered() {
        let mut tui = AnalyzeTui::new(false);
        let root = FileNode::new("root".into(), PathBuf::from("/tmp/root"), 1024, true, 1);
        tui.root = Some(root);

        tui.deleting = true;
        tui.delete_count = 0;

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| tui.render(f, f.area())).unwrap();
        let text0 = buffer_to_string(&terminal);

        assert!(
            text0.contains("Deleting: moving to Trash, please wait…"),
            "missing count=0 busy view line in:\n{text0}"
        );
        assert!(
            text0.contains("Ctrl+C Quit"),
            "missing Ctrl+C Quit hint in footer in:\n{text0}"
        );

        // Counter advances to 12
        tui.delete_count = 12;
        terminal.draw(|f| tui.render(f, f.area())).unwrap();
        let text12 = buffer_to_string(&terminal);

        assert!(
            text12.contains("Deleting: 12 items removed, please wait…"),
            "missing count>0 busy view line in:\n{text12}"
        );
    }

    #[test]
    fn test_input_locked_while_deleting() {
        let mut tui = AnalyzeTui::new(false);
        let mut root = FileNode::new("root".into(), PathBuf::from("/tmp/root"), 200, true, 2);
        let f1 = FileNode::new(
            "f1.txt".into(),
            PathBuf::from("/tmp/root/f1.txt"),
            100,
            false,
            1,
        );
        let f2 = FileNode::new(
            "f2.txt".into(),
            PathBuf::from("/tmp/root/f2.txt"),
            100,
            false,
            1,
        );
        root.children = vec![f1, f2];
        tui.root = Some(root);

        tui.deleting = true;
        tui.cursor = 0;

        // Keys must be ignored while deleting
        assert!(!tui.handle_key_event(dummy_key(KeyCode::Esc)));
        assert!(!tui.handle_key_event(dummy_key(KeyCode::Char('q'))));
        assert!(!tui.handle_key_event(dummy_key(KeyCode::Char('j'))));
        assert!(!tui.handle_key_event(dummy_key(KeyCode::Enter)));
        assert!(!tui.handle_key_event(dummy_key(KeyCode::Backspace)));
        assert!(!tui.handle_key_event(dummy_key(KeyCode::Delete)));
        assert!(!tui.handle_key_event(dummy_key(KeyCode::Char(' '))));

        assert_eq!(tui.cursor, 0, "cursor must not move while deleting");
        assert!(tui.deleting, "deleting state must not be aborted by Esc/q");

        // Ctrl+C must quit
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), crossterm::event::KeyModifiers::CONTROL);
        assert!(
            tui.handle_key_event(ctrl_c),
            "Ctrl+C must return true to quit"
        );
    }

    #[test]
    fn test_sentinel_esc_and_q_leaves_disk_untouched() {
        let d = tempdir("sentinel-esc-q");
        let s1 = d.join("sentinel1.txt");
        let s2 = d.join("sentinel2.txt");
        std::fs::write(&s1, b"sentinel1").unwrap();
        std::fs::write(&s2, b"sentinel2").unwrap();

        let mut tui = AnalyzeTui::new(false);
        let mut root = FileNode::new("d".into(), d.clone(), 20, true, 2);
        let n1 = FileNode::new("sentinel1.txt".into(), s1.clone(), 10, false, 1);
        let n2 = FileNode::new("sentinel2.txt".into(), s2.clone(), 10, false, 1);
        root.children = vec![n1, n2];
        tui.root = Some(root);

        tui.selected_paths.insert(s1.clone());
        tui.selected_paths.insert(s2.clone());

        // Open batch confirm and Esc
        tui.handle_key_event(dummy_key(KeyCode::Backspace));
        assert!(tui.confirm_delete.is_some());
        tui.handle_key_event(dummy_key(KeyCode::Esc));
        assert!(tui.confirm_delete.is_none());
        assert!(s1.exists(), "sentinel1 must exist on disk after Esc");
        assert!(s2.exists(), "sentinel2 must exist on disk after Esc");
        assert_eq!(tui.selected_paths.len(), 2);

        // Open batch confirm and 'q'
        tui.handle_key_event(dummy_key(KeyCode::Delete));
        assert!(tui.confirm_delete.is_some());
        tui.handle_key_event(dummy_key(KeyCode::Char('q')));
        assert!(tui.confirm_delete.is_none());
        assert!(s1.exists(), "sentinel1 must exist on disk after q");
        assert!(s2.exists(), "sentinel2 must exist on disk after q");
        assert_eq!(tui.selected_paths.len(), 2);

        // Single delete on cursor and Esc
        tui.selected_paths.clear();
        tui.cursor = 0;
        tui.handle_key_event(dummy_key(KeyCode::Char('d')));
        assert!(tui.confirm_delete.is_some());
        tui.handle_key_event(dummy_key(KeyCode::Esc));
        assert!(tui.confirm_delete.is_none());
        assert!(s1.exists(), "sentinel1 must exist on disk after Esc");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn test_async_delete_e2e_live_progress_and_completion() {
        let d = tempdir("async-e2e");
        let f1 = d.join("a.txt");
        let f2 = d.join("b.txt");
        let f3 = d.join("c.txt");
        std::fs::write(&f1, b"aaa").unwrap();
        std::fs::write(&f2, b"bbb").unwrap();
        std::fs::write(&f3, b"ccc").unwrap();

        let mut tui = AnalyzeTui::new(false);
        let mut root = FileNode::new("d".into(), d.clone(), 30, true, 3);
        let n1 = FileNode::new("a.txt".into(), f1.clone(), 10, false, 1);
        let n2 = FileNode::new("b.txt".into(), f2.clone(), 10, false, 1);
        let n3 = FileNode::new("c.txt".into(), f3.clone(), 10, false, 1);
        root.children = vec![n1, n2, n3];
        tui.root = Some(root);

        tui.selected_paths.insert(f1.clone());
        tui.selected_paths.insert(f2.clone());
        tui.selected_paths.insert(f3.clone());

        tui.handle_key_event(dummy_key(KeyCode::Backspace));
        assert!(tui.confirm_delete.is_some());

        // Confirm via Enter
        tui.handle_key_event(dummy_key(KeyCode::Enter));
        assert!(tui.deleting);

        // Wait for async deletion to complete
        tui.wait_for_delete();
        assert!(!tui.deleting);
        assert!(!f1.exists(), "f1 should be trashed");
        assert!(!f2.exists(), "f2 should be trashed");
        assert!(!f3.exists(), "f3 should be trashed");

        assert_eq!(tui.selected_paths.len(), 0);
        assert_eq!(tui.current_node().unwrap().children.len(), 0);
        assert_eq!(tui.current_node().unwrap().size, 0);
        assert!(
            tui.message
                .as_ref()
                .unwrap()
                .0
                .contains("Moved 3 items to trash"),
            "expected success message in {:?}",
            tui.message
        );

        let _ = std::fs::remove_dir_all(&d);
    }
}
