//! Shared data types — the contract between the git layer, diff engine,
//! TUI, and MCP server. Keep this file dependency-free (std only).

/// Which two states of the repository are being compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffMode {
    /// Current branch vs its merge-base with `base` (i.e. `git diff base...HEAD`).
    BranchToBase { base: String },
    /// HEAD vs working tree — everything not yet committed (`git diff HEAD`).
    WorkingTree,
    /// HEAD vs index — staged changes only (`git diff --cached`).
    Staged,
    /// Index vs working tree — unstaged changes only (`git diff`).
    Unstaged,
}

impl DiffMode {
    /// Short human label for the status bar, e.g. "branch vs main".
    pub fn label(&self) -> String {
        match self {
            DiffMode::BranchToBase { base } => format!("branch vs {base}"),
            DiffMode::WorkingTree => "working tree vs HEAD".to_string(),
            DiffMode::Staged => "staged".to_string(),
            DiffMode::Unstaged => "unstaged".to_string(),
        }
    }

    /// Cycle to the next mode. `base` is the configured base branch,
    /// used when cycling back into `BranchToBase`.
    pub fn next(&self, base: &str) -> DiffMode {
        match self {
            DiffMode::BranchToBase { .. } => DiffMode::WorkingTree,
            DiffMode::WorkingTree => DiffMode::Staged,
            DiffMode::Staged => DiffMode::Unstaged,
            DiffMode::Unstaged => DiffMode::BranchToBase {
                base: base.to_string(),
            },
        }
    }
}

/// Status of a changed file, as reported by `git diff --name-status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    /// Renamed; `from` is the old path (the entry's `path` is the new one).
    Renamed { from: String },
    /// Any other status letter git reports (C, T, U, ...).
    Other(char),
}

impl FileStatus {
    /// One-character marker for the file list, e.g. 'A', 'M', 'D', 'R'.
    pub fn marker(&self) -> char {
        match self {
            FileStatus::Added => 'A',
            FileStatus::Modified => 'M',
            FileStatus::Deleted => 'D',
            FileStatus::Renamed { .. } => 'R',
            FileStatus::Other(c) => *c,
        }
    }
}

/// One changed file in the active diff.
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// Path relative to the repo root (the new path for renames).
    pub path: String,
    pub status: FileStatus,
    /// Added line count (0 for binary files).
    pub additions: usize,
    /// Removed line count (0 for binary files).
    pub deletions: usize,
    /// True when git reports the file as binary (numstat shows "-").
    pub binary: bool,
}

/// A single line shown in one of the two panes.
#[derive(Debug, Clone)]
pub struct LineCell {
    /// 1-based line number in that version of the file.
    pub line_no: usize,
    /// Line content without trailing newline.
    pub content: String,
}

/// Classification of an aligned diff row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// Unchanged line — present in both panes.
    Context,
    /// Line only in the old version — left pane only.
    Removed,
    /// Line only in the new version — right pane only.
    Added,
    /// A removed line paired with an added line on the same row.
    Modified,
}

/// One visual row of the side-by-side view: old version on the left,
/// new version on the right. Either side may be empty (filler).
#[derive(Debug, Clone)]
pub struct DiffRow {
    pub left: Option<LineCell>,
    pub right: Option<LineCell>,
    pub kind: RowKind,
}

/// Fully aligned side-by-side diff for one file.
#[derive(Debug, Clone, Default)]
pub struct SideBySideDiff {
    pub rows: Vec<DiffRow>,
}
