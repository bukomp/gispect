//! GitHub Actions view state: the repository → workflow → run tree, the
//! run (dispatch) modal, key handling, and the background `gh` jobs that
//! load workflows, poll runs while any is active, and start, re-run or
//! cancel runs. Rendering lives in `actions_ui.rs`.
//!
//! Each repository is matched to its GitHub remote and to a logged-in
//! `gh` account on that host, preferring one that may push (dispatching
//! and re-running need write access).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use crate::gh::{self, Account, RunAction, Workflow, WorkflowRun};
use crate::pr::Load;
use crate::search;
use crate::workflow::{self, DispatchInput, InputKind};
use crate::workspace::Workspace;

/// Runs listed under an expanded workflow.
pub const RUNS_PER_WORKFLOW: usize = 8;
/// Run list refresh interval while a run is queued or in progress.
const ACTIVE_POLL: Duration = Duration::from_secs(8);
/// Refresh interval right after starting or re-running a run, until it
/// shows up (GitHub takes a few seconds to create it).
const WATCH_POLL: Duration = Duration::from_secs(3);
/// How long to keep watching for a run that was just requested.
const WATCH_FOR: Duration = Duration::from_secs(45);

/// Whether and how a workflow can be started by hand.
#[derive(Debug, Clone)]
pub enum Dispatch {
    /// Has a `workflow_dispatch` trigger with these inputs.
    Manual(Vec<DispatchInput>),
    /// No `workflow_dispatch` trigger (or a GitHub-managed workflow).
    NotManual,
    /// The workflow file couldn't be read or parsed.
    Unknown(String),
}

pub struct WorkflowNode {
    pub workflow: Workflow,
    pub dispatch: Dispatch,
}

impl WorkflowNode {
    pub fn runnable(&self) -> bool {
        self.workflow.is_active() && matches!(self.dispatch, Dispatch::Manual(_))
    }
}

/// A repository's Actions data, as seen by one account.
pub struct RepoData {
    pub account: Account,
    /// `owner/name` on GitHub.
    pub full_name: String,
    pub can_push: bool,
    pub default_branch: String,
    pub workflows: Vec<WorkflowNode>,
    /// The repository's most recent runs, newest first.
    pub runs: Vec<WorkflowRun>,
    /// Why the last run refresh failed, if it did.
    pub runs_error: Option<String>,
}

impl RepoData {
    /// Runs of one workflow, newest first.
    pub fn runs_of(&self, workflow: u64) -> impl Iterator<Item = &WorkflowRun> {
        self.runs.iter().filter(move |r| r.workflow_id == workflow)
    }

    pub fn latest_run(&self, workflow: u64) -> Option<&WorkflowRun> {
        self.runs_of(workflow).next()
    }

    pub fn active_runs(&self) -> usize {
        self.runs.iter().filter(|r| r.is_active()).count()
    }

    fn workflow(&self, id: u64) -> Option<&WorkflowNode> {
        self.workflows.iter().find(|w| w.workflow.id == id)
    }
}

/// One local repository in the tree.
pub struct RepoNode {
    /// Folder name.
    pub name: String,
    root: PathBuf,
    /// Branch checked out locally; the default ref to run on.
    pub branch: String,
    /// `(host, owner/name)` of the first remote that looks like GitHub.
    pub remote: Option<(String, String)>,
    pub data: Load<RepoData>,
    pub expanded: bool,
    /// Workflows whose runs are listed.
    pub open_workflows: HashSet<u64>,
    /// Bumped per full load so late results of an older one are dropped.
    generation: u64,
    refreshing: bool,
    next_poll: Option<Instant>,
    watch_until: Option<Instant>,
}

impl RepoNode {
    /// One-line reason the repository shows no workflows, if any.
    pub fn note(&self) -> Option<String> {
        if self.remote.is_none() {
            return Some("no GitHub remote".to_string());
        }
        match &self.data {
            Load::Loading => Some("loading…".to_string()),
            Load::Failed(e) => Some(e.clone()),
            Load::Ready(d) if d.workflows.is_empty() => Some("no workflows".to_string()),
            Load::Ready(_) => None,
        }
    }

    pub fn ready(&self) -> Option<&RepoData> {
        match &self.data {
            Load::Ready(d) => Some(d),
            _ => None,
        }
    }

    fn ready_mut(&mut self) -> Option<&mut RepoData> {
        match &mut self.data {
            Load::Ready(d) => Some(d),
            _ => None,
        }
    }

    /// Refresh runs soon and keep refreshing until a new run appears.
    fn watch(&mut self) {
        let now = Instant::now();
        self.watch_until = Some(now + WATCH_FOR);
        self.next_poll = Some(now + Duration::from_secs(2));
    }

    /// When to refresh runs next: often while a run is active or one was
    /// just requested, else not at all (press `r`).
    fn schedule_poll(&mut self) {
        let now = Instant::now();
        let active = self.ready().is_some_and(|d| d.active_runs() > 0);
        self.next_poll = if active {
            Some(now + ACTIVE_POLL)
        } else if self.watch_until.is_some_and(|t| t > now) {
            Some(now + WATCH_POLL)
        } else {
            None
        };
    }
}

/// One row of the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKey {
    Repo(usize),
    /// Repository status line (loading, error, no workflows).
    Note(usize),
    Workflow(usize, u64),
    Run(usize, u64, u64),
    /// "no recent runs" under an expanded workflow.
    NoRuns(usize, u64),
}

impl RowKey {
    pub fn repo(self) -> usize {
        match self {
            RowKey::Repo(r)
            | RowKey::Note(r)
            | RowKey::Workflow(r, _)
            | RowKey::Run(r, _, _)
            | RowKey::NoRuns(r, _) => r,
        }
    }

    fn parent(self) -> Option<RowKey> {
        match self {
            RowKey::Repo(_) => None,
            RowKey::Note(r) | RowKey::Workflow(r, _) => Some(RowKey::Repo(r)),
            RowKey::Run(r, w, _) | RowKey::NoRuns(r, w) => Some(RowKey::Workflow(r, w)),
        }
    }
}

/// Results of background `gh` jobs, drained by [`ActionsView::poll`].
pub enum ActionsEvent {
    Accounts(Result<Vec<Account>, String>),
    Repo {
        idx: usize,
        generation: u64,
        result: Result<RepoData, String>,
    },
    Runs {
        idx: usize,
        generation: u64,
        result: Result<Vec<WorkflowRun>, String>,
    },
    Dispatched {
        idx: usize,
        workflow: u64,
        name: String,
        git_ref: String,
        result: Result<(), String>,
    },
    RunDone {
        idx: usize,
        run_number: u64,
        action: RunAction,
        result: Result<(), String>,
    },
}

/// What a key press asks of the app.
pub enum KeyOutcome {
    /// Handled; optionally show a status message.
    Done(Option<String>),
    /// Leave the actions screen.
    Back,
    Quit,
}

/// The run (dispatch) modal for one workflow.
pub struct RunModal {
    pub repo: usize,
    pub workflow: u64,
    pub workflow_name: String,
    pub full_name: String,
    pub account: String,
    pub can_push: bool,
    pub git_ref: String,
    pub inputs: Vec<DispatchInput>,
    /// Current value of each input, parallel to `inputs`.
    pub values: Vec<String>,
    /// Focused field: 0 is the ref, then one per input.
    pub field: usize,
    pub submitting: bool,
    pub error: Option<String>,
}

enum ModalAction {
    None,
    Submit,
    Cancel,
}

impl RunModal {
    fn field_count(&self) -> usize {
        1 + self.inputs.len()
    }

    /// The focused input (`None` on the ref field).
    pub fn focused_input(&self) -> Option<&DispatchInput> {
        self.field.checked_sub(1).and_then(|i| self.inputs.get(i))
    }

    fn key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> ModalAction {
        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
        match code {
            KeyCode::Esc => return ModalAction::Cancel,
            KeyCode::Enter => return ModalAction::Submit,
            KeyCode::Char('s') if ctrl => return ModalAction::Submit,
            KeyCode::Tab | KeyCode::Down => self.field = (self.field + 1) % self.field_count(),
            KeyCode::BackTab | KeyCode::Up => {
                self.field = (self.field + self.field_count() - 1) % self.field_count();
            }
            _ => self.edit(code, ctrl),
        }
        ModalAction::None
    }

    /// Apply an editing key to the focused field.
    fn edit(&mut self, code: KeyCode, ctrl: bool) {
        let kind = self.focused_input().map(|i| i.kind.clone());
        let value = match self.field {
            0 => &mut self.git_ref,
            i => &mut self.values[i - 1],
        };
        match kind {
            Some(InputKind::Boolean) => {
                if matches!(
                    code,
                    KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right | KeyCode::Char('h' | 'l')
                ) {
                    *value = (value != "true").to_string();
                }
            }
            Some(InputKind::Choice(options)) if !options.is_empty() => {
                let n = options.len();
                let i = options.iter().position(|o| o == value).unwrap_or(0);
                let next = match code {
                    KeyCode::Right | KeyCode::Char(' ' | 'l') => (i + 1) % n,
                    KeyCode::Left | KeyCode::Char('h') => (i + n - 1) % n,
                    _ => return,
                };
                *value = options[next].clone();
            }
            _ => match code {
                KeyCode::Backspace => {
                    value.pop();
                }
                KeyCode::Char('u') if ctrl => value.clear(),
                KeyCode::Char(c) if !ctrl => value.push(c),
                _ => {}
            },
        }
    }

    /// The inputs to send, or why the form can't be sent. Empty optional
    /// inputs are left out so the workflow's own default applies.
    fn validate(&self) -> Result<Vec<(String, String)>, String> {
        if !gh::valid_ref(self.git_ref.trim()) {
            return Err("enter a valid branch or tag to run on".to_string());
        }
        let mut out = Vec::new();
        for (input, value) in self.inputs.iter().zip(&self.values) {
            let value = value.trim();
            if value.is_empty() {
                if input.required {
                    return Err(format!("{} is required", input.name));
                }
                continue;
            }
            if input.kind == InputKind::Number && value.parse::<f64>().is_err() {
                return Err(format!("{} must be a number", input.name));
            }
            out.push((input.name.clone(), value.to_string()));
        }
        Ok(out)
    }
}

/// The actions screen.
pub struct ActionsView {
    /// Several repositories (show repository rows) vs one.
    pub multi: bool,
    pub accounts: Load<Vec<Account>>,
    pub repos: Vec<RepoNode>,
    pub selected: Option<RowKey>,
    /// Committed name filter over repositories and workflows.
    pub text_filter: Option<String>,
    /// Filter prompt being typed; filters live while typing.
    pub filter_input: Option<String>,
    pub modal: Option<RunModal>,
    /// Run a first `X` asked to cancel; a second `X` confirms.
    cancel_armed: Option<u64>,
    /// Rows area and scroll offset of the last draw, for mouse mapping.
    pub(crate) view_area: Rect,
    pub(crate) view_offset: usize,
    tx: mpsc::Sender<ActionsEvent>,
    rx: mpsc::Receiver<ActionsEvent>,
}

impl ActionsView {
    /// The tree for every repository of `workspace`; starts loading gh
    /// accounts, then each repository's workflows.
    pub fn new(workspace: &Workspace) -> Self {
        let repos: Vec<RepoNode> = workspace
            .repos()
            .into_iter()
            .map(|(name, repo, branch)| RepoNode {
                name: gh::sanitize_line(&name),
                remote: repo.remote_urls().iter().find_map(|u| gh::parse_remote(u)),
                root: repo.root().to_path_buf(),
                branch: gh::sanitize_line(&branch),
                data: Load::Loading,
                expanded: false,
                open_workflows: HashSet::new(),
                generation: 0,
                refreshing: false,
                next_poll: None,
                watch_until: None,
            })
            .collect();
        let (tx, rx) = mpsc::channel();
        let mut view = ActionsView {
            multi: workspace.is_multi(),
            accounts: Load::Loading,
            repos,
            selected: None,
            text_filter: None,
            filter_input: None,
            modal: None,
            cancel_armed: None,
            view_area: Rect::default(),
            view_offset: 0,
            tx,
            rx,
        };
        // A single repository is always open.
        if !view.multi {
            view.repos[0].expanded = true;
        }
        view.load_accounts();
        view
    }

    fn load_accounts(&mut self) {
        self.accounts = Load::Loading;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = gh::accounts().map_err(|e| e.to_string());
            let _ = tx.send(ActionsEvent::Accounts(result));
        });
    }

    /// (Re)load one repository's workflows and runs.
    fn load_repo(&mut self, idx: usize) {
        let accounts = match &self.accounts {
            Load::Ready(a) => a.clone(),
            _ => return,
        };
        let repo = &mut self.repos[idx];
        let Some((host, full_name)) = repo.remote.clone() else {
            return;
        };
        repo.generation += 1;
        repo.refreshing = false;
        repo.next_poll = None;
        // Keep showing old data while reloading.
        if !matches!(repo.data, Load::Ready(_)) {
            repo.data = Load::Loading;
        }
        let generation = repo.generation;
        let root = repo.root.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = fetch_repo(&accounts, &host, &full_name, &root);
            let _ = tx.send(ActionsEvent::Repo {
                idx,
                generation,
                result,
            });
        });
    }

    fn refresh_runs(&mut self, idx: usize) {
        let repo = &mut self.repos[idx];
        let generation = repo.generation;
        let Some(data) = repo.ready() else {
            return;
        };
        let account = data.account.clone();
        let full_name = data.full_name.clone();
        repo.refreshing = true;
        repo.next_poll = None;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = gh::workflow_runs(&account, &full_name).map_err(|e| e.to_string());
            let _ = tx.send(ActionsEvent::Runs {
                idx,
                generation,
                result,
            });
        });
    }

    /// Reload everything (accounts too, if they failed).
    pub fn refresh(&mut self) {
        if matches!(self.accounts, Load::Failed(_)) {
            self.load_accounts();
            return;
        }
        for idx in 0..self.repos.len() {
            self.load_repo(idx);
        }
    }

    /// Start run refreshes that are due. Call regularly while the screen
    /// is open.
    pub fn tick(&mut self) {
        let now = Instant::now();
        for idx in 0..self.repos.len() {
            let repo = &self.repos[idx];
            if !repo.refreshing && repo.next_poll.is_some_and(|t| t <= now) {
                self.refresh_runs(idx);
            }
        }
    }

    /// Apply finished background jobs; returns status messages to show.
    pub fn poll(&mut self) -> Vec<String> {
        let mut messages = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            match event {
                ActionsEvent::Accounts(result) => match result {
                    Ok(accounts) => {
                        self.accounts = Load::Ready(accounts);
                        for idx in 0..self.repos.len() {
                            self.load_repo(idx);
                        }
                    }
                    Err(e) => self.accounts = Load::Failed(e),
                },
                ActionsEvent::Repo {
                    idx,
                    generation,
                    result,
                } => {
                    let repo = &mut self.repos[idx];
                    if repo.generation != generation {
                        continue;
                    }
                    repo.data = match result {
                        Ok(data) => Load::Ready(data),
                        Err(e) => Load::Failed(e),
                    };
                    repo.schedule_poll();
                }
                ActionsEvent::Runs {
                    idx,
                    generation,
                    result,
                } => {
                    let repo = &mut self.repos[idx];
                    if repo.generation != generation {
                        continue;
                    }
                    repo.refreshing = false;
                    if let Some(data) = repo.ready_mut() {
                        match result {
                            Ok(runs) => {
                                data.runs = runs;
                                data.runs_error = None;
                            }
                            Err(e) => data.runs_error = Some(e),
                        }
                    }
                    repo.schedule_poll();
                }
                ActionsEvent::Dispatched {
                    idx,
                    workflow,
                    name,
                    git_ref,
                    result,
                } => match result {
                    Ok(()) => {
                        if self.modal.as_ref().is_some_and(|m| m.workflow == workflow) {
                            self.modal = None;
                        }
                        let repo = &mut self.repos[idx];
                        repo.open_workflows.insert(workflow);
                        repo.watch();
                        messages.push(format!("started {name} on {git_ref}"));
                    }
                    Err(e) => match self.modal.as_mut().filter(|m| m.workflow == workflow) {
                        Some(modal) => {
                            modal.submitting = false;
                            modal.error = Some(e);
                        }
                        None => messages.push(format!("could not start {name}: {e}")),
                    },
                },
                ActionsEvent::RunDone {
                    idx,
                    run_number,
                    action,
                    result,
                } => match result {
                    Ok(()) => {
                        self.repos[idx].watch();
                        messages.push(format!("{} requested for #{run_number}", action.label()));
                    }
                    Err(e) => {
                        messages.push(format!("{} of #{run_number} failed: {e}", action.label()))
                    }
                },
            }
        }
        self.fix_selection();
        messages
    }

    pub fn active_filter(&self) -> Option<&str> {
        self.filter_input
            .as_deref()
            .or(self.text_filter.as_deref())
            .filter(|q| !q.is_empty())
    }

    /// Visible rows, top to bottom.
    pub fn rows(&self) -> Vec<RowKey> {
        let filter = self.active_filter();
        let hit = |hay: &str| filter.is_none_or(|q| !search::match_ranges(hay, q).is_empty());
        let mut out = Vec::new();
        for (i, repo) in self.repos.iter().enumerate() {
            let full_name = repo.remote.as_ref().map_or("", |(_, n)| n.as_str());
            let repo_hit = hit(&repo.name) || hit(full_name);
            let workflows: Vec<&WorkflowNode> = repo
                .ready()
                .map(|d| {
                    d.workflows
                        .iter()
                        .filter(|w| repo_hit || hit(&w.workflow.name) || hit(&w.workflow.path))
                        .collect()
                })
                .unwrap_or_default();
            if !repo_hit && workflows.is_empty() {
                continue;
            }
            if self.multi {
                out.push(RowKey::Repo(i));
                // A filter opens every repository it matches into.
                if !repo.expanded && filter.is_none() {
                    continue;
                }
            }
            if repo.note().is_some() {
                out.push(RowKey::Note(i));
                continue;
            }
            let Some(data) = repo.ready() else {
                continue;
            };
            for w in workflows {
                let id = w.workflow.id;
                out.push(RowKey::Workflow(i, id));
                if repo.open_workflows.contains(&id) {
                    let before = out.len();
                    out.extend(
                        data.runs_of(id)
                            .take(RUNS_PER_WORKFLOW)
                            .map(|r| RowKey::Run(i, id, r.id)),
                    );
                    if out.len() == before {
                        out.push(RowKey::NoRuns(i, id));
                    }
                }
            }
        }
        out
    }

    pub fn selected_index(&self, rows: &[RowKey]) -> usize {
        self.selected
            .and_then(|key| rows.iter().position(|r| *r == key))
            .unwrap_or(0)
    }

    /// Keep the selection on a visible row: the same row, else its
    /// nearest visible ancestor, else the first row.
    fn fix_selection(&mut self) {
        let rows = self.rows();
        let mut key = self.selected;
        while let Some(k) = key {
            if rows.contains(&k) {
                self.selected = Some(k);
                return;
            }
            key = k.parent();
        }
        self.selected = rows.first().copied();
    }

    fn move_selection(&mut self, delta: isize) {
        let rows = self.rows();
        if rows.is_empty() {
            return;
        }
        let idx = self.selected_index(&rows) as isize + delta;
        self.selected = Some(rows[idx.clamp(0, rows.len() as isize - 1) as usize]);
    }

    fn select_edge(&mut self, last: bool) {
        let rows = self.rows();
        self.selected = if last { rows.last() } else { rows.first() }.copied();
    }

    fn repo_data(&self, idx: usize) -> Option<&RepoData> {
        self.repos.get(idx).and_then(RepoNode::ready)
    }

    /// The run a run action applies to: the selected run, or the latest
    /// run of the selected workflow.
    fn target_run(&self) -> Option<(usize, &WorkflowRun)> {
        let (repo, run) = match self.selected? {
            RowKey::Run(r, _, run) => (r, Some(run)),
            RowKey::Workflow(r, _) | RowKey::NoRuns(r, _) => (r, None),
            _ => return None,
        };
        let data = self.repo_data(repo)?;
        let run = match (run, self.selected?) {
            (Some(id), _) => data.runs.iter().find(|r| r.id == id),
            (None, RowKey::Workflow(_, w) | RowKey::NoRuns(_, w)) => data.latest_run(w),
            _ => None,
        }?;
        Some((repo, run))
    }

    /// Expand (`Some(true)`), collapse (`Some(false)`) or toggle (`None`)
    /// the selected repository or workflow.
    fn set_expanded(&mut self, open: Option<bool>) -> bool {
        match self.selected {
            Some(RowKey::Repo(r)) => {
                let repo = &mut self.repos[r];
                let open = open.unwrap_or(!repo.expanded);
                let changed = repo.expanded != open;
                repo.expanded = open;
                changed
            }
            Some(RowKey::Workflow(r, w)) => {
                let set = &mut self.repos[r].open_workflows;
                let open = open.unwrap_or(!set.contains(&w));
                if open {
                    set.insert(w)
                } else {
                    set.remove(&w)
                }
            }
            _ => false,
        }
    }

    /// Expand or collapse everything: repositories when there are
    /// several, else the run lists of every workflow.
    fn toggle_all(&mut self) {
        if self.multi {
            let open = self.repos.iter().any(|r| !r.expanded);
            for repo in &mut self.repos {
                repo.expanded = open;
            }
        } else {
            let repo = &mut self.repos[0];
            let ids: Vec<u64> = repo
                .ready()
                .map(|d| d.workflows.iter().map(|w| w.workflow.id).collect())
                .unwrap_or_default();
            if ids.iter().all(|id| repo.open_workflows.contains(id)) {
                repo.open_workflows.clear();
            } else {
                repo.open_workflows.extend(ids);
            }
        }
    }

    /// Open the run modal for the selected workflow, or say why it can't
    /// be run.
    fn open_modal(&mut self, repo: usize, workflow: u64) -> Option<String> {
        let node = &self.repos[repo];
        let data = node.ready()?;
        let w = data.workflow(workflow)?;
        if !w.workflow.is_active() {
            return Some(format!("{} is disabled on GitHub", w.workflow.name));
        }
        let inputs = match &w.dispatch {
            Dispatch::Manual(inputs) => inputs.clone(),
            Dispatch::NotManual => {
                return Some(format!(
                    "{} can't be run by hand (no workflow_dispatch trigger)",
                    w.workflow.name
                ))
            }
            Dispatch::Unknown(e) => return Some(format!("can't read {}: {e}", w.workflow.path)),
        };
        // Run on the checked-out branch unless HEAD is detached.
        let git_ref = if gh::valid_ref(&node.branch) && node.branch != "HEAD" {
            node.branch.clone()
        } else {
            data.default_branch.clone()
        };
        self.modal = Some(RunModal {
            repo,
            workflow,
            workflow_name: w.workflow.name.clone(),
            full_name: data.full_name.clone(),
            account: data.account.label(),
            can_push: data.can_push,
            git_ref,
            values: inputs.iter().map(|i| i.default.clone()).collect(),
            inputs,
            field: 0,
            submitting: false,
            error: None,
        });
        None
    }

    fn submit_modal(&mut self) {
        let Some(modal) = self.modal.as_mut() else {
            return;
        };
        if modal.submitting {
            return;
        }
        let inputs = match modal.validate() {
            Ok(inputs) => inputs,
            Err(e) => {
                modal.error = Some(e);
                return;
            }
        };
        let Some(data) = self.repos.get(modal.repo).and_then(RepoNode::ready) else {
            return;
        };
        modal.submitting = true;
        modal.error = None;
        let account = data.account.clone();
        let full_name = data.full_name.clone();
        let (idx, workflow, name) = (modal.repo, modal.workflow, modal.workflow_name.clone());
        let git_ref = modal.git_ref.trim().to_string();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = gh::dispatch_workflow(&account, &full_name, workflow, &git_ref, &inputs)
                .map_err(|e| e.to_string());
            let _ = tx.send(ActionsEvent::Dispatched {
                idx,
                workflow,
                name,
                git_ref,
                result,
            });
        });
    }

    fn start_run_action(&mut self, action: RunAction) -> Option<String> {
        let Some((idx, run)) = self.target_run() else {
            return Some("no run selected".to_string());
        };
        match action {
            RunAction::Cancel if !run.is_active() => {
                return Some(format!("#{} is not running", run.run_number));
            }
            RunAction::Rerun | RunAction::RerunFailed if run.is_active() => {
                return Some(format!("#{} is still running", run.run_number));
            }
            _ => {}
        }
        let (run_id, run_number) = (run.id, run.run_number);
        let data = self.repo_data(idx)?;
        let account = data.account.clone();
        let full_name = data.full_name.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result =
                gh::run_action(&account, &full_name, run_id, action).map_err(|e| e.to_string());
            let _ = tx.send(ActionsEvent::RunDone {
                idx,
                run_number,
                action,
                result,
            });
        });
        Some(format!("requesting {} of #{run_number}…", action.label()))
    }

    fn open_in_browser(&self) -> Option<String> {
        let (repo, workflow, run) = match self.selected? {
            RowKey::Workflow(r, w) | RowKey::NoRuns(r, w) => (r, w, None),
            RowKey::Run(r, w, run) => (r, w, Some(run)),
            _ => return Some("select a workflow or run".to_string()),
        };
        let data = self.repo_data(repo)?;
        Some(
            match gh::open_actions_page(&data.account, &data.full_name, workflow, run) {
                Ok(()) => "opened in browser".to_string(),
                Err(e) => format!("error: {e}"),
            },
        )
    }

    pub fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> KeyOutcome {
        let armed = self.cancel_armed.take();
        if let Some(modal) = self.modal.as_mut() {
            match modal.key(code, modifiers) {
                ModalAction::Cancel => self.modal = None,
                ModalAction::Submit => self.submit_modal(),
                ModalAction::None => {}
            }
            return KeyOutcome::Done(None);
        }
        if let Some(input) = self.filter_input.as_mut() {
            match code {
                KeyCode::Esc => {
                    self.filter_input = None;
                    self.text_filter = None;
                }
                KeyCode::Enter => {
                    let query = self.filter_input.take().unwrap_or_default();
                    self.text_filter = (!query.is_empty()).then_some(query);
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => input.push(c),
                _ => {}
            }
            self.fix_selection();
            return KeyOutcome::Done(None);
        }

        let page = (self.view_area.height as isize).max(1);
        let mut status = None;
        match code {
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(page),
            KeyCode::PageUp => self.move_selection(-page),
            KeyCode::Char('g') | KeyCode::Home => self.select_edge(false),
            KeyCode::Char('G') | KeyCode::End => self.select_edge(true),
            KeyCode::Char('l') | KeyCode::Right => {
                self.set_expanded(Some(true));
            }
            KeyCode::Char(' ') => {
                self.set_expanded(None);
            }
            KeyCode::Char('h') | KeyCode::Left => {
                if !self.set_expanded(Some(false)) {
                    // Already collapsed: go up a level instead.
                    let parent = self.selected.and_then(RowKey::parent);
                    if parent.is_some_and(|p| self.rows().contains(&p)) {
                        self.selected = parent;
                    }
                }
            }
            KeyCode::Char('e') => self.toggle_all(),
            KeyCode::Enter => match self.selected {
                Some(RowKey::Repo(_)) => {
                    self.set_expanded(None);
                }
                Some(RowKey::Workflow(r, w)) => status = self.open_modal(r, w),
                Some(RowKey::Run(..)) => status = self.open_in_browser(),
                _ => {}
            },
            KeyCode::Char('R') => status = self.start_run_action(RunAction::Rerun),
            KeyCode::Char('F') => status = self.start_run_action(RunAction::RerunFailed),
            KeyCode::Char('X') => {
                let target = self
                    .target_run()
                    .map(|(_, r)| (r.id, r.run_number, r.is_active()));
                status = match target {
                    Some((id, _, true)) if armed == Some(id) => {
                        self.start_run_action(RunAction::Cancel)
                    }
                    Some((id, number, true)) => {
                        self.cancel_armed = Some(id);
                        Some(format!("press X again to cancel run #{number}"))
                    }
                    Some((_, number, false)) => Some(format!("#{number} is not running")),
                    None => Some("no run selected".to_string()),
                };
            }
            KeyCode::Char('o') => status = self.open_in_browser(),
            KeyCode::Char('r') => {
                self.refresh();
                status = Some("refreshing…".to_string());
            }
            KeyCode::Char('/') => {
                self.filter_input = Some(self.text_filter.clone().unwrap_or_default());
            }
            KeyCode::Esc => {
                if self.text_filter.take().is_none() {
                    return KeyOutcome::Back;
                }
            }
            KeyCode::Char('q') => return KeyOutcome::Quit,
            _ => {}
        }
        self.fix_selection();
        KeyOutcome::Done(status)
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.modal.is_some() {
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollDown => self.move_selection(3),
            MouseEventKind::ScrollUp => self.move_selection(-3),
            MouseEventKind::Down(MouseButton::Left) => {
                let area = self.view_area;
                if area.contains(Position::new(mouse.column, mouse.row)) {
                    let idx = self.view_offset + (mouse.row - area.y) as usize;
                    if let Some(key) = self.rows().get(idx) {
                        self.selected = Some(*key);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Everything the tree shows for one repository: pick the account, list
/// workflows and recent runs, and read each workflow's dispatch inputs.
fn fetch_repo(
    accounts: &[Account],
    host: &str,
    full_name: &str,
    root: &Path,
) -> Result<RepoData, String> {
    let host = resolve_host(accounts, host)
        .ok_or_else(|| format!("no gh account for {host} (run `gh auth login`)"))?;
    let (account, access) = pick_account(accounts, &host, full_name)?;
    let workflows = gh::workflows(&account, full_name).map_err(|e| e.to_string())?;
    let (runs, runs_error) = match gh::workflow_runs(&account, full_name) {
        Ok(runs) => (runs, None),
        Err(e) => (Vec::new(), Some(e.to_string())),
    };
    let workflows = workflows
        .into_iter()
        .map(|workflow| WorkflowNode {
            dispatch: read_dispatch(&account, full_name, root, &workflow),
            workflow,
        })
        .collect();
    Ok(RepoData {
        account,
        full_name: full_name.to_string(),
        can_push: access.can_push,
        default_branch: access.default_branch,
        workflows,
        runs,
        runs_error,
    })
}

/// The account host a remote host stands for: itself, or the real
/// hostname of an SSH alias (`git@work-gh:o/r` → `github.com`). GitHub's
/// SSH-over-443 endpoint `ssh.github.com` counts as `github.com`.
fn resolve_host(accounts: &[Account], host: &str) -> Option<String> {
    let known = |h: &str| accounts.iter().any(|a| a.host == h);
    let real = |h: &str| {
        [Some(h), h.strip_prefix("ssh.")]
            .into_iter()
            .flatten()
            .find(|h| known(h))
            .map(str::to_string)
    };
    real(host).or_else(|| real(&gh::ssh_hostname(host)?))
}

/// The first account on `host` that may push to the repository, else the
/// first that can see it. The account named like the owner goes first.
fn pick_account(
    accounts: &[Account],
    host: &str,
    full_name: &str,
) -> Result<(Account, gh::RepoAccess), String> {
    let owner = full_name.split('/').next().unwrap_or_default();
    let mut candidates: Vec<&Account> = accounts.iter().filter(|a| a.host == host).collect();
    candidates.sort_by_key(|a| !a.login.eq_ignore_ascii_case(owner));
    let logins: Vec<String> = candidates.iter().map(|a| a.login.clone()).collect();
    let mut readable = None;
    let mut last_error = None;
    for account in candidates {
        match gh::repo_access(account, full_name) {
            Ok(access) if access.can_push => return Ok((account.clone(), access)),
            Ok(access) => {
                readable.get_or_insert((account.clone(), access));
            }
            Err(e) => last_error = Some(e.to_string()),
        }
    }
    readable.ok_or_else(|| match last_error {
        // GitHub answers 404 for private repositories the token can't see.
        Some(e) if e.contains("HTTP 404") => format!(
            "not found as {} — private? log in to an account with access (`gh auth login`)",
            logins.join(", ")
        ),
        Some(e) => e,
        None => format!("no gh account on {host}"),
    })
}

/// Dispatch inputs from the workflow file: the local checkout's copy when
/// it has one, else the default branch's on GitHub.
fn read_dispatch(account: &Account, full_name: &str, root: &Path, workflow: &Workflow) -> Dispatch {
    if !workflow.is_file() {
        return Dispatch::NotManual;
    }
    let local = root.join(&workflow.path);
    let text = match std::fs::File::open(&local) {
        Ok(file) => read_bounded(file),
        Err(_) => match gh::workflow_file(account, full_name, &workflow.path) {
            Ok(text) => Ok(text),
            // Still listed but deleted from the default branch.
            Err(e) if e.to_string().contains("HTTP 404") => return Dispatch::NotManual,
            Err(e) => Err(e.to_string()),
        },
    };
    match text.and_then(|t| workflow::dispatch_inputs(&t).map_err(|e| e.to_string())) {
        Ok(Some(inputs)) => Dispatch::Manual(inputs.into_iter().map(sanitize_input).collect()),
        Ok(None) => Dispatch::NotManual,
        Err(e) => Dispatch::Unknown(gh::sanitize_line(&e)),
    }
}

/// A local workflow file's text, refusing anything but a regular file of
/// at most [`workflow::MAX_WORKFLOW_BYTES`]. The read itself is bounded,
/// because a checked-out symlink could point at `/dev/zero` or a tty.
fn read_bounded(file: std::fs::File) -> Result<String, String> {
    use std::io::Read;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("workflow path is not a regular file".to_string());
    }
    let mut text = String::new();
    file.take(workflow::MAX_WORKFLOW_BYTES as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    if text.len() > workflow::MAX_WORKFLOW_BYTES {
        return Err("workflow file is too large".to_string());
    }
    Ok(text)
}

/// Make an input's text safe to draw (it comes from a repository file).
fn sanitize_input(mut input: DispatchInput) -> DispatchInput {
    input.name = gh::sanitize_line(&input.name);
    input.description = gh::sanitize_line(&input.description);
    input.default = gh::sanitize_line(&input.default);
    if let InputKind::Choice(options) = &mut input.kind {
        for option in options {
            *option = gh::sanitize_line(option);
        }
    }
    input
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(name: &str, kind: InputKind, required: bool, default: &str) -> DispatchInput {
        DispatchInput {
            name: name.into(),
            description: String::new(),
            required,
            default: default.into(),
            kind,
        }
    }

    fn modal(inputs: Vec<DispatchInput>) -> RunModal {
        RunModal {
            repo: 0,
            workflow: 1,
            workflow_name: "CI".into(),
            full_name: "o/r".into(),
            account: "me".into(),
            can_push: true,
            git_ref: "main".into(),
            values: inputs.iter().map(|i| i.default.clone()).collect(),
            inputs,
            field: 0,
            submitting: false,
            error: None,
        }
    }

    fn press(m: &mut RunModal, code: KeyCode) {
        m.key(code, KeyModifiers::NONE);
    }

    #[test]
    fn modal_edits_fields_by_kind() {
        let mut m = modal(vec![
            input("version", InputKind::String, true, ""),
            input("dry", InputKind::Boolean, false, "false"),
            input(
                "env",
                InputKind::Choice(vec!["a".into(), "b".into()]),
                false,
                "a",
            ),
        ]);
        press(&mut m, KeyCode::Backspace);
        press(&mut m, KeyCode::Char('x'));
        assert_eq!(m.git_ref, "maix");
        press(&mut m, KeyCode::Tab);
        for c in "1.2".chars() {
            press(&mut m, KeyCode::Char(c));
        }
        press(&mut m, KeyCode::Tab);
        press(&mut m, KeyCode::Char(' '));
        press(&mut m, KeyCode::Tab);
        press(&mut m, KeyCode::Right);
        assert_eq!(m.values, ["1.2", "true", "b"]);
        press(&mut m, KeyCode::Right);
        assert_eq!(m.values[2], "a");
        press(&mut m, KeyCode::Tab);
        assert_eq!(m.field, 0, "tab wraps to the ref field");
        press(&mut m, KeyCode::BackTab);
        assert_eq!(m.field, 3);
    }

    #[test]
    fn modal_validation() {
        let mut m = modal(vec![
            input("version", InputKind::String, true, ""),
            input("count", InputKind::Number, false, ""),
            input("note", InputKind::String, false, ""),
        ]);
        assert_eq!(m.validate().unwrap_err(), "version is required");
        m.values[0] = "1.0".into();
        m.values[1] = "lots".into();
        assert_eq!(m.validate().unwrap_err(), "count must be a number");
        m.values[1] = "3".into();
        assert_eq!(
            m.validate().unwrap(),
            vec![
                ("version".to_string(), "1.0".to_string()),
                ("count".to_string(), "3".to_string())
            ],
            "empty optional inputs are left out"
        );
        m.git_ref = "bad ref".into();
        assert!(m.validate().is_err());
    }

    fn data(workflows: &[u64], runs: &[(u64, u64)]) -> RepoData {
        RepoData {
            account: Account::for_test("me"),
            full_name: "o/r".into(),
            can_push: true,
            default_branch: "main".into(),
            workflows: workflows
                .iter()
                .map(|&id| WorkflowNode {
                    workflow: Workflow {
                        id,
                        name: format!("wf{id}"),
                        path: format!(".github/workflows/{id}.yml"),
                        state: "active".into(),
                    },
                    dispatch: Dispatch::NotManual,
                })
                .collect(),
            runs: runs
                .iter()
                .map(|&(id, workflow_id)| WorkflowRun {
                    id,
                    workflow_id,
                    status: "completed".into(),
                    ..Default::default()
                })
                .collect(),
            runs_error: None,
        }
    }

    fn view(multi: bool, repos: Vec<(&str, Load<RepoData>)>) -> ActionsView {
        let (tx, rx) = mpsc::channel();
        ActionsView {
            multi,
            accounts: Load::Ready(vec![]),
            repos: repos
                .into_iter()
                .map(|(name, data)| RepoNode {
                    name: name.into(),
                    root: PathBuf::new(),
                    branch: "main".into(),
                    remote: Some(("github.com".into(), format!("o/{name}"))),
                    data,
                    expanded: !multi,
                    open_workflows: HashSet::new(),
                    generation: 0,
                    refreshing: false,
                    next_poll: None,
                    watch_until: None,
                })
                .collect(),
            selected: None,
            text_filter: None,
            filter_input: None,
            modal: None,
            cancel_armed: None,
            view_area: Rect::default(),
            view_offset: 0,
            tx,
            rx,
        }
    }

    #[test]
    fn single_repo_lists_workflows_at_top_level() {
        let mut v = view(
            false,
            vec![("r", Load::Ready(data(&[1, 2], &[(10, 1), (11, 1)])))],
        );
        assert_eq!(
            v.rows(),
            vec![RowKey::Workflow(0, 1), RowKey::Workflow(0, 2)]
        );
        v.repos[0].open_workflows.extend([1, 2]);
        assert_eq!(
            v.rows(),
            vec![
                RowKey::Workflow(0, 1),
                RowKey::Run(0, 1, 10),
                RowKey::Run(0, 1, 11),
                RowKey::Workflow(0, 2),
                RowKey::NoRuns(0, 2),
            ]
        );
    }

    #[test]
    fn multi_repo_tree_expands_and_filters() {
        let mut v = view(
            true,
            vec![
                ("api", Load::Ready(data(&[1], &[]))),
                ("web", Load::Loading),
                ("lib", Load::Ready(data(&[], &[]))),
            ],
        );
        assert_eq!(
            v.rows(),
            vec![RowKey::Repo(0), RowKey::Repo(1), RowKey::Repo(2)]
        );
        v.selected = Some(RowKey::Repo(0));
        v.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        v.selected = Some(RowKey::Repo(1));
        v.handle_key(KeyCode::Char('l'), KeyModifiers::NONE);
        assert_eq!(
            v.rows(),
            vec![
                RowKey::Repo(0),
                RowKey::Workflow(0, 1),
                RowKey::Repo(1),
                RowKey::Note(1),
                RowKey::Repo(2),
            ]
        );
        // h on a collapsed child goes to its parent, then collapses it.
        v.selected = Some(RowKey::Workflow(0, 1));
        v.handle_key(KeyCode::Char('h'), KeyModifiers::NONE);
        assert_eq!(v.selected, Some(RowKey::Repo(0)));
        v.handle_key(KeyCode::Char('h'), KeyModifiers::NONE);
        assert!(!v.repos[0].expanded);

        v.text_filter = Some("wf1".into());
        assert_eq!(v.rows(), vec![RowKey::Repo(0), RowKey::Workflow(0, 1)]);
        v.text_filter = Some("lib".into());
        assert_eq!(v.rows(), vec![RowKey::Repo(2), RowKey::Note(2)]);
    }

    #[test]
    fn selection_falls_back_to_parent() {
        let mut v = view(false, vec![("r", Load::Ready(data(&[1], &[(10, 1)])))]);
        v.repos[0].open_workflows.insert(1);
        v.selected = Some(RowKey::Run(0, 1, 10));
        v.repos[0].open_workflows.clear();
        v.fix_selection();
        assert_eq!(v.selected, Some(RowKey::Workflow(0, 1)));
    }

    #[test]
    fn cancel_needs_a_second_press() {
        let mut d = data(&[1], &[(10, 1)]);
        d.runs[0].status = "in_progress".into();
        d.runs[0].run_number = 7;
        let mut v = view(false, vec![("r", Load::Ready(d))]);
        v.selected = Some(RowKey::Workflow(0, 1));
        let KeyOutcome::Done(Some(msg)) = v.handle_key(KeyCode::Char('X'), KeyModifiers::NONE)
        else {
            panic!("expected a status");
        };
        assert_eq!(msg, "press X again to cancel run #7");
        assert_eq!(v.cancel_armed, Some(10));
        // Any other key disarms.
        v.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(v.cancel_armed, None);
    }

    #[test]
    fn poll_backs_off_when_idle() {
        let mut v = view(false, vec![("r", Load::Ready(data(&[1], &[(10, 1)])))]);
        v.repos[0].schedule_poll();
        assert!(v.repos[0].next_poll.is_none());
        v.repos[0].watch();
        assert!(v.repos[0].next_poll.is_some());
        if let Load::Ready(d) = &mut v.repos[0].data {
            d.runs[0].status = "queued".into();
        }
        v.repos[0].watch_until = None;
        v.repos[0].schedule_poll();
        assert!(v.repos[0].next_poll.is_some());
    }
}
