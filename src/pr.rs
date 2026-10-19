//! Pull request view state: the PR list (tabs, account/state/draft/text
//! filters and a per-query result cache), the per-PR review session
//! (fetched files, cached contents, draft inline comments), the comment
//! and review editors, and the background `gh` jobs feeding them through
//! one event channel. Rendering lives in `pr_ui.rs`, key handling in
//! `app.rs`.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::gh::{self, Account, DraftComment, PrDetails, PrSummary, ReviewEvent, Side};
use crate::git::GitRepo;
use crate::search;
use crate::types::{FileEntry, FileStatus};

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

/// One search, per account: results are cached under this key.
pub type QueryKey = (usize, PrTab, StateFilter);

/// A value fetched in the background.
#[derive(Debug, Clone)]
pub enum Load<T> {
    Loading,
    Ready(T),
    Failed(String),
}

/// Results of background `gh` jobs, drained by the UI thread.
pub enum PrEvent {
    Accounts(Result<Vec<Account>, String>),
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
}

/// The PR list screen.
pub struct PrList {
    pub accounts: Load<Vec<Account>>,
    pub tab: PrTab,
    pub state: StateFilter,
    /// Account index to show; `None` shows every account.
    pub account_filter: Option<usize>,
    pub hide_drafts: bool,
    /// Committed text filter over title, repo, author, branch and number.
    pub text_filter: Option<String>,
    /// Filter prompt being typed, if any; filters live while typing.
    pub filter_input: Option<String>,
    pub results: HashMap<QueryKey, Load<Vec<PrSummary>>>,
    pub selected: usize,
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
            hide_drafts: false,
            text_filter: None,
            filter_input: None,
            results: HashMap::new(),
            selected: 0,
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
            .map(|a| (a, self.tab, self.state))
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
            .filter_map(|a| match self.results.get(&(a, tab, self.state)) {
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
                    self.results.get(&(a, tab, self.state)),
                    Some(Load::Loading) | None
                )
            })
    }

    /// Failed searches in the current view, as (account label, error).
    pub fn errors(&self) -> Vec<(String, String)> {
        let accounts = self.account_list();
        self.shown_accounts()
            .into_iter()
            .filter_map(|a| match self.results.get(&(a, self.tab, self.state)) {
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

    /// Forget cached searches (for one account, or all) so they re-run.
    pub fn invalidate(&mut self, account: Option<usize>) {
        match account {
            Some(a) => self.results.retain(|k, _| k.0 != a),
            None => self.results.clear(),
        }
    }
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

/// Load every gh account (with tokens) in the background.
pub fn spawn_accounts(tx: mpsc::Sender<PrEvent>) {
    std::thread::spawn(move || {
        let result = gh::accounts().map_err(|e| e.to_string());
        let _ = tx.send(PrEvent::Accounts(result));
    });
}

/// Run one list search in the background.
pub fn spawn_search(tx: mpsc::Sender<PrEvent>, key: QueryKey, account: Account) {
    std::thread::spawn(move || {
        let query = search_query(key.1, key.2);
        let result = gh::search_prs(&account, &query).map_err(|e| e.to_string());
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
            let fetch = |sha: &str, path: &str| -> Result<String, String> {
                if let Some(text) = local.as_ref().and_then(|r| r.show_at(sha, path)) {
                    return Ok(text);
                }
                gh::file_content(&account, &repo, sha, path).map_err(|e| e.to_string())
            };
            let entry = &job.entry;
            let old_path = match &entry.status {
                FileStatus::Renamed { from } => from.as_str(),
                _ => entry.path.as_str(),
            };
            let old = match entry.status {
                FileStatus::Added => Ok(String::new()),
                _ => fetch(&base_sha, old_path),
            };
            let new = match entry.status {
                FileStatus::Deleted => Ok(String::new()),
                _ => fetch(&head_sha, &entry.path),
            };
            let result = old.and_then(|o| new.map(|n| (o, n))).and_then(|(o, n)| {
                // Diffing happens on the UI thread; keep a huge file in an
                // untrusted PR from freezing it.
                if o.len().max(n.len()) > MAX_FILE_BYTES {
                    return Err(format!(
                        "file too large to display (over {} MB)",
                        MAX_FILE_BYTES >> 20
                    ));
                }
                Ok((gh::sanitize_text(&o), gh::sanitize_text(&n)))
            });
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
            (0, PrTab::ReviewRequested, StateFilter::Open),
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
