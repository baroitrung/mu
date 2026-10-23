//! Ratatui ncdu-like interactive disk usage explorer.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::analyze::model::{FileNode, LargestFile, ScanMessage, SortMode, ViewMode};
use crate::paths;
use crate::runner::{CommandSpec, Runner};
use crate::trash;
use crate::tui::styles;

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
    /// If Some, modal confirmation is open for this target path.
    pub confirm_delete: Option<PathBuf>,
    pub confirm_size: u64,
    /// If Some, batch modal confirmation is open for these target paths.
    pub confirm_batch: Option<Vec<PathBuf>>,
    pub confirm_batch_size: u64,
    /// 0 = NO, 1 = YES (default 0).
    pub confirm_cursor: usize,
    /// Multi-selected paths for batch operations.
    pub selected_paths: HashSet<PathBuf>,
    /// Whether user is actively typing a name filter.
    pub filter_mode: bool,
    /// Current search/filter string.
    pub filter_text: String,
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
            confirm_delete: None,
            confirm_size: 0,
            confirm_batch: None,
            confirm_batch_size: 0,
            confirm_cursor: 0,
            selected_paths: HashSet::new(),
            filter_mode: false,
            filter_text: String::new(),
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
        }
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
                let (c_bytes, c_count) = count_cleanable_nodes(&root, &self.clean_roots);
                self.cleanable_cache_bytes = c_bytes;
                self.cleanable_cache_count = c_count;
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
    /// Returns true if any modal confirmation is open.
    pub fn is_confirm_open(&self) -> bool {
        self.confirm_delete.is_some() || self.confirm_batch.is_some()
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
                KeyCode::Up | KeyCode::Char('k') => {
                    self.navigate_up_cursor();
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.navigate_down();
                }
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                    self.enter_overview_location();
                }
                KeyCode::Char('r') | KeyCode::Char('R') => {
                    self.trigger_rescan();
                }
                KeyCode::Char('o') | KeyCode::Char('O') => {
                    self.trigger_open_dir();
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    return true;
                }
                _ => {}
            }
            return false;
        }

        match key.code {
            KeyCode::Char('q') => {
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
                if !self.filter_text.is_empty() {
                    self.filter_text.clear();
                    self.cursor = 0;
                    self.scroll_off = 0;
                } else if self.view_mode == ViewMode::LargestFiles {
                    self.view_mode = ViewMode::Tree;
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
            KeyCode::Tab => {
                self.view_mode = match self.view_mode {
                    ViewMode::Tree => ViewMode::LargestFiles,
                    ViewMode::LargestFiles => ViewMode::Tree,
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
            KeyCode::Char('/') if self.view_mode == ViewMode::Tree => {
                self.filter_mode = true;
            }
            KeyCode::Char(' ') if self.view_mode == ViewMode::Tree => {
                self.toggle_select_current();
            }
            KeyCode::Backspace | KeyCode::Delete if self.view_mode == ViewMode::Tree => {
                self.trigger_batch_delete();
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
                self.confirm_batch = None;
            }
            KeyCode::Enter => {
                if self.confirm_cursor == 1 {
                    if self.confirm_batch.is_some() {
                        self.execute_batch_delete();
                    } else {
                        self.execute_delete();
                    }
                } else {
                    self.confirm_delete = None;
                    self.confirm_batch = None;
                }
            }
            _ => {}
        }
        false
    }

    fn navigate_down(&mut self) {
        let max_items = match self.view_mode {
            ViewMode::Overview => self.overview_locations().len(),
            ViewMode::Tree => self.filtered_children().len(),
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
        let Some((_, child)) = self.current_child() else {
            return;
        };
        let target_path = child.path.clone();

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

        self.confirm_delete = Some(target_path);
        self.confirm_size = target_size;
        self.confirm_cursor = 0; // Default NO
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

        self.confirm_batch = Some(targets);
        self.confirm_batch_size = total_size;
        self.confirm_cursor = 0; // Default NO
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

    fn execute_batch_delete(&mut self) {
        let Some(batch_targets) = self.confirm_batch.take() else {
            return;
        };

        let targets = drop_descendants(&batch_targets);

        let mut success_count = 0usize;
        let mut fail_count = 0usize;
        let mut first_error: Option<String> = None;

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
            if self.is_path_protected(&target_path) {
                fail_count += 1;
                if first_error.is_none() {
                    first_error = Some(format!(
                        "Refused: {} is a protected path",
                        target_path.display()
                    ));
                }
                continue;
            }

            match trash::safe_delete(&target_path, self.dry_run) {
                Ok(()) => {
                    success_count += 1;
                    if !self.dry_run {
                        if let Some(root) = &mut self.root {
                            remove_from_tree(root, &target_path);
                        }
                        self.selected_paths.remove(&target_path);
                    }
                }
                Err(e) => {
                    fail_count += 1;
                    if first_error.is_none() {
                        first_error = Some(format!("{}: {e}", target_path.display()));
                    }
                }
            }
        }

        if !self.dry_run {
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

        let max_len = self.filtered_children().len();
        if self.cursor >= max_len && self.cursor > 0 {
            self.cursor = max_len.saturating_sub(1);
        }

        if fail_count == 0 {
            let msg = if self.dry_run {
                format!("Moved {success_count} items to trash (dry run)")
            } else {
                format!("Moved {success_count} items to trash")
            };
            self.message = Some((msg, false));
        } else if success_count > 0 {
            self.message = Some((
                format!(
                    "Moved {success_count} items to trash; {fail_count} failed: {}",
                    first_error.unwrap_or_default()
                ),
                true,
            ));
        } else {
            self.message = Some((
                format!(
                    "Batch delete failed ({fail_count} errors): {}",
                    first_error.unwrap_or_default()
                ),
                true,
            ));
        }
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
                if let Some(root) = &mut self.root {
                    remove_from_tree(root, &target_path);
                    root.recalculate_sizes();
                }

                self.selected_paths.remove(&target_path);

                // Re-adjust cursor if needed
                let max_len = self.filtered_children().len();
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
            ViewMode::Overview => self.render_overview(f, chunks[1]),
            ViewMode::Tree => self.render_tree(f, chunks[1]),
            ViewMode::LargestFiles => self.render_largest(f, chunks[1]),
        }

        self.render_footer(f, chunks[2]);

        if self.is_confirm_open() {
            self.render_confirm_dialog(f, area);
        } else if self.preview_modal.is_some() {
            self.render_preview_dialog(f, area);
        }
    }

    fn render_header(&self, f: &mut Frame, area: Rect) {
        if self.view_mode == ViewMode::Overview {
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
                "[View: Overview]  ",
                Style::default().fg(Color::Magenta),
            ));

            let lines = vec![
                Line::from(vec![Span::styled(
                    "Disk Usage Overview",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )]),
                Line::from(status_spans),
            ];

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
            ViewMode::Overview => "[View: Overview]  ",
            ViewMode::Tree => "[View: Tree]  ",
            ViewMode::LargestFiles => "[View: Largest Files]  ",
        };
        status_spans.push(Span::styled(
            view_label,
            Style::default().fg(Color::Magenta),
        ));

        if !self.filter_text.is_empty() {
            let matches = self.filtered_children().len();
            status_spans.push(Span::styled(
                format!("[Filter: \"{}\" ({matches} matches)]  ", self.filter_text),
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

        let mut lines = Vec::new();
        for (display_idx, (_orig_idx, child)) in items
            .into_iter()
            .enumerate()
            .skip(self.scroll_off)
            .take(visible_height)
        {
            let is_cursor = display_idx == self.cursor;
            let is_checked = self.selected_paths.contains(&child.path);

            let cursor_str = if is_cursor { "▶" } else { " " };
            let check_str = if is_checked { "[x] " } else { "    " };
            let prefix = format!("{cursor_str} {check_str}");

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

            let mut style = if is_cursor {
                selected_style()
            } else if is_checked {
                Style::default().fg(Color::Green)
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
            let clean_badge = if is_cleanable(child, &self.clean_roots) {
                Span::styled(
                    " [cleanable]",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("")
            };

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
                clean_badge,
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
        if self.filter_mode {
            let matches = self.filtered_children().len();
            let p = Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("/{} ", self.filter_text),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("({matches} matches)"),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    " · Esc clear/exit · Enter finish typing",
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
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

        let help = if self.view_mode == ViewMode::Overview {
            "↑/↓/j/k nav · Enter browse · R rescan · O open · q quit"
        } else {
            "↑/↓ nav · Space sel · / filter · ⌫ del · R rescan · O open · F reveal · P view · s sort · Tab · q quit"
        };
        let p = Paragraph::new(Line::from(vec![Span::styled(
            help,
            Style::default().fg(Color::DarkGray),
        )]));
        f.render_widget(p, area);
    }

    fn render_confirm_dialog(&self, f: &mut Frame, area: Rect) {
        if let Some(targets) = &self.confirm_batch {
            let count = targets.len();
            let size_str = crate::size::human_size(self.confirm_batch_size as i64);

            let dialog_area = centered_rect(70, 50, area);
            f.render_widget(Clear, dialog_area);

            let block = Block::default()
                .title(" Move to Trash (Batch) ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red));

            let prompt = format!("Move {count} selected items ({size_str}) to trash?");

            let mut lines = vec![
                Line::from(""),
                Line::from(Span::styled(
                    prompt,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
            ];

            for target in targets.iter().take(5) {
                let name = target
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| target.display().to_string());
                lines.push(Line::from(vec![
                    Span::raw("  • "),
                    Span::styled(name, Style::default().fg(Color::Yellow)),
                    Span::styled(
                        format!(" ({})", target.display()),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }

            if count > 5 {
                lines.push(Line::from(Span::styled(
                    format!("  ... and {} more", count - 5),
                    Style::default().fg(Color::DarkGray),
                )));
            }

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

            lines.extend([
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
            ]);

            let paragraph = Paragraph::new(lines)
                .block(block)
                .alignment(Alignment::Center);
            f.render_widget(paragraph, dialog_area);
            return;
        }

        let Some(target) = &self.confirm_delete else {
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

    fn enter_overview_location(&mut self) {
        let locs = self.overview_locations();
        if self.cursor >= locs.len() {
            return;
        }
        let loc_name = locs[self.cursor].name;
        let loc_path = locs[self.cursor].path.clone();
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
        self.scanning = true;
        self.rescan_requested = true;
        self.scan_progress_text = "Rescanning...".to_string();
        self.message = Some(("Rescanning...".to_string(), false));
    }

    fn trigger_open_dir(&mut self) {
        let target_dir = match self.view_mode {
            ViewMode::Overview => {
                let locs = self.overview_locations();
                if self.cursor < locs.len() {
                    locs[self.cursor].path.clone()
                } else {
                    return;
                }
            }
            ViewMode::Tree => {
                if let Some((_, child)) = self.current_child() {
                    if child.is_dir {
                        child.path.clone()
                    } else {
                        child.path.parent().unwrap_or(&child.path).to_path_buf()
                    }
                } else if let Some(node) = self.current_node() {
                    node.path.clone()
                } else {
                    return;
                }
            }
            ViewMode::LargestFiles => {
                if self.cursor < self.largest_files.len() {
                    let file = &self.largest_files[self.cursor];
                    file.path.parent().unwrap_or(&file.path).to_path_buf()
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
    }

    fn trigger_reveal_file(&mut self) {
        let target_path = match self.view_mode {
            ViewMode::Overview => return,
            ViewMode::Tree => {
                if let Some((_, child)) = self.current_child() {
                    child.path.clone()
                } else if let Some(node) = self.current_node() {
                    node.path.clone()
                } else {
                    return;
                }
            }
            ViewMode::LargestFiles => {
                if self.cursor < self.largest_files.len() {
                    self.largest_files[self.cursor].path.clone()
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

        let spec = CommandSpec::new("xdg-open", [target_path.as_os_str()]);
        match self.runner.run(&spec) {
            Ok(_) => {
                self.message = Some((format!("Revealed {}", target_path.display()), false));
            }
            Err(e) => {
                self.message = Some((
                    format!("Failed to reveal {}: {e}", target_path.display()),
                    true,
                ));
            }
        }
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
                if self.cursor < self.largest_files.len() {
                    self.largest_files[self.cursor].path.clone()
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
        assert!(tui.confirm_batch.is_some());
        // Batch confirm targets must contain only sub
        let targets = tui.confirm_batch.as_ref().unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0], sub);

        // Confirm YES
        tui.confirm_cursor = 1;
        tui.handle_key_event(dummy_key(KeyCode::Enter));

        assert!(!sub.exists(), "ancestor dir should be trashed");
        assert!(!file1.exists(), "child inside trashed dir should be gone");
        assert!(file2.exists(), "unrelated file should remain intact");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn test_no_means_no_change() {
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
        assert!(tui.confirm_batch.is_some());
        assert_eq!(tui.confirm_cursor, 0, "default confirm cursor must be NO");

        // Press Enter on NO
        tui.handle_key_event(dummy_key(KeyCode::Enter));
        assert!(tui.confirm_batch.is_none());
        assert_eq!(
            tui.selected_paths.len(),
            2,
            "selections must remain untouched on NO"
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
        assert!(tui.confirm_batch.is_some());

        // Confirm YES
        tui.confirm_cursor = 1;
        tui.handle_key_event(dummy_key(KeyCode::Enter));

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
        assert!(tui.confirm_batch.is_none());

        // Backspace again to empty
        tui.handle_key_event(dummy_key(KeyCode::Backspace));
        assert_eq!(tui.filter_text, "");
        assert!(tui.confirm_delete.is_none());
        assert!(tui.confirm_batch.is_none());

        // Backspace on empty filter text in filter mode must still NOT trigger delete
        tui.handle_key_event(dummy_key(KeyCode::Backspace));
        assert_eq!(tui.filter_text, "");
        assert!(tui.confirm_delete.is_none());
        assert!(tui.confirm_batch.is_none());
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
        assert!(tui.confirm_batch.is_some());

        // Confirm YES
        tui.confirm_cursor = 1;
        tui.handle_key_event(dummy_key(KeyCode::Enter));

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

        // Must display [cleanable]
        assert!(
            text.contains("[cleanable]"),
            "missing cleanable badge in:\n{text}"
        );

        // Separate lines check
        let lines: Vec<&str> = text.lines().collect();
        let node_modules_line = lines
            .iter()
            .find(|l| l.contains("node_modules/"))
            .expect("node_modules line");
        assert!(
            node_modules_line.contains("[cleanable]"),
            "node_modules must have badge: {node_modules_line}"
        );

        let target_line = lines
            .iter()
            .find(|l| l.contains("target/"))
            .expect("target line");
        assert!(
            target_line.contains("[cleanable]"),
            "target must have badge: {target_line}"
        );

        let venv_line = lines
            .iter()
            .find(|l| l.contains(".venv/"))
            .expect(".venv line");
        assert!(
            venv_line.contains("[cleanable]"),
            ".venv must have badge: {venv_line}"
        );

        let dist_line = lines
            .iter()
            .find(|l| l.contains("dist/"))
            .expect("dist line");
        assert!(
            dist_line.contains("[cleanable]"),
            "dist must have badge: {dist_line}"
        );

        let tagged_line = lines
            .iter()
            .find(|l| l.contains("tagged_cache/"))
            .expect("tagged line");
        assert!(
            tagged_line.contains("[cleanable]"),
            "tagged_cache must have badge: {tagged_line}"
        );

        let src_line = lines.iter().find(|l| l.contains("src/")).expect("src line");
        assert!(
            !src_line.contains("[cleanable]"),
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
}
