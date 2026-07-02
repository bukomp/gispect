//! Terminal application state and event loop.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::git::GitRepo;
use crate::types::{DiffMode, FileEntry, FileStatus, SideBySideDiff};
use crate::{diff, update};

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
    pub(crate) h_scroll: usize,
    pub(crate) show_help: bool,
    /// Compact view: hide alignment filler rows so each pane shows its
    /// version of the file contiguously.
    pub(crate) compact: bool,
    /// Whether the changed-files panel is visible.
    pub(crate) show_files: bool,
    pub(crate) status: Option<String>,
    pub(crate) update_available: Arc<Mutex<Option<String>>>,
    pub(crate) base_choices: Vec<String>,
    pub(crate) quit: bool,
    pub(crate) last_viewport_height: usize,
    /// Whether syntax highlighting is enabled for the diff panes.
    pub(crate) syntax: bool,
    /// Highlighted segments for the left (old) pane, indexed by line - 1.
    pub(crate) left_hl: Vec<Vec<(ratatui::style::Style, String)>>,
    /// Highlighted segments for the right (new) pane, indexed by line - 1.
    pub(crate) right_hl: Vec<Vec<(ratatui::style::Style, String)>>,
    highlighter: crate::highlight::Highlighter,
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
        App {
            repo,
            mode,
            base,
            branch,
            files: Vec::new(),
            selected: 0,
            diff: SideBySideDiff::default(),
            scroll: 0,
            h_scroll: 0,
            show_help: false,
            compact: false,
            show_files: true,
            status: None,
            update_available: Arc::new(Mutex::new(None)),
            base_choices,
            quit: false,
            last_viewport_height: 20,
            syntax: true,
            left_hl: Vec::new(),
            right_hl: Vec::new(),
            highlighter: crate::highlight::Highlighter::new(),
            trigger_update: false,
            awaiting_update_exit: false,
        }
    }

    /// Whether the app is exiting specifically to perform a self-update.
    pub(crate) fn pending_update(&self) -> bool {
        self.awaiting_update_exit
    }

    /// Reload the file list for the current mode, then reload the diff
    /// for the currently selected file.
    fn reload(&mut self) {
        match self.repo.changed_files(&self.mode) {
            Ok(files) => {
                self.files = files;
                if self.selected >= self.files.len() {
                    self.selected = self.files.len().saturating_sub(1);
                }
            }
            Err(e) => {
                self.files.clear();
                self.status = Some(format!("error: {e}"));
            }
        }
        self.reload_diff();
    }

    /// Recompute the side-by-side diff for the currently selected file.
    fn reload_diff(&mut self) {
        self.diff = SideBySideDiff::default();
        self.scroll = 0;
        self.h_scroll = 0;
        self.left_hl = Vec::new();
        self.right_hl = Vec::new();
        let Some(entry) = self.files.get(self.selected).cloned() else {
            return;
        };
        if entry.binary {
            self.status = Some("binary file".to_string());
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
                    self.left_hl = self.highlighter.highlight(&old_path, &old);
                    self.right_hl = self.highlighter.highlight(&entry.path, &new);
                }
            }
            Err(e) => {
                self.status = Some(format!("error: {e}"));
            }
        }
    }

    /// Number of scrollable rows in the diff view: aligned rows normally,
    /// the longer of the two compacted panes in compact mode.
    pub(crate) fn row_count(&self) -> usize {
        if self.compact {
            let left = self.diff.rows.iter().filter(|r| r.left.is_some()).count();
            let right = self.diff.rows.iter().filter(|r| r.right.is_some()).count();
            left.max(right)
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

    fn next_file(&mut self) {
        if self.files.is_empty() {
            return;
        }
        if self.selected + 1 < self.files.len() {
            self.selected += 1;
        }
        self.reload_diff();
    }

    fn prev_file(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
        self.reload_diff();
    }

    fn cycle_base(&mut self) {
        if self.base_choices.is_empty() {
            self.status = Some("no local branches found".to_string());
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
        self.status = Some(format!("base: {next_base}"));
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
                let half = (self.last_viewport_height / 2).max(1);
                self.scroll = self.scroll.saturating_add(half);
                self.clamp_scroll();
            }
            KeyCode::PageUp => {
                let half = (self.last_viewport_height / 2).max(1);
                self.scroll = self.scroll.saturating_sub(half);
            }
            KeyCode::Char('g') => self.scroll = 0,
            KeyCode::Char('G') => {
                self.scroll = self.row_count().saturating_sub(1);
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.h_scroll = self.h_scroll.saturating_sub(4);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.h_scroll = self.h_scroll.saturating_add(4);
            }
            KeyCode::Char('m') => {
                self.mode = self.mode.next(&self.base);
                let label = self.mode.label();
                self.reload();
                self.status = Some(label);
            }
            KeyCode::Char('f') => {
                self.show_files = !self.show_files;
            }
            KeyCode::Char('c') => {
                self.compact = !self.compact;
                self.clamp_scroll();
                self.status = Some(if self.compact {
                    "compact view: filler rows hidden (panes scroll independently)".to_string()
                } else {
                    "aligned view: filler rows shown".to_string()
                });
            }
            KeyCode::Char('s') => {
                self.syntax = !self.syntax;
                let scroll = self.scroll;
                let h_scroll = self.h_scroll;
                self.reload_diff();
                self.scroll = scroll;
                self.h_scroll = h_scroll;
                self.clamp_scroll();
                self.status = Some(if self.syntax {
                    "syntax highlighting on".to_string()
                } else {
                    "syntax highlighting off".to_string()
                });
            }
            KeyCode::Char('b') => self.cycle_base(),
            KeyCode::Char('r') => {
                self.reload();
                self.status = Some("reloaded".to_string());
            }
            KeyCode::Char('U') => {
                self.trigger_update = true;
            }
            KeyCode::Char('?') => self.show_help = true,
            _ => {}
        }
    }
}

/// Restores the terminal on drop, even on panic/early return.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

pub fn run(repo: GitRepo, mode: DiffMode) -> Result<()> {
    let mut app = App::new(repo, mode);
    app.reload();

    // Background update check; ignore errors silently.
    let update_slot = app.update_available.clone();
    std::thread::spawn(move || {
        if let Ok(Some(hash)) = update::check_for_update() {
            if let Ok(mut slot) = update_slot.lock() {
                *slot = Some(hash);
            }
        }
    });

    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let _guard = TerminalGuard;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(&mut terminal, &mut app);

    // Explicitly restore before doing anything post-loop (e.g. printing
    // update output); the guard will also restore on drop as a fallback.
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;

    if app.pending_update() {
        println!("Updating gispect...");
        match update::perform_update() {
            Ok(()) => println!("Update complete. Please restart gispect."),
            Err(e) => println!("Update failed: {e}"),
        }
    }

    result
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

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key.code, key.modifiers);
                }
            }
        }

        if app.trigger_update {
            app.trigger_update = false;
            if app.update_available.lock().ok().and_then(|g| g.clone()).is_some() {
                app.awaiting_update_exit = true;
                app.quit = true;
            } else {
                app.status = Some("no update available".to_string());
            }
        }

        if app.quit {
            break;
        }
    }
    Ok(())
}
