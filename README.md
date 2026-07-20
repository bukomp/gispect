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
gispect update          # check upstream for new commits and reinstall
gispect mcp             # run as an MCP server over stdio
```

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
| `?` | help overlay |
| `q` / `Esc` | quit |

## Configuration

Pressing `C` opens the config modal, which lists startup defaults. Editing and
saving writes them to disk and also applies them to the current session, so
gispect opens with your preferred setup every time.

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
means defaults. Saving from the modal writes the full file.

With no config file, gispect behaves exactly as today. With one, it opens in
the configured mode and view settings. Passing `--base <branch>` on the CLI
still forces branch vs base mode with that base — the explicit flag wins over
the configured default mode. The base branch itself is intentionally not
persisted (it is repo-specific).

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
