//! Workspace layer: one repository, or every repository directly under a
//! folder that is not itself inside one (e.g. a projects directory).
//!
//! In multi-repo mode each file path is prefixed with its repository's
//! folder name (`kanbu/src/main.rs`), so the merged file list groups by
//! repository in the tree view and paths stay unique.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use anyhow::{bail, Result};

use crate::git::GitRepo;
use crate::search;
use crate::types::{DiffMode, FileEntry, FileStatus};

/// One repository of the workspace.
struct Member {
    /// Folder name used as the path prefix; empty for a single repository.
    name: String,
    repo: GitRepo,
    /// Branch checked out when the workspace was discovered.
    branch: String,
}

/// The repositories gispect is inspecting.
#[derive(Clone)]
pub struct Workspace {
    members: std::sync::Arc<Vec<Member>>,
}

impl Workspace {
    /// The repository containing `path`; failing that, every repository
    /// directly under `path` (one level deep, sorted by folder name).
    pub fn discover(path: &Path) -> Result<Self> {
        if let Ok(repo) = GitRepo::discover(path) {
            let branch = repo.current_branch();
            return Ok(Self::from_members(vec![Member {
                name: String::new(),
                repo,
                branch,
            }]));
        }
        let mut members: Vec<Member> = std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().join(".git").exists())
            .filter_map(|entry| {
                let repo = GitRepo::discover(&entry.path()).ok()?;
                Some(Member {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    branch: repo.current_branch(),
                    repo,
                })
            })
            .collect();
        if members.is_empty() {
            bail!(
                "{} is not inside a git repository and has no repositories directly under it",
                path.display()
            );
        }
        members.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self::from_members(members))
    }

    fn from_members(members: Vec<Member>) -> Self {
        Workspace {
            members: std::sync::Arc::new(members),
        }
    }

    /// Whether this spans several repositories (paths are prefixed).
    pub fn is_multi(&self) -> bool {
        !self.members[0].name.is_empty()
    }

    /// Branch for the status bar: the checked-out branch, or the
    /// repository count in multi-repo mode.
    pub fn branch_label(&self) -> String {
        if self.is_multi() {
            format!("{} repos", self.members.len())
        } else {
            self.members[0].branch.clone()
        }
    }

    /// `name: branch` of the repository holding `path`; `None` for a
    /// single repository.
    pub fn repo_label(&self, path: &str) -> Option<String> {
        if !self.is_multi() {
            return None;
        }
        let (member, _) = self.member_for(path)?;
        Some(format!("{}: {}", member.name, member.branch))
    }

    /// Default base branch. In multi-repo mode, the one most repositories
    /// use; each repository still falls back to its own default when it
    /// lacks the chosen base (see [`Workspace::member_mode`]).
    pub fn default_base(&self) -> String {
        if !self.is_multi() {
            return self.members[0].repo.default_base();
        }
        let mut counts: HashMap<String, usize> = HashMap::new();
        for member in self.members.iter() {
            *counts.entry(member.repo.default_base()).or_default() += 1;
        }
        // Ties break by name so the choice is stable across runs.
        counts
            .into_iter()
            .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
            .map(|(base, _)| base)
            .unwrap_or_else(|| "main".to_string())
    }

    /// Local branches across all repositories, sorted and deduplicated.
    pub fn local_branches(&self) -> Result<Vec<String>> {
        if !self.is_multi() {
            return self.members[0].repo.local_branches();
        }
        let all: BTreeSet<String> = self
            .members
            .iter()
            .filter_map(|m| m.repo.local_branches().ok())
            .flatten()
            .collect();
        Ok(all.into_iter().collect())
    }

    /// Changed files for `mode`. In multi-repo mode, paths are prefixed
    /// with the repository name, and repositories that fail (e.g. no
    /// commits yet, so there is no HEAD to diff against) are left out;
    /// the error is returned only when every repository fails.
    pub fn changed_files(&self, mode: &DiffMode) -> Result<Vec<FileEntry>> {
        if !self.is_multi() {
            return self.members[0].repo.changed_files(mode);
        }
        let mut files = Vec::new();
        let mut first_err = None;
        let mut any_ok = false;
        for member in self.members.iter() {
            match member.repo.changed_files(&self.member_mode(member, mode)) {
                Ok(entries) => {
                    any_ok = true;
                    files.extend(entries.into_iter().map(|e| prefix_entry(&member.name, e)));
                }
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        match first_err {
            Some(e) if !any_ok => Err(e),
            _ => Ok(files),
        }
    }

    /// (old_content, new_content) of `entry` for `mode`.
    pub fn file_versions(&self, mode: &DiffMode, entry: &FileEntry) -> Result<(String, String)> {
        let Some((member, local)) = self.member_for(&entry.path) else {
            bail!("'{}' is not in any repository of the workspace", entry.path);
        };
        let mut local_entry = entry.clone();
        local_entry.path = local.to_string();
        if let FileStatus::Renamed { from } = &mut local_entry.status {
            if let Some((_, from_local)) = self.member_for(from) {
                *from = from_local.to_string();
            }
        }
        member
            .repo
            .file_versions(&self.member_mode(member, mode), &local_entry)
    }

    /// Unified `git diff` text of one file for `mode`.
    pub fn unified_diff(&self, mode: &DiffMode, path: &str) -> Result<String> {
        let Some((member, local)) = self.member_for(path) else {
            bail!("'{path}' is not in any repository of the workspace");
        };
        member
            .repo
            .unified_diff(&self.member_mode(member, mode), Some(local))
    }

    /// Changed-line contents per (prefixed) file path for `mode`. Failing
    /// repositories are skipped as in [`Workspace::changed_files`].
    pub fn changed_lines(&self, mode: &DiffMode) -> Result<HashMap<String, Vec<String>>> {
        if !self.is_multi() {
            let diff = self.members[0].repo.unified_diff(mode, None)?;
            return Ok(search::parse_changed_lines(&diff));
        }
        let mut out = HashMap::new();
        for member in self.members.iter() {
            let Ok(diff) = member.repo.unified_diff(&self.member_mode(member, mode), None) else {
                continue;
            };
            out.extend(
                search::parse_changed_lines(&diff)
                    .into_iter()
                    .map(|(path, lines)| (format!("{}/{path}", member.name), lines)),
            );
        }
        Ok(out)
    }

    /// Combined fingerprint of every repository's state.
    pub fn state_fingerprint(&self) -> u64 {
        if !self.is_multi() {
            return self.members[0].repo.state_fingerprint();
        }
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for member in self.members.iter() {
            member.repo.state_fingerprint().hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Every repository with its display name (the folder name, also for
    /// a single repository) and checked-out branch, in workspace order.
    pub fn repos(&self) -> Vec<(String, GitRepo, String)> {
        self.members
            .iter()
            .map(|m| {
                let name = if m.name.is_empty() {
                    m.repo
                        .root()
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                } else {
                    m.name.clone()
                };
                (name, m.repo.clone(), m.branch.clone())
            })
            .collect()
    }

    /// Local clone to read PR commits from: the repository itself, or in
    /// multi-repo mode the one whose folder matches the PR repository's
    /// name (`owner/name`). Commits it lacks are fetched from GitHub.
    pub fn local_for(&self, full_name: &str) -> Option<GitRepo> {
        if !self.is_multi() {
            return Some(self.members[0].repo.clone());
        }
        let name = full_name.rsplit('/').next()?;
        self.members
            .iter()
            .find(|m| m.name == name)
            .map(|m| m.repo.clone())
    }

    /// The repository holding `path`, and `path` relative to its root.
    fn member_for<'a>(&self, path: &'a str) -> Option<(&Member, &'a str)> {
        if !self.is_multi() {
            return Some((&self.members[0], path));
        }
        let (name, rest) = path.split_once('/')?;
        let member = self.members.iter().find(|m| m.name == name)?;
        Some((member, rest))
    }

    /// `mode` as applied to one repository: in multi-repo mode, a base
    /// branch the repository lacks becomes its own default base.
    fn member_mode(&self, member: &Member, mode: &DiffMode) -> DiffMode {
        match mode {
            DiffMode::BranchToBase { base } if self.is_multi() && !member.repo.has_ref(base) => {
                DiffMode::BranchToBase {
                    base: member.repo.default_base(),
                }
            }
            _ => mode.clone(),
        }
    }
}

/// Prefix `entry`'s paths (including a rename source) with `name/`.
fn prefix_entry(name: &str, mut entry: FileEntry) -> FileEntry {
    entry.path = format!("{name}/{}", entry.path);
    if let FileStatus::Renamed { from } = &mut entry.status {
        *from = format!("{name}/{from}");
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main"])
            .args(args)
            .output()
            .expect("run git");
        assert!(status.status.success(), "git {args:?} failed");
    }

    /// A temp folder with repos `a` (on main) and `b` (on master), each
    /// with one committed and one modified file, plus a plain folder.
    fn fixture(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("gispect-ws-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (name, branch) in [("a", "main"), ("b", "master")] {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            git(&dir, &["init", "-q", "-b", branch]);
            std::fs::write(dir.join("f.txt"), "one\n").unwrap();
            git(&dir, &["add", "."]);
            git(&dir, &["commit", "-q", "-m", "init"]);
            std::fs::write(dir.join("f.txt"), format!("one\n{name}\n")).unwrap();
        }
        std::fs::create_dir_all(root.join("plain")).unwrap();
        root
    }

    #[test]
    fn discovers_repos_under_a_plain_folder() {
        let root = fixture("discover");
        let ws = Workspace::discover(&root).unwrap();
        assert!(ws.is_multi());
        assert_eq!(ws.branch_label(), "2 repos");

        let files = ws.changed_files(&DiffMode::WorkingTree).unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["a/f.txt", "b/f.txt"]);
        assert_eq!(ws.repo_label("b/f.txt").as_deref(), Some("b: master"));

        let (old, new) = ws.file_versions(&DiffMode::WorkingTree, &files[1]).unwrap();
        assert_eq!((old.as_str(), new.as_str()), ("one\n", "one\nb\n"));

        let lines = ws.changed_lines(&DiffMode::WorkingTree).unwrap();
        assert_eq!(lines["a/f.txt"], vec!["a".to_string()]);
        assert!(ws.unified_diff(&DiffMode::WorkingTree, "b/f.txt").unwrap().contains("+b"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_base_falls_back_per_repo() {
        let root = fixture("base");
        let ws = Workspace::discover(&root).unwrap();
        // `b` has no `main`; it compares against its own `master` instead
        // of failing the whole list.
        let mode = DiffMode::BranchToBase {
            base: "main".to_string(),
        };
        assert!(ws.changed_files(&mode).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn inside_a_repo_stays_single() {
        let root = fixture("single");
        let ws = Workspace::discover(&root.join("a")).unwrap();
        assert!(!ws.is_multi());
        assert_eq!(ws.branch_label(), "main");
        let files = ws.changed_files(&DiffMode::WorkingTree).unwrap();
        assert_eq!(files[0].path, "f.txt");
        assert_eq!(ws.repo_label("f.txt"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn folder_without_repos_is_an_error() {
        let root = fixture("empty").join("plain");
        assert!(Workspace::discover(&root).is_err());
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }
}
