//! Filesystem walk → structural nodes (Directory/Repo/File/Manifest) and CONTAINS edges.

use aneural_core::config::{Config, Language};
use aneural_core::kinds::{EdgeKind, NodeKind, Source};
use aneural_core::{Edge, Node, NodeId, Workspace};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use std::path::{Path, PathBuf};

/// Files parsed as dependency manifests (by file name).
pub const MANIFEST_NAMES: &[&str] = &[
    "package.json",
    "Cargo.toml",
    "pyproject.toml",
    "requirements.txt",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "composer.json",
    "Gemfile",
];

/// Files larger than this are indexed as nodes but never parsed.
pub const MAX_PARSE_BYTES: u64 = 2 * 1024 * 1024;

pub fn is_manifest_name(name: &str) -> bool {
    MANIFEST_NAMES.contains(&name)
}

/// One entry discovered by the walker.
#[derive(Clone, Debug)]
pub struct Entry {
    pub abs: PathBuf,
    pub rel: String,
    pub is_dir: bool,
    pub is_repo: bool,
    /// An Obsidian vault root. A directory can be this *and* a repo, which is
    /// why it is a property rather than a kind.
    pub is_vault: bool,
    pub size: u64,
    pub mtime: i64,
}

pub fn is_repo_root(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// Does this directory hold an Obsidian vault?
///
/// `.obsidian/` itself is UI state and holds no knowledge — the notes do. Its
/// value is the one bit it carries: the markdown here is the subject matter
/// rather than documentation about code.
pub fn is_vault_root(dir: &Path) -> bool {
    dir.join(".obsidian").is_dir()
}

/// Whether the vault's own graph view is set to hide unresolved links.
///
/// The only preference in `.obsidian/` that has an Aneural equivalent, and the
/// user already answered it once.
pub fn vault_hides_unresolved(dir: &Path) -> Option<bool> {
    let text = std::fs::read_to_string(dir.join(".obsidian").join("graph.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("hideUnresolved")?.as_bool()
}

/// Build the ignore-aware walker for one root.
pub fn walker(ws: &Workspace, config: &Config, root: &Path) -> Result<ignore::Walk, ignore::Error> {
    let mut ob = OverrideBuilder::new(root);
    for pat in &config.ignore {
        ob.add(&format!("!{pat}"))?;
    }
    // never descend into our own cache/state or git internals
    ob.add("!**/.git/**")?;
    ob.add("!**/.git")?;
    ob.add("!**/.DS_Store")?;
    let _ = ws; // (reserved for per-workspace ignore files)
    let overrides = ob.build()?;
    let mut wb = WalkBuilder::new(root);
    wb.hidden(false)
        .follow_links(false)
        .git_ignore(config.respect_gitignore)
        .git_global(false)
        .git_exclude(config.respect_gitignore)
        .require_git(false)
        .add_custom_ignore_filename(".aneuralignore")
        .overrides(overrides)
        .sort_by_file_name(|a, b| a.cmp(b));
    Ok(wb.build())
}

/// Is this workspace-relative path one we should never index?
pub fn is_internal(rel: &str) -> bool {
    rel == ".aneural/cache"
        || rel.starts_with(".aneural/cache/")
        || rel == ".aneural/state"
        || rel.starts_with(".aneural/state/")
        || rel.ends_with(".tmp")
}

/// Convert a walk entry to an [`Entry`], or None if it should be skipped.
pub fn entry_for(ws: &Workspace, path: &Path) -> Option<Entry> {
    let rel = ws.rel(path)?;
    if is_internal(&rel) {
        return None;
    }
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Some(Entry {
        abs: path.to_path_buf(),
        rel,
        is_dir: meta.is_dir(),
        is_repo: meta.is_dir() && is_repo_root(path),
        is_vault: meta.is_dir() && is_vault_root(path),
        size: meta.len(),
        mtime,
    })
}

/// Structural node for an entry. `repo` is the nearest enclosing repo (or the entry itself).
pub fn node_for(ws: &Workspace, config: &Config, entry: &Entry, repo: Option<&NodeId>) -> Node {
    let name = if entry.rel == "." {
        if config.name.is_empty() {
            ws.root()
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| ".".into())
        } else {
            config.name.clone()
        }
    } else {
        entry
            .rel
            .rsplit('/')
            .next()
            .unwrap_or(&entry.rel)
            .to_string()
    };
    if entry.is_dir {
        let kind = if entry.is_repo {
            NodeKind::REPO
        } else {
            NodeKind::DIRECTORY
        };
        let mut n = Node::new(NodeId::dir(&entry.rel), kind, name, Source::WALKER)
            .with_path(entry.rel.clone())
            .with_repo(repo.cloned());
        if entry.is_repo {
            n = n.with_prop("repo", true);
        }
        if entry.is_vault {
            n = n.with_prop("vault", true);
            if let Some(hide) = vault_hides_unresolved(&entry.abs) {
                n = n.with_prop("hideUnresolvedLinks", hide);
            }
        }
        n
    } else {
        let ext = entry
            .rel
            .rsplit('.')
            .next()
            .filter(|e| !entry.rel.ends_with(&format!("/{e}")) && *e != entry.rel)
            .map(|e| e.to_lowercase());
        let lang = ext.as_deref().and_then(Language::from_extension);
        let kind = if is_manifest_name(&name) {
            NodeKind::MANIFEST
        } else {
            NodeKind::FILE
        };
        let mut n = Node::new(NodeId::file(&entry.rel), kind, name, Source::WALKER)
            .with_path(entry.rel.clone())
            .with_repo(repo.cloned())
            .with_prop("size", entry.size as i64);
        if let Some(ext) = ext {
            n = n.with_prop("ext", ext);
        }
        if let Some(lang) = lang {
            n = n.with_prop("lang", lang);
        }
        n
    }
}

/// CONTAINS edge from the parent directory of `rel` (None for the root).
pub fn contains_edge(rel: &str) -> Option<Edge> {
    if rel == "." {
        return None;
    }
    let parent = rel.rsplit_once('/').map(|(p, _)| p).unwrap_or(".");
    let child = if Path::new(rel).is_dir() {
        NodeId::dir(rel)
    } else {
        NodeId::file(rel)
    };
    Some(Edge::new(
        EdgeKind::CONTAINS,
        NodeId::dir(parent),
        child,
        Source::WALKER,
    ))
}

pub fn contains_edge_for(rel: &str, is_dir: bool) -> Option<Edge> {
    if rel == "." {
        return None;
    }
    let parent = rel.rsplit_once('/').map(|(p, _)| p).unwrap_or(".");
    let child = if is_dir {
        NodeId::dir(rel)
    } else {
        NodeId::file(rel)
    };
    Some(Edge::new(
        EdgeKind::CONTAINS,
        NodeId::dir(parent),
        child,
        Source::WALKER,
    ))
}

/// Nearest repo id for a relative path given the set of known repo dirs.
pub fn repo_for(rel: &str, repos: &std::collections::BTreeSet<String>) -> Option<NodeId> {
    let mut cur = rel.to_string();
    loop {
        if repos.contains(&cur) {
            return Some(NodeId::dir(&cur));
        }
        if cur == "." {
            return None;
        }
        cur = cur
            .rsplit_once('/')
            .map(|(p, _)| p.to_string())
            .unwrap_or_else(|| ".".to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vault_is_recognised_and_can_also_be_a_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        assert!(!is_vault_root(dir));

        std::fs::create_dir(dir.join(".obsidian")).unwrap();
        assert!(is_vault_root(dir));
        // Being a vault says nothing about being a repo, and a real vault is
        // often both — which is why neither can be the directory's kind.
        assert!(!is_repo_root(dir));
        std::fs::create_dir(dir.join(".git")).unwrap();
        assert!(is_repo_root(dir) && is_vault_root(dir));
    }

    #[test]
    fn the_vaults_own_unresolved_link_preference_is_read_when_it_has_one() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::create_dir(dir.join(".obsidian")).unwrap();
        // No graph.json at all: no opinion to honour.
        assert_eq!(vault_hides_unresolved(dir), None);

        let graph = dir.join(".obsidian").join("graph.json");
        std::fs::write(&graph, r#"{"hideUnresolved": true, "scale": 0.24}"#).unwrap();
        assert_eq!(vault_hides_unresolved(dir), Some(true));

        std::fs::write(&graph, r#"{"showTags": true}"#).unwrap();
        assert_eq!(vault_hides_unresolved(dir), None, "absent, not false");

        // Obsidian rewrites this file constantly; a torn write must not panic.
        std::fs::write(&graph, "{not json").unwrap();
        assert_eq!(vault_hides_unresolved(dir), None);
    }
}
