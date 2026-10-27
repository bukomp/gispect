//! On-disk copy of recent PR list searches, so the list shows the last
//! known PRs at once while fresh searches run. Holds no tokens; the file
//! is private to the user.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::gh::{self, PrSummary};
use crate::pr::{PrTab, StateFilter};

/// Searches kept on disk; the oldest go first.
const MAX_ENTRIES: usize = 48;

/// Order of saves, so a slow writer never replaces a newer snapshot.
static SAVES: AtomicU64 = AtomicU64::new(0);
/// The newest save written so far; held while writing.
static WRITTEN: Mutex<u64> = Mutex::new(0);

/// One cached search: who ran it, what it asked, and what it found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub host: String,
    pub login: String,
    pub tab: PrTab,
    pub state: StateFilter,
    /// Repositories the search was limited to; `None` when unscoped.
    pub repos: Option<Vec<String>>,
    /// Unix seconds when it was saved, for dropping the oldest.
    pub saved: u64,
    pub prs: Vec<PrSummary>,
}

impl Entry {
    fn same_search(&self, other: &Entry) -> bool {
        self.host == other.host
            && self.login == other.login
            && self.tab == other.tab
            && self.state == other.state
            && self.repos == other.repos
    }
}

#[derive(Debug, Default, Clone)]
pub struct Cache {
    entries: Vec<Entry>,
}

impl Cache {
    /// Read the cache file; empty when missing or unreadable.
    pub fn load() -> Cache {
        let Some(path) = cache_path() else {
            return Cache::default();
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return Cache::default();
        };
        Cache::from_json(&text)
    }

    fn from_json(text: &str) -> Cache {
        let mut entries: Vec<Entry> = serde_json::from_str(text).unwrap_or_default();
        // The file is outside the app's control: clean it like a response.
        for entry in &mut entries {
            entry.prs = std::mem::take(&mut entry.prs)
                .into_iter()
                .filter_map(gh::clean_summary)
                .collect();
        }
        Cache { entries }
    }

    /// The PRs `key`'s search found last time, if cached.
    pub fn get(&self, key: &Entry) -> Option<&[PrSummary]> {
        self.entries
            .iter()
            .find(|e| e.same_search(key))
            .map(|e| e.prs.as_slice())
    }

    /// Store `entry`, replacing the same search and dropping the oldest
    /// beyond [`MAX_ENTRIES`].
    pub fn put(&mut self, entry: Entry) {
        self.entries.retain(|e| !e.same_search(&entry));
        self.entries.push(entry);
        if self.entries.len() > MAX_ENTRIES {
            self.entries.sort_by_key(|e| std::cmp::Reverse(e.saved));
            self.entries.truncate(MAX_ENTRIES);
        }
    }

    /// Drop searches of accounts not in `accounts` (`(host, login)`), so
    /// a logged-out account's PRs don't stay on disk.
    pub fn retain_accounts(&mut self, accounts: &[(String, String)]) {
        self.entries
            .retain(|e| accounts.iter().any(|(h, l)| *h == e.host && *l == e.login));
    }

    /// Write the cache file on a background thread. Failures are ignored:
    /// the cache only saves time.
    pub fn save_in_background(&self) {
        let Some(path) = cache_path() else {
            return;
        };
        let Ok(json) = serde_json::to_vec(&self.entries) else {
            return;
        };
        let seq = SAVES.fetch_add(1, Ordering::Relaxed) + 1;
        std::thread::spawn(move || {
            let mut written = WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
            if seq > *written && write_private(&path, &json).is_ok() {
                *written = seq;
            }
        });
    }
}

/// Write via a temporary file and rename, so a reader never sees half a
/// file; on Unix only the user may read it or list its folder.
fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        let mut dir = std::fs::DirBuilder::new();
        dir.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            dir.mode(0o700);
        }
        dir.create(parent)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    // Never follow or reuse something already at the temporary name.
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = match options.open(&tmp) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Left over from a crashed run with the same pid.
            std::fs::remove_file(&tmp)?;
            options.open(&tmp)?
        }
        result => result?,
    };
    file.write_all(data)?;
    drop(file);
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// $XDG_CACHE_HOME/gispect/prs.json, else $HOME/.cache/gispect/prs.json.
/// A relative path is ignored, as the XDG spec requires.
fn cache_path() -> Option<PathBuf> {
    let absolute = |v: std::ffi::OsString| Some(PathBuf::from(v)).filter(|p| p.is_absolute());
    let base = std::env::var_os("XDG_CACHE_HOME")
        .and_then(absolute)
        .or_else(|| Some(absolute(std::env::var_os("HOME")?)?.join(".cache")))?;
    Some(base.join("gispect").join("prs.json"))
}

/// Unix seconds now.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(login: &str, saved: u64, title: &str) -> Entry {
        Entry {
            host: "github.com".into(),
            login: login.into(),
            tab: PrTab::Authored,
            state: StateFilter::Open,
            repos: Some(vec!["o/r".into()]),
            saved,
            prs: vec![PrSummary {
                number: 1,
                title: title.into(),
                author: "me".into(),
                repo: "o/r".into(),
                ..Default::default()
            }],
        }
    }

    #[test]
    fn round_trips_and_replaces_the_same_search() {
        let mut cache = Cache::default();
        cache.put(entry("a", 1, "old"));
        cache.put(entry("a", 2, "new"));
        cache.put(entry("b", 3, "other"));
        let json = serde_json::to_string(&cache.entries).unwrap();
        let back = Cache::from_json(&json);
        assert_eq!(back.entries.len(), 2);
        let got = back.get(&entry("a", 0, "")).unwrap();
        assert_eq!(got[0].title, "new");
        assert_eq!(got[0].author, "me");
        assert_eq!(got[0].repo, "o/r");
    }

    #[test]
    fn keeps_the_newest_and_cleans_loaded_text() {
        let mut cache = Cache::default();
        for i in 0..(MAX_ENTRIES as u64 + 5) {
            cache.put(entry(&format!("u{i}"), i, "t"));
        }
        assert_eq!(cache.entries.len(), MAX_ENTRIES);
        assert!(cache.get(&entry("u0", 0, "")).is_none());

        let mut bad = entry("a", 1, "evil\u{1b}[2J");
        bad.prs.push(PrSummary {
            repo: "not a repo".into(),
            ..Default::default()
        });
        let json = serde_json::to_string(&vec![bad]).unwrap();
        let back = Cache::from_json(&json);
        let prs = back.get(&entry("a", 0, "")).unwrap();
        assert_eq!(prs.len(), 1);
        assert!(!prs[0].title.contains('\u{1b}'));
    }
}
