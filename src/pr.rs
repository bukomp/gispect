//! Pull request view state: the PR list (tabs, account/state/draft/text
//! filters, the local-repository scope, a per-query result cache and the
//! PRs marked for a dash list), the per-PR review session
//! (fetched files, cached contents, draft inline comments), the comment
//! and review editors, the background preloader that fetches the PRs of
//! the open list tab ahead of time, and the background `gh` jobs feeding
//! them through one event channel. Rendering lives in `pr_ui.rs`, key handling in
//! `app.rs`.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::gh::{self, Account, DraftComment, PrDetails, PrSummary, ReviewEvent, Side};
use crate::git::GitRepo;
use crate::search;
use crate::types::{FileEntry, FileStatus};
use crate::workspace::Workspace;

/// Which search a list tab runs, relative to each account (`@me`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrTab {
    ReviewRequested,
    Authored,
    Assigned,
    Involved,
}

impl PrTab {
    pub const ALL: [PrTab; 4] = [
        PrTab::ReviewRequested,
        PrTab::Authored,
        PrTab::Assigned,
        PrTab::Involved,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PrTab::ReviewRequested => "Review requested",
            PrTab::Authored => "Authored",
            PrTab::Assigned => "Assigned",
            PrTab::Involved => "Involved",
        }
    }

    fn qualifier(self) -> &'static str {
        match self {
            PrTab::ReviewRequested => "review-requested:@me",
            PrTab::Authored => "author:@me",
            PrTab::Assigned => "assignee:@me",
            PrTab::Involved => "involves:@me",
        }
    }

    fn index(self) -> usize {
        PrTab::ALL.iter().position(|&t| t == self).unwrap_or(0)
    }

    pub fn cycle(self, forward: bool) -> PrTab {
        let n = PrTab::ALL.len();
        let i = self.index();
        PrTab::ALL[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }]
    }
}

/// PR state filter, applied server-side in the search query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StateFilter {
    Open,
    Closed,
    Merged,
    All,
}

impl StateFilter {
    pub fn label(self) -> &'static str {
        match self {
            StateFilter::Open => "open",
            StateFilter::Closed => "closed",
            StateFilter::Merged => "merged",
            StateFilter::All => "all",
        }
    }

    fn qualifier(self) -> &'static str {
        match self {
            StateFilter::Open => "is:open",
            StateFilter::Closed => "is:closed is:unmerged",
            StateFilter::Merged => "is:merged",
            StateFilter::All => "",
        }
    }

    pub fn next(self) -> StateFilter {
        match self {
            StateFilter::Open => StateFilter::Closed,
            StateFilter::Closed => StateFilter::Merged,
            StateFilter::Merged => StateFilter::All,
            StateFilter::All => StateFilter::Open,
        }
    }
}

/// GitHub search string for one tab and state.
pub fn search_query(tab: PrTab, state: StateFilter) -> String {
    let mut q = format!("is:pr {} archived:false", tab.qualifier());
    if !state.qualifier().is_empty() {
        q.push(' ');
        q.push_str(state.qualifier());
    }
    q.push_str(" sort:updated-desc");
    q
}

/// Repositories per search; a scope with more is split over several.
const REPOS_PER_QUERY: usize = 20;

/// `base` limited to `repos` (`owner/name`), one search string per chunk
/// of [`REPOS_PER_QUERY`] repositories.
pub fn scoped_queries(base: &str, repos: &[String]) -> Vec<String> {
    repos
        .chunks(REPOS_PER_QUERY)
        .map(|chunk| {
            let mut q = base.to_string();
            for repo in chunk {
                q.push_str(" repo:");
                q.push_str(repo);
            }
            q
        })
        .collect()
}

/// One search, per account, and whether it is limited to the local
/// repositories: results are cached under this key.
pub type QueryKey = (usize, PrTab, StateFilter, bool);

/// A value fetched in the background.
#[derive(Debug, Clone)]
pub enum Load<T> {
    Loading,
    Ready(T),
    Failed(String),
}

/// Results of background `gh` jobs, drained by the UI thread.
pub enum PrEvent {
    Accounts {
        result: Result<Vec<Account>, String>,
        /// See [`PrList::scope`].
        scope: Option<Vec<(String, String)>>,
    },
    List {
        key: QueryKey,
        result: Result<Vec<PrSummary>, String>,
    },
    Session {
        generation: u64,
        result: Result<PrDetails, String>,
    },
    File {
        generation: u64,
        path: String,
        result: Result<(String, String), String>,
    },
    Submitted {
        generation: u64,
        event: ReviewEvent,
        result: Result<(), String>,
    },
    PreloadDetails {
        key: PrKey,
        updated_at: String,
        result: Result<PrDetails, String>,
    },
    PreloadFile {
        key: PrKey,
        head_sha: String,
        path: String,
        result: Result<(String, String), String>,
    },
}

/// The PR list screen.
pub struct PrList {
    pub accounts: Load<Vec<Account>>,
    pub tab: PrTab,
    pub state: StateFilter,
    /// Account index to show; `None` shows every account.
    pub account_filter: Option<usize>,
    /// GitHub repositories (account host, `owner/name`) of the local
    /// workspace; `None` outside one or when none of its remotes is on a
    /// logged-in host.
    pub scope: Option<Vec<(String, String)>>,
    /// Show only PRs of `scope`'s repositories (when there is a scope).
    pub scoped: bool,
    pub hide_drafts: bool,
    /// Committed text filter over title, repo, author, branch and number.
    pub text_filter: Option<String>,
    /// Filter prompt being typed, if any; filters live while typing.
    pub filter_input: Option<String>,
    pub results: HashMap<QueryKey, Load<Vec<PrSummary>>>,
    pub selected: usize,
    /// PRs marked with Space, in marking order; one entry per URL even
    /// when several accounts list the PR.
    pub marked: Vec<PrSummary>,
    /// Scroll of the selected PR's info popup; `Some` while it is open.
    pub info_scroll: Option<u16>,
    /// The dash list popup, while it is open.
    pub dash_list: Option<DashList>,
    /// Rows area and scroll offset of the last draw, for mouse mapping.
    pub(crate) view_area: ratatui::layout::Rect,
    pub(crate) view_offset: usize,
}

impl PrList {
    pub fn new() -> Self {
        PrList {
            accounts: Load::Loading,
            tab: PrTab::ReviewRequested,
            state: StateFilter::Open,
            account_filter: None,
            scope: None,
            scoped: true,
            hide_drafts: false,
            text_filter: None,
            filter_input: None,
            results: HashMap::new(),
            selected: 0,
            marked: Vec::new(),
            info_scroll: None,
            dash_list: None,
            view_area: ratatui::layout::Rect::default(),
            view_offset: 0,
        }
    }

    pub fn account_list(&self) -> &[Account] {
        match &self.accounts {
            Load::Ready(accounts) => accounts,
            _ => &[],
        }
    }

    /// Account indices the current view covers.
    fn shown_accounts(&self) -> Vec<usize> {
        match self.account_filter {
            Some(i) => vec![i],
            None => (0..self.account_list().len()).collect(),
        }
    }

    /// The repositories searches are limited to, if any.
    pub fn active_scope(&self) -> Option<&[(String, String)]> {
        self.scope.as_deref().filter(|_| self.scoped)
    }

    /// Cache key of `tab`'s search for account `a` in the current view.
    fn key(&self, a: usize, tab: PrTab) -> QueryKey {
        (a, tab, self.state, self.active_scope().is_some())
    }

    /// Repositories (`owner/name`) on `account`'s host that its search is
    /// limited to; `None` when unscoped.
    pub fn scope_for(&self, account: &Account) -> Option<Vec<String>> {
        self.active_scope().map(|scope| {
            scope
                .iter()
                .filter(|(host, _)| *host == account.host)
                .map(|(_, repo)| repo.clone())
                .collect()
        })
    }

    /// The text filter in effect: live prompt text, else the committed one.
    pub fn active_text_filter(&self) -> Option<&str> {
        self.filter_input
            .as_deref()
            .or(self.text_filter.as_deref())
            .filter(|q| !q.is_empty())
    }

    /// Searches the current view needs that haven't been started yet.
    pub fn missing_queries(&self) -> Vec<QueryKey> {
        self.shown_accounts()
            .into_iter()
            .map(|a| self.key(a, self.tab))
            .filter(|k| !self.results.contains_key(k))
            .collect()
    }

    /// Rows for `tab` under the current account/state/draft/text filters,
    /// newest update first. Each row pairs an account index with a PR.
    pub fn rows_for(&self, tab: PrTab) -> Vec<(usize, &PrSummary)> {
        let filter = self.active_text_filter();
        let mut rows: Vec<(usize, &PrSummary)> = self
            .shown_accounts()
            .into_iter()
            .filter_map(|a| match self.results.get(&self.key(a, tab)) {
                Some(Load::Ready(prs)) => Some(prs.iter().map(move |pr| (a, pr))),
                _ => None,
            })
            .flatten()
            .filter(|(_, pr)| !(self.hide_drafts && pr.is_draft))
            .filter(|(_, pr)| filter.is_none_or(|q| pr_matches(pr, q)))
            .collect();
        // ISO-8601 UTC timestamps sort lexicographically.
        rows.sort_by(|a, b| b.1.updated_at.cmp(&a.1.updated_at));
        rows
    }

    pub fn rows(&self) -> Vec<(usize, &PrSummary)> {
        self.rows_for(self.tab)
    }

    /// Whether any search for `tab` in the current view is in flight.
    pub fn loading(&self, tab: PrTab) -> bool {
        matches!(self.accounts, Load::Loading)
            || self.shown_accounts().into_iter().any(|a| {
                matches!(
                    self.results.get(&self.key(a, tab)),
                    Some(Load::Loading) | None
                )
            })
    }

    /// Failed searches in the current view, as (account label, error).
    pub fn errors(&self) -> Vec<(String, String)> {
        let accounts = self.account_list();
        self.shown_accounts()
            .into_iter()
            .filter_map(|a| match self.results.get(&self.key(a, self.tab)) {
                Some(Load::Failed(e)) => Some((
                    accounts.get(a).map(Account::label).unwrap_or_default(),
                    e.clone(),
                )),
                _ => None,
            })
            .collect()
    }

    pub fn clamp_selection(&mut self) {
        let len = self.rows().len();
        if self.selected >= len {
            self.selected = len.saturating_sub(1);
        }
    }

    /// The selected row as (account, PR), cloned out of the cache.
    pub fn selected_row(&self) -> Option<(Account, PrSummary)> {
        let (a, pr) = self.rows().get(self.selected).copied()?;
        Some((self.account_list().get(a)?.clone(), pr.clone()))
    }

    pub fn is_marked(&self, pr: &PrSummary) -> bool {
        self.marked.iter().any(|m| m.url == pr.url)
    }

    /// Mark or unmark the selected PR; returns whether it is now marked.
    pub fn toggle_mark(&mut self) -> Option<bool> {
        let (_, pr) = self.selected_row()?;
        match self.marked.iter().position(|m| m.url == pr.url) {
            Some(i) => {
                self.marked.remove(i);
                Some(false)
            }
            None => {
                self.marked.push(pr);
                Some(true)
            }
        }
    }

    /// Dash list of the marked PRs, else of the selected one.
    pub fn dash_list_text(&self) -> Option<String> {
        let selected;
        let prs: Vec<&PrSummary> = if self.marked.is_empty() {
            selected = self.selected_row()?.1;
            vec![&selected]
        } else {
            self.marked.iter().collect()
        };
        Some(
            prs.into_iter()
                .map(dash_line)
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    /// Forget cached searches (for one account, or all) so they re-run.
    pub fn invalidate(&mut self, account: Option<usize>) {
        match account {
            Some(a) => self.results.retain(|k, _| k.0 != a),
            None => self.results.clear(),
        }
    }
}

/// The dash list popup: its text and what copying it did.
pub struct DashList {
    pub text: String,
    pub copied: Result<String, String>,
}

/// One dash list line: `- title — url`.
fn dash_line(pr: &PrSummary) -> String {
    format!("- {} — {}", pr.title, gh::sanitize_line(&pr.url))
}

/// Smart-case text match against a PR's title, repo, author, branch and
/// `#number`.
fn pr_matches(pr: &PrSummary, query: &str) -> bool {
    let number = format!("#{}", pr.number);
    [
        pr.title.as_str(),
        pr.repo.as_str(),
        pr.author.as_str(),
        pr.head_ref_name.as_str(),
        number.as_str(),
    ]
    .iter()
    .any(|hay| !search::match_ranges(hay, query).is_empty())
}

/// Largest file version the review view will diff.
const MAX_FILE_BYTES: usize = 5 << 20;

/// A file content request for the session's fetch worker.
struct FileJob {
    generation: u64,
    entry: FileEntry,
}

/// Reviewing one PR: its files feed the normal diff view.
pub struct PrSession {
    pub generation: u64,
    pub account: Account,
    pub summary: PrSummary,
    pub details: Load<PrDetails>,
    /// (old, new) contents per path, fetched on demand.
    pub contents: HashMap<String, (String, String)>,
    /// Inline comments queued for the next review submission.
    pub drafts: Vec<DraftComment>,
    file_jobs: Option<mpsc::Sender<FileJob>>,
}

impl PrSession {
    pub fn new(generation: u64, account: Account, summary: PrSummary) -> Self {
        PrSession {
            generation,
            account,
            summary,
            details: Load::Loading,
            contents: HashMap::new(),
            drafts: Vec::new(),
            file_jobs: None,
        }
    }

    pub fn details(&self) -> Option<&PrDetails> {
        match &self.details {
            Load::Ready(d) => Some(d),
            _ => None,
        }
    }

    /// Store freshly loaded details and start a fetch worker for them.
    /// When the head moved (a refresh after new pushes), drafts on lines
    /// no longer in the diff are dropped; returns how many.
    pub fn set_details(
        &mut self,
        details: PrDetails,
        tx: mpsc::Sender<PrEvent>,
        local: Option<GitRepo>,
    ) -> usize {
        let before = self.drafts.len();
        if self
            .details()
            .is_some_and(|d| d.head_sha != details.head_sha)
        {
            self.drafts.retain(|draft| {
                details.files.iter().any(|f| {
                    f.entry.path == draft.path && f.commentable.contains(&(draft.side, draft.line))
                })
            });
        }
        self.contents.clear();
        self.file_jobs = Some(spawn_file_worker(
            tx,
            self.account.clone(),
            self.summary.repo.clone(),
            details.base_sha.clone(),
            details.head_sha.clone(),
            local,
        ));
        self.details = Load::Ready(details);
        before - self.drafts.len()
    }

    /// Ask the fetch worker for both versions of `entry`.
    pub fn request_file(&self, entry: &FileEntry) {
        if let Some(jobs) = &self.file_jobs {
            let _ = jobs.send(FileJob {
                generation: self.generation,
                entry: entry.clone(),
            });
        }
    }

    pub fn file(&self, path: &str) -> Option<&gh::PrFile> {
        self.details()?.files.iter().find(|f| f.entry.path == path)
    }

    pub fn draft_at(&self, path: &str, side: Side, line: usize) -> Option<usize> {
        self.drafts
            .iter()
            .position(|d| d.path == path && d.side == side && d.line == line)
    }

    /// Whether the PR was opened by the reviewing account; GitHub refuses
    /// approvals and change requests on your own PR.
    pub fn is_own(&self) -> bool {
        self.summary
            .author
            .eq_ignore_ascii_case(&self.account.login)
    }
}

/// Multi-line text being typed into a modal. Edits happen at the end of
/// the buffer.
#[derive(Debug, Clone, Default)]
pub struct TextBuffer {
    pub text: String,
}

/// Inline comment editor, anchored to one line of one side.
pub struct CommentEditor {
    pub path: String,
    pub side: Side,
    pub line: usize,
    pub buffer: TextBuffer,
}

/// Review submission modal: verdict plus summary body.
pub struct ReviewModal {
    pub event: ReviewEvent,
    pub buffer: TextBuffer,
    /// Approve / request changes are offered (false on your own PR).
    pub can_approve: bool,
    pub submitting: bool,
    pub error: Option<String>,
}

impl ReviewModal {
    pub fn new(can_approve: bool) -> Self {
        ReviewModal {
            event: ReviewEvent::Comment,
            buffer: TextBuffer::default(),
            can_approve,
            submitting: false,
            error: None,
        }
    }

    pub fn cycle_event(&mut self, forward: bool) {
        let options: Vec<ReviewEvent> = ReviewEvent::ALL
            .into_iter()
            .filter(|&e| self.can_approve || e == ReviewEvent::Comment)
            .collect();
        let n = options.len();
        let i = options.iter().position(|&e| e == self.event).unwrap_or(0);
        self.event = options[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }];
    }

    /// Why the review can't be sent as-is, if anything.
    pub fn validate(&self, drafts: usize) -> Option<&'static str> {
        let blank = self.buffer.text.trim().is_empty();
        match self.event {
            ReviewEvent::RequestChanges if blank => {
                Some("requesting changes needs a summary comment")
            }
            ReviewEvent::Comment if blank && drafts == 0 => {
                Some("write a summary or add inline comments first")
            }
            _ => None,
        }
    }
}

/// Load every gh account (with tokens) in the background, along with
/// the GitHub repositories of `workspace` the list is scoped to.
pub fn spawn_accounts(tx: mpsc::Sender<PrEvent>, workspace: Option<Workspace>) {
    std::thread::spawn(move || {
        let (result, remotes) = std::thread::scope(|s| {
            let remotes = s.spawn(|| workspace.as_ref().map(local_remotes));
            let result = gh::accounts().map_err(|e| e.to_string());
            (result, remotes.join().ok().flatten())
        });
        let scope = match (&result, remotes) {
            (Ok(accounts), Some(remotes)) => resolve_scope(accounts, remotes),
            _ => None,
        };
        let _ = tx.send(PrEvent::Accounts { result, scope });
    });
}

/// `(host, owner/name)` of every GitHub-style remote of the workspace's
/// repositories and those nested below it. All remotes count, so PRs to
/// a fork's upstream show too.
fn local_remotes(workspace: &Workspace) -> BTreeSet<(String, String)> {
    workspace
        .scope_repos()
        .iter()
        .flat_map(|repo| repo.remote_urls())
        .filter_map(|url| gh::parse_remote(&url))
        .collect()
}

/// `remotes` with hosts mapped to account hosts (resolving SSH aliases);
/// remotes on no logged-in host are dropped. `None` when none remain.
fn resolve_scope(
    accounts: &[Account],
    remotes: BTreeSet<(String, String)>,
) -> Option<Vec<(String, String)>> {
    let mut hosts: HashMap<String, Option<String>> = HashMap::new();
    let scope: BTreeSet<(String, String)> = remotes
        .into_iter()
        .filter_map(|(host, repo)| {
            let resolved = hosts
                .entry(host)
                .or_insert_with_key(|h| gh::resolve_host(accounts, h))
                .clone()?;
            Some((resolved, repo))
        })
        .collect();
    (!scope.is_empty()).then(|| scope.into_iter().collect())
}

/// Run one list search in the background, limited to `repos` when
/// given. An empty `repos` (nothing on the account's host) finds nothing
/// without asking GitHub.
pub fn spawn_search(
    tx: mpsc::Sender<PrEvent>,
    key: QueryKey,
    account: Account,
    repos: Option<Vec<String>>,
) {
    std::thread::spawn(move || {
        let base = search_query(key.1, key.2);
        let queries = match &repos {
            Some(repos) => scoped_queries(&base, repos),
            None => vec![base],
        };
        let mut prs = Vec::new();
        let mut error = None;
        for query in &queries {
            match gh::search_prs(&account, query) {
                Ok(found) => prs.extend(found),
                Err(e) => {
                    error = Some(e.to_string());
                    break;
                }
            }
        }
        let result = match error {
            Some(e) => Err(e),
            None => Ok(prs),
        };
        let _ = tx.send(PrEvent::List { key, result });
    });
}

/// Load a PR's details in the background.
pub fn spawn_session(
    tx: mpsc::Sender<PrEvent>,
    generation: u64,
    account: Account,
    repo: String,
    number: u64,
) {
    std::thread::spawn(move || {
        let result = gh::pr_details(&account, &repo, number).map_err(|e| e.to_string());
        let _ = tx.send(PrEvent::Session { generation, result });
    });
}

/// Submit a review of `session`'s PR, pinned to `commit_id`, with its
/// draft comments, in the background.
pub fn spawn_submit(
    tx: mpsc::Sender<PrEvent>,
    session: &PrSession,
    commit_id: String,
    event: ReviewEvent,
    body: String,
) {
    let generation = session.generation;
    let account = session.account.clone();
    let summary = session.summary.clone();
    let comments = session.drafts.clone();
    std::thread::spawn(move || {
        let result = gh::submit_review(
            &account,
            &summary.repo,
            summary.number,
            &commit_id,
            event,
            &body,
            &comments,
        )
        .map_err(|e| e.to_string());
        let _ = tx.send(PrEvent::Submitted {
            generation,
            event,
            result,
        });
    });
}

/// Spawn the per-session file fetch worker. Queued requests collapse to
/// the most recent one, so holding j/k doesn't queue a fetch for every
/// file skipped past. Commits present in the local repository are read
/// with `git show` instead of the API. Exits when the session drops.
fn spawn_file_worker(
    tx: mpsc::Sender<PrEvent>,
    account: Account,
    repo: String,
    base_sha: String,
    head_sha: String,
    local: Option<GitRepo>,
) -> mpsc::Sender<FileJob> {
    let (job_tx, job_rx) = mpsc::channel::<FileJob>();
    std::thread::spawn(move || {
        while let Ok(mut job) = job_rx.recv() {
            while let Ok(newer) = job_rx.try_recv() {
                job = newer;
            }
            let entry = &job.entry;
            let result = fetch_versions(&account, &repo, &base_sha, &head_sha, entry, &local);
            let event = PrEvent::File {
                generation: job.generation,
                path: entry.path.clone(),
                result,
            };
            if tx.send(event).is_err() {
                return;
            }
        }
    });
    job_tx
}

/// Both versions of `entry`, sanitized for display. Commits present in
/// the local repository are read with `git show` instead of the API.
fn fetch_versions(
    account: &Account,
    repo: &str,
    base_sha: &str,
    head_sha: &str,
    entry: &FileEntry,
    local: &Option<GitRepo>,
) -> Result<(String, String), String> {
    let fetch = |sha: &str, path: &str| -> Result<String, String> {
        if let Some(text) = local.as_ref().and_then(|r| r.show_at(sha, path)) {
            return Ok(text);
        }
        gh::file_content(account, repo, sha, path).map_err(|e| e.to_string())
    };
    let old_path = match &entry.status {
        FileStatus::Renamed { from } => from.as_str(),
        _ => entry.path.as_str(),
    };
    let old = match entry.status {
        FileStatus::Added => Ok(String::new()),
        _ => fetch(base_sha, old_path),
    };
    let new = match entry.status {
        FileStatus::Deleted => Ok(String::new()),
        _ => fetch(head_sha, &entry.path),
    };
    old.and_then(|o| new.map(|n| (o, n))).and_then(|(o, n)| {
        // Diffing happens on the UI thread; keep a huge file in an
        // untrusted PR from freezing it.
        if o.len().max(n.len()) > MAX_FILE_BYTES {
            return Err(format!(
                "file too large to display (over {} MB)",
                MAX_FILE_BYTES >> 20
            ));
        }
        Ok((gh::sanitize_text(&o), gh::sanitize_text(&n)))
    })
}

/// One PR as seen by one account: (host, login, PR url).
pub type PrKey = (String, String, String);

pub fn pr_key(account: &Account, pr: &PrSummary) -> PrKey {
    (account.host.clone(), account.login.clone(), pr.url.clone())
}

/// (old, new) contents per path.
pub type FileContents = HashMap<String, (String, String)>;

/// Background fetches running at once.
const PRELOAD_WORKERS: usize = 4;
/// Files preloaded per PR; the rest load when opened.
const PRELOAD_MAX_FILES: usize = 100;
/// Total file contents kept preloaded, in bytes.
const PRELOAD_BUDGET: usize = 256 << 20;

/// A PR fetched ahead of being opened.
pub struct Preloaded {
    /// The summary's `updatedAt` when fetched; a newer one makes it stale.
    updated_at: String,
    details: Load<PrDetails>,
    /// (old, new) contents per path; `Loading` while queued or in flight.
    files: HashMap<String, Load<(String, String)>>,
}

/// How far preloading of one PR has got, for the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreloadStatus {
    Loading,
    Ready,
    Failed,
}

/// A PR the open list view shows, to preload.
pub struct PreloadTarget {
    pub account: Account,
    pub summary: PrSummary,
    /// Local clone to read commits from, if any.
    pub local: Option<GitRepo>,
}

enum PreloadJob {
    Details {
        key: PrKey,
        updated_at: String,
        account: Account,
        repo: String,
        number: u64,
    },
    File {
        key: PrKey,
        account: Account,
        repo: String,
        base_sha: String,
        head_sha: String,
        entry: FileEntry,
        local: Option<GitRepo>,
    },
}

type JobQueue = Arc<(Mutex<VecDeque<PreloadJob>>, Condvar)>;

/// Fetches details and file contents of every PR in the open list view
/// on a small worker pool, so opening one shows it at once. The queue is
/// rebuilt in list order whenever the view changes; jobs of PRs that left
/// the view are dropped before they start.
pub struct Preloader {
    cache: HashMap<PrKey, Preloaded>,
    queue: JobQueue,
    tx: mpsc::Sender<PrEvent>,
    /// Workers still to spawn; they start with the first job.
    idle_workers: usize,
    bytes: usize,
}

impl Preloader {
    pub fn new(tx: mpsc::Sender<PrEvent>) -> Self {
        Self::with_workers(tx, PRELOAD_WORKERS)
    }

    fn with_workers(tx: mpsc::Sender<PrEvent>, workers: usize) -> Self {
        Preloader {
            cache: HashMap::new(),
            queue: Arc::new((Mutex::new(VecDeque::new()), Condvar::new())),
            tx,
            idle_workers: workers,
            bytes: 0,
        }
    }

    /// Preload `targets` (in priority order) and nothing else: queued work
    /// for other PRs is dropped, missing work for these is queued.
    pub fn retarget(&mut self, targets: Vec<PreloadTarget>) {
        let shared = self.queue.clone();
        let (lock, cvar) = &*shared;
        let mut queue = lock.lock().unwrap_or_else(|e| e.into_inner());
        // Un-mark everything not yet started; the plan below re-queues
        // whatever is still wanted, in the new order.
        for job in queue.drain(..) {
            match job {
                PreloadJob::Details { key, .. } => {
                    self.cache.remove(&key);
                }
                PreloadJob::File { key, entry, .. } => {
                    if let Some(p) = self.cache.get_mut(&key) {
                        p.files.remove(&entry.path);
                    }
                }
            }
        }
        for target in targets {
            let key = pr_key(&target.account, &target.summary);
            if self
                .cache
                .get(&key)
                .is_some_and(|p| p.updated_at != target.summary.updated_at)
            {
                self.forget(&key);
            }
            let Some(preloaded) = self.cache.get_mut(&key) else {
                self.cache.insert(
                    key.clone(),
                    Preloaded {
                        updated_at: target.summary.updated_at.clone(),
                        details: Load::Loading,
                        files: HashMap::new(),
                    },
                );
                queue.push_back(PreloadJob::Details {
                    key,
                    updated_at: target.summary.updated_at,
                    account: target.account,
                    repo: target.summary.repo,
                    number: target.summary.number,
                });
                continue;
            };
            let Load::Ready(details) = &preloaded.details else {
                continue;
            };
            for file in details.files.iter().take(PRELOAD_MAX_FILES) {
                if self.bytes >= PRELOAD_BUDGET {
                    break;
                }
                if file.entry.binary || preloaded.files.contains_key(&file.entry.path) {
                    continue;
                }
                preloaded
                    .files
                    .insert(file.entry.path.clone(), Load::Loading);
                queue.push_back(PreloadJob::File {
                    key: key.clone(),
                    account: target.account.clone(),
                    repo: target.summary.repo.clone(),
                    base_sha: details.base_sha.clone(),
                    head_sha: details.head_sha.clone(),
                    entry: file.entry.clone(),
                    local: target.local.clone(),
                });
            }
        }
        if !queue.is_empty() {
            for _ in 0..std::mem::take(&mut self.idle_workers) {
                spawn_preload_worker(self.queue.clone(), self.tx.clone());
            }
            cvar.notify_all();
        }
    }

    fn forget(&mut self, key: &PrKey) {
        if let Some(p) = self.cache.remove(key) {
            self.bytes -= p.files.values().map(load_size).sum::<usize>();
        }
    }

    /// Store fetched details; false when they're stale or unasked for.
    pub fn apply_details(
        &mut self,
        key: &PrKey,
        updated_at: &str,
        result: Result<PrDetails, String>,
    ) -> bool {
        let Some(preloaded) = self
            .cache
            .get_mut(key)
            .filter(|p| p.updated_at == updated_at && matches!(p.details, Load::Loading))
        else {
            return false;
        };
        preloaded.details = match result {
            Ok(details) => Load::Ready(details),
            Err(e) => Load::Failed(e),
        };
        true
    }

    /// Store fetched contents of `path`; false when stale or unasked for.
    pub fn apply_file(
        &mut self,
        key: &PrKey,
        head_sha: &str,
        path: &str,
        result: Result<(String, String), String>,
    ) -> bool {
        let Some(preloaded) = self.cache.get_mut(key) else {
            return false;
        };
        let current = matches!(&preloaded.details, Load::Ready(d) if d.head_sha == head_sha);
        let Some(slot) = preloaded.files.get_mut(path).filter(|_| current) else {
            return false;
        };
        *slot = match result {
            Ok(versions) => Load::Ready(versions),
            Err(e) => Load::Failed(e),
        };
        self.bytes += load_size(slot);
        true
    }

    /// Preloaded details of `summary` (loading, failed or ready), if
    /// they are still current.
    pub fn details(&self, account: &Account, summary: &PrSummary) -> Option<&Load<PrDetails>> {
        self.cache
            .get(&pr_key(account, summary))
            .filter(|p| p.updated_at == summary.updated_at)
            .map(|p| &p.details)
    }

    /// Preloaded details and ready file contents of `summary`, if its
    /// details are in and still match the list.
    pub fn take(
        &self,
        account: &Account,
        summary: &PrSummary,
    ) -> Option<(PrDetails, FileContents)> {
        let preloaded = self
            .cache
            .get(&pr_key(account, summary))
            .filter(|p| p.updated_at == summary.updated_at)?;
        let Load::Ready(details) = &preloaded.details else {
            return None;
        };
        let contents = preloaded
            .files
            .iter()
            .filter_map(|(path, load)| match load {
                Load::Ready(v) => Some((path.clone(), v.clone())),
                _ => None,
            })
            .collect();
        Some((details.clone(), contents))
    }

    pub fn status(&self, account: &Account, summary: &PrSummary) -> Option<PreloadStatus> {
        let preloaded = self
            .cache
            .get(&pr_key(account, summary))
            .filter(|p| p.updated_at == summary.updated_at)?;
        Some(match &preloaded.details {
            Load::Loading => PreloadStatus::Loading,
            Load::Failed(_) => PreloadStatus::Failed,
            Load::Ready(_) if preloaded.files.values().any(|f| matches!(f, Load::Loading)) => {
                PreloadStatus::Loading
            }
            Load::Ready(_) => PreloadStatus::Ready,
        })
    }

    /// Jobs not yet picked up by a worker.
    #[cfg(test)]
    fn queued(&self) -> usize {
        self.queue.0.lock().unwrap().len()
    }
}

fn load_size(load: &Load<(String, String)>) -> usize {
    match load {
        Load::Ready((old, new)) => old.len() + new.len(),
        _ => 0,
    }
}

/// One preload worker: takes jobs off the shared queue for as long as
/// the app runs.
fn spawn_preload_worker(queue: JobQueue, tx: mpsc::Sender<PrEvent>) {
    std::thread::spawn(move || loop {
        let job = {
            let (lock, cvar) = &*queue;
            let mut jobs = lock.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(job) = jobs.pop_front() {
                    break job;
                }
                jobs = cvar.wait(jobs).unwrap_or_else(|e| e.into_inner());
            }
        };
        let event = match job {
            PreloadJob::Details {
                key,
                updated_at,
                account,
                repo,
                number,
            } => PrEvent::PreloadDetails {
                key,
                updated_at,
                result: gh::pr_details(&account, &repo, number).map_err(|e| e.to_string()),
            },
            PreloadJob::File {
                key,
                account,
                repo,
                base_sha,
                head_sha,
                entry,
                local,
            } => PrEvent::PreloadFile {
                result: fetch_versions(&account, &repo, &base_sha, &head_sha, &entry, &local),
                key,
                head_sha,
                path: entry.path,
            },
        };
        if tx.send(event).is_err() {
            return;
        }
    });
}

/// Compact age of an ISO-8601 UTC timestamp, e.g. `5m`, `3h`, `2d`, `4w`.
pub fn age(timestamp: &str) -> String {
    let Some(then) = parse_utc(timestamp) else {
        return String::new();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(then);
    format_age(now.saturating_sub(then))
}

fn format_age(secs: i64) -> String {
    let secs = secs.max(0);
    match secs {
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s if s < 86_400 * 14 => format!("{}d", s / 86_400),
        s if s < 86_400 * 365 => format!("{}w", s / (86_400 * 7)),
        s => format!("{}y", s / (86_400 * 365)),
    }
}

/// Unix seconds of `YYYY-MM-DDTHH:MM:SSZ`.
fn parse_utc(ts: &str) -> Option<i64> {
    let num = |range: std::ops::Range<usize>| -> Option<i64> { ts.get(range)?.parse().ok() };
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // Days from civil (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: u64, title: &str, updated: &str, draft: bool) -> PrSummary {
        PrSummary {
            number,
            title: title.into(),
            updated_at: updated.into(),
            is_draft: draft,
            repo: "o/r".into(),
            author: "alice".into(),
            ..PrSummary::default()
        }
    }

    fn list_with(prs: Vec<PrSummary>) -> PrList {
        let mut list = PrList::new();
        list.accounts = Load::Ready(Vec::new());
        list.account_filter = Some(0);
        list.results.insert(
            (0, PrTab::ReviewRequested, StateFilter::Open, false),
            Load::Ready(prs),
        );
        list
    }

    #[test]
    fn query_combines_tab_and_state() {
        assert_eq!(
            search_query(PrTab::ReviewRequested, StateFilter::Open),
            "is:pr review-requested:@me archived:false is:open sort:updated-desc"
        );
        assert_eq!(
            search_query(PrTab::Authored, StateFilter::All),
            "is:pr author:@me archived:false sort:updated-desc"
        );
    }

    #[test]
    fn scoped_queries_chunk_repos() {
        let repos: Vec<String> = (0..REPOS_PER_QUERY + 1)
            .map(|i| format!("o/r{i}"))
            .collect();
        let queries = scoped_queries("is:pr", &repos);
        assert_eq!(queries.len(), 2);
        assert!(queries[0].starts_with("is:pr repo:o/r0 repo:o/r1 "));
        assert_eq!(queries[1], format!("is:pr repo:o/r{REPOS_PER_QUERY}"));
        assert!(scoped_queries("is:pr", &[]).is_empty());
    }

    #[test]
    fn scope_limits_searches_per_host() {
        let mut list = PrList::new();
        let account = Account::for_test("me");
        list.scope = Some(vec![
            (account.host.clone(), "o/a".to_string()),
            ("ghe.corp".to_string(), "o/b".to_string()),
        ]);
        assert_eq!(list.scope_for(&account), Some(vec!["o/a".to_string()]));
        assert!(list.key(0, PrTab::Authored).3);
        list.scoped = false;
        assert_eq!(list.scope_for(&account), None);
        assert!(!list.key(0, PrTab::Authored).3);
    }

    #[test]
    fn rows_sort_newest_first_and_filter() {
        let mut list = list_with(vec![
            pr(1, "Old fix", "2026-01-01T00:00:00Z", false),
            pr(2, "New feature", "2026-05-01T00:00:00Z", true),
        ]);
        let numbers = |l: &PrList| l.rows().iter().map(|(_, p)| p.number).collect::<Vec<_>>();
        assert_eq!(numbers(&list), vec![2, 1]);
        list.hide_drafts = true;
        assert_eq!(numbers(&list), vec![1]);
        list.hide_drafts = false;
        list.text_filter = Some("feat".into());
        assert_eq!(numbers(&list), vec![2]);
        list.text_filter = Some("#1".into());
        assert_eq!(numbers(&list), vec![1]);
        list.text_filter = Some("Feat".into()); // smart-case: no match
        assert!(numbers(&list).is_empty());
    }

    #[test]
    fn marks_dedupe_by_url_and_feed_the_dash_list() {
        let mut one = pr(1, "Fix bug", "2026-01-01T00:00:00Z", false);
        one.url = "https://github.com/o/r/pull/1".into();
        let mut two = pr(2, "Add feature", "2026-05-01T00:00:00Z", false);
        two.url = "https://github.com/o/r/pull/2".into();
        let mut list = list_with(vec![one, two]);
        list.accounts = Load::Ready(vec![Account::for_test("me")]);
        assert_eq!(
            list.dash_list_text().unwrap(),
            "- Add feature — https://github.com/o/r/pull/2",
            "nothing marked: the selected PR"
        );
        assert_eq!(list.toggle_mark(), Some(true));
        list.selected = 1;
        assert_eq!(list.toggle_mark(), Some(true));
        assert_eq!(
            list.dash_list_text().unwrap(),
            "- Add feature — https://github.com/o/r/pull/2\n- Fix bug — https://github.com/o/r/pull/1"
        );
        assert_eq!(list.toggle_mark(), Some(false));
        assert_eq!(list.marked.len(), 1);
        assert!(list.is_marked(list.rows()[0].1));
    }

    #[test]
    fn review_modal_validation_and_own_pr_cycle() {
        let mut modal = ReviewModal::new(false);
        modal.cycle_event(true);
        assert_eq!(modal.event, ReviewEvent::Comment);
        assert!(modal.validate(0).is_some());
        assert!(modal.validate(2).is_none());

        let mut modal = ReviewModal::new(true);
        modal.cycle_event(true);
        assert_eq!(modal.event, ReviewEvent::Approve);
        assert!(modal.validate(0).is_none());
        modal.cycle_event(true);
        assert_eq!(modal.event, ReviewEvent::RequestChanges);
        assert!(modal.validate(3).is_some());
    }

    fn details(head: &str, paths: &[&str]) -> PrDetails {
        PrDetails {
            head_sha: head.into(),
            base_sha: "b".repeat(40),
            body: String::new(),
            files: paths
                .iter()
                .map(|p| gh::PrFile {
                    entry: FileEntry {
                        path: p.to_string(),
                        status: FileStatus::Modified,
                        additions: 1,
                        deletions: 1,
                        binary: false,
                    },
                    patch: None,
                    commentable: Default::default(),
                })
                .collect(),
        }
    }

    fn target(account: &Account, summary: &PrSummary) -> PreloadTarget {
        PreloadTarget {
            account: account.clone(),
            summary: summary.clone(),
            local: None,
        }
    }

    #[test]
    fn preloader_fetches_details_then_files() {
        let (tx, _rx) = mpsc::channel();
        let mut preloader = Preloader::with_workers(tx, 0);
        let account = Account::for_test("alice");
        let mut summary = pr(1, "a", "2026-10-01T00:00:00Z", false);
        summary.url = "https://github.com/o/r/pull/1".into();
        let key = pr_key(&account, &summary);

        preloader.retarget(vec![target(&account, &summary)]);
        assert_eq!(preloader.queued(), 1);
        assert_eq!(
            preloader.status(&account, &summary),
            Some(PreloadStatus::Loading)
        );
        assert!(preloader.take(&account, &summary).is_none());

        // Details arrive: the worker took the job, so the queue is empty
        // and re-planning queues one job per file.
        preloader.queue.0.lock().unwrap().clear();
        let head = "a".repeat(40);
        assert!(preloader.apply_details(
            &key,
            &summary.updated_at,
            Ok(details(&head, &["x", "y"]))
        ));
        preloader.retarget(vec![target(&account, &summary)]);
        assert_eq!(preloader.queued(), 2);

        preloader.queue.0.lock().unwrap().clear();
        assert!(preloader.apply_file(&key, &head, "x", Ok(("o".into(), "n".into()))));
        assert!(!preloader.apply_file(&key, "stale", "y", Ok(Default::default())));
        assert!(preloader.apply_file(&key, &head, "y", Err("boom".into())));
        assert_eq!(
            preloader.status(&account, &summary),
            Some(PreloadStatus::Ready)
        );
        let (got, contents) = preloader.take(&account, &summary).unwrap();
        assert_eq!(got.files.len(), 2);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents["x"], ("o".to_string(), "n".to_string()));

        // A newer list entry makes the preload stale and starts over.
        summary.updated_at = "2026-10-02T00:00:00Z".into();
        assert!(preloader.take(&account, &summary).is_none());
        preloader.retarget(vec![target(&account, &summary)]);
        assert_eq!(preloader.queued(), 1);
        assert_eq!(preloader.bytes, 0);
    }

    #[test]
    fn preloader_drops_queued_work_outside_the_view() {
        let (tx, _rx) = mpsc::channel();
        let mut preloader = Preloader::with_workers(tx, 0);
        let account = Account::for_test("alice");
        let mut one = pr(1, "a", "2026-10-01T00:00:00Z", false);
        one.url = "u1".into();
        let mut two = pr(2, "b", "2026-10-01T00:00:00Z", false);
        two.url = "u2".into();

        preloader.retarget(vec![target(&account, &one), target(&account, &two)]);
        assert_eq!(preloader.queued(), 2);
        preloader.retarget(vec![target(&account, &two)]);
        assert_eq!(preloader.queued(), 1);
        assert_eq!(preloader.status(&account, &one), None);
        // A late result for a dropped job is ignored.
        assert!(!preloader.apply_details(
            &pr_key(&account, &one),
            &one.updated_at,
            Ok(details("h", &[]))
        ));
    }

    #[test]
    fn utc_parsing_and_age_format() {
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_utc("2026-10-19T00:00:00Z"), Some(1_792_368_000));
        assert_eq!(parse_utc("bad"), None);
        assert_eq!(format_age(90), "1m");
        assert_eq!(format_age(7200), "2h");
        assert_eq!(format_age(86_400 * 3), "3d");
        assert_eq!(format_age(86_400 * 30), "4w");
    }
}
