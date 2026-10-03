//! The `.aneural/` directory: paths, discovery, init, and the small JSON files.

use crate::config::{Config, NodeTypeDef, builtin_node_types};
use crate::focus::Focus;
use crate::{Error, Result};
use std::path::{Path, PathBuf};

pub const ANEURAL_DIR: &str = ".aneural";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// A workspace rooted at `root` (which may or may not have `.aneural/` yet).
    /// The root is canonicalised when it exists so that resolved import paths
    /// (which tools like oxc_resolver return canonicalised) compare equal.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        let root: PathBuf = root.into();
        let root = root.canonicalize().unwrap_or(root);
        Workspace { root }
    }

    /// Walk up from `start` to the nearest directory containing `.aneural/`.
    pub fn find(start: impl AsRef<Path>) -> Result<Self> {
        let start = start.as_ref();
        let mut cur = Some(if start.is_absolute() {
            start.to_path_buf()
        } else {
            std::env::current_dir()?.join(start)
        });
        while let Some(dir) = cur {
            if dir.join(ANEURAL_DIR).is_dir() {
                return Ok(Workspace::at(dir));
            }
            cur = dir.parent().map(Path::to_path_buf);
        }
        Err(Error::NoWorkspace(start.display().to_string()))
    }

    pub fn exists(&self) -> bool {
        self.aneural_dir().is_dir()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn aneural_dir(&self) -> PathBuf {
        self.root.join(ANEURAL_DIR)
    }
    pub fn config_path(&self) -> PathBuf {
        self.aneural_dir().join("config.json")
    }
    pub fn nodes_dir(&self) -> PathBuf {
        self.aneural_dir().join("nodes")
    }
    pub fn spores_dir(&self) -> PathBuf {
        self.aneural_dir().join("spores")
    }
    pub fn notes_dir(&self) -> PathBuf {
        self.aneural_dir().join("notes")
    }
    pub fn plans_dir(&self) -> PathBuf {
        self.aneural_dir().join("plans")
    }
    pub fn icebox_dir(&self) -> PathBuf {
        self.aneural_dir().join("icebox")
    }
    pub fn state_dir(&self) -> PathBuf {
        self.aneural_dir().join("state")
    }
    pub fn focus_path(&self) -> PathBuf {
        self.state_dir().join("focus.json")
    }
    pub fn layout_path(&self) -> PathBuf {
        self.state_dir().join("layout.json")
    }
    pub fn cache_dir(&self) -> PathBuf {
        self.aneural_dir().join("cache")
    }
    pub fn db_path(&self) -> PathBuf {
        self.cache_dir().join("index.db")
    }
    /// The run journal: committed, append-only, one directory per machine.
    ///
    /// Outside `cache/` and `state/` on purpose — this is the one part of the
    /// run story that is meant to travel, so a colleague pulling the repository
    /// sees that the nightly job has been failing since Tuesday.
    pub fn runs_dir(&self) -> PathBuf {
        self.aneural_dir().join("runs")
    }
    /// The index built from the journal. Derived, gitignored, and deliberately
    /// its own file rather than a table in [`Workspace::db_path`]: that one is
    /// dropped whenever the graph's shape changes, and run history must not be
    /// lost because a node kind was added.
    pub fn runs_db_path(&self) -> PathBuf {
        self.cache_dir().join("runs.db")
    }
    pub fn gitattributes_path(&self) -> PathBuf {
        self.aneural_dir().join(".gitattributes")
    }

    /// Workspace-relative canonical path for an absolute path inside the workspace.
    pub fn rel(&self, abs: &Path) -> Option<String> {
        abs.strip_prefix(&self.root)
            .ok()
            .map(crate::id::canonical_rel)
    }

    pub fn abs(&self, rel: &str) -> PathBuf {
        if rel == "." {
            self.root.clone()
        } else {
            self.root.join(rel)
        }
    }

    /// Create `.aneural/` with a default config. Errors if it exists unless `force`.
    pub fn init(&self, name: Option<&str>, force: bool) -> Result<Config> {
        let dir = self.aneural_dir();
        if dir.exists() && !force && self.config_path().exists() {
            return Err(Error::Invalid(format!(
                "{} already exists (use --force to overwrite config)",
                dir.display()
            )));
        }
        for d in [
            dir.clone(),
            self.nodes_dir(),
            self.spores_dir(),
            self.notes_dir(),
            self.plans_dir(),
            self.icebox_dir(),
            self.state_dir(),
            self.cache_dir(),
        ] {
            std::fs::create_dir_all(&d)?;
        }
        let config = Config {
            name: name
                .map(String::from)
                .or_else(|| {
                    self.root
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                })
                .unwrap_or_else(|| "workspace".into()),
            ..Config::default()
        };
        self.save_config(&config)?;
        let gi = dir.join(".gitignore");
        if !gi.exists() {
            std::fs::write(&gi, "cache/\nstate/\n")?;
        }
        write_gitattributes(&self.gitattributes_path())?;
        let icebox = self.icebox_dir().join("ideas.md");
        if !icebox.exists() {
            std::fs::write(
                &icebox,
                "# Icebox\n\nEach `## heading` below becomes an Idea node.\n",
            )?;
        }
        Ok(config)
    }

    pub fn load_config(&self) -> Result<Config> {
        let path = self.config_path();
        if !path.exists() {
            return Ok(Config {
                name: self
                    .root
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                ..Config::default()
            });
        }
        let text = std::fs::read_to_string(&path)?;
        Ok(serde_json::from_str(&text)?)
    }

    pub fn save_config(&self, config: &Config) -> Result<()> {
        std::fs::create_dir_all(self.aneural_dir())?;
        write_atomic(&self.config_path(), &serde_json::to_vec_pretty(config)?)
    }

    pub fn read_focus(&self) -> Result<Option<Focus>> {
        let path = self.focus_path();
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)?;
        Ok(Some(serde_json::from_str(&text)?))
    }

    pub fn write_focus(&self, focus: &Focus) -> Result<()> {
        std::fs::create_dir_all(self.state_dir())?;
        write_atomic(&self.focus_path(), &serde_json::to_vec_pretty(focus)?)
    }

    /// Node type definitions: builtins, then `.aneural/nodes/*.json`, then
    /// `config.nodeTypes` overrides. Spore-provided kinds are merged by the
    /// engine via [`Workspace::compose_node_types`].
    pub fn load_node_types(&self, config: &Config) -> Result<Vec<NodeTypeDef>> {
        self.compose_node_types(config, Vec::new())
    }

    /// Layer node types: builtins < `extra` (spores) < `.aneural/nodes/*.json` < config overrides.
    pub fn compose_node_types(
        &self,
        config: &Config,
        extra: Vec<NodeTypeDef>,
    ) -> Result<Vec<NodeTypeDef>> {
        let mut defs = builtin_node_types();
        for def in extra {
            merge_node_type(&mut defs, def);
        }
        for def in self.workspace_node_types()? {
            merge_node_type(&mut defs, def);
        }
        apply_node_type_overrides(&mut defs, config);
        Ok(defs)
    }

    /// Definitions declared in `.aneural/nodes/*.json`.
    pub fn workspace_node_types(&self) -> Result<Vec<NodeTypeDef>> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(self.nodes_dir()) {
            let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
            entries.sort();
            for path in entries {
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let text = std::fs::read_to_string(&path)?;
                let mut def: NodeTypeDef = serde_json::from_str(&text)
                    .map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?;
                def.provider = "workspace".into();
                if def.label.is_empty() {
                    def.label = def.kind.clone();
                }
                out.push(def);
            }
        }
        Ok(out)
    }
}

/// Apply `config.nodeTypes` style overrides, creating lightweight kinds as needed.
pub fn apply_node_type_overrides(defs: &mut Vec<NodeTypeDef>, config: &Config) {
    for (kind, style) in &config.node_types {
        if let Some(def) = defs.iter_mut().find(|d| &d.kind == kind) {
            def.apply(style);
        } else {
            let mut def = NodeTypeDef::new(
                kind,
                kind,
                NodeTypeDef::FALLBACK_ICON,
                "#9aa0a6",
                "circle",
                "",
            );
            def.provider = "workspace".into();
            def.apply(style);
            defs.push(def);
        }
    }
}

/// Replace an existing definition of the same kind or append.
pub fn merge_node_type(defs: &mut Vec<NodeTypeDef>, def: NodeTypeDef) {
    if let Some(existing) = defs.iter_mut().find(|d| d.kind == def.kind) {
        *existing = def;
    } else {
        defs.push(def);
    }
}

/// The merge rule the run journal needs, written into `.aneural/.gitattributes`.
///
/// `merge=union` is built into git and it is *correct* here rather than merely
/// convenient: the journal is append-only, so two machines' lines are both true
/// and keeping all of them is the right resolution. Duplicates that a union
/// merge leaves behind are collapsed when the journal is indexed, which is why
/// a run id is derived from its content rather than randomly assigned.
///
/// Only ever adds the line, never rewrites the file: a workspace may have put
/// its own rules here.
pub fn write_gitattributes(path: &Path) -> Result<()> {
    const RULE: &str = "runs/**/*.jsonl merge=union";
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == RULE) {
        return Ok(());
    }
    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if out.is_empty() {
        out.push_str("# The run journal is append-only: every machine's lines are true.\n");
    }
    out.push_str(RULE);
    out.push('\n');
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, out)?;
    Ok(())
}

/// Write via temp file + rename so readers never observe a partial file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Invalid(format!("no parent for {}", path.display())))?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id()
    ));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_find_and_focus_round_trip() {
        let tmp = std::env::temp_dir().join(format!("aneural-core-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("a/b")).unwrap();
        let ws = Workspace::at(&tmp);
        assert!(Workspace::find(tmp.join("a/b")).is_err());
        let cfg = ws.init(Some("test"), false).unwrap();
        assert_eq!(cfg.name, "test");
        assert!(ws.init(None, false).is_err());
        let found = Workspace::find(tmp.join("a/b")).unwrap();
        assert_eq!(found.root(), tmp.canonicalize().unwrap().as_path());
        assert_eq!(found.load_config().unwrap().name, "test");

        assert!(ws.read_focus().unwrap().is_none());
        let f = Focus::new(tmp.to_string_lossy());
        ws.write_focus(&f).unwrap();
        assert_eq!(ws.read_focus().unwrap().unwrap(), f);

        std::fs::write(
            ws.nodes_dir().join("decision.json"),
            r##"{"kind":"Decision","icon":"LuScale","color":"#8ab4f8"}"##,
        )
        .unwrap();
        let types = ws.load_node_types(&cfg).unwrap();
        let d = types.iter().find(|t| t.kind == "Decision").unwrap();
        assert_eq!(d.label, "Decision");
        assert_eq!(d.provider, "workspace");
        assert_eq!(
            ws.rel(&tmp.canonicalize().unwrap().join("a/b")).as_deref(),
            Some("a/b")
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
