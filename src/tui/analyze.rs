//! Ratatui ncdu-like interactive disk usage explorer.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

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

        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Esc => {
                if !self.filter_text.is_empty() {
                    self.filter_text.clear();
                    self.cursor = 0;
                    self.scroll_off = 0;
                } else if self.view_mode == ViewMode::LargestFiles {
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
                    self.navigate_up();
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
            self.cursor = prev_idx;
            self.scroll_off = 0;
            self.apply_sort();
            let max_len = self.filtered_children().len();
            if self.cursor >= max_len && max_len > 0 {
                self.cursor = max_len - 1;
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
            ViewMode::Tree => self.render_tree(f, chunks[1]),
            ViewMode::LargestFiles => self.render_largest(f, chunks[1]),
        }

        self.render_footer(f, chunks[2]);

        if self.is_confirm_open() {
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

        let help = "↑/↓/j/k nav · Space select · / filter · ⌫/Del batch · d del · s sort · Tab largest · q quit";
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
}
