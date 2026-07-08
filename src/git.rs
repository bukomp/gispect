//! Git layer: shells out to the `git` CLI to discover repos, list changed
//! files, and fetch file contents for the diff views.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::types::{DiffMode, FileEntry, FileStatus};

/// A discovered git repository rooted at a working-tree top-level.
#[derive(Clone)]
pub struct GitRepo {
    root: PathBuf,
}

impl GitRepo {
    /// Discover the repository containing `path` via
    /// `git -C <path> rev-parse --show-toplevel`.
    pub fn discover(path: &Path) -> Result<Self> {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .with_context(|| format!("failed to run git in {}", path.display()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "{} is not inside a git repository: {}",
                path.display(),
                stderr.trim()
            );
        }

        let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if root.is_empty() {
            bail!("{} is not inside a git repository", path.display());
        }

        Ok(GitRepo {
            root: PathBuf::from(root),
        })
    }

    /// The repository's working-tree root.
    #[allow(dead_code)]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Run a git command in the repo root, returning stdout as UTF-8
    /// (lossy). On non-zero exit, the error message includes stderr.
    fn git(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()
            .with_context(|| format!("failed to run `git {}`", args.join(" ")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("`git {}` failed: {}", args.join(" "), stderr.trim());
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Like [`GitRepo::git`], but returns `None` on any failure instead of
    /// an `Err`.
    fn git_opt(&self, args: &[&str]) -> Option<String> {
        self.git(args).ok()
    }

    /// Current branch name, or "HEAD" if it cannot be determined (e.g.
    /// detached HEAD, empty repo).
    pub fn current_branch(&self) -> String {
        self.git_opt(&["rev-parse", "--abbrev-ref", "HEAD"])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "HEAD".to_string())
    }

    /// First of "main"/"master" that exists as a ref, else the current
    /// branch.
    pub fn default_base(&self) -> String {
        for candidate in ["main", "master"] {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&self.root)
                .args(["rev-parse", "--verify", "--quiet", candidate])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if ok {
                return candidate.to_string();
            }
        }
        self.current_branch()
    }

    /// All local branches, sorted.
    pub fn local_branches(&self) -> Result<Vec<String>> {
        let out = self.git(&["branch", "--format=%(refname:short)"])?;
        let mut branches: Vec<String> = out
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        branches.sort();
        Ok(branches)
    }

    /// Diff range arguments for `git diff` corresponding to `mode`.
    fn diff_args(mode: &DiffMode) -> Vec<String> {
        match mode {
            DiffMode::BranchToBase { base } => vec![format!("{base}...HEAD")],
            DiffMode::WorkingTree => vec!["HEAD".to_string()],
            DiffMode::Staged => vec!["--cached".to_string()],
            DiffMode::Unstaged => vec![],
        }
    }

    /// Changed files for `mode`, with per-file add/delete line counts.
    pub fn changed_files(&self, mode: &DiffMode) -> Result<Vec<FileEntry>> {
        let range = Self::diff_args(mode);

        let mut name_status_args: Vec<&str> = vec!["diff", "--name-status", "-M"];
        let range_refs: Vec<&str> = range.iter().map(|s| s.as_str()).collect();
        name_status_args.extend(range_refs.iter());
        let name_status_out = self.git(&name_status_args)?;

        let mut numstat_args: Vec<&str> = vec!["diff", "--numstat", "-M"];
        numstat_args.extend(range_refs.iter());
        let numstat_out = self.git(&numstat_args)?;

        struct StatusRow {
            status: FileStatus,
            path: String,
        }

        let status_rows: Vec<StatusRow> = name_status_out
            .lines()
            .filter(|l| !l.is_empty())
            .filter_map(|line| {
                let mut parts = line.split('\t');
                let status_field = parts.next()?;
                let first_char = status_field.chars().next()?;
                if first_char == 'R' {
                    let from = parts.next()?.to_string();
                    let to = parts.next()?.to_string();
                    Some(StatusRow {
                        status: FileStatus::Renamed { from },
                        path: to,
                    })
                } else {
                    let path = parts.next()?.to_string();
                    let status = match first_char {
                        'A' => FileStatus::Added,
                        'M' => FileStatus::Modified,
                        'D' => FileStatus::Deleted,
                        c => FileStatus::Other(c),
                    };
                    Some(StatusRow { status, path })
                }
            })
            .collect();

        struct NumstatRow {
            additions: usize,
            deletions: usize,
            binary: bool,
        }

        let numstat_rows: Vec<NumstatRow> = numstat_out
            .lines()
            .filter(|l| !l.is_empty())
            .filter_map(|line| {
                let mut parts = line.split('\t');
                let adds = parts.next()?;
                let dels = parts.next()?;
                if adds == "-" || dels == "-" {
                    Some(NumstatRow {
                        additions: 0,
                        deletions: 0,
                        binary: true,
                    })
                } else {
                    Some(NumstatRow {
                        additions: adds.parse().unwrap_or(0),
                        deletions: dels.parse().unwrap_or(0),
                        binary: false,
                    })
                }
            })
            .collect();

        let mut entries = Vec::with_capacity(status_rows.len());
        for (i, row) in status_rows.into_iter().enumerate() {
            let (additions, deletions, binary) = match numstat_rows.get(i) {
                Some(n) => (n.additions, n.deletions, n.binary),
                None => (0, 0, false),
            };
            entries.push(FileEntry {
                path: row.path,
                status: row.status,
                additions,
                deletions,
                binary,
            });
        }

        Ok(entries)
    }

    /// `git show <rev>:<path>`, returning `None` on any failure (missing
    /// path, missing rev, etc.) instead of propagating an error.
    fn show(&self, rev: &str, path: &str) -> Option<String> {
        self.git_opt(&["show", &format!("{rev}:{path}")])
    }

    /// (old_content, new_content) of `entry` for `mode`. Missing sides
    /// degrade to "" (e.g. added/deleted files); binary entries return
    /// ("", "") immediately.
    pub fn file_versions(&self, mode: &DiffMode, entry: &FileEntry) -> Result<(String, String)> {
        if entry.binary {
            return Ok((String::new(), String::new()));
        }

        let old_path = match &entry.status {
            FileStatus::Renamed { from } => from.clone(),
            _ => entry.path.clone(),
        };
        let is_added = matches!(entry.status, FileStatus::Added);
        let is_deleted = matches!(entry.status, FileStatus::Deleted);

        let old_content = if is_added {
            Some(String::new())
        } else {
            self.old_side(mode, &old_path)
        };

        let new_content = if is_deleted {
            Some(String::new())
        } else {
            self.new_side(mode, &entry.path)
        };

        if matches!(entry.status, FileStatus::Modified)
            && old_content.is_none()
            && new_content.is_none()
        {
            bail!(
                "could not read either version of '{}' for mode {:?}",
                entry.path,
                mode
            );
        }

        Ok((
            old_content.unwrap_or_default(),
            new_content.unwrap_or_default(),
        ))
    }

    /// Fetch the "old" side of a diff for `mode`, returning `None` if it
    /// cannot be read.
    fn old_side(&self, mode: &DiffMode, old_path: &str) -> Option<String> {
        match mode {
            DiffMode::BranchToBase { base } => {
                let merge_base = self
                    .git_opt(&["merge-base", base, "HEAD"])
                    .map(|s| s.trim().to_string())?;
                self.show(&merge_base, old_path)
            }
            DiffMode::WorkingTree => self.show("HEAD", old_path),
            DiffMode::Staged => self.show("HEAD", old_path),
            DiffMode::Unstaged => self.show(":0", old_path).or_else(|| self.show(":", old_path)),
        }
    }

    /// Fetch the "new" side of a diff for `mode`, returning `None` if it
    /// cannot be read.
    fn new_side(&self, mode: &DiffMode, path: &str) -> Option<String> {
        match mode {
            DiffMode::BranchToBase { .. } => self.show("HEAD", path),
            DiffMode::WorkingTree => std::fs::read_to_string(self.root.join(path)).ok(),
            DiffMode::Staged => self.show(":0", path).or_else(|| self.show(":", path)),
            DiffMode::Unstaged => std::fs::read_to_string(self.root.join(path)).ok(),
        }
    }

    /// Fingerprint of all repository state any diff mode can show: HEAD
    /// commit, index/working-tree status, and uncommitted content (`git
    /// diff HEAD` catches re-edits to files that were already modified,
    /// which `status --porcelain` alone would not).
    pub fn state_fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.git_opt(&["rev-parse", "HEAD"]).hash(&mut hasher);
        self.git_opt(&["status", "--porcelain"]).hash(&mut hasher);
        self.git_opt(&["diff", "HEAD"]).hash(&mut hasher);
        hasher.finish()
    }

    /// Plain unified `git diff` text for `mode`, optionally limited to one
    /// path.
    pub fn unified_diff(&self, mode: &DiffMode, path: Option<&str>) -> Result<String> {
        let range = Self::diff_args(mode);
        let mut args: Vec<&str> = vec!["diff"];
        let range_refs: Vec<&str> = range.iter().map(|s| s.as_str()).collect();
        args.extend(range_refs.iter());
        if let Some(p) = path {
            args.push("--");
            args.push(p);
        }
        self.git(&args)
    }
}
