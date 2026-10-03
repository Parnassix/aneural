//! What the work here has cost, rolled up to the places a person asks about.
//!
//! A session already carries its own tally (see [`super::claude::Usage`]), and
//! a plan gets one from the stretch of transcript that ran under it. Neither
//! answers "what has this repository cost", because the nodes that would hold
//! that answer — a `Repo`, the root `Directory` — belong to the walker, and a
//! node has exactly one origin. So the rollups are nodes of their own,
//! annotating the place they are about.
//!
//! Attribution is deliberately blunt: a session's whole cost goes to the
//! repository its working directory sat in, and nowhere else. A session that
//! edits three repositories could have its tokens split between them, but
//! there is no honest ratio to split by — an edit is not a token — and a
//! number arrived at by inventing one is worse than a coarse number that says
//! what it is. `sessions` and `sessionsElsewhere` on each node are there so
//! the coarseness is visible rather than implied.

use super::claude::{ByModel, Session, Usage, merge, total, write_usage};
use aneural_core::kinds::{EdgeKind, NodeKind, Source};
use aneural_core::{Edge, Node, NodeId};

/// Every rollup hangs off this one origin: they are recomputed together,
/// because one session being read changes several of them at once.
pub const ORIGIN: &str = "claude://usage";

/// One place, and what was spent working in it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scope {
    /// Workspace-relative repository root, or `None` for the whole workspace.
    pub repo: Option<String>,
    pub label: String,
    /// What was spent here, split by model. The only tally kept: a total is
    /// derived from it rather than counted alongside it, so the two can never
    /// disagree.
    pub by_model: ByModel,
    /// Sessions whose working directory was in here.
    pub sessions: u64,
    /// Sessions that touched a file in here but were rooted somewhere else,
    /// so their tokens are counted against a different scope. The honest
    /// footnote on a coarse number.
    pub sessions_elsewhere: u64,
    /// Sessions counted here that are not on the canvas: they edited nothing
    /// that survives and approved no plan, so the graph has swept them, but
    /// the tokens were still spent. Without this the total would be a number
    /// you could not arrive at by adding up what you can see.
    pub sessions_swept: u64,
    /// What those swept sessions cost.
    pub swept: Usage,
}

impl Scope {
    /// Fold one session into this scope.
    pub fn add(&mut self, session: &Session, spent: Usage, on_canvas: bool) {
        self.sessions += 1;
        if !on_canvas {
            self.sessions_swept += 1;
            self.swept.add(&spent);
        }
        merge(&mut self.by_model, &session.usage);
    }

    /// What was spent here, across every model.
    pub fn used(&self) -> Usage {
        total(&self.by_model)
    }

    fn id(&self) -> NodeId {
        match &self.repo {
            Some(rel) => NodeId::repo_usage(rel),
            None => NodeId::workspace_usage(),
        }
    }

    /// The node this tally is about: the repository, or the workspace root.
    fn about(&self) -> NodeId {
        match &self.repo {
            Some(rel) => NodeId::dir(rel),
            None => NodeId::dir("."),
        }
    }
}

/// Roll the scopes up into nodes and the edges that attach them.
///
/// A scope nothing was spent in is dropped rather than emitted as a row of
/// zeroes: an empty repository should not grow a tally node saying so.
pub fn to_graph(scopes: &[Scope]) -> (Vec<Node>, Vec<Edge>) {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for scope in scopes {
        if scope.used().is_zero() {
            continue;
        }
        let id = scope.id();
        let mut node = Node::new(id.clone(), NodeKind::USAGE, &scope.label, Source::CLAUDE)
            .with_origin(ORIGIN)
            .with_prop("scope", scope.repo.clone().unwrap_or_else(|| ".".into()))
            .with_prop("sessions", scope.sessions as i64);
        node = write_usage(node, &scope.by_model);
        if scope.sessions_elsewhere > 0 {
            node = node.with_prop("sessionsElsewhere", scope.sessions_elsewhere as i64);
        }
        // The reconciliation: `tokens` minus `tokensSwept` is what the session
        // nodes on the canvas add up to.
        if scope.sessions_swept > 0 {
            node = node
                .with_prop("sessionsSwept", scope.sessions_swept as i64)
                .with_prop("tokensSwept", scope.swept.total() as i64);
        }
        edges.push(
            Edge::new(EdgeKind::ANNOTATES, id, scope.about(), Source::CLAUDE)
                .with_origin(ORIGIN)
                .with_prop("via", "usage"),
        );
        nodes.push(node);
    }
    (nodes, edges)
}

/// The repository a session was working in: the longest repository root that
/// its working directory sits inside.
///
/// Longest wins because repositories nest — a workspace can hold a repository
/// that itself holds one — and the innermost is the one the session was
/// actually in.
pub fn repo_of<'a>(cwd_rel: &str, repos: impl IntoIterator<Item = &'a String>) -> Option<String> {
    repos
        .into_iter()
        .filter(|root| inside(cwd_rel, root))
        .max_by_key(|root| match root.as_str() {
            "." => 0,
            other => other.len(),
        })
        .cloned()
}

/// Whether `rel` is `root` or sits beneath it.
pub fn inside(rel: &str, root: &str) -> bool {
    match root {
        "." => true,
        root => rel == root || rel.starts_with(&format!("{root}/")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn spent(tokens: u64) -> Usage {
        Usage {
            messages: 1,
            input: tokens,
            ..Default::default()
        }
    }

    /// A scope whose whole spend was one model's.
    fn on(model: &str, tokens: u64) -> ByModel {
        BTreeMap::from([(model.to_string(), spent(tokens))])
    }

    #[test]
    fn a_session_belongs_to_the_innermost_repository_it_sat_in() {
        let repos = [".".to_string(), "vendor/lib".to_string()];
        assert_eq!(
            repo_of("vendor/lib/src", repos.iter()),
            Some("vendor/lib".into())
        );
        assert_eq!(repo_of("crates/core", repos.iter()), Some(".".into()));
        // a prefix that is not a path boundary is not a parent
        assert_eq!(repo_of("vendor/library", repos.iter()), Some(".".into()));
    }

    #[test]
    fn a_scope_nobody_spent_anything_in_grows_no_node() {
        let (nodes, edges) = to_graph(&[Scope {
            repo: Some("docs".into()),
            label: "docs".into(),
            ..Default::default()
        }]);
        assert!(nodes.is_empty(), "{nodes:?}");
        assert!(edges.is_empty());
    }

    #[test]
    fn a_scope_says_how_much_of_its_total_is_not_on_the_canvas() {
        let mut scope = Scope {
            repo: None,
            label: "ws".into(),
            ..Default::default()
        };
        let seen = |tokens: u64| Session {
            usage: on("claude-opus-5", tokens),
            ..Default::default()
        };
        scope.add(&seen(100), spent(100), true);
        scope.add(&seen(40), spent(40), false);
        let (nodes, _) = to_graph(&[scope]);
        assert_eq!(nodes[0].prop_i64("tokens"), Some(140));
        assert_eq!(nodes[0].prop_i64("sessions"), Some(2));
        assert_eq!(nodes[0].prop_i64("sessionsSwept"), Some(1));
        // what is left is exactly what the visible session nodes add up to
        assert_eq!(nodes[0].prop_i64("tokensSwept"), Some(40));
    }

    #[test]
    fn a_tally_annotates_the_place_it_is_about() {
        let (nodes, edges) = to_graph(&[
            Scope {
                repo: None,
                label: "Whole workspace".into(),
                by_model: on("claude-opus-5", 300),
                sessions: 3,
                ..Default::default()
            },
            Scope {
                repo: Some("vendor/lib".into()),
                label: "vendor/lib".into(),
                by_model: on("claude-fable-5-1", 100),
                sessions: 1,
                sessions_elsewhere: 2,
                ..Default::default()
            },
        ]);
        assert_eq!(nodes.len(), 2);
        assert_eq!(edges.len(), 2);
        assert_eq!(nodes[0].id, NodeId::workspace_usage());
        assert_eq!(edges[0].dst, NodeId::dir("."));
        assert_eq!(nodes[1].id, NodeId::repo_usage("vendor/lib"));
        assert_eq!(edges[1].dst, NodeId::dir("vendor/lib"));
        assert_eq!(nodes[0].prop_i64("tokens"), Some(300));
        assert_eq!(nodes[1].prop_i64("sessionsElsewhere"), Some(2));
        assert_eq!(nodes[0].prop_i64("sessionsElsewhere"), None);
    }
}
