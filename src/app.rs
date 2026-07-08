//! Terminal application state and event loop.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Position, Rect};
use ratatui::Terminal;

use crate::git::GitRepo;
use crate::highlight::{self, HlLines};
use crate::types::{DiffMode, FileEntry, FileStatus, RowKind, SideBySideDiff};
use crate::{diff, update};

/// How long a footer notification stays visible before the shortcut
/// hints come back.
const STATUS_TTL: Duration = Duration::from_secs(2);

/// How often the background watcher re-fingerprints the repository to
/// pick up external changes (edits, commits, staging).
const WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// Context rows kept above a change hunk when jumping with n/N, so the
/// target doesn't sit flush against the top of the viewport.
const JUMP_PADDING: usize = 5;

/// Highlight cache entries beyond this count are dropped wholesale;
/// keeps memory bounded without LRU bookkeeping.
const HL_CACHE_MAX: usize = 256;

/// One side of a file handed to the highlight worker thread.
struct HlJob {
    generation: u64,
    is_left: bool,
    key: u64,
    path: String,
    content: String,
}

/// A finished highlight, sent back from the worker to the UI thread.
struct HlUpdate {
    generation: u64,
    is_left: bool,
    key: u64,
    lines: Arc<HlLines>,
}

/// What the file panel actually showed on the last draw, recorded by the
/// renderer so mouse events can be mapped back to panel rows.
#[derive(Default)]
pub(crate) struct FilePanelView {
    /// Inner (borderless) area of the panel; zero-sized when hidden.
    pub(crate) area: Rect,
    /// Effective scroll offset after clamping.
    pub(crate) offset: usize,
    /// Total number of rows (tree view includes directory rows).
    pub(crate) total: usize,
    /// File index per visible row, top to bottom; `None` for dir rows.
    pub(crate) rows: Vec<Option<usize>>,
}

/// Outcome of the background update check, shared with the checker thread.
#[derive(Debug, Clone)]
pub(crate) enum UpdateState {
    Checking,
    UpToDate,
    Available(String),
    Failed(String),
}

/// All state needed to render and drive the TUI.
pub struct App {
    pub(crate) repo: GitRepo,
    pub(crate) mode: DiffMode,
    pub(crate) base: String,
    pub(crate) branch: String,
    pub(crate) files: Vec<FileEntry>,
    pub(crate) selected: usize,
    pub(crate) diff: SideBySideDiff,
    pub(crate) scroll: usize,
    /// Horizontal scroll offset of the old (left) diff pane.
    pub(crate) h_scroll_old: usize,
    /// Horizontal scroll offset of the new (right) diff pane.
    pub(crate) h_scroll_new: usize,
    /// Horizontal scroll offset of the file panel.
    pub(crate) h_scroll_files: usize,
    pub(crate) show_help: bool,
    /// Compact view: hide alignment filler rows so each pane shows its
    /// version of the file contiguously.
    pub(crate) compact: bool,
    /// Whether the changed-files panel is visible.
    pub(crate) show_files: bool,
    /// Whether the file panel renders as a directory tree (vs a flat list).
    pub(crate) tree_view: bool,
    /// Whether the file panel expands to fit its widest row (vs the
    /// fixed default width).
    pub(crate) wide_files: bool,
    /// Whether the old (left) diff pane is visible.
    pub(crate) show_old: bool,
    /// Whether the new (right) diff pane is visible.
    pub(crate) show_new: bool,
    /// Transient footer notification and when it was set; cleared after
    /// [`STATUS_TTL`] so the shortcut hints reappear.
    pub(crate) status: Option<(String, Instant)>,
    pub(crate) update_state: Arc<Mutex<UpdateState>>,
    pub(crate) base_choices: Vec<String>,
    pub(crate) quit: bool,
    pub(crate) last_viewport_height: usize,
    /// Inner height of the diff panes as of the last draw.
    pub(crate) diff_height: usize,
    /// Snapshot of the file panel from the last draw.
    pub(crate) file_view: FilePanelView,
    /// Inner area of the old (left) diff pane as of the last draw,
    /// recorded by the renderer so mouse events can be routed; zero-sized
    /// when the pane is hidden.
    pub(crate) old_pane_area: Rect,
    /// Inner area of the new (right) diff pane as of the last draw,
    /// recorded by the renderer so mouse events can be routed; zero-sized
    /// when the pane is hidden.
    pub(crate) new_pane_area: Rect,
    /// Manual file-panel scroll set by the mouse wheel / PgUp / PgDn;
    /// `None` means the panel follows the selection.
    pub(crate) file_scroll: Option<usize>,
    /// Last known mouse position, used to route scrolling to the pane
    /// under the cursor.
    mouse_pos: (u16, u16),
    /// Whether syntax highlighting is enabled for the diff panes.
    pub(crate) syntax: bool,
    /// Highlighted segments for the left (old) pane, indexed by line - 1.
    pub(crate) left_hl: Arc<HlLines>,
    /// Highlighted segments for the right (new) pane, indexed by line - 1.
    pub(crate) right_hl: Arc<HlLines>,
    /// Jobs to the background highlight worker.
    hl_jobs: mpsc::Sender<HlJob>,
    /// Finished highlights back from the worker.
    hl_results: mpsc::Receiver<HlUpdate>,
    /// Finished highlights keyed by (path, content) hash, so revisiting a
    /// file (or toggling syntax) is instant.
    hl_cache: HashMap<u64, Arc<HlLines>>,
    /// Bumped on every diff reload; stale worker results (from a file the
    /// user already navigated away from) are cached but not displayed.
    hl_generation: u64,
    /// Set by the repo watcher thread when the repository changed on disk.
    repo_changed: Arc<AtomicBool>,
    trigger_update: bool,
    awaiting_update_exit: bool,
}

impl App {
    fn new(repo: GitRepo, mode: DiffMode) -> Self {
        let base = match &mode {
            DiffMode::BranchToBase { base } => base.clone(),
            _ => repo.default_base(),
        };
        let branch = repo.current_branch();
        let base_choices = repo.local_branches().unwrap_or_default();
        let (hl_jobs, hl_results) = spawn_highlight_worker();
        let repo_changed = Arc::new(AtomicBool::new(false));
        spawn_repo_watcher(repo.clone(), repo_changed.clone());
        App {
            repo,
            mode,
            base,
            branch,
            files: Vec::new(),
            selected: 0,
            diff: SideBySideDiff::default(),
            scroll: 0,
            h_scroll_old: 0,
            h_scroll_new: 0,
            h_scroll_files: 0,
            show_help: false,
            compact: false,
            show_files: true,
            tree_view: false,
            wide_files: false,
            show_old: true,
            show_new: true,
            status: None,
            update_state: Arc::new(Mutex::new(UpdateState::Checking)),
            base_choices,
            quit: false,
            last_viewport_height: 20,
            diff_height: 20,
            file_view: FilePanelView::default(),
            old_pane_area: Rect::default(),
            new_pane_area: Rect::default(),
            file_scroll: None,
            mouse_pos: (0, 0),
            syntax: true,
            left_hl: Arc::new(Vec::new()),
            right_hl: Arc::new(Vec::new()),
            hl_jobs,
            hl_results,
            hl_cache: HashMap::new(),
            hl_generation: 0,
            repo_changed,
            trigger_update: false,
            awaiting_update_exit: false,
        }
    }

    /// Show a transient notification in the footer; it expires after
    /// [`STATUS_TTL`].
    fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    /// Clear the notification once its display time has elapsed.
    pub(crate) fn expire_status(&mut self) {
        if let Some((_, set_at)) = &self.status {
            if set_at.elapsed() >= STATUS_TTL {
                self.status = None;
            }
        }
    }

    /// Whether the app is exiting specifically to perform a self-update.
    pub(crate) fn pending_update(&self) -> bool {
        self.awaiting_update_exit
    }

    /// Reload the file list for the current mode, then reload the diff
    /// for the currently selected file.
    fn reload(&mut self) {
        self.file_scroll = None;
        self.h_scroll_files = 0;
        match self.repo.changed_files(&self.mode) {
            Ok(files) => {
                self.files = files;
                if self.selected >= self.files.len() {
                    self.selected = self.files.len().saturating_sub(1);
                }
            }
            Err(e) => {
                self.files.clear();
                self.set_status(format!("error: {e}"));
            }
        }
        self.reload_diff();
    }

    /// Recompute the side-by-side diff for the currently selected file.
    /// Highlighting is served from cache when possible, otherwise handed
    /// to the background worker — the unstyled diff renders immediately
    /// and styled lines swap in when the worker finishes.
    fn reload_diff(&mut self) {
        self.diff = SideBySideDiff::default();
        self.scroll = 0;
        self.h_scroll_old = 0;
        self.h_scroll_new = 0;
        self.hl_generation += 1;
        self.left_hl = Arc::new(Vec::new());
        self.right_hl = Arc::new(Vec::new());
        let Some(entry) = self.files.get(self.selected).cloned() else {
            return;
        };
        if entry.binary {
            self.set_status("binary file".to_string());
            return;
        }
        match self.repo.file_versions(&self.mode, &entry) {
            Ok((old, new)) => {
                self.diff = diff::side_by_side(&old, &new);
                if self.syntax {
                    let old_path = match &entry.status {
                        FileStatus::Renamed { from } => from.clone(),
                        _ => entry.path.clone(),
                    };
                    self.request_highlight(true, old_path, old);
                    self.request_highlight(false, entry.path.clone(), new);
                }
            }
            Err(e) => {
                self.set_status(format!("error: {e}"));
            }
        }
    }

    /// Apply a cached highlight for one pane, or queue it on the worker.
    fn request_highlight(&mut self, is_left: bool, path: String, content: String) {
        let key = highlight::cache_key(&path, &content);
        if let Some(lines) = self.hl_cache.get(&key) {
            if is_left {
                self.left_hl = lines.clone();
            } else {
                self.right_hl = lines.clone();
            }
            return;
        }
        let _ = self.hl_jobs.send(HlJob {
            generation: self.hl_generation,
            is_left,
            key,
            path,
            content,
        });
    }

    /// Drain finished highlights from the worker: cache every result, and
    /// display those still matching the current file.
    pub(crate) fn apply_highlight_results(&mut self) {
        while let Ok(update) = self.hl_results.try_recv() {
            if self.hl_cache.len() >= HL_CACHE_MAX {
                self.hl_cache.clear();
            }
            self.hl_cache.insert(update.key, update.lines.clone());
            if update.generation == self.hl_generation && self.syntax {
                if update.is_left {
                    self.left_hl = update.lines;
                } else {
                    self.right_hl = update.lines;
                }
            }
        }
    }

    /// If the watcher flagged a repository change, reload while keeping
    /// the selected file (matched by path) and scroll position.
    pub(crate) fn poll_auto_refresh(&mut self) {
        if !self.repo_changed.swap(false, Ordering::Relaxed) {
            return;
        }
        let selected_path = self.files.get(self.selected).map(|e| e.path.clone());
        let scroll = self.scroll;
        let h_scroll_old = self.h_scroll_old;
        let h_scroll_new = self.h_scroll_new;
        let h_scroll_files = self.h_scroll_files;
        match self.repo.changed_files(&self.mode) {
            Ok(files) => self.files = files,
            // Transient failure (e.g. mid-rebase): keep the current view.
            Err(_) => return,
        }
        if let Some(path) = selected_path {
            if let Some(idx) = self.files.iter().position(|e| e.path == path) {
                self.selected = idx;
            }
        }
        if self.selected >= self.files.len() {
            self.selected = self.files.len().saturating_sub(1);
        }
        self.reload_diff();
        self.scroll = scroll;
        self.h_scroll_old = h_scroll_old;
        self.h_scroll_new = h_scroll_new;
        self.h_scroll_files = h_scroll_files;
        self.clamp_scroll();
    }

    /// Number of scrollable rows in the diff view: aligned rows normally,
    /// the longer of the two compacted panes in compact mode.
    pub(crate) fn row_count(&self) -> usize {
        if self.compact {
            let left = self.diff.rows.iter().filter(|r| r.left.is_some()).count();
            let right = self.diff.rows.iter().filter(|r| r.right.is_some()).count();
            match (self.show_old, self.show_new) {
                (true, false) => left,
                (false, true) => right,
                _ => left.max(right),
            }
        } else {
            self.diff.rows.len()
        }
    }

    fn clamp_scroll(&mut self) {
        let max = self.row_count().saturating_sub(1);
        if self.scroll > max {
            self.scroll = max;
        }
    }

    /// Start indices (in aligned row space) of each contiguous run of
    /// changed rows — the "hunks" that n/N jump between.
    fn hunk_starts(&self) -> Vec<usize> {
        let mut starts = Vec::new();
        let mut in_hunk = false;
        for (i, row) in self.diff.rows.iter().enumerate() {
            let changed = row.kind != RowKind::Context;
            if changed && !in_hunk {
                starts.push(i);
            }
            in_hunk = changed;
        }
        starts
    }

    /// Convert an aligned row index into a scroll position for the current
    /// view. In compact mode each pane renders its own filtered row list at
    /// the shared scroll offset, so take the earlier of the two per-pane
    /// positions — that way neither pane has scrolled past the change.
    fn row_to_scroll(&self, idx: usize) -> usize {
        if !self.compact {
            return idx;
        }
        let left = self.diff.rows[..idx].iter().filter(|r| r.left.is_some()).count();
        let right = self.diff.rows[..idx].iter().filter(|r| r.right.is_some()).count();
        match (self.show_old, self.show_new) {
            (true, false) => left,
            (false, true) => right,
            _ => left.min(right),
        }
    }

    /// Jump to the next (`forward`) or previous change hunk relative to the
    /// current scroll position.
    fn jump_change(&mut self, forward: bool) {
        let starts = self.hunk_starts();
        if starts.is_empty() {
            self.set_status("no changes in this file".to_string());
            return;
        }
        let positions: Vec<usize> = starts.iter().map(|&i| self.row_to_scroll(i)).collect();
        // The hunk currently "at" the viewport sits JUMP_PADDING rows below
        // the top, so compare against that anchor rather than raw scroll.
        let anchor = self.scroll + JUMP_PADDING;
        let target = if forward {
            positions.iter().position(|&p| p > anchor)
        } else {
            positions.iter().rposition(|&p| p < anchor)
        };
        match target {
            Some(i) => {
                self.scroll = positions[i].saturating_sub(JUMP_PADDING);
                self.clamp_scroll();
                self.set_status(format!("change {}/{}", i + 1, positions.len()));
            }
            None => {
                self.set_status(if forward {
                    "no more changes below".to_string()
                } else {
                    "no more changes above".to_string()
                });
            }
        }
    }

    fn next_file(&mut self) {
        if self.files.is_empty() {
            return;
        }
        if self.selected + 1 < self.files.len() {
            self.selected += 1;
        }
        self.file_scroll = None;
        self.reload_diff();
    }

    fn prev_file(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
        self.file_scroll = None;
        self.reload_diff();
    }

    /// Whether the mouse currently hovers the file panel.
    fn over_files(&self) -> bool {
        let (x, y) = self.mouse_pos;
        self.show_files && self.file_view.area.contains(Position::new(x, y))
    }

    /// Whether the mouse currently hovers the old (left) diff pane.
    fn over_old(&self) -> bool {
        let (x, y) = self.mouse_pos;
        self.old_pane_area.contains(Position::new(x, y))
    }

    /// Whether the mouse currently hovers the new (right) diff pane.
    fn over_new(&self) -> bool {
        let (x, y) = self.mouse_pos;
        self.new_pane_area.contains(Position::new(x, y))
    }

    /// Route a horizontal scroll delta to whichever pane sits under the
    /// mouse; if the mouse hasn't entered any pane yet (e.g. a keyboard
    /// user who never moved it), scroll both diff panes together.
    fn h_scroll_by(&mut self, delta: isize) {
        let apply = |offset: &mut usize, delta: isize| {
            if delta < 0 {
                *offset = offset.saturating_sub(delta.unsigned_abs());
            } else {
                *offset = offset.saturating_add(delta as usize);
            }
        };
        if self.over_files() {
            apply(&mut self.h_scroll_files, delta);
        } else if self.over_old() {
            apply(&mut self.h_scroll_old, delta);
        } else if self.over_new() {
            apply(&mut self.h_scroll_new, delta);
        } else {
            apply(&mut self.h_scroll_old, delta);
            apply(&mut self.h_scroll_new, delta);
        }
    }

    /// Scroll the file panel by `delta` rows, detaching it from the
    /// selection until the user navigates with the keyboard again.
    fn scroll_files(&mut self, delta: isize) {
        let height = self.file_view.area.height as usize;
        let max = self.file_view.total.saturating_sub(height) as isize;
        let next = (self.file_view.offset as isize + delta).clamp(0, max.max(0));
        self.file_scroll = Some(next as usize);
    }

    /// Select the file rendered on the clicked panel row, if any.
    fn click_file(&mut self, row: u16) {
        let inner = self.file_view.area;
        let Some(visible_row) = row.checked_sub(inner.y) else {
            return;
        };
        if let Some(Some(idx)) = self.file_view.rows.get(visible_row as usize).copied() {
            if idx != self.selected {
                self.selected = idx;
                self.reload_diff();
            }
        }
    }

    /// Toggle one of the diff panes, keeping at least one visible.
    fn toggle_pane(&mut self, left: bool) {
        let (this, other) = if left {
            (&mut self.show_old, self.show_new)
        } else {
            (&mut self.show_new, self.show_old)
        };
        if *this && !other {
            self.set_status("at least one pane must stay visible".to_string());
            return;
        }
        *this = !*this;
        self.clamp_scroll();
        let name = if left { "old" } else { "new" };
        let shown = if left { self.show_old } else { self.show_new };
        self.set_status(format!(
            "{name} pane {}",
            if shown { "shown" } else { "hidden" }
        ));
    }

    fn cycle_base(&mut self) {
        if self.base_choices.is_empty() {
            self.set_status("no local branches found".to_string());
            return;
        }
        let current_pos = self
            .base_choices
            .iter()
            .position(|b| b == &self.base)
            .unwrap_or(usize::MAX);
        let next_pos = if current_pos == usize::MAX {
            0
        } else {
            (current_pos + 1) % self.base_choices.len()
        };
        let next_base = self.base_choices[next_pos].clone();
        self.base = next_base.clone();
        if let DiffMode::BranchToBase { base } = &mut self.mode {
            *base = next_base.clone();
            self.reload();
        }
        self.set_status(format!("base: {next_base}"));
    }

    fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        // Help overlay swallows most keys except close/quit.
        if self.show_help {
            match code {
                KeyCode::Esc | KeyCode::Char('?') => self.show_help = false,
                KeyCode::Char('q') => self.quit = true,
                _ => {}
            }
            return;
        }

        match code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.next_file(),
            KeyCode::Char('k') | KeyCode::Up => self.prev_file(),
            KeyCode::Char('J') => {
                self.scroll = self.scroll.saturating_add(1);
                self.clamp_scroll();
            }
            KeyCode::Char('K') => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
                let half = (self.last_viewport_height / 2).max(1);
                self.scroll = self.scroll.saturating_add(half);
                self.clamp_scroll();
            }
            KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                let half = (self.last_viewport_height / 2).max(1);
                self.scroll = self.scroll.saturating_sub(half);
            }
            KeyCode::PageDown => {
                if self.over_files() {
                    self.scroll_files(self.file_view.area.height.max(1) as isize);
                } else {
                    let page = self.diff_height.max(1);
                    self.scroll = self.scroll.saturating_add(page);
                    self.clamp_scroll();
                }
            }
            KeyCode::PageUp => {
                if self.over_files() {
                    self.scroll_files(-(self.file_view.area.height.max(1) as isize));
                } else {
                    let page = self.diff_height.max(1);
                    self.scroll = self.scroll.saturating_sub(page);
                }
            }
            KeyCode::Char('n') => self.jump_change(true),
            KeyCode::Char('N') => self.jump_change(false),
            KeyCode::Char('g') => self.scroll = 0,
            KeyCode::Char('G') => {
                self.scroll = self.row_count().saturating_sub(1);
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.h_scroll_by(-4);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.h_scroll_by(4);
            }
            KeyCode::Char('m') => {
                self.mode = self.mode.next(&self.base);
                let label = self.mode.label();
                self.reload();
                self.set_status(label);
            }
            KeyCode::Char('f') => {
                self.show_files = !self.show_files;
            }
            KeyCode::Char('t') => {
                self.tree_view = !self.tree_view;
                self.set_status(if self.tree_view {
                    "file panel: tree view".to_string()
                } else {
                    "file panel: list view".to_string()
                });
            }
            KeyCode::Char('F') => {
                self.wide_files = !self.wide_files;
                if self.wide_files && !self.show_files {
                    self.show_files = true;
                }
                self.set_status(if self.wide_files {
                    "file panel: expanded".to_string()
                } else {
                    "file panel: normal width".to_string()
                });
            }
            KeyCode::Char('1') => self.toggle_pane(true),
            KeyCode::Char('2') => self.toggle_pane(false),
            KeyCode::Char('c') => {
                self.compact = !self.compact;
                self.clamp_scroll();
                self.set_status(if self.compact {
                    "compact view: filler rows hidden (panes scroll independently)".to_string()
                } else {
                    "aligned view: filler rows shown".to_string()
                });
            }
            KeyCode::Char('s') => {
                self.syntax = !self.syntax;
                let scroll = self.scroll;
                let h_scroll_old = self.h_scroll_old;
                let h_scroll_new = self.h_scroll_new;
                self.reload_diff();
                self.scroll = scroll;
                self.h_scroll_old = h_scroll_old;
                self.h_scroll_new = h_scroll_new;
                self.clamp_scroll();
                self.set_status(if self.syntax {
                    "syntax highlighting on".to_string()
                } else {
                    "syntax highlighting off".to_string()
                });
            }
            KeyCode::Char('b') => self.cycle_base(),
            KeyCode::Char('r') => {
                self.reload();
                self.set_status("reloaded".to_string());
            }
            KeyCode::Char('U') => {
                self.trigger_update = true;
            }
            KeyCode::Char('?') => self.show_help = true,
            _ => {}
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        self.mouse_pos = (mouse.column, mouse.row);
        if self.show_help {
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                if self.over_files() {
                    self.scroll_files(3);
                } else {
                    self.scroll = self.scroll.saturating_add(3);
                    self.clamp_scroll();
                }
            }
            MouseEventKind::ScrollUp => {
                if self.over_files() {
                    self.scroll_files(-3);
                } else {
                    self.scroll = self.scroll.saturating_sub(3);
                }
            }
            MouseEventKind::ScrollRight => {
                self.h_scroll_by(4);
            }
            MouseEventKind::ScrollLeft => {
                self.h_scroll_by(-4);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.over_files() {
                    self.click_file(mouse.row);
                }
            }
            _ => {}
        }
    }
}

/// Restores the terminal on drop, even on panic/early return.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
    }
}

pub fn run(repo: GitRepo, mode: DiffMode) -> Result<()> {
    let mut app = App::new(repo, mode);
    app.reload();

    spawn_update_check(app.update_state.clone());

    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    let _guard = TerminalGuard;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(&mut terminal, &mut app);

    // Explicitly restore before doing anything post-loop (e.g. printing
    // update output); the guard will also restore on drop as a fallback.
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;

    if app.pending_update() {
        println!("Updating gispect...");
        match update::perform_update() {
            Ok(()) => {
                println!("Update complete. Restarting gispect...");
                if let Err(e) = update::restart() {
                    println!("Restart failed: {e}. Please restart gispect manually.");
                }
            }
            Err(e) => println!("Update failed: {e}"),
        }
    }

    result
}

/// Spawn the highlight worker thread. It owns the (slow-to-load) syntect
/// syntax set, so startup and the UI thread never block on it. Queued
/// jobs are drained latest-generation-first, so holding j/k doesn't pile
/// up highlight work for files the user has already skipped past.
fn spawn_highlight_worker() -> (mpsc::Sender<HlJob>, mpsc::Receiver<HlUpdate>) {
    let (job_tx, job_rx) = mpsc::channel::<HlJob>();
    let (res_tx, res_rx) = mpsc::channel::<HlUpdate>();
    std::thread::spawn(move || {
        let highlighter = highlight::Highlighter::new();
        while let Ok(first) = job_rx.recv() {
            let mut batch = vec![first];
            while let Ok(job) = job_rx.try_recv() {
                batch.push(job);
            }
            let newest = batch.iter().map(|j| j.generation).max().unwrap_or(0);
            for job in batch.into_iter().filter(|j| j.generation == newest) {
                let lines = Arc::new(highlighter.highlight(&job.path, &job.content));
                let update = HlUpdate {
                    generation: job.generation,
                    is_left: job.is_left,
                    key: job.key,
                    lines,
                };
                if res_tx.send(update).is_err() {
                    return;
                }
            }
        }
    });
    (job_tx, res_rx)
}

/// Poll the repository fingerprint on a background thread and raise
/// `flag` whenever it changes, so the UI auto-refreshes on external
/// edits, commits, or staging. Exits with the process.
fn spawn_repo_watcher(repo: GitRepo, flag: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let mut last = repo.state_fingerprint();
        loop {
            std::thread::sleep(WATCH_INTERVAL);
            let current = repo.state_fingerprint();
            if current != last {
                last = current;
                flag.store(true, Ordering::Relaxed);
            }
        }
    });
}

/// Run `update::check_for_update` on a background thread, storing the
/// outcome (including failures) in the shared state slot.
fn spawn_update_check(slot: Arc<Mutex<UpdateState>>) {
    std::thread::spawn(move || {
        let state = match update::check_for_update() {
            Ok(Some(hash)) => UpdateState::Available(hash),
            Ok(None) => UpdateState::UpToDate,
            Err(e) => UpdateState::Failed(e.to_string()),
        };
        if let Ok(mut guard) = slot.lock() {
            *guard = state;
        }
    });
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    loop {
        terminal.draw(|f| {
            app.last_viewport_height = f.area().height as usize;
            crate::ui::draw(f, app);
        })?;

        app.expire_status();
        app.apply_highlight_results();
        app.poll_auto_refresh();

        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app.handle_key(key.code, key.modifiers);
                }
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                _ => {}
            }
        }

        if app.trigger_update {
            app.trigger_update = false;
            let state = app
                .update_state
                .lock()
                .map(|g| g.clone())
                .unwrap_or(UpdateState::Checking);
            match state {
                UpdateState::Available(_) => {
                    app.awaiting_update_exit = true;
                    app.quit = true;
                }
                UpdateState::Checking => {
                    app.set_status("update check still in progress…");
                }
                UpdateState::UpToDate | UpdateState::Failed(_) => {
                    // Re-check on demand: up-to-date may be stale, and a
                    // failed check deserves a retry.
                    if let Ok(mut slot) = app.update_state.lock() {
                        *slot = UpdateState::Checking;
                    }
                    spawn_update_check(app.update_state.clone());
                    app.set_status(match state {
                        UpdateState::Failed(e) => {
                            let brief = e.lines().next().unwrap_or("").to_string();
                            format!("last check failed ({brief}) — retrying…")
                        }
                        _ => "checking for updates…".to_string(),
                    });
                }
            }
        }

        if app.quit {
            break;
        }
    }
    Ok(())
}
