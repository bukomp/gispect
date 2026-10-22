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

use crate::actions;
use crate::config;
use crate::gh::{self, Side};
use crate::workspace::Workspace;
use crate::highlight::{self, HlLines};
use crate::pr::{self, Load, PrEvent};
use crate::search;
use crate::types::{DiffMode, FileEntry, FileStatus, RowKind, SideBySideDiff};
use crate::{clipboard, diff, update};

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
    /// Local repository (or repositories); `None` when launched outside
    /// one (`gispect prs`).
    pub(crate) workspace: Option<Workspace>,
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
    /// Search prompt currently being typed, if any; keystrokes are routed
    /// to it instead of the normal bindings.
    pub(crate) search_input: Option<search::SearchInput>,
    /// Committed in-file search: matches highlighted in the diff panes,
    /// n/N jump between them until Esc clears it.
    pub(crate) file_search: Option<search::FileSearch>,
    /// Committed file-panel path filter (`p`); `None` shows all files.
    pub(crate) path_filter: Option<String>,
    /// Committed changed-content filter (`S`); `None` shows all files.
    pub(crate) content_filter: Option<String>,
    /// Changed-line contents per file for the current mode, parsed from
    /// one `git diff` and cached while a content filter is in play.
    changed_lines: Option<HashMap<String, Vec<String>>>,
    /// Set by the repo watcher thread when the repository changed on disk.
    repo_changed: Arc<AtomicBool>,
    trigger_update: bool,
    awaiting_update_exit: bool,
    /// The last-saved startup config; overwritten when the config modal
    /// saves a new draft or `persist_settings` saves a live change.
    pub(crate) config: config::Config,
    /// Mode to open in next time. Only an explicit switch changes it, so a
    /// `--base` launch doesn't overwrite the saved default on its own.
    startup_mode: config::ModeKind,
    /// Open config modal, if any; `Some` swallows most key handling.
    pub(crate) config_modal: Option<config::ConfigModal>,
    /// PR list state, kept across visits so results stay cached.
    pub(crate) pr_list: Option<pr::PrList>,
    /// Whether the PR list screen is showing.
    pub(crate) pr_list_open: bool,
    /// PR under review; while `Some`, files and diffs come from it
    /// instead of the local repository.
    pub(crate) pr: Option<pr::PrSession>,
    /// Bumped per opened PR so late results from a previous one are dropped.
    pr_generation: u64,
    pr_tx: mpsc::Sender<PrEvent>,
    pr_rx: mpsc::Receiver<PrEvent>,
    /// Fetches the PRs of the open list tab ahead of time.
    pub(crate) preloader: pr::Preloader,
    /// Selected local file, restored when leaving a PR review.
    local_selected_path: Option<String>,
    /// The selected PR file's contents are still being fetched.
    pub(crate) diff_loading: bool,
    /// Line cursor for inline comments (aligned row index; PR review only).
    pub(crate) cursor: usize,
    /// Open inline comment editor, if any.
    pub(crate) comment_editor: Option<pr::CommentEditor>,
    /// Open review submission modal, if any.
    pub(crate) review_modal: Option<pr::ReviewModal>,
    /// Scroll of the PR description popup; `Some` while it is open.
    pub(crate) pr_info_scroll: Option<u16>,
    /// Set by a first quit/leave with unsent drafts; a second one confirms.
    discard_armed: bool,
    /// GitHub Actions view state, kept across visits.
    pub(crate) actions: Option<actions::ActionsView>,
    /// Whether the actions screen is showing.
    pub(crate) actions_open: bool,
}

impl App {
    fn new(workspace: Option<Workspace>, mode: DiffMode, config: config::Config) -> Self {
        let base = match (&mode, &workspace) {
            (DiffMode::BranchToBase { base }, _) => base.clone(),
            (_, Some(ws)) => ws.default_base(),
            (_, None) => "main".to_string(),
        };
        let branch = workspace
            .as_ref()
            .map(Workspace::branch_label)
            .unwrap_or_default();
        let base_choices = workspace
            .as_ref()
            .and_then(|w| w.local_branches().ok())
            .unwrap_or_default();
        let (hl_jobs, hl_results) = spawn_highlight_worker();
        let repo_changed = Arc::new(AtomicBool::new(false));
        if let Some(ws) = &workspace {
            spawn_repo_watcher(ws.clone(), repo_changed.clone());
        }
        let (pr_tx, pr_rx) = mpsc::channel();
        let compact = config.compact;
        let syntax = config.syntax;
        let show_files = config.show_files;
        let tree_view = config.tree_view;
        let wide_files = config.wide_files;
        // Defend against a hand-edited config file with both panes off:
        // the app invariant is at least one pane always visible.
        let (show_old, show_new) = if !config.show_old && !config.show_new {
            (true, true)
        } else {
            (config.show_old, config.show_new)
        };
        App {
            workspace,
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
            compact,
            show_files,
            tree_view,
            wide_files,
            show_old,
            show_new,
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
            syntax,
            left_hl: Arc::new(Vec::new()),
            right_hl: Arc::new(Vec::new()),
            hl_jobs,
            hl_results,
            hl_cache: HashMap::new(),
            hl_generation: 0,
            search_input: None,
            file_search: None,
            path_filter: None,
            content_filter: None,
            changed_lines: None,
            repo_changed,
            trigger_update: false,
            awaiting_update_exit: false,
            startup_mode: config.default_mode,
            config,
            config_modal: None,
            pr_list: None,
            pr_list_open: false,
            pr: None,
            pr_generation: 0,
            preloader: pr::Preloader::new(pr_tx.clone()),
            pr_tx,
            pr_rx,
            local_selected_path: None,
            diff_loading: false,
            cursor: 0,
            comment_editor: None,
            review_modal: None,
            pr_info_scroll: None,
            discard_armed: false,
            actions: None,
            actions_open: false,
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
        // Changed lines are per-mode and per-repo-state; rebuild only if a
        // content filter still needs them.
        self.changed_lines = None;
        if self.active_content_filter().is_some() {
            self.ensure_changed_lines();
        }
        let files = match (&self.pr, &self.workspace) {
            (Some(session), _) => Ok(session
                .details()
                .map(|d| d.files.iter().map(|f| f.entry.clone()).collect())
                .unwrap_or_default()),
            (None, Some(ws)) => ws.changed_files(&self.mode),
            (None, None) => Ok(Vec::new()),
        };
        match files {
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

    /// The path filter currently in effect: the live prompt text while
    /// the user is typing a `p` filter, else the committed filter.
    pub(crate) fn active_path_filter(&self) -> Option<&str> {
        if let Some(input) = &self.search_input {
            if input.kind == search::SearchKind::PathFilter {
                return Some(input.query.as_str());
            }
        }
        self.path_filter.as_deref()
    }

    /// The changed-content filter currently in effect: the live prompt
    /// text while the user is typing an `S` filter, else the committed
    /// filter.
    pub(crate) fn active_content_filter(&self) -> Option<&str> {
        if let Some(input) = &self.search_input {
            if input.kind == search::SearchKind::Content {
                return Some(input.query.as_str());
            }
        }
        self.content_filter.as_deref()
    }

    /// Whether the file at `idx` passes both active filters (path and
    /// changed-content); files must pass every filter in play.
    pub(crate) fn passes_filter(&self, idx: usize) -> bool {
        let Some(entry) = self.files.get(idx) else {
            return false;
        };
        if let Some(filter) = self.active_path_filter() {
            if !search::path_matches(&entry.path, filter) {
                return false;
            }
        }
        match self.active_content_filter() {
            Some(query) if !query.is_empty() => self
                .changed_lines
                .as_ref()
                .and_then(|m| m.get(&entry.path))
                .map(|lines| search::any_line_matches(lines, query))
                .unwrap_or(false),
            _ => true,
        }
    }

    /// Parse changed lines for the current mode from `git diff` (one per
    /// repository), unless already cached; needed while a content filter
    /// is in play.
    fn ensure_changed_lines(&mut self) {
        if self.changed_lines.is_some() {
            return;
        }
        if let Some(session) = &self.pr {
            // GitHub's per-file patches carry the same +/- lines.
            let lines = session
                .details()
                .map(|d| {
                    d.files
                        .iter()
                        .map(|f| {
                            let changed = f
                                .patch
                                .as_deref()
                                .unwrap_or_default()
                                .lines()
                                .filter_map(|l| l.strip_prefix('+').or_else(|| l.strip_prefix('-')))
                                .map(str::to_string)
                                .collect();
                            (f.entry.path.clone(), changed)
                        })
                        .collect()
                })
                .unwrap_or_default();
            self.changed_lines = Some(lines);
            return;
        }
        let Some(ws) = &self.workspace else {
            self.changed_lines = Some(HashMap::new());
            return;
        };
        match ws.changed_lines(&self.mode) {
            Ok(lines) => self.changed_lines = Some(lines),
            Err(e) => {
                self.changed_lines = Some(HashMap::new());
                self.set_status(format!("error: {e}"));
            }
        }
    }

    /// If the selection is hidden by the path filter, move it to the
    /// first visible file (when any file is still visible).
    fn ensure_selected_visible(&mut self) {
        if self.files.is_empty() || self.passes_filter(self.selected) {
            return;
        }
        match (0..self.files.len()).find(|&i| self.passes_filter(i)) {
            Some(idx) => {
                self.selected = idx;
                self.file_scroll = None;
                self.reload_diff();
            }
            None => self.set_status("no files match the filter".to_string()),
        }
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
        self.diff_loading = false;
        self.cursor = 0;
        let Some(entry) = self.files.get(self.selected).cloned() else {
            return;
        };
        if entry.binary {
            self.set_status("binary file".to_string());
            return;
        }
        let versions = match (&self.pr, &self.workspace) {
            (Some(session), _) => match session.contents.get(&entry.path) {
                Some(v) => Ok(v.clone()),
                None => {
                    // Shown when the fetch worker reports back.
                    session.request_file(&entry);
                    self.diff_loading = true;
                    return;
                }
            },
            (None, Some(ws)) => ws.file_versions(&self.mode, &entry),
            (None, None) => return,
        };
        match versions {
            Ok((old, new)) => self.show_versions(&entry, old, new),
            Err(e) => {
                self.set_status(format!("error: {e}"));
            }
        }
    }

    /// Display a freshly loaded (old, new) pair for `entry`: build the
    /// side-by-side diff, queue highlighting, re-run an active search.
    fn show_versions(&mut self, entry: &FileEntry, old: String, new: String) {
        self.diff_loading = false;
        self.diff = diff::side_by_side(&old, &new);
        if self.syntax {
            let old_path = match &entry.status {
                FileStatus::Renamed { from } => from.clone(),
                _ => entry.path.clone(),
            };
            self.request_highlight(true, old_path, old);
            self.request_highlight(false, entry.path.clone(), new);
        }
        // Keep an active in-file search in sync with the newly loaded diff.
        if let Some(fs) = &mut self.file_search {
            fs.matches = search::search_diff(&self.diff, &fs.query);
            fs.current = 0;
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
        // A PR review doesn't follow the local repo; leaving it reloads.
        if self.pr.is_some() || !self.repo_changed.swap(false, Ordering::Relaxed) {
            return;
        }
        let Some(ws) = &self.workspace else {
            return;
        };
        let selected_path = self.files.get(self.selected).map(|e| e.path.clone());
        let scroll = self.scroll;
        let h_scroll_old = self.h_scroll_old;
        let h_scroll_new = self.h_scroll_new;
        let h_scroll_files = self.h_scroll_files;
        match ws.changed_files(&self.mode) {
            Ok(files) => self.files = files,
            // Transient failure (e.g. mid-rebase): keep the current view.
            Err(_) => return,
        }
        self.changed_lines = None;
        if self.active_content_filter().is_some() {
            self.ensure_changed_lines();
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
                self.cursor = starts[i];
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

    /// Select the next file that passes the active path filter, if any.
    fn next_file(&mut self) {
        let Some(next) = (self.selected + 1..self.files.len()).find(|&i| self.passes_filter(i))
        else {
            return;
        };
        self.selected = next;
        self.file_scroll = None;
        self.reload_diff();
    }

    /// Select the previous file that passes the active path filter, if any.
    fn prev_file(&mut self) {
        let Some(prev) = (0..self.selected).rev().find(|&i| self.passes_filter(i)) else {
            return;
        };
        self.selected = prev;
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

    /// Open a search prompt of the given kind. Filter prompts are
    /// prefilled with the committed filter so it can be edited in place.
    fn open_search(&mut self, kind: search::SearchKind) {
        let query = match kind {
            search::SearchKind::PathFilter => self.path_filter.clone().unwrap_or_default(),
            search::SearchKind::Content => {
                self.ensure_changed_lines();
                self.content_filter.clone().unwrap_or_default()
            }
            search::SearchKind::File => String::new(),
        };
        self.search_input = Some(search::SearchInput { kind, query });
    }

    /// Route a key press into the active search prompt.
    fn handle_search_input(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        match code {
            KeyCode::Esc => {
                let kind = self.search_input.take().map(|i| i.kind);
                match kind {
                    Some(search::SearchKind::PathFilter) => {
                        if self.path_filter.take().is_some() {
                            self.set_status("file filter cleared".to_string());
                        }
                    }
                    Some(search::SearchKind::Content) => {
                        if self.content_filter.take().is_some() {
                            self.set_status("content filter cleared".to_string());
                        }
                    }
                    _ => {}
                }
            }
            KeyCode::Enter => self.commit_search(),
            KeyCode::Backspace => {
                if let Some(input) = &mut self.search_input {
                    input.query.pop();
                }
            }
            KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(input) = &mut self.search_input {
                    input.query.push(c);
                }
            }
            _ => {}
        }
    }

    /// Execute the search the user just typed (Enter on a prompt).
    fn commit_search(&mut self) {
        let Some(input) = self.search_input.take() else {
            return;
        };
        match input.kind {
            search::SearchKind::File => {
                if input.query.is_empty() {
                    self.file_search = None;
                    return;
                }
                let matches = search::search_diff(&self.diff, &input.query);
                let found = !matches.is_empty();
                self.file_search = Some(search::FileSearch {
                    query: input.query,
                    matches,
                    current: 0,
                });
                if found {
                    self.scroll_to_match(0);
                } else {
                    self.set_status("no matches".to_string());
                }
            }
            search::SearchKind::PathFilter => {
                self.path_filter = if input.query.is_empty() {
                    None
                } else {
                    Some(input.query)
                };
                self.ensure_selected_visible();
            }
            search::SearchKind::Content => {
                self.content_filter = if input.query.is_empty() {
                    None
                } else {
                    Some(input.query)
                };
                self.ensure_selected_visible();
            }
        }
    }

    /// Move the in-file search cursor to match `idx` and scroll to it.
    fn scroll_to_match(&mut self, idx: usize) {
        let (row, total) = match &self.file_search {
            Some(fs) => match fs.matches.get(idx) {
                Some(m) => (m.row, fs.matches.len()),
                None => return,
            },
            None => return,
        };
        if let Some(fs) = &mut self.file_search {
            fs.current = idx;
        }
        self.cursor = row;
        self.scroll = self.row_to_scroll(row).saturating_sub(JUMP_PADDING);
        self.clamp_scroll();
        self.set_status(format!("match {}/{}", idx + 1, total));
    }

    /// Jump to the next/previous in-file search match, wrapping around.
    fn jump_match(&mut self, forward: bool) {
        let (len, current) = match &self.file_search {
            Some(fs) if !fs.matches.is_empty() => (fs.matches.len(), fs.current),
            _ => {
                self.set_status("no matches".to_string());
                return;
            }
        };
        let next = if forward {
            (current + 1) % len
        } else {
            (current + len - 1) % len
        };
        self.scroll_to_match(next);
    }

    fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        // Any key other than a repeated quit/leave disarms the draft
        // discard confirmation.
        let armed = std::mem::take(&mut self.discard_armed);
        if self.actions_open {
            self.handle_actions_key(code, modifiers, armed);
            return;
        }
        if self.pr_list_open {
            self.handle_pr_list_key(code, modifiers, armed);
            self.sync_preload();
            return;
        }
        // PR modals take text input, so they come before everything else.
        if self.comment_editor.is_some() {
            self.handle_comment_key(code, modifiers);
            return;
        }
        if self.review_modal.is_some() {
            self.handle_review_key(code, modifiers);
            return;
        }
        if self.pr_info_scroll.is_some() {
            self.handle_info_key(code);
            return;
        }
        // A search prompt swallows everything: keys are text input.
        if self.search_input.is_some() {
            self.handle_search_input(code, modifiers);
            return;
        }
        // Config modal swallows all keys while open.
        if self.config_modal.is_some() {
            self.handle_config_key(code);
            return;
        }
        // Help overlay swallows most keys except close/quit.
        if self.show_help {
            match code {
                KeyCode::Esc | KeyCode::Char('?') => self.show_help = false,
                KeyCode::Char('q') => self.quit = true,
                _ => {}
            }
            return;
        }
        if self.pr.is_some() && self.handle_pr_review_key(code) {
            self.clamp_cursor_to_view();
            return;
        }

        match code {
            KeyCode::Char('q') => self.request_quit(armed),
            // Esc peels back one layer of search state before quitting
            // (or, in a PR review, before going back to the PR list).
            KeyCode::Esc => {
                if self.file_search.take().is_some() {
                    self.set_status("search cleared".to_string());
                } else if self.content_filter.take().is_some() {
                    self.set_status("content filter cleared".to_string());
                } else if self.path_filter.take().is_some() {
                    self.set_status("file filter cleared".to_string());
                } else if self.pr.is_some() {
                    self.leave_pr(armed);
                } else {
                    self.quit = true;
                }
            }
            KeyCode::Char('P') => self.open_pr_list(),
            KeyCode::Char('a') => self.open_actions(),
            KeyCode::Char('/') => self.open_search(search::SearchKind::File),
            KeyCode::Char('S') => self.open_search(search::SearchKind::Content),
            KeyCode::Char('p') => self.open_search(search::SearchKind::PathFilter),
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
            // n/N walk search matches while a search is active, hunks otherwise.
            KeyCode::Char('n') => {
                if self.file_search.is_some() {
                    self.jump_match(true);
                } else {
                    self.jump_change(true);
                }
            }
            KeyCode::Char('N') => {
                if self.file_search.is_some() {
                    self.jump_match(false);
                } else {
                    self.jump_change(false);
                }
            }
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
                self.startup_mode = config::ModeKind::from_diff_mode(&self.mode);
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
            KeyCode::Char('C') => {
                self.config_modal = Some(config::ConfigModal::new(self.config.clone()));
            }
            _ => {}
        }
        if self.pr.is_some() {
            self.clamp_cursor_to_view();
        }
    }

    /// Route a key press into the open config modal: navigate, toggle the
    /// selected field, seed the draft from the live session, save, or
    /// close without saving.
    fn handle_config_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(modal) = self.config_modal.as_mut() {
                    modal.next();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(modal) = self.config_modal.as_mut() {
                    modal.prev();
                }
            }
            KeyCode::Char(' ') | KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                if let Some(modal) = self.config_modal.as_mut() {
                    let field = modal.field();
                    modal.draft.toggle(field, true);
                }
            }
            KeyCode::Char('h') | KeyCode::Left => {
                if let Some(modal) = self.config_modal.as_mut() {
                    let field = modal.field();
                    modal.draft.toggle(field, false);
                }
            }
            KeyCode::Char('u') => {
                // Seed the draft from the live session; read the seven
                // live values first so the borrow of config_modal below
                // doesn't overlap a borrow of the rest of self.
                let live_mode = config::ModeKind::from_diff_mode(&self.mode);
                let live_compact = self.compact;
                let live_syntax = self.syntax;
                let live_show_files = self.show_files;
                let live_tree_view = self.tree_view;
                let live_wide_files = self.wide_files;
                let live_show_old = self.show_old;
                let live_show_new = self.show_new;
                if let Some(modal) = self.config_modal.as_mut() {
                    modal.draft.default_mode = live_mode;
                    modal.draft.compact = live_compact;
                    modal.draft.syntax = live_syntax;
                    modal.draft.show_files = live_show_files;
                    modal.draft.tree_view = live_tree_view;
                    modal.draft.wide_files = live_wide_files;
                    modal.draft.show_old = live_show_old;
                    modal.draft.show_new = live_show_new;
                }
            }
            KeyCode::Char('s') => {
                let Some(draft) = self.config_modal.as_ref().map(|m| m.draft.clone()) else {
                    return;
                };
                if !draft.show_old && !draft.show_new {
                    self.set_status("at least one pane must stay visible");
                    return;
                }
                match draft.save() {
                    Ok(()) => {
                        self.config = draft.clone();
                        self.config_modal = None;
                        self.apply_config(&draft);
                        self.set_status("config saved");
                    }
                    Err(e) => {
                        self.set_status(format!("config save failed: {e}"));
                    }
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.config_modal = None;
            }
            _ => {}
        }
    }

    /// Apply a saved config draft to the live session: update the view
    /// flags and, if the mode or syntax setting changed, reload the diff
    /// so the panes reflect it immediately.
    fn apply_config(&mut self, draft: &config::Config) {
        let syntax_changed = self.syntax != draft.syntax;
        let mode_changed = draft.default_mode != config::ModeKind::from_diff_mode(&self.mode);
        self.startup_mode = draft.default_mode;
        self.compact = draft.compact;
        self.syntax = draft.syntax;
        self.show_files = draft.show_files;
        self.tree_view = draft.tree_view;
        self.wide_files = draft.wide_files;
        self.show_old = draft.show_old;
        self.show_new = draft.show_new;
        if mode_changed {
            self.mode = draft.default_mode.to_diff_mode(&self.base);
            self.reload();
        } else if syntax_changed {
            let scroll = self.scroll;
            let h_scroll_old = self.h_scroll_old;
            let h_scroll_new = self.h_scroll_new;
            self.reload_diff();
            self.scroll = scroll;
            self.h_scroll_old = h_scroll_old;
            self.h_scroll_new = h_scroll_new;
        }
        self.clamp_scroll();
    }

    /// Save the live view settings to disk when they differ from the
    /// last-saved config, so the next launch opens the same way. The
    /// default mode comes from `startup_mode`, not the live mode.
    fn persist_settings(&mut self) {
        let live = config::Config {
            default_mode: self.startup_mode,
            compact: self.compact,
            syntax: self.syntax,
            show_files: self.show_files,
            tree_view: self.tree_view,
            wide_files: self.wide_files,
            show_old: self.show_old,
            show_new: self.show_new,
        };
        if live == self.config {
            return;
        }
        // Record it even if the write fails, so a broken config dir
        // reports once instead of retrying on every event.
        let result = live.save();
        self.config = live;
        if let Err(e) = result {
            self.set_status(format!("settings save failed: {e}"));
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        self.mouse_pos = (mouse.column, mouse.row);
        if self.actions_open {
            if let Some(view) = self.actions.as_mut() {
                view.handle_mouse(mouse);
            }
            return;
        }
        if self.pr_list_open {
            self.handle_pr_list_mouse(mouse);
            return;
        }
        if self.show_help
            || self.comment_editor.is_some()
            || self.review_modal.is_some()
            || self.pr_info_scroll.is_some()
        {
            return;
        }
        self.handle_diff_mouse(mouse);
        if self.pr.is_some() {
            self.clamp_cursor_to_view();
        }
    }

    fn handle_diff_mouse(&mut self, mouse: MouseEvent) {
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
                } else if self.pr.is_some() && (self.over_old() || self.over_new()) {
                    let left = self.over_old();
                    self.click_diff_row(mouse.row, left);
                }
            }
            _ => {}
        }
    }
}

/// GitHub Actions view.
impl App {
    /// Show the actions screen, loading workflows the first time.
    pub(crate) fn open_actions(&mut self) {
        let Some(workspace) = &self.workspace else {
            self.set_status("actions need a local repository");
            return;
        };
        if self.actions.is_none() {
            self.actions = Some(actions::ActionsView::new(workspace));
        }
        self.actions_open = true;
    }

    fn handle_actions_key(&mut self, code: KeyCode, modifiers: KeyModifiers, armed: bool) {
        let Some(view) = self.actions.as_mut() else {
            self.actions_open = false;
            return;
        };
        match view.handle_key(code, modifiers) {
            actions::KeyOutcome::Done(Some(status)) => self.set_status(status),
            actions::KeyOutcome::Done(None) => {}
            actions::KeyOutcome::Back => self.actions_open = false,
            actions::KeyOutcome::Quit => self.request_quit(armed),
        }
    }

    /// Apply finished actions jobs and start due run refreshes.
    pub(crate) fn poll_actions(&mut self) {
        let Some(view) = self.actions.as_mut() else {
            return;
        };
        let messages = view.poll();
        if self.actions_open {
            view.tick();
        }
        if let Some(last) = messages.into_iter().last() {
            self.set_status(last);
        }
    }
}

/// Pull request list and review.
impl App {
    /// Show the PR list, loading accounts the first time.
    fn open_pr_list(&mut self) {
        if self.pr_list.is_none() {
            self.pr_list = Some(pr::PrList::new());
            pr::spawn_accounts(self.pr_tx.clone(), self.workspace.clone());
        }
        self.pr_list_open = true;
        self.start_list_queries();
    }

    fn list_mut(&mut self) -> &mut pr::PrList {
        self.pr_list.get_or_insert_with(pr::PrList::new)
    }

    /// Start any searches the list's current view still needs.
    fn start_list_queries(&mut self) {
        let Some(list) = self.pr_list.as_mut() else {
            return;
        };
        for key in list.missing_queries() {
            if let Some(account) = list.account_list().get(key.0).cloned() {
                list.results.insert(key, Load::Loading);
                let repos = list.scope_for(&account);
                pr::spawn_search(self.pr_tx.clone(), key, account, repos);
            }
        }
        list.clamp_selection();
        self.sync_preload();
    }

    /// Point the preloader at the PRs the list currently shows: the
    /// selected one first (so its info and review open soonest), then the
    /// rest from the top.
    fn sync_preload(&mut self) {
        let Some(list) = self.pr_list.as_ref() else {
            return;
        };
        let accounts = list.account_list();
        let mut rows = list.rows();
        if list.selected < rows.len() {
            let selected = rows.remove(list.selected);
            rows.insert(0, selected);
        }
        let targets = rows
            .into_iter()
            .filter_map(|(a, pr)| {
                Some(pr::PreloadTarget {
                    account: accounts.get(a)?.clone(),
                    summary: pr.clone(),
                    local: self
                        .workspace
                        .as_ref()
                        .and_then(|ws| ws.local_for(&pr.repo)),
                })
            })
            .collect();
        self.preloader.retarget(targets);
    }

    /// Drain finished background `gh` jobs into the list and session.
    pub(crate) fn poll_pr_events(&mut self) {
        while let Ok(event) = self.pr_rx.try_recv() {
            match event {
                PrEvent::Accounts { result, scope } => {
                    let list = self.list_mut();
                    list.accounts = match result {
                        Ok(accounts) => Load::Ready(accounts),
                        Err(e) => Load::Failed(e),
                    };
                    list.scope = scope;
                    self.start_list_queries();
                }
                PrEvent::List { key, result } => {
                    let list = self.list_mut();
                    list.results.insert(
                        key,
                        match result {
                            Ok(prs) => Load::Ready(prs),
                            Err(e) => Load::Failed(e),
                        },
                    );
                    list.clamp_selection();
                    self.sync_preload();
                }
                PrEvent::PreloadDetails {
                    key,
                    updated_at,
                    result,
                } => {
                    // Queue the files of the PR whose details just came in.
                    if self.preloader.apply_details(&key, &updated_at, result) {
                        self.sync_preload();
                    }
                }
                PrEvent::PreloadFile {
                    key,
                    head_sha,
                    path,
                    result,
                } => {
                    let versions = result.as_ref().ok().cloned();
                    self.preloader.apply_file(&key, &head_sha, &path, result);
                    // Hand it to the open review if it's the same PR and head.
                    let Some(session) = self.pr.as_mut().filter(|s| {
                        pr::pr_key(&s.account, &s.summary) == key
                            && s.details().is_some_and(|d| d.head_sha == head_sha)
                    }) else {
                        continue;
                    };
                    let Some((old, new)) = versions else {
                        continue;
                    };
                    if session.contents.contains_key(&path) {
                        continue;
                    }
                    session
                        .contents
                        .insert(path.clone(), (old.clone(), new.clone()));
                    let current = self
                        .files
                        .get(self.selected)
                        .filter(|e| e.path == path && self.diff_loading)
                        .cloned();
                    if let Some(entry) = current {
                        self.show_versions(&entry, old, new);
                    }
                }
                PrEvent::Session { generation, result } => {
                    let tx = self.pr_tx.clone();
                    let local = match (&self.workspace, &self.pr) {
                        (Some(ws), Some(session)) => ws.local_for(&session.summary.repo),
                        _ => None,
                    };
                    let selected_path = self.files.get(self.selected).map(|e| e.path.clone());
                    let Some(session) = self.pr.as_mut().filter(|s| s.generation == generation)
                    else {
                        continue;
                    };
                    match result {
                        Ok(details) => {
                            // A refresh keeps the selected file when it's still there.
                            let idx = selected_path
                                .and_then(|p| details.files.iter().position(|f| f.entry.path == p))
                                .unwrap_or(0);
                            let old_head = session.details().map(|d| d.head_sha.clone());
                            let new_head = details.head_sha.clone();
                            let dropped = session.set_details(details, tx, local);
                            self.selected = idx;
                            self.changed_lines = None;
                            self.reload();
                            // Reviews are pinned to the head that was loaded;
                            // make a moved head visible before anyone approves.
                            if old_head.is_some_and(|h| h != new_head) {
                                let mut msg = format!(
                                    "PR head moved to {} — re-check before submitting",
                                    &new_head[..new_head.len().min(7)]
                                );
                                if dropped > 0 {
                                    msg.push_str(&format!(
                                        "; {dropped} draft(s) on lines no longer in the diff dropped"
                                    ));
                                }
                                self.set_status(msg);
                            }
                        }
                        Err(e) => {
                            // A failed refresh keeps the files already loaded.
                            if session.details().is_none() {
                                session.details = Load::Failed(e.clone());
                            }
                            self.diff_loading = false;
                            self.set_status(format!("error: {e}"));
                        }
                    }
                }
                PrEvent::File {
                    generation,
                    path,
                    result,
                } => {
                    let Some(session) = self.pr.as_mut().filter(|s| s.generation == generation)
                    else {
                        continue;
                    };
                    let current = self
                        .files
                        .get(self.selected)
                        .filter(|e| e.path == path && self.diff_loading)
                        .cloned();
                    match result {
                        Ok((old, new)) => {
                            session.contents.insert(path, (old.clone(), new.clone()));
                            if let Some(entry) = current {
                                self.show_versions(&entry, old, new);
                            }
                        }
                        Err(e) => {
                            if current.is_some() {
                                self.diff_loading = false;
                                self.set_status(format!("error: {e}"));
                            }
                        }
                    }
                }
                PrEvent::Submitted {
                    generation,
                    event,
                    result,
                } => {
                    if self.pr.as_ref().map(|s| s.generation) != Some(generation) {
                        continue;
                    }
                    match result {
                        Ok(()) => {
                            self.review_modal = None;
                            if let Some(session) = self.pr.as_mut() {
                                session.drafts.clear();
                            }
                            // The PR may have moved between tabs (e.g. out of
                            // review requested); re-run searches next visit.
                            if let Some(list) = self.pr_list.as_mut() {
                                list.invalidate(None);
                            }
                            self.set_status(format!("review submitted: {}", event.label()));
                        }
                        Err(e) => match self.review_modal.as_mut() {
                            Some(modal) => {
                                modal.submitting = false;
                                modal.error = Some(e);
                            }
                            None => self.set_status(format!("review failed: {e}")),
                        },
                    }
                }
            }
        }
    }

    /// With unsent draft comments, the first call arms a confirmation and
    /// returns false; the repeated (armed) call goes through.
    fn confirm_discard(&mut self, armed: bool) -> bool {
        let drafts = self.pr.as_ref().map_or(0, |s| s.drafts.len());
        if drafts == 0 || armed {
            return true;
        }
        self.discard_armed = true;
        self.set_status(format!(
            "{drafts} unsent draft comment(s) — press again to discard"
        ));
        false
    }

    fn request_quit(&mut self, armed: bool) {
        if self.confirm_discard(armed) {
            self.quit = true;
        }
    }

    /// Reset per-source view state when switching between the local
    /// repository and a PR.
    fn clear_view_state(&mut self) {
        self.files.clear();
        self.selected = 0;
        self.file_scroll = None;
        self.h_scroll_files = 0;
        self.file_search = None;
        self.search_input = None;
        self.path_filter = None;
        self.content_filter = None;
        self.changed_lines = None;
        self.comment_editor = None;
        self.review_modal = None;
        self.pr_info_scroll = None;
        self.reload_diff();
    }

    /// Start reviewing the PR selected in the list.
    fn open_selected_pr(&mut self, armed: bool) {
        let Some((account, summary)) = self.pr_list.as_ref().and_then(|l| l.selected_row()) else {
            return;
        };
        match &self.pr {
            Some(session)
                if session.summary.url == summary.url
                    && session.account.host == account.host
                    && session.account.login == account.login =>
            {
                // Same PR as the one under review: just go back to it.
                self.pr_list_open = false;
                return;
            }
            Some(_) => {
                if !self.confirm_discard(armed) {
                    return;
                }
            }
            None => {
                self.local_selected_path = self.files.get(self.selected).map(|e| e.path.clone());
            }
        }
        self.pr_generation += 1;
        let preloaded = self.preloader.take(&account, &summary);
        if preloaded.is_none() {
            pr::spawn_session(
                self.pr_tx.clone(),
                self.pr_generation,
                account.clone(),
                summary.repo.clone(),
                summary.number,
            );
        }
        let local = self
            .workspace
            .as_ref()
            .and_then(|ws| ws.local_for(&summary.repo));
        let mut session = pr::PrSession::new(self.pr_generation, account, summary);
        self.pr_list_open = false;
        match preloaded {
            // Fetched in the background while the list was open: show it
            // now, as fresh as the list it came from (r refreshes).
            Some((details, contents)) => {
                session.set_details(details, self.pr_tx.clone(), local);
                session.contents = contents;
                self.pr = Some(session);
                self.clear_view_state();
                self.reload();
            }
            None => {
                self.pr = Some(session);
                self.clear_view_state();
                self.diff_loading = true;
            }
        }
    }

    /// Stop reviewing and return to the PR list (the local diff is
    /// reloaded behind it).
    fn leave_pr(&mut self, armed: bool) {
        if !self.confirm_discard(armed) {
            return;
        }
        self.pr = None;
        self.clear_view_state();
        self.reload();
        if let Some(path) = self.local_selected_path.take() {
            if let Some(idx) = self.files.iter().position(|e| e.path == path) {
                self.selected = idx;
                self.reload_diff();
            }
        }
        self.open_pr_list();
    }

    /// Re-fetch the PR under review (new commits, changed files). Drafts
    /// are kept.
    fn refresh_pr(&mut self) {
        let Some(session) = self.pr.as_mut() else {
            return;
        };
        // New generation: in-flight results for the old head are dropped.
        self.pr_generation += 1;
        session.generation = self.pr_generation;
        pr::spawn_session(
            self.pr_tx.clone(),
            session.generation,
            session.account.clone(),
            session.summary.repo.clone(),
            session.summary.number,
        );
        self.set_status("refreshing PR…");
    }

    /// Open the selected list PR, or the one under review, in the browser.
    fn open_in_browser(&mut self) {
        let target = if self.pr_list_open {
            self.pr_list.as_ref().and_then(|l| l.selected_row())
        } else {
            self.pr
                .as_ref()
                .map(|s| (s.account.clone(), s.summary.clone()))
        };
        let Some((account, summary)) = target else {
            return;
        };
        match gh::open_in_browser(&account, &summary.url) {
            Ok(()) => self.set_status(format!("opened {}#{}", summary.repo, summary.number)),
            Err(e) => self.set_status(format!("error: {e}")),
        }
    }

    fn handle_pr_list_key(&mut self, code: KeyCode, modifiers: KeyModifiers, armed: bool) {
        let list = self.list_mut();
        if list.dash_list.is_some() {
            match code {
                KeyCode::Char('y') => self.copy_dash_list(),
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => list.dash_list = None,
                _ => {}
            }
            return;
        }
        let len = list.rows().len();
        if let Some(scroll) = list.info_scroll.as_mut() {
            match code {
                KeyCode::Char('j') | KeyCode::Down => *scroll = scroll.saturating_add(1),
                KeyCode::Char('k') | KeyCode::Up => *scroll = scroll.saturating_sub(1),
                KeyCode::PageDown => *scroll = scroll.saturating_add(10),
                KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
                // Step through PRs without closing the popup.
                KeyCode::Char('J') => {
                    list.selected = (list.selected + 1).min(len.saturating_sub(1));
                    list.info_scroll = Some(0);
                }
                KeyCode::Char('K') => {
                    list.selected = list.selected.saturating_sub(1);
                    list.info_scroll = Some(0);
                }
                KeyCode::Char(' ') => {
                    list.toggle_mark();
                }
                KeyCode::Char('o') => self.open_in_browser(),
                KeyCode::Enter => {
                    list.info_scroll = None;
                    self.open_selected_pr(armed);
                }
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('i') => list.info_scroll = None,
                _ => {}
            }
            return;
        }
        if let Some(input) = list.filter_input.as_mut() {
            match code {
                KeyCode::Esc => {
                    list.filter_input = None;
                    list.text_filter = None;
                }
                KeyCode::Enter => {
                    let query = list.filter_input.take().unwrap_or_default();
                    list.text_filter = (!query.is_empty()).then_some(query);
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
                    input.push(c);
                    list.selected = 0;
                }
                _ => {}
            }
            list.clamp_selection();
            return;
        }

        let page = (list.view_area.height as usize).max(1);
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                list.selected = (list.selected + 1).min(len.saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => list.selected = list.selected.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => list.selected = 0,
            KeyCode::Char('G') | KeyCode::End => list.selected = len.saturating_sub(1),
            KeyCode::PageDown => {
                list.selected = (list.selected + page).min(len.saturating_sub(1));
            }
            KeyCode::PageUp => list.selected = list.selected.saturating_sub(page),
            KeyCode::Tab | KeyCode::BackTab => {
                list.tab = list.tab.cycle(code == KeyCode::Tab);
                list.selected = 0;
                self.start_list_queries();
            }
            KeyCode::Char(c @ '1'..='4') => {
                list.tab = pr::PrTab::ALL[c as usize - '1' as usize];
                list.selected = 0;
                self.start_list_queries();
            }
            KeyCode::Char('a') => {
                let n = list.account_list().len();
                if n <= 1 {
                    self.set_status("only one gh account is logged in");
                    return;
                }
                list.account_filter = match list.account_filter {
                    None => Some(0),
                    Some(i) if i + 1 < n => Some(i + 1),
                    Some(_) => None,
                };
                list.selected = 0;
                self.start_list_queries();
            }
            KeyCode::Char('s') => {
                list.state = list.state.next();
                list.selected = 0;
                self.start_list_queries();
            }
            KeyCode::Char('d') => {
                list.hide_drafts = !list.hide_drafts;
                list.clamp_selection();
            }
            KeyCode::Char('w') => {
                if list.scope.is_none() {
                    self.set_status("no local GitHub repositories to limit the list to");
                    return;
                }
                list.scoped = !list.scoped;
                list.selected = 0;
                self.start_list_queries();
            }
            KeyCode::Char('/') => {
                list.filter_input = Some(list.text_filter.clone().unwrap_or_default());
            }
            KeyCode::Char('r') => {
                list.invalidate(None);
                if matches!(list.accounts, Load::Failed(_)) {
                    list.accounts = Load::Loading;
                    pr::spawn_accounts(self.pr_tx.clone(), self.workspace.clone());
                }
                self.start_list_queries();
                self.set_status("refreshing…");
            }
            KeyCode::Char('o') => self.open_in_browser(),
            KeyCode::Char('i') => {
                if !list.rows().is_empty() {
                    list.info_scroll = Some(0);
                }
            }
            KeyCode::Char(' ') => {
                if list.toggle_mark().is_some() {
                    list.selected = (list.selected + 1).min(len.saturating_sub(1));
                }
            }
            KeyCode::Char('u') => {
                let n = std::mem::take(&mut list.marked).len();
                self.set_status(format!("unmarked {n} PR(s)"));
            }
            KeyCode::Char('y') => self.copy_dash_list(),
            KeyCode::Enter => self.open_selected_pr(armed),
            KeyCode::Esc => {
                if list.text_filter.take().is_some() {
                    list.clamp_selection();
                } else {
                    self.pr_list_open = false;
                    // Nothing to go back to outside a repository.
                    if self.pr.is_none() && self.workspace.is_none() {
                        self.quit = true;
                    }
                }
            }
            KeyCode::Char('q') => self.request_quit(armed),
            _ => {}
        }
    }

    /// Copy the dash list of marked PRs (else the selected one) and show
    /// it in a popup.
    fn copy_dash_list(&mut self) {
        let list = self.list_mut();
        let Some(text) = list.dash_list_text() else {
            self.set_status("no pull request selected");
            return;
        };
        let count = text.lines().count();
        let copied = match clipboard::copy(&text) {
            Ok(clipboard::Copied::Tool(tool)) => Ok(format!("copied {count} line(s) via {tool}")),
            Ok(clipboard::Copied::Terminal) => Ok(format!(
                "sent {count} line(s) to the terminal clipboard (OSC 52)"
            )),
            Err(e) => Err(format!("could not copy: {e}")),
        };
        list.dash_list = Some(pr::DashList { text, copied });
    }

    fn handle_pr_list_mouse(&mut self, mouse: MouseEvent) {
        let list = self.list_mut();
        if list.dash_list.is_some() {
            return;
        }
        if let Some(scroll) = list.info_scroll.as_mut() {
            match mouse.kind {
                MouseEventKind::ScrollDown => *scroll = scroll.saturating_add(3),
                MouseEventKind::ScrollUp => *scroll = scroll.saturating_sub(3),
                _ => {}
            }
            return;
        }
        let len = list.rows().len();
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                list.selected = (list.selected + 3).min(len.saturating_sub(1));
            }
            MouseEventKind::ScrollUp => list.selected = list.selected.saturating_sub(3),
            MouseEventKind::Down(MouseButton::Left) => {
                let area = list.view_area;
                if area.contains(Position::new(mouse.column, mouse.row)) {
                    let idx = list.view_offset + (mouse.row - area.y) as usize;
                    if idx < len {
                        list.selected = idx;
                    }
                }
            }
            _ => {}
        }
    }

    /// PR-review-only bindings; returns true when the key was handled.
    fn handle_pr_review_key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Char('J') => self.move_cursor(true),
            KeyCode::Char('K') => self.move_cursor(false),
            KeyCode::Char('a') => self.open_comment_editor(Side::Right),
            KeyCode::Char('A') => self.open_comment_editor(Side::Left),
            KeyCode::Char('x') => self.delete_drafts_at_cursor(),
            KeyCode::Char('R') => self.open_review_modal(),
            KeyCode::Char('i') => self.pr_info_scroll = Some(0),
            KeyCode::Char('o') => self.open_in_browser(),
            KeyCode::Char('r') => self.refresh_pr(),
            KeyCode::Char('m') | KeyCode::Char('b') => {
                self.set_status("not available while reviewing a PR");
            }
            _ => return false,
        }
        true
    }

    /// Whether aligned row `i` is drawn in at least one visible pane.
    fn row_visible(&self, i: usize) -> bool {
        let Some(row) = self.diff.rows.get(i) else {
            return false;
        };
        !self.compact
            || (self.show_old && row.left.is_some())
            || (self.show_new && row.right.is_some())
    }

    /// `row_to_scroll` for every aligned row, in one pass.
    fn scroll_positions(&self) -> Vec<usize> {
        let (mut left, mut right) = (0usize, 0usize);
        let mut out = Vec::with_capacity(self.diff.rows.len());
        for (i, row) in self.diff.rows.iter().enumerate() {
            out.push(if !self.compact {
                i
            } else {
                match (self.show_old, self.show_new) {
                    (true, false) => left,
                    (false, true) => right,
                    _ => left.min(right),
                }
            });
            left += row.left.is_some() as usize;
            right += row.right.is_some() as usize;
        }
        out
    }

    /// Move the comment cursor to the next/previous visible row and
    /// scroll it into view.
    fn move_cursor(&mut self, forward: bool) {
        let n = self.diff.rows.len();
        let mut i = self.cursor.min(n.saturating_sub(1));
        loop {
            let next = match (forward, i.checked_sub(1)) {
                (true, _) => i + 1,
                (false, Some(prev)) => prev,
                (false, None) => break,
            };
            if next >= n {
                break;
            }
            i = next;
            if self.row_visible(i) {
                self.cursor = i;
                break;
            }
        }
        let pos = self.row_to_scroll(self.cursor.min(n));
        let height = self.diff_height.max(1);
        if pos < self.scroll {
            self.scroll = pos;
        } else if pos >= self.scroll + height {
            self.scroll = pos + 1 - height;
        }
        self.clamp_scroll();
    }

    /// Keep the comment cursor on screen after the view scrolled.
    fn clamp_cursor_to_view(&mut self) {
        let n = self.diff.rows.len();
        if n == 0 {
            self.cursor = 0;
            return;
        }
        self.cursor = self.cursor.min(n - 1);
        let positions = self.scroll_positions();
        let (top, height) = (self.scroll, self.diff_height.max(1));
        let pos = positions[self.cursor];
        if pos >= top && pos < top + height && self.row_visible(self.cursor) {
            return;
        }
        let target = if pos < top {
            (0..n).find(|&i| self.row_visible(i) && positions[i] >= top)
        } else {
            (0..n).rev().find(|&i| self.row_visible(i) && positions[i] < top + height)
        };
        if let Some(i) = target {
            self.cursor = i;
        }
    }

    /// Put the comment cursor on the diff row clicked in one pane.
    fn click_diff_row(&mut self, row: u16, left: bool) {
        let area = if left { self.old_pane_area } else { self.new_pane_area };
        let Some(offset) = row.checked_sub(area.y) else {
            return;
        };
        let visible = self.scroll + offset as usize;
        let idx = if self.compact {
            self.diff
                .rows
                .iter()
                .enumerate()
                .filter(|(_, r)| if left { r.left.is_some() } else { r.right.is_some() })
                .nth(visible)
                .map(|(i, _)| i)
        } else {
            (visible < self.diff.rows.len()).then_some(visible)
        };
        if let Some(i) = idx {
            self.cursor = i;
        }
    }

    /// Open the inline comment editor on the cursor row: the new side
    /// for `Side::Right` (old side for pure removals), the old side for
    /// `Side::Left` (new side for pure additions).
    fn open_comment_editor(&mut self, prefer: Side) {
        let editor = (|| {
            let session = self.pr.as_ref()?;
            let entry = self.files.get(self.selected)?;
            let row = self.diff.rows.get(self.cursor)?;
            let left = row.left.as_ref().map(|c| (Side::Left, c.line_no));
            let right = row.right.as_ref().map(|c| (Side::Right, c.line_no));
            let (side, line) = match prefer {
                Side::Right => right.or(left),
                Side::Left => left.or(right),
            }?;
            let file = session.file(&entry.path)?;
            if file.patch.is_none() {
                return Some(Err(
                    "GitHub has no diff for this file (too large); inline comments unavailable"
                        .to_string(),
                ));
            }
            if !file.commentable.contains(&(side, line)) {
                return Some(Err(format!(
                    "line {line} is outside the PR's diff hunks — GitHub only accepts comments on changed lines and nearby context"
                )));
            }
            let text = session
                .draft_at(&entry.path, side, line)
                .map(|i| session.drafts[i].body.clone())
                .unwrap_or_default();
            Some(Ok(pr::CommentEditor {
                path: entry.path.clone(),
                side,
                line,
                buffer: pr::TextBuffer { text },
            }))
        })();
        match editor {
            Some(Ok(editor)) => self.comment_editor = Some(editor),
            Some(Err(msg)) => self.set_status(msg),
            None => self.set_status("no line to comment on"),
        }
    }

    fn handle_comment_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        match code {
            KeyCode::Esc => self.comment_editor = None,
            KeyCode::Char('s') if modifiers.contains(KeyModifiers::CONTROL) => self.save_comment(),
            _ => {
                if let Some(editor) = self.comment_editor.as_mut() {
                    edit_text(&mut editor.buffer, code, modifiers);
                }
            }
        }
    }

    /// Store the edited comment as a draft; an emptied draft is removed.
    fn save_comment(&mut self) {
        let Some(editor) = self.comment_editor.take() else {
            return;
        };
        let Some(session) = self.pr.as_mut() else {
            return;
        };
        let body = editor.buffer.text.trim_end().to_string();
        let msg = match (session.draft_at(&editor.path, editor.side, editor.line), body.is_empty()) {
            (Some(i), true) => {
                session.drafts.remove(i);
                "draft comment removed"
            }
            (Some(i), false) => {
                session.drafts[i].body = body;
                "draft comment updated"
            }
            (None, true) => return,
            (None, false) => {
                session.drafts.push(gh::DraftComment {
                    path: editor.path,
                    side: editor.side,
                    line: editor.line,
                    body,
                });
                "draft comment added — R to submit the review"
            }
        };
        self.set_status(msg);
    }

    /// Drop the draft comments on either side of the cursor row.
    fn delete_drafts_at_cursor(&mut self) {
        let (Some(session), Some(entry), Some(row)) = (
            self.pr.as_mut(),
            self.files.get(self.selected),
            self.diff.rows.get(self.cursor),
        ) else {
            return;
        };
        let left = row.left.as_ref().map(|c| c.line_no);
        let right = row.right.as_ref().map(|c| c.line_no);
        let before = session.drafts.len();
        session.drafts.retain(|d| {
            d.path != entry.path
                || match d.side {
                    Side::Left => Some(d.line) != left,
                    Side::Right => Some(d.line) != right,
                }
        });
        let removed = before - session.drafts.len();
        self.set_status(if removed == 0 {
            "no draft comment on this line".to_string()
        } else {
            format!("removed {removed} draft comment(s)")
        });
    }

    fn open_review_modal(&mut self) {
        let Some(session) = &self.pr else {
            return;
        };
        if session.details().is_none() {
            self.set_status("PR is still loading");
            return;
        }
        self.review_modal = Some(pr::ReviewModal::new(!session.is_own()));
    }

    fn handle_review_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        let Some(modal) = self.review_modal.as_mut() else {
            return;
        };
        if modal.submitting {
            return;
        }
        match code {
            KeyCode::Esc => self.review_modal = None,
            KeyCode::Tab => modal.cycle_event(true),
            KeyCode::BackTab => modal.cycle_event(false),
            KeyCode::Char('s') if modifiers.contains(KeyModifiers::CONTROL) => self.submit_review(),
            _ => edit_text(&mut modal.buffer, code, modifiers),
        }
    }

    fn submit_review(&mut self) {
        let (Some(session), Some(modal)) = (self.pr.as_ref(), self.review_modal.as_mut()) else {
            return;
        };
        let Some(details) = session.details() else {
            return;
        };
        if let Some(problem) = modal.validate(session.drafts.len()) {
            modal.error = Some(problem.to_string());
            return;
        }
        modal.submitting = true;
        modal.error = None;
        pr::spawn_submit(
            self.pr_tx.clone(),
            session,
            details.head_sha.clone(),
            modal.event,
            modal.buffer.text.clone(),
        );
    }

    fn handle_info_key(&mut self, code: KeyCode) {
        let Some(scroll) = self.pr_info_scroll.as_mut() else {
            return;
        };
        match code {
            KeyCode::Char('j') | KeyCode::Down => *scroll = scroll.saturating_add(1),
            KeyCode::Char('k') | KeyCode::Up => *scroll = scroll.saturating_sub(1),
            KeyCode::PageDown => *scroll = scroll.saturating_add(10),
            KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('i') => self.pr_info_scroll = None,
            _ => {}
        }
    }

    /// Placeholder for an empty diff pane: loading, load failure, or no diff.
    pub(crate) fn diff_placeholder(&self) -> String {
        if self.diff_loading {
            return "(loading…)".to_string();
        }
        match self.pr.as_ref().map(|s| &s.details) {
            Some(Load::Failed(e)) => format!("(failed to load PR: {e})"),
            _ => "(no textual diff)".to_string(),
        }
    }

    /// Whether a draft comment sits on `side`/`line` of the selected file.
    pub(crate) fn has_draft(&self, side: Side, line: usize) -> bool {
        match (&self.pr, self.files.get(self.selected)) {
            (Some(session), Some(entry)) => session.draft_at(&entry.path, side, line).is_some(),
            _ => false,
        }
    }
}

/// Apply a key to a modal text buffer: printable characters and Enter
/// (newline) append, Backspace deletes the last character.
fn edit_text(buffer: &mut pr::TextBuffer, code: KeyCode, modifiers: KeyModifiers) {
    match code {
        KeyCode::Enter => buffer.text.push('\n'),
        KeyCode::Backspace => {
            buffer.text.pop();
        }
        KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => buffer.text.push(c),
        _ => {}
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

/// Screen the TUI opens on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartView {
    Diff,
    Prs,
    Actions,
}

/// Run the TUI, starting on `start`.
pub fn run(
    workspace: Option<Workspace>,
    mode: DiffMode,
    config: config::Config,
    start: StartView,
) -> Result<()> {
    let mut app = App::new(workspace, mode, config);
    app.reload();
    if start == StartView::Prs || app.workspace.is_none() {
        app.open_pr_list();
    } else if start == StartView::Actions {
        app.open_actions();
    }

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

/// Poll the workspace fingerprint on a background thread and raise
/// `flag` whenever it changes, so the UI auto-refreshes on external
/// edits, commits, or staging. Exits with the process.
fn spawn_repo_watcher(workspace: Workspace, flag: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let mut last = workspace.state_fingerprint();
        loop {
            std::thread::sleep(WATCH_INTERVAL);
            let current = workspace.state_fingerprint();
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
        app.poll_pr_events();
        app.poll_actions();

        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app.handle_key(key.code, key.modifiers);
                }
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                _ => {}
            }
            app.persist_settings();
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
