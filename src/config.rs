//! Persisted startup defaults and shared config-modal types.

use crate::types::DiffMode;
use serde::{Deserialize, Serialize};

/// Which diff mode to open in, without the repo-specific base branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModeKind {
    BranchToBase,
    WorkingTree,
    Staged,
    Unstaged,
}

impl ModeKind {
    /// Short human label for the config modal.
    pub fn label(self) -> &'static str {
        match self {
            ModeKind::BranchToBase => "branch vs base",
            ModeKind::WorkingTree => "working tree vs HEAD",
            ModeKind::Staged => "staged",
            ModeKind::Unstaged => "unstaged",
        }
    }

    /// Materialize into a full `DiffMode`, filling in `base` for `BranchToBase`.
    pub fn to_diff_mode(self, base: &str) -> DiffMode {
        match self {
            ModeKind::BranchToBase => DiffMode::BranchToBase {
                base: base.to_string(),
            },
            ModeKind::WorkingTree => DiffMode::WorkingTree,
            ModeKind::Staged => DiffMode::Staged,
            ModeKind::Unstaged => DiffMode::Unstaged,
        }
    }

    /// Inverse of `to_diff_mode`, dropping the `base` string.
    pub fn from_diff_mode(mode: &DiffMode) -> ModeKind {
        match mode {
            DiffMode::BranchToBase { .. } => ModeKind::BranchToBase,
            DiffMode::WorkingTree => ModeKind::WorkingTree,
            DiffMode::Staged => ModeKind::Staged,
            DiffMode::Unstaged => ModeKind::Unstaged,
        }
    }

    /// Cycle to the next (or previous, if `!forward`) mode, wrapping in
    /// declaration order: BranchToBase -> WorkingTree -> Staged -> Unstaged.
    pub fn cycle(self, forward: bool) -> ModeKind {
        const ORDER: [ModeKind; 4] = [
            ModeKind::BranchToBase,
            ModeKind::WorkingTree,
            ModeKind::Staged,
            ModeKind::Unstaged,
        ];
        let len = ORDER.len();
        let idx = ORDER.iter().position(|m| *m == self).unwrap_or(0);
        let next_idx = if forward {
            (idx + 1) % len
        } else {
            (idx + len - 1) % len
        };
        ORDER[next_idx]
    }
}

/// Startup defaults, stored as JSON in the user config dir.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub default_mode: ModeKind,
    pub compact: bool,
    pub syntax: bool,
    pub show_files: bool,
    pub tree_view: bool,
    pub wide_files: bool,
    pub show_old: bool,
    pub show_new: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            default_mode: ModeKind::BranchToBase,
            compact: false,
            syntax: true,
            show_files: true,
            tree_view: false,
            wide_files: false,
            show_old: true,
            show_new: true,
        }
    }
}

impl Config {
    /// Load from disk; missing, unreadable, or invalid file falls back to defaults. Never errors.
    pub fn load() -> Config {
        let Some(path) = config_path() else {
            return Config::default();
        };
        let Ok(contents) = std::fs::read_to_string(path) else {
            return Config::default();
        };
        Self::from_json(&contents)
    }

    /// Deserialize from a JSON string, falling back to `Config::default()` on any
    /// parse error. Factored out of `load()` so the fallback path is unit-testable
    /// without touching the real config dir.
    fn from_json(s: &str) -> Config {
        serde_json::from_str(s).unwrap_or_default()
    }

    /// Save as pretty JSON, creating parent directories. Errors if no config dir is resolvable.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = config_path().ok_or_else(|| anyhow::anyhow!("no resolvable config dir"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Flip a bool field, or cycle `DefaultMode` (forward tells cycle direction).
    pub fn toggle(&mut self, field: ConfigField, forward: bool) {
        match field {
            ConfigField::DefaultMode => {
                self.default_mode = self.default_mode.cycle(forward);
            }
            ConfigField::Compact => self.compact = !self.compact,
            ConfigField::Syntax => self.syntax = !self.syntax,
            ConfigField::ShowFiles => self.show_files = !self.show_files,
            ConfigField::TreeView => self.tree_view = !self.tree_view,
            ConfigField::WideFiles => self.wide_files = !self.wide_files,
            ConfigField::ShowOld => self.show_old = !self.show_old,
            ConfigField::ShowNew => self.show_new = !self.show_new,
        }
    }

    /// Display value for the modal: "on"/"off" for bools, the mode label for DefaultMode.
    pub fn value_label(&self, field: ConfigField) -> String {
        match field {
            ConfigField::DefaultMode => self.default_mode.label().to_string(),
            ConfigField::Compact => bool_label(self.compact),
            ConfigField::Syntax => bool_label(self.syntax),
            ConfigField::ShowFiles => bool_label(self.show_files),
            ConfigField::TreeView => bool_label(self.tree_view),
            ConfigField::WideFiles => bool_label(self.wide_files),
            ConfigField::ShowOld => bool_label(self.show_old),
            ConfigField::ShowNew => bool_label(self.show_new),
        }
    }
}

fn bool_label(v: bool) -> String {
    if v { "on".to_string() } else { "off".to_string() }
}

/// $XDG_CONFIG_HOME/gispect/config.json, else $HOME/.config/gispect/config.json; None if neither env var is set.
pub fn config_path() -> Option<std::path::PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(std::path::PathBuf::from(xdg).join("gispect").join("config.json"));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        if !home.is_empty() {
            return Some(
                std::path::PathBuf::from(home)
                    .join(".config")
                    .join("gispect")
                    .join("config.json"),
            );
        }
    }
    None
}

/// One editable row of the config modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigField {
    DefaultMode,
    Compact,
    Syntax,
    ShowFiles,
    TreeView,
    WideFiles,
    ShowOld,
    ShowNew,
}

impl ConfigField {
    pub const ALL: [ConfigField; 8] = [
        ConfigField::DefaultMode,
        ConfigField::Compact,
        ConfigField::Syntax,
        ConfigField::ShowFiles,
        ConfigField::TreeView,
        ConfigField::WideFiles,
        ConfigField::ShowOld,
        ConfigField::ShowNew,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ConfigField::DefaultMode => "default mode",
            ConfigField::Compact => "compact view",
            ConfigField::Syntax => "syntax highlighting",
            ConfigField::ShowFiles => "file panel",
            ConfigField::TreeView => "tree view",
            ConfigField::WideFiles => "wide file panel",
            ConfigField::ShowOld => "old pane",
            ConfigField::ShowNew => "new pane",
        }
    }
}

/// In-progress edit of the config, driven by the app's key handler.
pub struct ConfigModal {
    pub selected: usize,
    pub draft: Config,
}

impl ConfigModal {
    pub fn new(draft: Config) -> Self {
        ConfigModal { selected: 0, draft }
    }

    /// Move selection down, wrapping.
    pub fn next(&mut self) {
        self.selected = (self.selected + 1) % ConfigField::ALL.len();
    }

    /// Move selection up, wrapping.
    pub fn prev(&mut self) {
        self.selected = (self.selected + ConfigField::ALL.len() - 1) % ConfigField::ALL.len();
    }

    pub fn field(&self) -> ConfigField {
        ConfigField::ALL[self.selected]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_values_are_exact() {
        let c = Config::default();
        assert_eq!(c.default_mode, ModeKind::BranchToBase);
        assert_eq!(c.compact, false);
        assert_eq!(c.syntax, true);
        assert_eq!(c.show_files, true);
        assert_eq!(c.tree_view, false);
        assert_eq!(c.wide_files, false);
        assert_eq!(c.show_old, true);
        assert_eq!(c.show_new, true);
    }

    #[test]
    fn serde_roundtrip() {
        let c = Config {
            default_mode: ModeKind::Staged,
            compact: true,
            syntax: false,
            show_files: false,
            tree_view: true,
            wide_files: true,
            show_old: false,
            show_new: true,
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn partial_json_fills_defaults() {
        let c = Config::from_json(r#"{"compact": true}"#);
        assert_eq!(c.compact, true);
        let d = Config::default();
        assert_eq!(c.default_mode, d.default_mode);
        assert_eq!(c.syntax, d.syntax);
        assert_eq!(c.show_files, d.show_files);
        assert_eq!(c.tree_view, d.tree_view);
        assert_eq!(c.wide_files, d.wide_files);
        assert_eq!(c.show_old, d.show_old);
        assert_eq!(c.show_new, d.show_new);
    }

    #[test]
    fn invalid_json_falls_back_to_default() {
        let c = Config::from_json("{ not json");
        assert_eq!(c, Config::default());
    }

    #[test]
    fn empty_json_falls_back_to_default() {
        let c = Config::from_json("");
        assert_eq!(c, Config::default());
    }

    #[test]
    fn mode_kind_cycle_wraps_forward() {
        assert_eq!(ModeKind::BranchToBase.cycle(true), ModeKind::WorkingTree);
        assert_eq!(ModeKind::WorkingTree.cycle(true), ModeKind::Staged);
        assert_eq!(ModeKind::Staged.cycle(true), ModeKind::Unstaged);
        assert_eq!(ModeKind::Unstaged.cycle(true), ModeKind::BranchToBase);
    }

    #[test]
    fn mode_kind_cycle_wraps_backward() {
        assert_eq!(ModeKind::BranchToBase.cycle(false), ModeKind::Unstaged);
        assert_eq!(ModeKind::Unstaged.cycle(false), ModeKind::Staged);
        assert_eq!(ModeKind::Staged.cycle(false), ModeKind::WorkingTree);
        assert_eq!(ModeKind::WorkingTree.cycle(false), ModeKind::BranchToBase);
    }

    #[test]
    fn diff_mode_roundtrip_all_variants() {
        for kind in [
            ModeKind::BranchToBase,
            ModeKind::WorkingTree,
            ModeKind::Staged,
            ModeKind::Unstaged,
        ] {
            let dm = kind.to_diff_mode("main");
            assert_eq!(ModeKind::from_diff_mode(&dm), kind);
        }
        match ModeKind::BranchToBase.to_diff_mode("develop") {
            DiffMode::BranchToBase { base } => assert_eq!(base, "develop"),
            other => panic!("expected BranchToBase, got {other:?}"),
        }
    }

    #[test]
    fn toggle_flips_bool_and_cycles_mode() {
        let mut c = Config::default();
        c.toggle(ConfigField::Compact, true);
        assert_eq!(c.compact, true);
        c.toggle(ConfigField::Compact, true);
        assert_eq!(c.compact, false);

        c.toggle(ConfigField::DefaultMode, true);
        assert_eq!(c.default_mode, ModeKind::WorkingTree);
        c.toggle(ConfigField::DefaultMode, false);
        assert_eq!(c.default_mode, ModeKind::BranchToBase);
    }

    #[test]
    fn value_label_formats() {
        let c = Config::default();
        assert_eq!(c.value_label(ConfigField::Syntax), "on");
        assert_eq!(c.value_label(ConfigField::Compact), "off");
        assert_eq!(c.value_label(ConfigField::DefaultMode), "branch vs base");
    }

    #[test]
    fn config_modal_navigation_wraps() {
        let mut m = ConfigModal::new(Config::default());
        assert_eq!(m.selected, 0);
        assert_eq!(m.field(), ConfigField::DefaultMode);
        m.prev();
        assert_eq!(m.selected, ConfigField::ALL.len() - 1);
        assert_eq!(m.field(), ConfigField::ShowNew);
        m.next();
        assert_eq!(m.selected, 0);
    }
}
