//! Commits into nodes and edges.

use aneural_core::kinds::{EdgeKind, NodeKind, Source};
use aneural_core::{Edge, Node, NodeId};

/// Turn one repository's commits into graph nodes and edges.
///
/// `repo_rel` locates the repository within the workspace, so that a commit's
/// repository-relative paths can be resolved to workspace-relative file ids.
/// `known` answers whether a file id is in the graph: a commit routinely
/// touches files that have since been deleted, or that this workspace does not
/// index, and an edge to a node nobody has is an edge nobody can draw.
pub fn to_graph(
    repo_rel: &str,
    commits: &[aneural_git::Commit],
    known: &dyn Fn(&NodeId) -> bool,
) -> (Vec<Node>, Vec<Edge>) {
    let origin = super::git_origin(repo_rel);
    let repo_id = NodeId::dir(repo_rel);
    let mut nodes = Vec::with_capacity(commits.len());
    let mut edges = Vec::new();

    for c in commits {
        let id = NodeId::commit(repo_rel, &c.sha);
        let mut touched = 0u64;
        for changed in &c.changed {
            let file = NodeId::file(join(repo_rel, &changed.path));
            if !known(&file) {
                continue;
            }
            touched += 1;
            edges.push(
                Edge::new(EdgeKind::MODIFIES, id.clone(), file, Source::GIT)
                    .with_origin(&origin)
                    .with_prop("status", changed.status.as_str())
                    .with_prop("at", c.at.clone()),
            );
        }

        let mut node = Node::new(id, NodeKind::COMMIT, &c.subject, Source::GIT)
            .with_origin(&origin)
            .with_repo(Some(repo_id.clone()))
            .with_prop("sha", c.sha.clone())
            .with_prop("short", c.short.clone())
            .with_prop("author", c.author.clone())
            .with_prop("at", c.at.clone())
            .with_prop("seconds", c.seconds)
            // Counted here rather than on the edges: an edge is unique per
            // (kind, src, dst, source) and its props are replaced wholesale, so
            // a tally living there could not survive a partial re-run.
            .with_prop("files", touched as i64)
            .with_prop("filesInCommit", c.changed.len() as i64);
        if !c.body.is_empty() {
            node = node.with_prop("body", c.body.clone());
        }
        if let Some(session) = &c.session {
            node = node.with_prop("session", session.clone());
        }
        nodes.push(node);
    }
    (nodes, edges)
}

/// A repository-relative path as a workspace-relative one.
fn join(repo_rel: &str, path: &str) -> String {
    if repo_rel == "." || repo_rel.is_empty() {
        path.to_string()
    } else {
        format!("{repo_rel}/{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aneural_git::{Changed, Commit, Status};

    fn commit(sha: &str, session: Option<&str>, changed: &[(&str, Status)]) -> Commit {
        Commit {
            sha: sha.into(),
            short: sha.chars().take(8).collect(),
            subject: "Grow the graph".into(),
            body: String::new(),
            author: "Ada Lovelace".into(),
            email: "ada@example.com".into(),
            at: "2026-09-17T18:22:49Z".into(),
            seconds: 1_789_669_369,
            session: session.map(String::from),
            parents: vec!["parent".into()],
            changed: changed
                .iter()
                .map(|(p, s)| Changed {
                    path: (*p).into(),
                    status: *s,
                })
                .collect(),
        }
    }

    #[test]
    fn a_commit_becomes_a_node_and_one_edge_per_file_we_actually_have() {
        let c = commit(
            "abc",
            Some("01HU8B"),
            &[("src/a.ts", Status::Modified), ("gone.ts", Status::Deleted)],
        );
        let (nodes, edges) = to_graph(".", &[c], &|id| id == &NodeId::file("src/a.ts"));

        assert_eq!(nodes.len(), 1);
        let node = &nodes[0];
        assert_eq!(node.id, NodeId::commit(".", "abc"));
        assert_eq!(node.kind, NodeKind::COMMIT);
        assert_eq!(node.source, Source::GIT);
        assert_eq!(node.origin.as_deref(), Some("git://./log"));
        assert_eq!(node.prop_str("session"), Some("01HU8B"));

        assert_eq!(edges.len(), 1, "the deleted file is not in the graph");
        assert_eq!(edges[0].kind, EdgeKind::MODIFIES);
        assert_eq!(edges[0].dst, NodeId::file("src/a.ts"));
        assert_eq!(
            node.props.get("files").and_then(|v| v.as_i64()),
            Some(1),
            "the count follows the edges that were actually drawn"
        );
        assert_eq!(
            node.props.get("filesInCommit").and_then(|v| v.as_i64()),
            Some(2),
            "while the commit's real size is kept"
        );
    }

    #[test]
    fn a_repository_below_the_root_resolves_its_paths_against_the_workspace() {
        let c = commit("abc", None, &[("src/a.ts", Status::Added)]);
        let (nodes, edges) = to_graph("packages/api", &[c], &|_| true);
        assert_eq!(edges[0].dst, NodeId::file("packages/api/src/a.ts"));
        assert_eq!(nodes[0].id, NodeId::commit("packages/api", "abc"));
        assert_eq!(nodes[0].repo_id, Some(NodeId::dir("packages/api")));
        assert!(nodes[0].props.get("session").is_none());
    }

    #[test]
    fn a_commit_touching_nothing_we_index_still_has_a_node_to_collect() {
        let c = commit("abc", None, &[("vendor/x.js", Status::Added)]);
        let (nodes, edges) = to_graph(".", &[c], &|_| false);
        assert_eq!(nodes.len(), 1);
        assert!(edges.is_empty(), "no edge may point at a node nobody has");
    }
}
