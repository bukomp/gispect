# gispect

A terminal UI for inspecting git diffs side-by-side: old code on the left with
removed lines marked, new code on the right with added lines marked, plus a
changed-files panel, switchable diff modes, a built-in MCP server, and
self-update from the upstream repository.

## Install

```sh
cargo install --git https://github.com/bukomp/gispect
```

Or from a checkout: `cargo install --path .`

The upstream URL used for update checks is embedded at build time; override it
with the `GISPECT_REPO_URL` environment variable, either at build time or at
runtime.

## Usage

```sh
gispect                 # TUI, comparing the current branch against main/master
gispect --base develop  # choose the base branch
gispect --repo ~/src/x  # inspect another repository
gispect --repo ~/src    # inspect every repository directly under a folder
gispect update          # check upstream for new commits and reinstall
gispect mcp             # run as an MCP server over stdio
gispect prs             # pull request view: review requests and your PRs
gispect actions         # GitHub Actions: run workflows, watch runs
```

## Multiple repositories

When the start folder (the current directory or `--repo`) is not inside a git
repository, gispect opens every repository directly under it (one level deep),
for example a projects folder. Inside a repository, only that repository is
used, as before.

- Changed files from all repositories go into one list, each path prefixed
  with its repository's folder name (`kanbu/src/main.rs`). The tree view (`t`)
  groups them by repository.
- The status bar shows the repository count, plus the repository and branch of
  the selected file.
- The default base is the one most repositories use (`main` or `master`). In
  branch vs base mode, a repository that does not have the selected base
  compares against its own default base instead. `b` cycles through the local
  branches of all repositories.
- Repositories that cannot be diffed (for example a repository with no
  commits) are left out of the list.
- All repositories are watched for changes, and searches and filters apply to
  all of them.
- When you review a PR, gispect reads commits from the local repository whose
  folder name matches the PR repository's name, if it has them. Otherwise it
  gets the files from GitHub.
- `gispect mcp` serves the same combined view: paths in tool input and output
  have the repository prefix.
- The actions view (`a`) shows a tree with one collapsible row per repository.

## Diff modes

Cycle with `m`:

| Mode | Comparison |
|---|---|
| branch vs base | merge-base of the base branch vs `HEAD` (`git diff base...HEAD`) |
| working tree vs HEAD | everything not yet committed |
| staged | index vs `HEAD` (`git diff --cached`) |
| unstaged | working tree vs index (`git diff`) |

## Keys

| Key | Action |
|---|---|
| `j` / `k`, `↓` / `↑` | select file |
| `J` / `K`, `PgDn` / `PgUp`, `Ctrl-d` / `Ctrl-u` | scroll diff |
| `n` / `N` | jump to next / previous change; jumps to next / previous search match instead while a search is active |
| `g` / `G` | top / bottom of diff |
| `h` / `l`, `←` / `→` | horizontal scroll |
| `m` | cycle diff mode |
| `c` | toggle compact view — hide alignment filler rows so each pane reads contiguously |
| `s` | toggle syntax highlighting |
| `b` | cycle base branch |
| `r` | reload |
| `/` | search code in the current file's diff |
| `S` | filter files by changed-line content (live) |
| `p` | filter the file panel by file/folder name |
| `C` | open config (startup defaults) |
| `U` | apply available update and restart into the new version |
| `P` | pull request view (PRs across logged-in gh accounts) |
| `a` | GitHub Actions view (run workflows of this repository or of every repository in the folder) |
| `?` | help overlay |
| `q` / `Esc` | quit |

## Configuration

View settings are saved automatically: toggling them with their keys (`t`,
`f`, `F`, `1`, `2`, `c`, `s`) or switching mode with `m` writes the change to
disk, so gispect opens the same way next time.

Pressing `C` opens the config modal, which lists startup defaults. Editing and
saving writes them to disk and also applies them to the current session.

| Key | Action |
|---|---|
| `j` / `k`, `↓` / `↑` | move between settings |
| `Space`, `Enter`, `l`, `→` | toggle / cycle forward |
| `h`, `←` | cycle backward (matters for the default-mode setting, which cycles through modes) |
| `u` | copy the current session's settings into the form |
| `s` | save and close (persist to disk and apply to the session) |
| `Esc`, `q` | cancel without saving |

Settings are stored in `$XDG_CONFIG_HOME/gispect/config.json`, falling back to
`~/.config/gispect/config.json`, as JSON:

| Field | Meaning | Default |
|---|---|---|
| `default_mode` | startup diff mode: `"branch-to-base"` \| `"working-tree"` \| `"staged"` \| `"unstaged"` | `"branch-to-base"` (branch vs base) |
| `compact` | compact view | off (`false`) |
| `syntax` | syntax highlighting | on (`true`) |
| `show_files` | file panel | on (`true`) |
| `tree_view` | tree view | off (`false`) |
| `wide_files` | wide file panel | off (`false`) |
| `show_old` | old pane | on (`true`) |
| `show_new` | new pane | on (`true`) |

Missing fields fall back to defaults; a missing or invalid config file just
means defaults. Every save writes the full file.

With no config file, gispect behaves exactly as today. With one, it opens in
the configured mode and view settings. Passing `--base <branch>` on the CLI
still forces branch vs base mode with that base — the explicit flag wins over
the configured default mode, and does not change the saved default. The base
branch itself is intentionally not persisted (it is repo-specific).

## Search

- `/` searches within the currently selected file's diff. While a search is
  active, `n` / `N` jump to the next / previous match instead of the next /
  previous change hunk, and `Esc` clears the search.
- `S` live-filters the changed-files panel to files whose changed lines
  (added or removed lines only — never untouched parts of a file) contain the
  typed text, updating as you type. `Enter` keeps the filter applied; `Esc`
  clears it. It stacks with the `p` name filter: when both are set, a file
  must pass both to stay in the panel.
- `p` filters the file panel to files/folders whose path contains the typed
  text (case-insensitive substring match), updating live as you type. `Enter`
  keeps the filter applied; `Esc` clears it.
- All searches are smart-case: an all-lowercase query matches
  case-insensitively, while a query containing an uppercase letter matches
  case-sensitively.

## Pull requests

`P` (or `gispect prs`) opens the PR list. `gispect prs` also works outside a
git repository. Requires the GitHub CLI (`gh`) logged in (`gh auth login`).

It lists PRs for every account `gh` is logged in to, across hosts (github.com
and GitHub Enterprise). Each request runs as that account by passing its token
to `gh` through the environment, so the active `gh` account is never switched.

Tabs: Review requested (`review-requested:@me`), Authored, Assigned, Involved.
Results are the 50 most recently updated per account and tab, cached until
refreshed. Rows show state (● open/merged/closed, ◌ draft), `repo#number`,
title, account (when more than one), author, `+added -removed`, review
decision and age.

The PRs in the open tab (after filters) preload in the background, top row
first: details, then the contents of up to 100 files per PR, 4 fetches at a
time, up to 256 MB in total. `◇` after the state glyph means the PR is still loading,
`◆` means it is ready and opens at once, and the status line shows
`preloaded: n/m`. When you switch tabs or filters, queued work for PRs that
are no longer shown is dropped. A preloaded PR is as fresh as the list it came from;
press `r` to refresh. Preloading uses the GitHub API rate limit (about two
requests per file).

| Key | Action |
|---|---|
| `j` / `k`, `↓` / `↑` | select |
| `g` / `G` | top / bottom |
| `Tab` / `Shift-Tab`, `1`–`4` | switch tab |
| `a` | cycle account filter (all / one account) |
| `s` | cycle state (open / closed / merged / all) |
| `d` | hide / show drafts |
| `/` | text filter over title, repo, author, branch and `#number` (smart-case, live; `Enter` keeps, `Esc` clears) |
| `r` | refresh |
| `o` | open in browser |
| `Enter` | review the selected PR |
| `Esc` | back (clears the filter first) |
| `q` | quit |

Mouse: wheel moves the selection, click selects.

### Reviewing

`Enter` opens the PR in the normal side-by-side view, comparing the merge base
with the PR head, exactly like GitHub's "Files changed". File contents load on
demand (from the local clone when it already has the commits, else from the
GitHub API). Search, filters, compact view, syntax highlighting and hunk
navigation all work as usual; `m` and `b` are disabled.

| Key | Action |
|---|---|
| `J` / `K` | move the line cursor (shown as an inverted line number; mouse click also sets it) |
| `a` | comment on the new side of the cursor line (old side for removed lines) |
| `A` | comment on the old side |
| `x` | delete draft comments on the cursor line |
| `R` | submit review |
| `i` | PR description |
| `o` | open in browser |
| `r` | re-fetch the PR (drafts kept) |
| `Esc` | back to the PR list |

Comments are drafts until the review is submitted; lines with a draft show `●`
in the gutter and the status bar shows the draft count. GitHub only accepts
inline comments on lines inside the diff hunks (changed lines plus surrounding
context); other lines are refused with a message. Pressing `a` on a line that
already has a draft edits it; saving it empty removes it.

Comment editor: type to write, `Enter` newline, `Backspace` delete, `Ctrl-S`
save draft, `Esc` cancel.

Review modal: `Tab` / `Shift-Tab` choose the verdict (Comment / Approve /
Request changes), type a summary, `Ctrl-S` submit, `Esc` cancel. Approve and
Request changes are unavailable on your own PR. Request changes needs a
summary; Comment needs a summary or at least one inline comment. All drafts go
out in one review, pinned to the head commit that was reviewed. On failure the
modal stays open with GitHub's error and the drafts are kept.

Leaving a PR or quitting with unsent drafts asks for confirmation (press the
key again to discard).

Text from GitHub (titles, descriptions, file names, file contents) has
terminal control characters and Unicode bidi overrides replaced with `�` before
display, so a PR can't inject escape sequences and "Trojan Source" characters
stay visible.

## GitHub Actions

`a` (or `gispect actions`) opens the Actions view. It needs a local repository
and the GitHub CLI (`gh`) logged in. It handles tokens per account like the PR
view, so the active `gh` account is never switched.

Each repository is matched to GitHub through its remotes (`origin` first).
HTTPS, `ssh://` and `git@host:owner/name` URLs work. SSH config aliases (for
example `git@work-gh:owner/name`) are resolved with `ssh -G`, and
`ssh.github.com` (SSH over port 443) counts as github.com.

For each repository, gispect picks a logged-in account on that host that can
push (needed to run or re-run workflows), else the first that can read it. The
account named like the repository owner is tried first. A private repository
that no logged-in account can see shows "not found" with a hint to log in with
`gh auth login`.

Opened inside a repository, the view lists that repository's workflows at the
top level. Opened in a parent folder, it shows one collapsed row per
repository: the GitHub name, local branch, one status glyph per workflow (its
latest run), the workflow count and how many runs are active. Repositories
without a GitHub remote or without workflows say so.

Workflow rows show the latest run glyph, the name, `▶ run` when it can be run
by hand, `disabled` when it is disabled on GitHub, and the latest run's number,
branch and age. GitHub-managed workflows (Copilot, Dependabot, Pages) are
dimmed. Expanding a workflow lists its latest 8 runs (from the repository's
100 most recent runs) with event, branch, actor and age.

Glyphs: `✓` success, `✗` failure / timed out, `⊘` cancelled / skipped, `●` in
progress, `◌` queued / waiting, `!` action required, `·` no recent runs.

| Key | Action |
|---|---|
| `j` / `k`, `↓` / `↑` | select |
| `g` / `G` | top / bottom |
| `PgDn` / `PgUp` | page |
| `Space` | expand / collapse |
| `l` / `→` | expand |
| `h` / `←` | collapse, or go to the parent row |
| `e` | expand / collapse all (repositories, or every workflow's runs in a single repository) |
| `Enter` | run the workflow (form), expand a repository, or open a run in the browser |
| `R` | re-run the selected run (on a workflow row: its latest run) |
| `F` | re-run failed jobs |
| `X` | cancel an active run (press twice to confirm) |
| `o` | open the workflow or run in the browser |
| `/` | filter repositories and workflows by name or path (smart-case, live; `Enter` keeps, `Esc` clears) |
| `r` | refresh everything |
| `Esc` | back (clears the filter first) |
| `q` | quit |

Mouse: wheel moves the selection, click selects.

### Running a workflow

`Enter` on a workflow with a `workflow_dispatch` trigger opens a form. The ref
defaults to the checked-out branch (the default branch when HEAD is detached)
and must exist on GitHub. Inputs come from the workflow file in the local
checkout, or from the default branch on GitHub when the file is not checked
out. Input types: text, number, boolean (`[x]`), choice (`‹ value ›`) and
environment. Required inputs are marked `*`. Empty optional inputs are left
out, so the workflow's default applies. A warning shows when the account may
not have write access.

| Key | Action |
|---|---|
| `Tab` / `Shift-Tab`, `↓` / `↑` | move between fields |
| type | edit text |
| `Backspace` | delete |
| `Ctrl-U` | clear the field |
| `Space`, `←` / `→` | toggle booleans, cycle choices |
| `Enter`, `Ctrl-S` | run |
| `Esc` | cancel |

On failure the form stays open with GitHub's error.

After a run is started, re-run or cancelled, the run list refreshes every 3
seconds for 45 seconds, so the new state shows up. While any run of a
repository is queued or in progress, it refreshes every 8 seconds. Otherwise
press `r`.

Workflow files are parsed with node, nesting-depth and 1 MB size limits, so a
file with nested YAML aliases or deep nesting can't exhaust memory or the
stack. Text from GitHub and from
workflow files has control characters replaced before display, as in the PR
view.

## MCP server

`gispect mcp` speaks JSON-RPC 2.0 over stdio (newline-delimited). Example
client configuration:

```json
{
  "mcpServers": {
    "gispect": {
      "command": "gispect",
      "args": ["--repo", "/path/to/repo", "mcp"]
    }
  }
}
```

Tools (all accept `mode`: `branch` | `working` | `staged` | `unstaged`,
default `working`, and `base` for `mode=branch`):

- `list_changed_files` — changed files with status and add/delete counts
- `get_file_diff` — unified diff of one file (`path` required)
- `diff_summary` — file count, total additions/deletions, per-status counts

## Self-update

`build.rs` embeds the commit hash the binary was built from. `gispect update`
(or the background check in the TUI, surfaced as an `UPDATE AVAILABLE` banner —
press `U`) runs `git ls-remote <repo-url> HEAD`; when upstream has new commits
it reinstalls via `cargo install --git <repo-url> --force`, which replaces the
binary in `~/.cargo/bin` — so it works for `cargo install`-ed binaries too.
Requires `git` and `cargo` on `PATH`. Binaries built from a tree without
commits report `unknown` and always offer the update. Applying the update from
the TUI (`U`) restarts gispect in place on the new binary with the same
arguments; `gispect update` from the CLI reinstalls and exits.
