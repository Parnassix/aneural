//! Stable node identifiers: `<prefix>:<workspace-relative path>[#<fragment>]`.
//!
//! Ids are content-independent so that layout positions and focus selections
//! survive edits. Paths always use forward slashes and never start with `./`.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;

#[derive(
    Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct NodeId(pub String);

impl NodeId {
    pub fn new(raw: impl Into<String>) -> Self {
        NodeId(raw.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `dir:apps/web/src` (the workspace root itself is `dir:.`).
    pub fn dir(rel: impl AsRef<Path>) -> Self {
        NodeId(format!("dir:{}", canonical_rel(rel.as_ref())))
    }

    pub fn file(rel: impl AsRef<Path>) -> Self {
        NodeId(format!("file:{}", canonical_rel(rel.as_ref())))
    }

    pub fn repo(rel: impl AsRef<Path>) -> Self {
        NodeId(format!("repo:{}", canonical_rel(rel.as_ref())))
    }

    pub fn manifest(rel: impl AsRef<Path>) -> Self {
        NodeId(format!("manifest:{}", canonical_rel(rel.as_ref())))
    }

    /// `pkg:npm/react`, `pkg:cargo/serde`, `pkg:pypi/requests`, `pkg:go/github.com/x/y`.
    pub fn package(ecosystem: &str, name: &str) -> Self {
        NodeId(format!("pkg:{ecosystem}/{name}"))
    }

    /// `comment:<file>#<hash(text)>` — survives line shifts, changes with text.
    pub fn comment(file_rel: impl AsRef<Path>, text: &str) -> Self {
        NodeId(format!(
            "comment:{}#{}",
            canonical_rel(file_rel.as_ref()),
            crate::short_hash(text.trim())
        ))
    }

    pub fn plan(rel: impl AsRef<Path>) -> Self {
        NodeId(format!("plan:{}", canonical_rel(rel.as_ref())))
    }

    /// A plan that lives outside the workspace, under the user's home — e.g.
    /// `plan:~/.claude/plans/wise-phoenix.md`. The leading `~/` is a segment
    /// `canonical_rel` can never produce, so these can never collide with a
    /// workspace plan of the same name.
    pub fn home_plan(name: &str) -> Self {
        NodeId(format!("plan:~/.claude/plans/{name}"))
    }

    /// `usage:workspace`, or `usage:repo/<rel>` — a tally of what was spent
    /// working somewhere. Not path-derived: a tally is about a place rather
    /// than being a thing at one, and `usage:repo/.` has to stay distinct from
    /// the workspace total even when the root is itself the only repository.
    pub fn usage(scope: &str) -> Self {
        NodeId(format!("usage:{scope}"))
    }

    /// The tally for one repository, named by its workspace-relative root.
    pub fn repo_usage(repo_rel: impl AsRef<Path>) -> Self {
        NodeId::usage(&format!("repo/{}", canonical_rel(repo_rel.as_ref())))
    }

    /// The tally for everything in the workspace.
    pub fn workspace_usage() -> Self {
        NodeId::usage("workspace")
    }

    /// `session:<uuid>` — one Claude Code session. Not path-derived: the
    /// transcript lives outside the workspace.
    pub fn session(uuid: &str) -> Self {
        NodeId(format!("session:{uuid}"))
    }

    /// `commit:<repo rel>@<sha>` — scoped by repo because a workspace can hold
    /// several, and two of them can legitimately share a sha after a fork.
    pub fn commit(repo_rel: impl AsRef<Path>, sha: &str) -> Self {
        NodeId(format!("commit:{}@{sha}", canonical_rel(repo_rel.as_ref())))
    }

    /// `script:scripts/download_pubmed.py`, or `script:package.json#build` when
    /// one file declares several runnable entries.
    pub fn script(rel: impl AsRef<Path>, entry: Option<&str>) -> Self {
        NodeId::custom("script", rel, entry)
    }

    /// A schedule *declared in a workspace file*: `schedule:<rel>#<key>`. The
    /// key is what the declaration is about, so two rows of one table become
    /// two schedules rather than overwriting each other.
    pub fn schedule(rel: impl AsRef<Path>, key: &str) -> Self {
        NodeId(format!(
            "schedule:{}#{}",
            canonical_rel(rel.as_ref()),
            crate::slug(key)
        ))
    }

    /// A schedule this machine's launchd or crontab actually has —
    /// `schedule:~/installed/<label>`. The leading `~/` is the [`Self::home_plan`]
    /// trick: a segment `canonical_rel` can never produce, so a machine-local
    /// schedule can never collide with one declared in a file.
    pub fn installed_schedule(label: &str) -> Self {
        NodeId(format!("schedule:~/installed/{label}"))
    }

    /// A schedule Aneural itself manages, from config: `schedule:~/managed/<key>`.
    pub fn managed_schedule(script_key: &str) -> Self {
        NodeId(format!("schedule:~/managed/{script_key}"))
    }

    /// The latest run of one script, mirroring that script's own id. Stable for
    /// the life of the script: a run-derived id would replace the node on every
    /// fire, and the graph would churn an entity each time.
    pub fn run(rel: impl AsRef<Path>, entry: Option<&str>) -> Self {
        NodeId::custom("run", rel, entry)
    }

    pub fn note(rel: impl AsRef<Path>) -> Self {
        NodeId(format!("note:{}", canonical_rel(rel.as_ref())))
    }

    pub fn idea(rel: impl AsRef<Path>, heading: &str) -> Self {
        NodeId(format!(
            "idea:{}#{}",
            canonical_rel(rel.as_ref()),
            crate::slug(heading)
        ))
    }

    /// Generic constructor for spore-defined kinds: `<prefix>:<path>[#fragment]`.
    pub fn custom(prefix: &str, rel: impl AsRef<Path>, fragment: Option<&str>) -> Self {
        match fragment {
            Some(f) => NodeId(format!("{prefix}:{}#{f}", canonical_rel(rel.as_ref()))),
            None => NodeId(format!("{prefix}:{}", canonical_rel(rel.as_ref()))),
        }
    }

    /// The `<prefix>` part, e.g. `file`.
    pub fn prefix(&self) -> &str {
        self.0.split_once(':').map(|(p, _)| p).unwrap_or("")
    }

    /// The path-ish part after the prefix and before any `#fragment`.
    pub fn path_part(&self) -> &str {
        let rest = self.0.split_once(':').map(|(_, r)| r).unwrap_or("");
        rest.split_once('#').map(|(p, _)| p).unwrap_or(rest)
    }

    pub fn fragment(&self) -> Option<&str> {
        self.0.split_once('#').map(|(_, f)| f)
    }

    pub fn is_file(&self) -> bool {
        self.prefix() == "file"
    }

    pub fn is_dir(&self) -> bool {
        self.prefix() == "dir"
    }

    /// Validate the `<prefix>:<rest>` shape.
    pub fn parse(raw: &str) -> crate::Result<Self> {
        match raw.split_once(':') {
            Some((p, r)) if !p.is_empty() && !r.is_empty() => Ok(NodeId(raw.to_string())),
            _ => Err(crate::Error::InvalidId(raw.to_string())),
        }
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({})", self.0)
    }
}

impl From<&str> for NodeId {
    fn from(s: &str) -> Self {
        NodeId(s.to_string())
    }
}

impl From<String> for NodeId {
    fn from(s: String) -> Self {
        NodeId(s)
    }
}

/// Normalise a workspace-relative path: forward slashes, no `./`, no trailing
/// slash, `.` for the root.
pub fn canonical_rel(p: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for comp in p.components() {
        use std::path::Component::*;
        match comp {
            CurDir | RootDir | Prefix(_) => {}
            ParentDir => {
                parts.pop();
            }
            Normal(s) => parts.push(s.to_string_lossy().into_owned()),
        }
    }
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_paths() {
        assert_eq!(canonical_rel(Path::new("./apps/web/")), "apps/web");
        assert_eq!(canonical_rel(Path::new("apps/../src/./a.ts")), "src/a.ts");
        assert_eq!(canonical_rel(Path::new("")), ".");
        assert_eq!(canonical_rel(Path::new(".")), ".");
    }

    #[test]
    fn constructors_and_parts() {
        let id = NodeId::file("./apps/web/src/index.ts");
        assert_eq!(id.as_str(), "file:apps/web/src/index.ts");
        assert_eq!(id.prefix(), "file");
        assert_eq!(id.path_part(), "apps/web/src/index.ts");
        assert_eq!(id.fragment(), None);

        let c = NodeId::comment("a.ts", "TODO: x");
        assert_eq!(c.prefix(), "comment");
        assert_eq!(c.path_part(), "a.ts");
        assert_eq!(c.fragment().unwrap().len(), 12);
        assert_eq!(c, NodeId::comment("a.ts", "  TODO: x \n"));

        assert_eq!(NodeId::package("npm", "react").as_str(), "pkg:npm/react");
        assert_eq!(
            NodeId::idea("x.md", "Big Idea!").as_str(),
            "idea:x.md#big-idea"
        );
        assert!(NodeId::parse("nope").is_err());
        assert!(NodeId::parse("file:a").is_ok());
    }

    #[test]
    fn a_runnable_entry_inside_a_file_is_distinct_from_the_file_itself() {
        assert_eq!(
            NodeId::script("./scripts/download_pubmed.py", None).as_str(),
            "script:scripts/download_pubmed.py"
        );
        assert_eq!(
            NodeId::script("package.json", Some("build")).as_str(),
            "script:package.json#build"
        );
        // the run mirrors its script, so one is derivable from the other
        assert_eq!(
            NodeId::run("package.json", Some("build")).path_part(),
            NodeId::script("package.json", Some("build")).path_part()
        );
    }

    #[test]
    fn a_machine_local_schedule_can_never_collide_with_a_declared_one() {
        // `~/` is a segment `canonical_rel` never produces, so no file in the
        // workspace can be named such that its schedule collides with these.
        let declared = NodeId::schedule("Data Sources/Refresh Cadences.md", "pubmed_updates");
        assert_eq!(
            declared.as_str(),
            "schedule:Data Sources/Refresh Cadences.md#pubmed-updates"
        );
        assert!(!declared.as_str().contains("~/"));
        assert_eq!(
            NodeId::installed_schedule("dev.aneural.acme.download-pubmed").as_str(),
            "schedule:~/installed/dev.aneural.acme.download-pubmed"
        );
        assert_eq!(
            NodeId::managed_schedule("scripts/download_pubmed.py").as_str(),
            "schedule:~/managed/scripts/download_pubmed.py"
        );
        // two rows of one table are two schedules, not one overwriting the other
        assert_ne!(NodeId::schedule("x.md", "a"), NodeId::schedule("x.md", "b"));
    }
}
