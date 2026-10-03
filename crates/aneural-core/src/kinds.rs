//! Built-in node and edge kinds. Kinds are open strings (spores and
//! `.aneural/nodes/*.json` can add more); these constants are the ones the
//! engine itself produces.

use std::borrow::Cow;

/// Node kind names. Stored as plain strings in the graph.
pub struct NodeKind;

impl NodeKind {
    pub const DIRECTORY: &'static str = "Directory";
    pub const FILE: &'static str = "File";
    pub const REPO: &'static str = "Repo";
    pub const MANIFEST: &'static str = "Manifest";
    pub const PACKAGE: &'static str = "Package";
    pub const SYMBOL: &'static str = "Symbol";
    /// A Claude Code session that worked in this workspace.
    pub const SESSION: &'static str = "Session";
    /// One commit in one of the workspace's repositories.
    pub const COMMIT: &'static str = "Commit";
    /// A plan document. Produced by the `plans` spore from workspace files and
    /// by the `claude` producer from `~/.claude/plans`, so the type is builtin
    /// even though a spore also emits it — otherwise disabling that spore would
    /// leave the producer's nodes untyped.
    pub const PLAN: &'static str = "Plan";
    /// What a stretch of work cost in tokens: one per repository, one for the
    /// whole workspace. A tally has nowhere else to live — the nodes it is
    /// about (a repository, the root directory) belong to the walker, and a
    /// node has one origin.
    pub const USAGE: &'static str = "Usage";
    /// Something in this workspace you can run. Found by the `scripts` spore
    /// from the workspace's own files; the type is builtin because the `runs`
    /// producer names it too, and a `Run` whose script is untyped says nothing.
    pub const SCRIPT: &'static str = "Script";
    /// When a script is declared to run. There is deliberately one of these per
    /// *declaration*, not one per script: a cadence written in a document, a
    /// launch agent actually installed, and what Aneural itself manages are
    /// three separate claims, and the disagreement between them is the point.
    pub const SCHEDULE: &'static str = "Schedule";
    /// The most recent run of one script. One node per script, ever — the full
    /// history lives in the runs journal, because the graph is a cache and a
    /// node per run would churn an entity every time a schedule fired.
    pub const RUN: &'static str = "Run";
    // spore-provided, but well known to the GUI defaults
    pub const COMMENT: &'static str = "Comment";
    pub const IDEA: &'static str = "Idea";
    pub const NOTE: &'static str = "Note";

    /// Kinds produced by the core engine (not by spores).
    pub const BUILTIN: &'static [&'static str] = &[
        Self::DIRECTORY,
        Self::FILE,
        Self::REPO,
        Self::MANIFEST,
        Self::PACKAGE,
        Self::SESSION,
        Self::COMMIT,
        Self::PLAN,
        Self::USAGE,
        Self::SCRIPT,
        Self::SCHEDULE,
        Self::RUN,
    ];

    /// The id prefix used for a kind (see [`crate::id`]).
    pub fn id_prefix(kind: &str) -> &str {
        match kind {
            Self::DIRECTORY => "dir",
            Self::FILE => "file",
            Self::REPO => "repo",
            Self::MANIFEST => "manifest",
            Self::PACKAGE => "pkg",
            Self::SYMBOL => "sym",
            Self::SESSION => "session",
            Self::COMMIT => "commit",
            Self::COMMENT => "comment",
            Self::PLAN => "plan",
            Self::USAGE => "usage",
            Self::SCRIPT => "script",
            Self::SCHEDULE => "schedule",
            Self::RUN => "run",
            Self::IDEA => "idea",
            Self::NOTE => "note",
            other => other,
        }
    }
}

/// Edge kind names (directed, Neo4j-style SCREAMING_CASE).
pub struct EdgeKind;

impl EdgeKind {
    /// Directory → Directory/File, Repo → root Directory. The folder tree
    /// every other kind hangs off, so it is always drawn.
    pub const CONTAINS: &'static str = "CONTAINS";
    /// File → File/Directory/Package: anything a file pulls in by `import`,
    /// `use` or `export … from`. The edge's `importKind` prop keeps the syntax.
    pub const IMPORTS: &'static str = "IMPORTS";
    /// File → File, looser than an import (require(), `mod`, include), and
    /// between spore nodes such as tables linked by a foreign key.
    pub const REFERENCES: &'static str = "REFERENCES";
    /// Comment/Plan → File/Directory: a note pointed at code.
    pub const ANNOTATES: &'static str = "ANNOTATES";
    /// Note/Idea/Plan → anything (wiki-link, source file). Not drawn as a
    /// strand: the node floats near what it relates to instead.
    pub const RELATES_TO: &'static str = "RELATES_TO";
    /// Commit/Session/PullRequest → File: this actually changed the file.
    /// Cumulative counts belong on the actor's node, never here — edges are
    /// unique per `(kind, src, dst, source)` and their props are replaced
    /// wholesale, so a per-run tally written here would overwrite the total.
    pub const MODIFIES: &'static str = "MODIFIES";
    /// Session/Commit/PullRequest → Plan: this carried the plan out. The
    /// counterpart to ANNOTATES, which is what the plan *said* it would touch.
    pub const REALIZES: &'static str = "REALIZES";

    pub const ALL: &'static [&'static str] = &[
        Self::CONTAINS,
        Self::IMPORTS,
        Self::REFERENCES,
        Self::ANNOTATES,
        Self::RELATES_TO,
        Self::MODIFIES,
        Self::REALIZES,
    ];

    /// Is this kind drawn as a line between two nodes?
    ///
    /// A kind that is not a strand hangs a node *off* the tree rather than
    /// joining two places in it, and is shown by where that node floats — the
    /// thread appears only while one end is hovered or selected. Which kinds
    /// those are is not a matter of taste: a commit that changed twenty files
    /// would otherwise draw twenty lines from wherever it sat to every corner
    /// of the tree, and thirty commits turn the graph into a starburst.
    pub fn is_strand(kind: &str) -> bool {
        !matches!(kind, Self::RELATES_TO | Self::MODIFIES | Self::REALIZES)
    }

    /// Kinds the user can switch off. The folder tree is the skeleton, and a
    /// kind that is not a strand has no line to hide in the first place.
    pub fn is_toggleable(kind: &str) -> bool {
        kind != Self::CONTAINS && Self::is_strand(kind)
    }

    /// Human-readable name for a kind, e.g. `RE_EXPORTS` → "Re-exports".
    /// Unknown kinds (spores can add their own) are de-screamed generically.
    pub fn label(kind: &str) -> Cow<'static, str> {
        match kind {
            Self::CONTAINS => "Contains".into(),
            Self::IMPORTS => "Imports".into(),
            Self::REFERENCES => "References".into(),
            Self::ANNOTATES => "Annotates".into(),
            Self::RELATES_TO => "Relates to".into(),
            Self::MODIFIES => "Modifies".into(),
            Self::REALIZES => "Realizes".into(),
            other => Cow::Owned(humanize(other)),
        }
    }
}

/// `SCREAMING_SNAKE` (or anything else) to sentence case: underscores become
/// spaces and only the first letter is capitalised.
fn humanize(kind: &str) -> String {
    let lower = kind.replace('_', " ").to_lowercase();
    let mut chars = lower.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => lower,
    }
}

/// Which producer created a node/edge. Stored in the `source` column so a
/// re-run of one producer can replace exactly its own output.
pub struct Source;

impl Source {
    pub const WALKER: &'static str = "walker";
    pub const LANG: &'static str = "lang";
    /// Commits read out of each repository's own history.
    pub const GIT: &'static str = "git";
    /// Claude Code's sessions and plans, read from `~/.claude`. Off unless the
    /// workspace opts in; switching it off retracts everything through
    /// `Store::delete_by_source`.
    pub const CLAUDE: &'static str = "claude";
    /// Script schedules this machine actually has, and the due and drift
    /// verdicts over them. Off unless the workspace opts in: it reads
    /// `~/Library/LaunchAgents`, which is outside the workspace.
    pub const RUNS: &'static str = "runs";
    pub fn spore(name: &str) -> String {
        format!("{}{name}", Self::SPORE_PREFIX)
    }
    /// What every spore's `source` starts with, so a reader can tell "some
    /// spore made this" from "a producer made this" without knowing which.
    pub const SPORE_PREFIX: &'static str = "spore:";
    /// Whether this source is any spore's.
    pub fn is_spore(source: &str) -> bool {
        source.starts_with(Self::SPORE_PREFIX)
    }
}

#[cfg(test)]
mod tests {
    use super::EdgeKind;

    #[test]
    fn edge_labels_are_human_readable() {
        assert_eq!(EdgeKind::label(EdgeKind::RELATES_TO), "Relates to");
        assert_eq!(
            EdgeKind::label("TASTES_LIKE_MUSHROOM"),
            "Tastes like mushroom"
        );
        assert_eq!(EdgeKind::label(""), "");
        for kind in EdgeKind::ALL {
            let label = EdgeKind::label(kind);
            assert!(!label.contains('_'), "{kind} -> {label}");
        }
    }
}
