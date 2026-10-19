//! GitHub layer: shells out to the `gh` CLI for the pull request view —
//! logged-in accounts, PR search, PR files and contents, and review
//! submission. Every request runs as one specific account by handing gh
//! that account's token through the environment, so the user's active
//! `gh` account is never switched.

use std::collections::HashSet;
use std::io::Write;
use std::process::{Command, Output, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::types::{FileEntry, FileStatus};

/// One `gh`-authenticated account on one host.
#[derive(Clone)]
pub struct Account {
    pub host: String,
    pub login: String,
    /// OAuth token from `gh auth token`; only ever passed to child `gh`
    /// processes through the environment, never displayed or logged.
    token: String,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("host", &self.host)
            .field("login", &self.login)
            .finish_non_exhaustive()
    }
}

impl Account {
    /// Short label: the login, plus the host when it isn't github.com.
    pub fn label(&self) -> String {
        if self.host == "github.com" {
            self.login.clone()
        } else {
            format!("{}@{}", self.login, self.host)
        }
    }

    /// A `gh` command that authenticates as this account. Only the token
    /// variable gh reads for this account's host is set, so a command that
    /// resolves another host can't send the token there.
    fn gh(&self) -> Command {
        let mut cmd = Command::new("gh");
        let token_var = if self.host == "github.com" {
            "GH_TOKEN"
        } else {
            "GH_ENTERPRISE_TOKEN"
        };
        cmd.env_remove("GH_TOKEN")
            .env_remove("GH_ENTERPRISE_TOKEN")
            .env(token_var, &self.token)
            .env("GH_HOST", &self.host)
            .env("GH_PROMPT_DISABLED", "1")
            .stdin(Stdio::null());
        cmd
    }

    /// `gh api --hostname <host> <args>`, returning stdout bytes. On
    /// non-zero exit the error carries gh's message and, when present,
    /// GitHub's validation details from the response body.
    fn api(&self, args: &[&str]) -> Result<Vec<u8>> {
        let output = self
            .gh()
            .args(["api", "--hostname", &self.host])
            .args(args)
            .output()
            .context("failed to run gh — is the GitHub CLI installed?")?;
        if !output.status.success() {
            return Err(api_error(&output));
        }
        Ok(output.stdout)
    }
}

/// Build an error from a failed `gh api` run: gh's stderr line plus any
/// `message`/`errors` GitHub put in the JSON response body.
fn api_error(output: &Output) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut msg = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("gh api failed")
        .trim_start_matches("gh: ")
        .to_string();
    if let Ok(body) = serde_json::from_slice::<Value>(&output.stdout) {
        let details: Vec<String> = body
            .get("errors")
            .and_then(Value::as_array)
            .map(|errs| {
                errs.iter()
                    .filter_map(|e| match e {
                        Value::String(s) => Some(s.clone()),
                        _ => e.get("message").and_then(Value::as_str).map(str::to_string),
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !details.is_empty() {
            msg = format!("{msg}: {}", details.join("; "));
        }
    }
    anyhow!(sanitize_line(&msg))
}

/// Every account `gh` is logged in to, across all hosts, each with its
/// token. Falls back to just the active account on gh versions without
/// `gh auth status --json`.
pub fn accounts() -> Result<Vec<Account>> {
    let output = Command::new("gh")
        .args(["auth", "status", "--json", "hosts"])
        .stdin(Stdio::null())
        .output()
        .context("failed to run gh — is the GitHub CLI installed?")?;
    // gh exits non-zero when any one account is in a bad state, but still
    // prints the JSON; trust the JSON when it parses.
    let logins = match parse_auth_status(&String::from_utf8_lossy(&output.stdout)) {
        Some(logins) => logins,
        None => return active_account().map(|a| vec![a]),
    };
    if logins.is_empty() {
        bail!("no GitHub accounts — run `gh auth login`");
    }
    logins
        .into_iter()
        .map(|(host, login)| {
            let token = token_for(&host, Some(&login))?;
            Ok(Account { host, login, token })
        })
        .collect()
}

/// The active github.com account, for gh versions that can't list
/// accounts as JSON.
fn active_account() -> Result<Account> {
    let token = token_for("github.com", None)?;
    let mut account = Account {
        host: "github.com".to_string(),
        login: String::new(),
        token,
    };
    let login = account.api(&["user", "--jq", ".login"])?;
    account.login = String::from_utf8_lossy(&login).trim().to_string();
    Ok(account)
}

/// `gh auth token` for one host (and user, when given).
fn token_for(host: &str, user: Option<&str>) -> Result<String> {
    let mut cmd = Command::new("gh");
    cmd.args(["auth", "token", "--hostname", host]);
    if let Some(user) = user {
        cmd.args(["--user", user]);
    }
    let output = cmd
        .stdin(Stdio::null())
        .output()
        .context("failed to run gh — is the GitHub CLI installed?")?;
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || token.is_empty() {
        bail!(
            "no token for {} on {host} — run `gh auth login`",
            user.unwrap_or("the active account")
        );
    }
    Ok(token)
}

/// (host, login) of every successfully authenticated account in
/// `gh auth status --json hosts` output, active account first per host.
/// `None` when the output isn't that JSON (older gh).
fn parse_auth_status(json: &str) -> Option<Vec<(String, String)>> {
    let value: Value = serde_json::from_str(json).ok()?;
    let hosts = value.get("hosts")?.as_object()?;
    let mut out = Vec::new();
    for (host, entries) in hosts {
        let mut entries: Vec<&Value> = entries.as_array()?.iter().collect();
        entries.sort_by_key(|e| !e.get("active").and_then(Value::as_bool).unwrap_or(false));
        for entry in entries {
            if entry.get("state").and_then(Value::as_str) != Some("success") {
                continue;
            }
            if let Some(login) = entry.get("login").and_then(Value::as_str) {
                out.push((host.clone(), login.to_string()));
            }
        }
    }
    Some(out)
}

/// One pull request as listed by a search.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PrSummary {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub is_draft: bool,
    /// `OPEN`, `CLOSED` or `MERGED`.
    pub state: String,
    /// ISO-8601 UTC timestamp, e.g. `2026-10-15T11:37:21Z`.
    pub updated_at: String,
    pub additions: usize,
    pub deletions: usize,
    pub changed_files: usize,
    /// `APPROVED`, `CHANGES_REQUESTED`, `REVIEW_REQUIRED`, or absent.
    pub review_decision: Option<String>,
    pub head_ref_name: String,
    pub base_ref_name: String,
    #[serde(deserialize_with = "login_of")]
    pub author: String,
    #[serde(rename = "repository", deserialize_with = "name_with_owner")]
    pub repo: String,
}

fn login_of<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let v = Option::<Value>::deserialize(d)?;
    Ok(v.as_ref()
        .and_then(|v| v.get("login"))
        .and_then(Value::as_str)
        // Deleted users come back as null authors.
        .unwrap_or("ghost")
        .to_string())
}

fn name_with_owner<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let v = Option::<Value>::deserialize(d)?;
    Ok(v.as_ref()
        .and_then(|v| v.get("nameWithOwner"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string())
}

const SEARCH_QUERY: &str = "query($q: String!) { search(query: $q, type: ISSUE, first: 50) { \
    nodes { ... on PullRequest { number title url isDraft state updatedAt additions deletions \
    changedFiles reviewDecision headRefName baseRefName author { login } \
    repository { nameWithOwner } } } } }";

/// Run a GitHub PR search (`q` uses search syntax, `@me` = this account).
pub fn search_prs(account: &Account, q: &str) -> Result<Vec<PrSummary>> {
    let output = account
        .gh()
        .args(["api", "--hostname", &account.host, "graphql"])
        .arg("-f")
        .arg(format!("query={SEARCH_QUERY}"))
        .arg("-f")
        .arg(format!("q={q}"))
        .output()
        .context("failed to run gh — is the GitHub CLI installed?")?;
    // Orgs the token can't see (e.g. SAML-protected) make gh exit non-zero
    // while the rest of the results still arrive; keep those.
    match parse_search(&output.stdout) {
        Some(prs) => Ok(prs),
        None if !output.status.success() => Err(api_error(&output)),
        None => bail!("unexpected search response from GitHub"),
    }
}

/// PRs from a GraphQL search response; `None` when there is no data.
fn parse_search(body: &[u8]) -> Option<Vec<PrSummary>> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let nodes = value.pointer("/data/search/nodes")?.as_array()?;
    Some(
        nodes
            .iter()
            .filter(|n| n.get("number").is_some())
            .filter_map(|n| serde_json::from_value::<PrSummary>(n.clone()).ok())
            .filter(|pr| valid_repo(&pr.repo))
            .map(|mut pr| {
                for field in [
                    &mut pr.title,
                    &mut pr.author,
                    &mut pr.head_ref_name,
                    &mut pr.base_ref_name,
                    &mut pr.state,
                ] {
                    *field = sanitize_line(field);
                }
                pr
            })
            .collect(),
    )
}

/// Make untrusted text safe to draw. ratatui's paragraph rendering
/// passes control characters straight to the terminal, so an escape
/// sequence in a PR title or file could drive it; those become U+FFFD,
/// as do bidi overrides ("Trojan Source") so reviewers see them.
/// Newlines and tabs are kept; the CR of a CRLF is dropped.
pub fn sanitize_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' | '\t' => out.push(c),
            '\r' if chars.peek() == Some(&'\n') => {}
            '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => out.push('\u{FFFD}'),
            c if c.is_control() => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
    out
}

/// [`sanitize_text`] for one-line fields: newlines and tabs become spaces.
pub fn sanitize_line(text: &str) -> String {
    sanitize_text(text).replace(['\n', '\t'], " ")
}

/// Which version of a file an inline comment targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// The base (old) version.
    Left,
    /// The head (new) version.
    Right,
}

impl Side {
    fn api_name(self) -> &'static str {
        match self {
            Side::Left => "LEFT",
            Side::Right => "RIGHT",
        }
    }
}

/// One changed file of a PR.
#[derive(Debug, Clone)]
pub struct PrFile {
    pub entry: FileEntry,
    /// Unified-diff hunks from GitHub; `None` when the diff is too large.
    pub patch: Option<String>,
    /// Lines GitHub accepts inline comments on (those inside the hunks).
    pub commentable: HashSet<(Side, usize)>,
}

/// What a review session needs beyond the search summary.
#[derive(Debug, Clone)]
pub struct PrDetails {
    pub head_sha: String,
    /// Merge base of base and head — the "old" side of every file.
    pub base_sha: String,
    pub body: String,
    /// Changed files, sorted by path.
    pub files: Vec<PrFile>,
}

#[derive(Deserialize)]
struct RawPull {
    head: RawRef,
    base: RawRef,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Deserialize)]
struct RawRef {
    sha: String,
}

#[derive(Deserialize)]
struct RawFile {
    filename: String,
    status: String,
    #[serde(default)]
    additions: usize,
    #[serde(default)]
    deletions: usize,
    #[serde(default)]
    previous_filename: Option<String>,
    #[serde(default)]
    patch: Option<String>,
}

/// Fetch head/merge-base SHAs, description and changed files of a PR.
pub fn pr_details(account: &Account, repo: &str, number: u64) -> Result<PrDetails> {
    ensure_repo(repo)?;
    let pull: RawPull =
        serde_json::from_slice(&account.api(&[&format!("repos/{repo}/pulls/{number}")])?)
            .context("unexpected pull request response from GitHub")?;
    ensure_sha(&pull.head.sha)?;
    ensure_sha(&pull.base.sha)?;

    // GitHub diffs a PR against the merge base, not the base branch tip.
    let base_sha = account
        .api(&[
            &format!("repos/{repo}/compare/{}...{}", pull.base.sha, pull.head.sha),
            "--jq",
            ".merge_base_commit.sha",
        ])
        .map(|out| String::from_utf8_lossy(&out).trim().to_string())
        .context("could not find the PR's merge base")?;
    // Without the merge base, old-side line numbers wouldn't match
    // GitHub's diff, so comments could land on the wrong lines.
    ensure_sha(&base_sha)?;

    let pages: Vec<Vec<RawFile>> = serde_json::from_slice(&account.api(&[
        "--paginate",
        "--slurp",
        &format!("repos/{repo}/pulls/{number}/files?per_page=100"),
    ])?)
    .context("unexpected file list response from GitHub")?;

    let mut files: Vec<PrFile> = pages
        .into_iter()
        .flatten()
        .filter(|f| valid_path(&f.filename))
        .map(pr_file)
        .collect();
    files.sort_by(|a, b| a.entry.path.cmp(&b.entry.path));

    Ok(PrDetails {
        head_sha: pull.head.sha,
        base_sha,
        body: sanitize_text(&pull.body.unwrap_or_default()),
        files,
    })
}

fn pr_file(raw: RawFile) -> PrFile {
    let status = match raw.status.as_str() {
        "added" => FileStatus::Added,
        "removed" => FileStatus::Deleted,
        "renamed" => match raw.previous_filename.filter(|p| valid_path(p)) {
            Some(from) => FileStatus::Renamed { from },
            None => FileStatus::Modified,
        },
        "copied" => FileStatus::Other('C'),
        _ => FileStatus::Modified,
    };
    // No patch and no counted lines: GitHub's marker for binary files.
    // (Oversized text diffs also lack a patch but do report counts.)
    let binary = raw.patch.is_none()
        && raw.additions + raw.deletions == 0
        && !matches!(status, FileStatus::Renamed { .. });
    let commentable = raw
        .patch
        .as_deref()
        .map(commentable_lines)
        .unwrap_or_default();
    PrFile {
        entry: FileEntry {
            path: raw.filename,
            status,
            additions: raw.additions,
            deletions: raw.deletions,
            binary,
        },
        patch: raw.patch,
        commentable,
    }
}

/// Lines inside a patch's hunks, per side: context lines count on both
/// sides, removals on the left, additions on the right.
pub fn commentable_lines(patch: &str) -> HashSet<(Side, usize)> {
    let mut lines = HashSet::new();
    let mut cursor: Option<(usize, usize)> = None;
    for line in patch.lines() {
        if let Some(header) = line.strip_prefix("@@ ") {
            cursor = parse_hunk_header(header);
            continue;
        }
        let Some((old, new)) = cursor.as_mut() else {
            continue;
        };
        match line.chars().next() {
            Some('-') => {
                lines.insert((Side::Left, *old));
                *old += 1;
            }
            Some('+') => {
                lines.insert((Side::Right, *new));
                *new += 1;
            }
            Some('\\') => {}
            _ => {
                lines.insert((Side::Left, *old));
                lines.insert((Side::Right, *new));
                *old += 1;
                *new += 1;
            }
        }
    }
    lines
}

/// Start lines (old, new) from the body of a `@@ -a,b +c,d @@` header.
fn parse_hunk_header(header: &str) -> Option<(usize, usize)> {
    let mut parts = header.split_whitespace();
    let start = |part: Option<&str>, sign: char| -> Option<usize> {
        part?.strip_prefix(sign)?.split(',').next()?.parse().ok()
    };
    let old = start(parts.next(), '-')?;
    let new = start(parts.next(), '+')?;
    Some((old, new))
}

/// Raw contents of `path` at commit `sha`.
pub fn file_content(account: &Account, repo: &str, sha: &str, path: &str) -> Result<String> {
    ensure_repo(repo)?;
    ensure_sha(sha)?;
    if !valid_path(path) {
        bail!("refusing unexpected file path {path:?}");
    }
    let endpoint = format!("repos/{repo}/contents/{}?ref={sha}", encode_path(path));
    let raw = account.api(&["-H", "Accept: application/vnd.github.raw", &endpoint])?;
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

/// Review verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewEvent {
    Comment,
    Approve,
    RequestChanges,
}

impl ReviewEvent {
    pub const ALL: [ReviewEvent; 3] = [
        ReviewEvent::Comment,
        ReviewEvent::Approve,
        ReviewEvent::RequestChanges,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ReviewEvent::Comment => "Comment",
            ReviewEvent::Approve => "Approve",
            ReviewEvent::RequestChanges => "Request changes",
        }
    }

    fn api_name(self) -> &'static str {
        match self {
            ReviewEvent::Comment => "COMMENT",
            ReviewEvent::Approve => "APPROVE",
            ReviewEvent::RequestChanges => "REQUEST_CHANGES",
        }
    }
}

/// An inline comment queued for the next review submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftComment {
    pub path: String,
    pub side: Side,
    /// 1-based line number in that side's version of the file.
    pub line: usize,
    pub body: String,
}

/// JSON body for `POST /repos/{repo}/pulls/{n}/reviews`.
fn review_payload(
    commit_id: &str,
    event: ReviewEvent,
    body: &str,
    comments: &[DraftComment],
) -> Value {
    let mut payload = json!({
        "commit_id": commit_id,
        "event": event.api_name(),
        "comments": comments
            .iter()
            .map(|c| json!({
                "path": c.path,
                "line": c.line,
                "side": c.side.api_name(),
                "body": c.body,
            }))
            .collect::<Vec<_>>(),
    });
    if !body.trim().is_empty() {
        payload["body"] = Value::String(body.to_string());
    }
    payload
}

/// Submit a review (verdict, summary body and inline comments) in one
/// request, pinned to `commit_id` so comments land on the reviewed code.
pub fn submit_review(
    account: &Account,
    repo: &str,
    number: u64,
    commit_id: &str,
    event: ReviewEvent,
    body: &str,
    comments: &[DraftComment],
) -> Result<()> {
    ensure_repo(repo)?;
    ensure_sha(commit_id)?;
    let payload = review_payload(commit_id, event, body, comments).to_string();
    let mut child = account
        .gh()
        .args(["api", "--hostname", &account.host, "-X", "POST"])
        .arg(format!("repos/{repo}/pulls/{number}/reviews"))
        .args(["--input", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run gh — is the GitHub CLI installed?")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(payload.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(api_error(&output));
    }
    Ok(())
}

/// Open a PR in the browser via `gh pr view --web`.
pub fn open_in_browser(account: &Account, url: &str) -> Result<()> {
    if !url.starts_with(&format!("https://{}/", account.host)) {
        bail!("refusing to open a URL outside {}", account.host);
    }
    let status = account
        .gh()
        .args(["pr", "view", url, "--web"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to run gh — is the GitHub CLI installed?")?;
    if !status.success() {
        bail!("gh pr view --web failed");
    }
    Ok(())
}

/// `owner/name` with only the characters GitHub allows in each part.
fn valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let ok = |p: Option<&str>| {
        p.is_some_and(|p| {
            !p.is_empty()
                && p != "."
                && p != ".."
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        })
    };
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

fn ensure_repo(repo: &str) -> Result<()> {
    if !valid_repo(repo) {
        bail!("refusing unexpected repository name {repo:?}");
    }
    Ok(())
}

fn ensure_sha(sha: &str) -> Result<()> {
    if !(7..=64).contains(&sha.len()) || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("refusing unexpected commit id {sha:?}");
    }
    Ok(())
}

/// A repo-relative path with no empty, `.` or `..` segments.
fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\0')
        && path
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

/// Percent-encode a repo path for a URL, keeping `/` separators.
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_status_lists_successful_accounts_active_first() {
        let json = r#"{"hosts":{"github.com":[
            {"state":"success","active":false,"login":"alt"},
            {"state":"error","active":false,"login":"broken"},
            {"state":"success","active":true,"login":"main"}]}}"#;
        assert_eq!(
            parse_auth_status(json).unwrap(),
            vec![
                ("github.com".to_string(), "main".to_string()),
                ("github.com".to_string(), "alt".to_string()),
            ]
        );
        assert!(parse_auth_status("not json").is_none());
    }

    #[test]
    fn search_parses_nodes_and_skips_nulls() {
        let body = br#"{"data":{"search":{"nodes":[
            {"number":7,"title":"Fix","url":"https://github.com/o/r/pull/7","isDraft":true,
             "state":"OPEN","updatedAt":"2026-10-15T11:37:21Z","additions":3,"deletions":1,
             "changedFiles":2,"reviewDecision":null,"headRefName":"fix","baseRefName":"main",
             "author":null,"repository":{"nameWithOwner":"o/r"}},
            null, {}]}}}"#;
        let prs = parse_search(body).unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 7);
        assert_eq!(prs[0].repo, "o/r");
        assert_eq!(prs[0].author, "ghost");
        assert!(prs[0].is_draft);
        assert!(parse_search(br#"{"errors":[]}"#).is_none());
    }

    #[test]
    fn commentable_lines_follow_hunk_sides() {
        let patch =
            "@@ -10,3 +10,3 @@ fn x()\n ctx\n-old\n+new\n ctx2\n\\ No newline at end of file";
        let lines = commentable_lines(patch);
        assert!(lines.contains(&(Side::Left, 10)) && lines.contains(&(Side::Right, 10)));
        assert!(lines.contains(&(Side::Left, 11)));
        assert!(lines.contains(&(Side::Right, 11)));
        assert!(lines.contains(&(Side::Left, 12)) && lines.contains(&(Side::Right, 12)));
        assert!(!lines.contains(&(Side::Left, 13)));
        assert_eq!(lines.len(), 6);
    }

    #[test]
    fn hunk_header_without_counts() {
        assert_eq!(parse_hunk_header("-1 +1 @@"), Some((1, 1)));
        assert_eq!(parse_hunk_header("-0,0 +1,4 @@"), Some((0, 1)));
        assert_eq!(parse_hunk_header("garbage"), None);
    }

    #[test]
    fn review_payload_omits_blank_body() {
        let comments = vec![DraftComment {
            path: "src/a.rs".into(),
            side: Side::Right,
            line: 4,
            body: "nit".into(),
        }];
        let p = review_payload("abc1234", ReviewEvent::Comment, "  ", &comments);
        assert!(p.get("body").is_none());
        assert_eq!(p["event"], "COMMENT");
        assert_eq!(p["comments"][0]["side"], "RIGHT");
        assert_eq!(p["comments"][0]["line"], 4);
        let p = review_payload("abc1234", ReviewEvent::Approve, "lgtm", &[]);
        assert_eq!(p["body"], "lgtm");
        assert_eq!(p["event"], "APPROVE");
    }

    #[test]
    fn validation_rejects_odd_names() {
        assert!(valid_repo("SokoPro-Oy/sok-web"));
        assert!(valid_repo("a/b.c_d"));
        assert!(!valid_repo("a/../b"));
        assert!(!valid_repo("a/.."));
        assert!(!valid_repo("a"));
        assert!(!valid_repo("a/b?x=1"));
        assert!(valid_path("src/a b.rs"));
        assert!(!valid_path("../etc/passwd"));
        assert!(!valid_path("a//b"));
        assert!(ensure_sha("0d05fa8339139236fd813e1c7e75bb014e81f8ee").is_ok());
        assert!(ensure_sha("main").is_err());
    }

    #[test]
    fn sanitize_replaces_control_and_bidi_chars() {
        assert_eq!(sanitize_text("a\x1b[2Jb\r\nc\td"), "a\u{FFFD}[2Jb\nc\td");
        assert_eq!(sanitize_text("x\u{202E}y\u{9b}"), "x\u{FFFD}y\u{FFFD}");
        assert_eq!(sanitize_line("t1\nt2\tz"), "t1 t2 z");
    }

    #[test]
    fn encode_path_escapes_reserved_chars() {
        assert_eq!(encode_path("src/a b#?.rs"), "src/a%20b%23%3F.rs");
        assert_eq!(encode_path("ä"), "%C3%A4");
    }
}
