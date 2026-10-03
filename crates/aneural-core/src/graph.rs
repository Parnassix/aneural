//! Nodes, edges and the incremental delta stream the engine emits.

use crate::NodeId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    pub id: NodeId,
    pub kind: String,
    pub label: String,
    /// Workspace-relative path for filesystem-backed nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Id of the `Repo` node this belongs to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<NodeId>,
    #[serde(default)]
    pub props: Value,
    /// Content fingerprint for files (blake3 hex) — cheap change detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Producer (`walker`, `lang`, `manifest`, `spore:<name>`).
    pub source: String,
    /// Workspace-relative path of the file whose processing produced this
    /// node; reindexing that file replaces everything with this origin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

impl Node {
    pub fn new(
        id: NodeId,
        kind: impl Into<String>,
        label: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Node {
            id,
            kind: kind.into(),
            label: label.into(),
            path: None,
            repo_id: None,
            props: Value::Object(Default::default()),
            fingerprint: None,
            source: source.into(),
            origin: None,
        }
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub fn with_repo(mut self, repo: Option<NodeId>) -> Self {
        self.repo_id = repo;
        self
    }

    pub fn with_origin(mut self, origin: impl Into<String>) -> Self {
        self.origin = Some(origin.into());
        self
    }

    pub fn with_prop(mut self, key: &str, value: impl Into<Value>) -> Self {
        if let Value::Object(map) = &mut self.props {
            map.insert(key.to_string(), value.into());
        }
        self
    }

    pub fn prop_str(&self, key: &str) -> Option<&str> {
        self.props.get(key).and_then(Value::as_str)
    }

    /// A counting prop. Every tally Aneural writes is an `i64`, so this is
    /// the accessor for them; a prop that is not a whole number reads as
    /// `None` rather than being rounded into one.
    pub fn prop_i64(&self, key: &str) -> Option<i64> {
        self.props.get(key).and_then(Value::as_i64)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Edge {
    pub kind: String,
    pub src: NodeId,
    pub dst: NodeId,
    #[serde(default)]
    pub props: Value,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

impl Edge {
    pub fn new(
        kind: impl Into<String>,
        src: NodeId,
        dst: NodeId,
        source: impl Into<String>,
    ) -> Self {
        Edge {
            kind: kind.into(),
            src,
            dst,
            props: Value::Object(Default::default()),
            source: source.into(),
            origin: None,
        }
    }

    pub fn with_origin(mut self, origin: impl Into<String>) -> Self {
        self.origin = Some(origin.into());
        self
    }

    pub fn with_prop(mut self, key: &str, value: impl Into<Value>) -> Self {
        if let Value::Object(map) = &mut self.props {
            map.insert(key.to_string(), value.into());
        }
        self
    }
}

/// Where in the indexing lifecycle a delta was produced. The GUI uses this to
/// decide how dramatic the growth animation should be.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum DeltaPhase {
    /// Part of the initial full index.
    #[default]
    Initial,
    /// Reaction to a filesystem change while watching.
    Live,
}

/// One batch of graph changes. Removals are applied before additions so a
/// reindexed file's replaced nodes do not flicker.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GraphDelta {
    #[serde(default = "GraphDelta::default_phase")]
    pub phase: DeltaPhase,
    #[serde(default)]
    pub removed_node_ids: Vec<NodeId>,
    #[serde(default)]
    pub removed_edges: Vec<Edge>,
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub edges: Vec<Edge>,
    /// True on the last delta of the initial index.
    #[serde(default)]
    pub initial_complete: bool,
}

impl GraphDelta {
    fn default_phase() -> DeltaPhase {
        DeltaPhase::Initial
    }

    pub fn new(phase: DeltaPhase) -> Self {
        GraphDelta {
            phase,
            ..Default::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
            && self.edges.is_empty()
            && self.removed_node_ids.is_empty()
            && self.removed_edges.is_empty()
    }

    pub fn len(&self) -> usize {
        self.nodes.len() + self.edges.len() + self.removed_node_ids.len() + self.removed_edges.len()
    }

    pub fn merge(&mut self, other: GraphDelta) {
        self.removed_node_ids.extend(other.removed_node_ids);
        self.removed_edges.extend(other.removed_edges);
        self.nodes.extend(other.nodes);
        self.edges.extend(other.edges);
        self.initial_complete |= other.initial_complete;
    }
}

/// A materialised neighbourhood / query result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Subgraph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// True when a `limit` truncated the result.
    #[serde(default)]
    pub truncated: bool,
}

/// Summary counts after an index run.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IndexStats {
    pub files_scanned: u64,
    pub files_indexed: u64,
    pub files_skipped: u64,
    pub nodes: u64,
    pub edges: u64,
    pub unresolved: u64,
    pub duration_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_json_shape_is_camel_case() {
        let n = Node::new(NodeId::file("a.ts"), "File", "a.ts", "walker")
            .with_path("a.ts")
            .with_prop("lang", "typescript");
        let v = serde_json::to_value(&n).unwrap();
        assert_eq!(v["id"], "file:a.ts");
        assert_eq!(v["props"]["lang"], "typescript");
        assert!(v.get("repoId").is_none());
        let back: Node = serde_json::from_value(v).unwrap();
        assert_eq!(back, n);
    }

    #[test]
    fn delta_merge() {
        let mut a = GraphDelta::new(DeltaPhase::Initial);
        a.nodes
            .push(Node::new(NodeId::dir("."), "Directory", ".", "walker"));
        let mut b = GraphDelta::new(DeltaPhase::Initial);
        b.initial_complete = true;
        a.merge(b);
        assert_eq!(a.len(), 1);
        assert!(a.initial_complete);
    }
}
