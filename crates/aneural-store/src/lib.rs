//! SQLite-backed index store (`.aneural/cache/index.db`).
//!
//! The store is a cache: everything in it is re-derivable from the workspace.
//! Nodes are keyed by their stable id; edges are unique per
//! (kind, src, dst, source). Foreign keys are not enforced so that producers
//! may emit edges before the walker has emitted the target node; dangling
//! edges are filtered out at query time and cascaded manually on delete.

use aneural_core::focus::Direction;
use aneural_core::graph::DeltaPhase;
use aneural_core::kinds::NodeKind;
use aneural_core::{Edge, GraphDelta, Node, NodeId, Subgraph};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Bump whenever what gets indexed changes shape, not just the tables: a cache
/// from before is dropped and rebuilt rather than served stale.
/// 2: RE_EXPORTS and DEPENDS_ON folded into IMPORTS, manifests no longer parsed.
/// 3: Session and Commit nodes, MODIFIES and REALIZES edges, Plan became a
///    builtin kind — an existing cache has none of them and cannot grow them
///    without re-reading history.
/// 4: Script, Schedule and Run became builtin kinds, and the `scripts` spore
///    harvests the first two. An existing cache holds neither, and an unchanged
///    file is replayed from cache rather than re-harvested, so without this the
///    spore would find nothing until every file it matches happened to change.
pub const USER_VERSION: i32 = 4;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub struct Store {
    conn: Connection,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct NodeQuery {
    pub kinds: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<NodeId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
    /// Case-insensitive substring over label and path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct EdgeQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src: Option<NodeId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dst: Option<NodeId>,
    pub kinds: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct NeighborhoodQuery {
    pub depth: u32,
    pub direction: Option<Direction>,
    pub edge_kinds: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileRecord {
    pub path: String,
    pub mtime: i64,
    pub size: i64,
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    pub indexed_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Unresolved {
    pub origin: String,
    pub specifier: String,
    pub line: u32,
    pub reason: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub nodes: u64,
    pub edges: u64,
    pub files: u64,
    pub unresolved: u64,
    pub by_kind: Vec<(String, u64)>,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS nodes(
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  label TEXT NOT NULL,
  path TEXT,
  repo_id TEXT,
  props TEXT NOT NULL DEFAULT '{}',
  fingerprint TEXT,
  source TEXT NOT NULL,
  origin TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS nodes_kind ON nodes(kind);
CREATE INDEX IF NOT EXISTS nodes_path ON nodes(path);
CREATE INDEX IF NOT EXISTS nodes_repo ON nodes(repo_id);
CREATE INDEX IF NOT EXISTS nodes_origin ON nodes(origin);
CREATE TABLE IF NOT EXISTS edges(
  id INTEGER PRIMARY KEY,
  kind TEXT NOT NULL,
  src TEXT NOT NULL,
  dst TEXT NOT NULL,
  props TEXT NOT NULL DEFAULT '{}',
  source TEXT NOT NULL,
  origin TEXT,
  UNIQUE(kind, src, dst, source)
);
CREATE INDEX IF NOT EXISTS edges_src ON edges(src);
CREATE INDEX IF NOT EXISTS edges_dst ON edges(dst);
CREATE INDEX IF NOT EXISTS edges_kind ON edges(kind);
CREATE INDEX IF NOT EXISTS edges_origin ON edges(origin);
CREATE TABLE IF NOT EXISTS files(
  path TEXT PRIMARY KEY,
  mtime INTEGER NOT NULL,
  size INTEGER NOT NULL,
  fingerprint TEXT NOT NULL,
  lang TEXT,
  indexed_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS unresolved(
  origin TEXT NOT NULL,
  specifier TEXT NOT NULL,
  line INTEGER NOT NULL,
  reason TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS unresolved_origin ON unresolved(origin);
"#;

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        let version: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version != 0 && version != USER_VERSION {
            // Cache only: wipe and rebuild on schema change.
            conn.execute_batch(
                "DROP TABLE IF EXISTS nodes; DROP TABLE IF EXISTS edges; DROP TABLE IF EXISTS files;
                 DROP TABLE IF EXISTS unresolved; DROP TABLE IF EXISTS meta;",
            )?;
        }
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "user_version", USER_VERSION)?;
        Ok(Store { conn })
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    // ---- meta -------------------------------------------------------------

    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    /// Forget one `meta` key. Used when the thing it tracked is gone — a
    /// transcript that was deleted leaves no reason to remember how far it had
    /// been read.
    pub fn meta_delete(&self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM meta WHERE key = ?1", params![key])?;
        Ok(())
    }

    /// Forget every `meta` key under a prefix, for when a whole producer is
    /// switched off and the positions it remembered are meaningless.
    pub fn meta_delete_prefix(&self, prefix: &str) -> Result<()> {
        let like = format!("{}%", prefix.replace('%', "\\%").replace('_', "\\_"));
        self.conn.execute(
            "DELETE FROM meta WHERE key LIKE ?1 ESCAPE '\\'",
            params![like],
        )?;
        Ok(())
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- writes -----------------------------------------------------------

    pub fn upsert_nodes(&mut self, nodes: &[Node]) -> Result<()> {
        let tx = self.conn.transaction()?;
        upsert_nodes_tx(&tx, nodes)?;
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_edges(&mut self, edges: &[Edge]) -> Result<()> {
        let tx = self.conn.transaction()?;
        upsert_edges_tx(&tx, edges)?;
        tx.commit()?;
        Ok(())
    }

    /// Apply a delta from the engine: removals first, then upserts.
    pub fn apply_delta(&mut self, delta: &GraphDelta) -> Result<()> {
        let tx = self.conn.transaction()?;
        for e in &delta.removed_edges {
            tx.execute(
                "DELETE FROM edges WHERE kind = ?1 AND src = ?2 AND dst = ?3 AND source = ?4",
                params![e.kind, e.src.as_str(), e.dst.as_str(), e.source],
            )?;
        }
        delete_nodes_tx(&tx, &delta.removed_node_ids)?;
        upsert_nodes_tx(&tx, &delta.nodes)?;
        upsert_edges_tx(&tx, &delta.edges)?;
        tx.commit()?;
        Ok(())
    }

    /// Apply a delta and record `meta` keys in the same transaction.
    ///
    /// For a producer tailing an append-only file: the graph and the offset it
    /// was read up to have to land together, or a crash between two commits
    /// replays lines that were already applied.
    pub fn apply_delta_with_meta(
        &mut self,
        delta: &GraphDelta,
        meta: &[(&str, &str)],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        for e in &delta.removed_edges {
            tx.execute(
                "DELETE FROM edges WHERE kind = ?1 AND src = ?2 AND dst = ?3 AND source = ?4",
                params![e.kind, e.src.as_str(), e.dst.as_str(), e.source],
            )?;
        }
        delete_nodes_tx(&tx, &delta.removed_node_ids)?;
        upsert_nodes_tx(&tx, &delta.nodes)?;
        upsert_edges_tx(&tx, &delta.edges)?;
        for (key, value) in meta {
            tx.execute(
                "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every distinct origin starting with `prefix`, e.g. `claude://`.
    ///
    /// A full index prunes by walking the filesystem, so nodes hanging off a
    /// synthetic origin are invisible to it. This is how a producer finds the
    /// origins it wrote last time and retracts the ones whose source is gone.
    pub fn origins_with_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let like = format!("{}%", prefix.replace('%', "\\%").replace('_', "\\_"));
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT origin FROM nodes WHERE origin LIKE ?1 ESCAPE '\\'
             UNION SELECT DISTINCT origin FROM edges WHERE origin LIKE ?1 ESCAPE '\\'",
        )?;
        let rows = stmt.query_map([&like], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Replace everything previously produced from `origin` with the given
    /// nodes and edges, returning the delta a live consumer should apply.
    pub fn replace_origin(
        &mut self,
        origin: &str,
        nodes: &[Node],
        edges: &[Edge],
    ) -> Result<GraphDelta> {
        let tx = self.conn.transaction()?;
        let old_ids: Vec<NodeId> = {
            let mut st = tx.prepare("SELECT id FROM nodes WHERE origin = ?1")?;
            let rows = st.query_map([origin], |r| r.get::<_, String>(0))?;
            rows.map(|r| r.map(NodeId::new))
                .collect::<std::result::Result<_, _>>()?
        };
        let old_edges: Vec<Edge> = {
            let mut st = tx.prepare(
                "SELECT kind, src, dst, props, source, origin FROM edges WHERE origin = ?1",
            )?;
            let rows = st.query_map([origin], row_to_edge)?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        let new_ids: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        let removed_node_ids: Vec<NodeId> = old_ids
            .into_iter()
            .filter(|id| !new_ids.contains(id.as_str()))
            .collect();
        let removed_edges: Vec<Edge> = old_edges
            .into_iter()
            .filter(|old| {
                !edges.iter().any(|e| {
                    e.kind == old.kind
                        && e.src == old.src
                        && e.dst == old.dst
                        && e.source == old.source
                })
            })
            .collect();

        tx.execute("DELETE FROM edges WHERE origin = ?1", [origin])?;
        delete_nodes_tx(&tx, &removed_node_ids)?;
        tx.execute("DELETE FROM unresolved WHERE origin = ?1", [origin])?;
        upsert_nodes_tx(&tx, nodes)?;
        upsert_edges_tx(&tx, edges)?;
        tx.commit()?;
        Ok(GraphDelta {
            phase: DeltaPhase::Live,
            removed_node_ids,
            removed_edges,
            nodes: nodes.to_vec(),
            edges: edges.to_vec(),
            initial_complete: false,
        })
    }

    /// Delete nodes (and their edges) by id.
    pub fn delete_nodes(&mut self, ids: &[NodeId]) -> Result<()> {
        let tx = self.conn.transaction()?;
        delete_nodes_tx(&tx, ids)?;
        tx.commit()?;
        Ok(())
    }

    /// Remove everything produced from a given origin (a deleted file).
    pub fn delete_origin(&mut self, origin: &str) -> Result<GraphDelta> {
        self.replace_origin(origin, &[], &[])
    }

    /// Remove everything a single producer emitted, e.g. `spore:acme.adr`.
    ///
    /// Disabling a spore has to *retract* its nodes, not just stop making new
    /// ones, and every node and edge already records which producer made it —
    /// so this is a delete by `source` rather than a full reindex.
    pub fn delete_by_source(&mut self, source: &str) -> Result<GraphDelta> {
        let ids: Vec<NodeId> = {
            let mut st = self
                .conn
                .prepare("SELECT id FROM nodes WHERE source = ?1")?;
            let rows = st.query_map(params![source], |r| r.get::<_, String>(0))?;
            rows.map(|r| r.map(NodeId::new))
                .collect::<std::result::Result<_, _>>()?
        };
        let edges: Vec<Edge> = {
            let mut st = self.conn.prepare(
                "SELECT kind, src, dst, props, source, origin FROM edges WHERE source = ?1",
            )?;
            let rows = st.query_map(params![source], row_to_edge)?;
            rows.collect::<std::result::Result<_, _>>()?
        };

        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM edges WHERE source = ?1", params![source])?;
        tx.execute("DELETE FROM nodes WHERE source = ?1", params![source])?;
        tx.commit()?;

        let mut delta = GraphDelta::new(DeltaPhase::Live);
        delta.removed_node_ids = ids;
        delta.removed_edges = edges;
        Ok(delta)
    }

    /// Remove `Package` nodes nothing imports any more.
    pub fn gc_orphan_packages(&mut self) -> Result<Vec<NodeId>> {
        let ids: Vec<NodeId> = {
            let mut st = self.conn.prepare(
                "SELECT n.id FROM nodes n WHERE n.kind = ?1 AND NOT EXISTS (SELECT 1 FROM edges e WHERE e.dst = n.id)",
            )?;
            let rows = st.query_map(params![NodeKind::PACKAGE], |r| r.get::<_, String>(0))?;
            rows.map(|r| r.map(NodeId::new))
                .collect::<std::result::Result<_, _>>()?
        };
        if !ids.is_empty() {
            self.delete_nodes(&ids)?;
        }
        Ok(ids)
    }

    /// Remove nodes of the given kinds that nothing points at any more.
    ///
    /// For nodes shared between files and therefore emitted with no origin —
    /// an unresolved wiki-link placeholder is asked for by many notes and owned
    /// by none — so `replace_origin` can never retract them. Same inbound-only
    /// test as `gc_orphan_packages`, for the same reason: they are pointed at,
    /// never outward.
    pub fn gc_orphan_by_kind(&mut self, kinds: &[String]) -> Result<Vec<NodeId>> {
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; kinds.len()].join(",");
        let ids: Vec<NodeId> = {
            let mut st = self.conn.prepare(&format!(
                "SELECT n.id FROM nodes n
                  WHERE n.kind IN ({placeholders}) AND n.origin IS NULL
                    AND NOT EXISTS (SELECT 1 FROM edges e WHERE e.dst = n.id)"
            ))?;
            let rows =
                st.query_map(rusqlite::params_from_iter(kinds), |r| r.get::<_, String>(0))?;
            rows.map(|r| r.map(NodeId::new))
                .collect::<std::result::Result<_, _>>()?
        };
        if !ids.is_empty() {
            self.delete_nodes(&ids)?;
        }
        Ok(ids)
    }

    /// Of these ids, the derived nodes that belong to no single file.
    ///
    /// Replaying an unchanged file has to carry its shared nodes along with its
    /// edges, or the edge arrives pointing at nothing. The walker's own
    /// directories and files have no origin either, but the walk delivers those
    /// itself, so they are not shared nodes and are left out.
    pub fn shared_nodes(&self, ids: &[NodeId]) -> Result<Vec<Node>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let structural = [
            NodeKind::DIRECTORY,
            NodeKind::REPO,
            NodeKind::FILE,
            NodeKind::MANIFEST,
        ];
        let id_slots = vec!["?"; ids.len()].join(",");
        let kind_slots = vec!["?"; structural.len()].join(",");
        let mut st = self.conn.prepare(&format!(
            "SELECT {NODE_COLS} FROM nodes
              WHERE origin IS NULL AND kind NOT IN ({kind_slots}) AND id IN ({id_slots})
              ORDER BY id"
        ))?;
        let params = structural
            .iter()
            .copied()
            .chain(ids.iter().map(NodeId::as_str));
        let rows = st.query_map(rusqlite::params_from_iter(params), row_to_node)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Remove `Commit` and `Session` nodes with no edges left in either
    /// direction — a commit that only touched files this workspace does not
    /// index, or a session that never reached anything here.
    ///
    /// Note the rule is "no edges at all", not the inbound-only test
    /// `gc_orphan_packages` uses: these nodes point *outward* at files, so an
    /// inbound test would collect every one of them on sight.
    pub fn gc_disconnected_history(&mut self) -> Result<Vec<NodeId>> {
        let ids: Vec<NodeId> = {
            let mut st = self.conn.prepare(
                "SELECT n.id FROM nodes n WHERE n.kind IN (?1, ?2)
                   AND NOT EXISTS (SELECT 1 FROM edges e WHERE e.dst = n.id OR e.src = n.id)",
            )?;
            let rows = st.query_map(params![NodeKind::COMMIT, NodeKind::SESSION], |r| {
                r.get::<_, String>(0)
            })?;
            rows.map(|r| r.map(NodeId::new))
                .collect::<std::result::Result<_, _>>()?
        };
        if !ids.is_empty() {
            self.delete_nodes(&ids)?;
        }
        Ok(ids)
    }

    pub fn clear(&mut self) -> Result<()> {
        self.conn.execute_batch(
            "DELETE FROM edges; DELETE FROM nodes; DELETE FROM files; DELETE FROM unresolved;",
        )?;
        Ok(())
    }

    // ---- reads ------------------------------------------------------------

    pub fn get_node(&self, id: &NodeId) -> Result<Option<Node>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {NODE_COLS} FROM nodes WHERE id = ?1"),
                [id.as_str()],
                row_to_node,
            )
            .optional()?)
    }

    pub fn get_nodes(&self, ids: &[NodeId]) -> Result<Vec<Node>> {
        let mut out = Vec::with_capacity(ids.len());
        let mut st = self
            .conn
            .prepare(&format!("SELECT {NODE_COLS} FROM nodes WHERE id = ?1"))?;
        for id in ids {
            if let Some(n) = st.query_row([id.as_str()], row_to_node).optional()? {
                out.push(n);
            }
        }
        Ok(out)
    }

    pub fn query_nodes(&self, q: &NodeQuery) -> Result<Vec<Node>> {
        let mut sql = format!("SELECT {NODE_COLS} FROM nodes WHERE 1=1");
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        if !q.kinds.is_empty() {
            sql.push_str(" AND kind IN (");
            sql.push_str(&placeholders(args.len(), q.kinds.len()));
            sql.push(')');
            args.extend(q.kinds.iter().map(|k| k.clone().into()));
        }
        if let Some(repo) = &q.repo {
            args.push(repo.as_str().to_string().into());
            sql.push_str(&format!(" AND repo_id = ?{}", args.len()));
        }
        if let Some(prefix) = &q.path_prefix {
            args.push(format!("{}%", like_escape(prefix)).into());
            sql.push_str(&format!(" AND path LIKE ?{} ESCAPE '\\'", args.len()));
        }
        if let Some(text) = q.text.as_deref().filter(|t| !t.trim().is_empty()) {
            args.push(format!("%{}%", like_escape(text.trim())).into());
            let i = args.len();
            sql.push_str(&format!(" AND (label LIKE ?{i} ESCAPE '\\' OR path LIKE ?{i} ESCAPE '\\' OR id LIKE ?{i} ESCAPE '\\')"));
        }
        sql.push_str(" ORDER BY kind, path, label");
        if let Some(limit) = q.limit {
            sql.push_str(&format!(" LIMIT {limit}"));
        }
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(rusqlite::params_from_iter(args), row_to_node)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn get_edges(&self, q: &EdgeQuery) -> Result<Vec<Edge>> {
        let mut sql =
            "SELECT kind, src, dst, props, source, origin FROM edges WHERE 1=1".to_string();
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(src) = &q.src {
            args.push(src.as_str().to_string().into());
            sql.push_str(&format!(" AND src = ?{}", args.len()));
        }
        if let Some(dst) = &q.dst {
            args.push(dst.as_str().to_string().into());
            sql.push_str(&format!(" AND dst = ?{}", args.len()));
        }
        if !q.kinds.is_empty() {
            sql.push_str(" AND kind IN (");
            sql.push_str(&placeholders(args.len(), q.kinds.len()));
            sql.push(')');
            args.extend(q.kinds.iter().map(|k| k.clone().into()));
        }
        sql.push_str(" ORDER BY kind, src, dst");
        if let Some(limit) = q.limit {
            sql.push_str(&format!(" LIMIT {limit}"));
        }
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(rusqlite::params_from_iter(args), row_to_edge)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn nodes_by_origin(&self, origin: &str) -> Result<Vec<Node>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {NODE_COLS} FROM nodes WHERE origin = ?1 ORDER BY id"
        ))?;
        let rows = st.query_map([origin], row_to_node)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn edges_by_origin(&self, origin: &str) -> Result<Vec<Edge>> {
        let mut st = self.conn.prepare("SELECT kind, src, dst, props, source, origin FROM edges WHERE origin = ?1 ORDER BY kind, src, dst")?;
        let rows = st.query_map([origin], row_to_edge)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Ids of filesystem-backed nodes whose path is `prefix` or lies under `prefix/`.
    pub fn node_ids_under(&self, prefix: &str) -> Result<Vec<NodeId>> {
        let mut st = self.conn.prepare(
            "SELECT id FROM nodes WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\' ORDER BY id",
        )?;
        let like = format!("{}/%", like_escape(prefix));
        let rows = st.query_map(params![prefix, like], |r| r.get::<_, String>(0))?;
        Ok(rows
            .map(|r| r.map(NodeId::new))
            .collect::<std::result::Result<_, _>>()?)
    }

    /// Node ids whose `origin` is `prefix` or lies under `prefix/` (derived nodes of deleted files).
    pub fn origins_under(&self, prefix: &str) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT DISTINCT origin FROM nodes WHERE origin = ?1 OR origin LIKE ?2 ESCAPE '\\' UNION SELECT DISTINCT origin FROM edges WHERE origin = ?1 OR origin LIKE ?2 ESCAPE '\\' UNION SELECT path FROM files WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'")?;
        let like = format!("{}/%", like_escape(prefix));
        let rows = st.query_map(params![prefix, like], |r| r.get::<_, Option<String>>(0))?;
        Ok(rows.filter_map(|r| r.ok().flatten()).collect())
    }

    /// Everything in the store (dangling edges excluded).
    pub fn snapshot(&self) -> Result<Subgraph> {
        let nodes = self.query_nodes(&NodeQuery::default())?;
        let ids: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        let edges = self
            .get_edges(&EdgeQuery::default())?
            .into_iter()
            .filter(|e| ids.contains(e.src.as_str()) && ids.contains(e.dst.as_str()))
            .collect();
        Ok(Subgraph {
            nodes,
            edges,
            truncated: false,
        })
    }

    /// BFS from `roots` up to `depth` hops. Depth 0 returns just the roots.
    pub fn neighborhood(&self, roots: &[NodeId], q: &NeighborhoodQuery) -> Result<Subgraph> {
        let direction = q.direction.unwrap_or(Direction::Both);
        let limit = q.limit.map(|l| l as usize).unwrap_or(usize::MAX);
        let mut seen: HashSet<NodeId> = roots.iter().cloned().collect();
        let mut order: Vec<NodeId> = roots.to_vec();
        let mut frontier: Vec<NodeId> = roots.to_vec();
        let mut edges: HashMap<(String, String, String, String), Edge> = HashMap::new();
        let mut truncated = false;

        'outer: for _ in 0..q.depth {
            if frontier.is_empty() {
                break;
            }
            let mut next: Vec<NodeId> = Vec::new();
            for id in &frontier {
                let mut found: Vec<Edge> = Vec::new();
                if matches!(direction, Direction::Out | Direction::Both) {
                    found.extend(self.get_edges(&EdgeQuery {
                        src: Some(id.clone()),
                        kinds: q.edge_kinds.clone(),
                        ..Default::default()
                    })?);
                }
                if matches!(direction, Direction::In | Direction::Both) {
                    found.extend(self.get_edges(&EdgeQuery {
                        dst: Some(id.clone()),
                        kinds: q.edge_kinds.clone(),
                        ..Default::default()
                    })?);
                }
                for e in found {
                    let other = if &e.src == id {
                        e.dst.clone()
                    } else {
                        e.src.clone()
                    };
                    if seen.insert(other.clone()) {
                        if order.len() >= limit {
                            truncated = true;
                            break 'outer;
                        }
                        order.push(other.clone());
                        next.push(other);
                    }
                    edges.insert(
                        (
                            e.kind.clone(),
                            e.src.0.clone(),
                            e.dst.0.clone(),
                            e.source.clone(),
                        ),
                        e,
                    );
                }
            }
            frontier = next;
        }

        let nodes = self.get_nodes(&order)?;
        let present: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        let mut edges: Vec<Edge> = edges
            .into_values()
            .filter(|e| present.contains(e.src.as_str()) && present.contains(e.dst.as_str()))
            .collect();
        edges.sort_by(|a, b| (&a.kind, &a.src, &a.dst).cmp(&(&b.kind, &b.src, &b.dst)));
        Ok(Subgraph {
            nodes,
            edges,
            truncated,
        })
    }

    pub fn counts(&self) -> Result<Counts> {
        let count = |sql: &str| -> Result<u64> {
            Ok(self.conn.query_row(sql, [], |r| r.get::<_, i64>(0))? as u64)
        };
        let nodes = count("SELECT COUNT(*) FROM nodes")?;
        let edges = count("SELECT COUNT(*) FROM edges")?;
        let files = count("SELECT COUNT(*) FROM files")?;
        let unresolved = count("SELECT COUNT(*) FROM unresolved")?;
        let mut st = self
            .conn
            .prepare("SELECT kind, COUNT(*) FROM nodes GROUP BY kind ORDER BY kind")?;
        let by_kind = st
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(Counts {
            nodes,
            edges,
            files,
            unresolved,
            by_kind,
        })
    }

    // ---- files ------------------------------------------------------------

    pub fn file_record(&self, path: &str) -> Result<Option<FileRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT path, mtime, size, fingerprint, lang, indexed_at FROM files WHERE path = ?1",
                [path],
                row_to_file,
            )
            .optional()?)
    }

    pub fn all_files(&self) -> Result<Vec<FileRecord>> {
        let mut st = self.conn.prepare(
            "SELECT path, mtime, size, fingerprint, lang, indexed_at FROM files ORDER BY path",
        )?;
        let rows = st.query_map([], row_to_file)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn upsert_file(&self, rec: &FileRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO files(path, mtime, size, fingerprint, lang, indexed_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(path) DO UPDATE SET mtime = excluded.mtime, size = excluded.size,
             fingerprint = excluded.fingerprint, lang = excluded.lang, indexed_at = excluded.indexed_at",
            params![rec.path, rec.mtime, rec.size, rec.fingerprint, rec.lang, rec.indexed_at],
        )?;
        Ok(())
    }

    pub fn delete_file(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM files WHERE path = ?1", [path])?;
        Ok(())
    }

    // ---- unresolved -------------------------------------------------------

    pub fn record_unresolved(&self, items: &[Unresolved]) -> Result<()> {
        let mut st = self.conn.prepare(
            "INSERT INTO unresolved(origin, specifier, line, reason) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for u in items {
            st.execute(params![u.origin, u.specifier, u.line, u.reason])?;
        }
        Ok(())
    }

    pub fn clear_unresolved(&self, origin: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM unresolved WHERE origin = ?1", [origin])?;
        Ok(())
    }

    pub fn list_unresolved(&self, limit: Option<u32>) -> Result<Vec<Unresolved>> {
        let sql = format!(
            "SELECT origin, specifier, line, reason FROM unresolved ORDER BY origin, line{}",
            limit.map(|l| format!(" LIMIT {l}")).unwrap_or_default()
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map([], |r| {
            Ok(Unresolved {
                origin: r.get(0)?,
                specifier: r.get(1)?,
                line: r.get(2)?,
                reason: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
}

const NODE_COLS: &str = "id, kind, label, path, repo_id, props, fingerprint, source, origin";

fn row_to_node(r: &Row<'_>) -> rusqlite::Result<Node> {
    let props: String = r.get(5)?;
    Ok(Node {
        id: NodeId::new(r.get::<_, String>(0)?),
        kind: r.get(1)?,
        label: r.get(2)?,
        path: r.get(3)?,
        repo_id: r.get::<_, Option<String>>(4)?.map(NodeId::new),
        props: serde_json::from_str(&props)
            .unwrap_or(serde_json::Value::Object(Default::default())),
        fingerprint: r.get(6)?,
        source: r.get(7)?,
        origin: r.get(8)?,
    })
}

fn row_to_edge(r: &Row<'_>) -> rusqlite::Result<Edge> {
    let props: String = r.get(3)?;
    Ok(Edge {
        kind: r.get(0)?,
        src: NodeId::new(r.get::<_, String>(1)?),
        dst: NodeId::new(r.get::<_, String>(2)?),
        props: serde_json::from_str(&props)
            .unwrap_or(serde_json::Value::Object(Default::default())),
        source: r.get(4)?,
        origin: r.get(5)?,
    })
}

fn row_to_file(r: &Row<'_>) -> rusqlite::Result<FileRecord> {
    Ok(FileRecord {
        path: r.get(0)?,
        mtime: r.get(1)?,
        size: r.get(2)?,
        fingerprint: r.get(3)?,
        lang: r.get(4)?,
        indexed_at: r.get(5)?,
    })
}

fn upsert_nodes_tx(tx: &rusqlite::Transaction<'_>, nodes: &[Node]) -> Result<()> {
    let now = aneural_core::now_millis();
    let mut st = tx.prepare_cached(
        "INSERT INTO nodes(id, kind, label, path, repo_id, props, fingerprint, source, origin, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)
         ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, label = excluded.label, path = excluded.path,
           repo_id = excluded.repo_id, props = excluded.props, fingerprint = excluded.fingerprint,
           source = excluded.source, origin = excluded.origin, updated_at = excluded.updated_at",
    )?;
    for n in nodes {
        st.execute(params![
            n.id.as_str(),
            n.kind,
            n.label,
            n.path,
            n.repo_id.as_ref().map(|r| r.as_str()),
            serde_json::to_string(&n.props)?,
            n.fingerprint,
            n.source,
            n.origin,
            now
        ])?;
    }
    Ok(())
}

fn upsert_edges_tx(tx: &rusqlite::Transaction<'_>, edges: &[Edge]) -> Result<()> {
    let mut st = tx.prepare_cached(
        "INSERT INTO edges(kind, src, dst, props, source, origin) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(kind, src, dst, source) DO UPDATE SET props = excluded.props, origin = excluded.origin",
    )?;
    for e in edges {
        st.execute(params![
            e.kind,
            e.src.as_str(),
            e.dst.as_str(),
            serde_json::to_string(&e.props)?,
            e.source,
            e.origin
        ])?;
    }
    Ok(())
}

fn delete_nodes_tx(tx: &rusqlite::Transaction<'_>, ids: &[NodeId]) -> Result<()> {
    let mut del_edges = tx.prepare_cached("DELETE FROM edges WHERE src = ?1 OR dst = ?1")?;
    let mut del_node = tx.prepare_cached("DELETE FROM nodes WHERE id = ?1")?;
    for id in ids {
        del_edges.execute([id.as_str()])?;
        del_node.execute([id.as_str()])?;
    }
    Ok(())
}

fn placeholders(offset: usize, n: usize) -> String {
    (0..n)
        .map(|i| format!("?{}", offset + i + 1))
        .collect::<Vec<_>>()
        .join(", ")
}

fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use aneural_core::kinds::{EdgeKind, Source};

    fn file(p: &str) -> Node {
        Node::new(
            NodeId::file(p),
            NodeKind::FILE,
            p.rsplit('/').next().unwrap(),
            Source::WALKER,
        )
        .with_path(p)
    }

    #[test]
    fn a_shared_placeholder_outlives_one_file_and_dies_with_the_last() {
        let mut s = Store::open_in_memory().unwrap();
        // Two notes, both linking to a note nobody has written. The placeholder
        // belongs to neither, so it carries no origin.
        let missing = Node::new(
            NodeId::new("missing:not-written-yet"),
            "MissingNote",
            "Not Written Yet",
            "spore:aneural.wiki-links",
        );
        let link = |from: &str| {
            Edge::new(
                EdgeKind::RELATES_TO,
                NodeId::file(from),
                NodeId::new("missing:not-written-yet"),
                "spore:aneural.wiki-links",
            )
            .with_origin(from)
        };

        s.replace_origin("a.md", &[file("a.md"), missing.clone()], &[link("a.md")])
            .unwrap();
        s.replace_origin("b.md", &[file("b.md"), missing.clone()], &[link("b.md")])
            .unwrap();
        assert!(
            s.get_node(&NodeId::new("missing:not-written-yet"))
                .unwrap()
                .is_some()
        );

        // Replaying an unchanged file has to carry the shared node with it, or
        // the edge arrives pointing at nothing.
        let shared = s
            .shared_nodes(&[NodeId::new("missing:not-written-yet"), NodeId::file("a.md")])
            .unwrap();
        assert_eq!(
            shared.len(),
            1,
            "the placeholder is shared; a.md is structural and the walk delivers it"
        );
        assert_eq!(shared[0].id, NodeId::new("missing:not-written-yet"));

        let kinds = vec!["MissingNote".to_string()];

        // a.md stops linking to it. b.md still does, so it stays.
        s.replace_origin("a.md", &[file("a.md")], &[]).unwrap();
        assert!(s.gc_orphan_by_kind(&kinds).unwrap().is_empty());
        assert!(
            s.get_node(&NodeId::new("missing:not-written-yet"))
                .unwrap()
                .is_some()
        );

        // The last link goes and it is collected.
        s.replace_origin("b.md", &[file("b.md")], &[]).unwrap();
        assert_eq!(
            s.gc_orphan_by_kind(&kinds).unwrap(),
            vec![NodeId::new("missing:not-written-yet")]
        );
        assert!(
            s.get_node(&NodeId::new("missing:not-written-yet"))
                .unwrap()
                .is_none()
        );
        // Nothing to collect is not an error, and no kinds means no query.
        assert!(s.gc_orphan_by_kind(&kinds).unwrap().is_empty());
        assert!(s.gc_orphan_by_kind(&[]).unwrap().is_empty());
    }

    fn seeded() -> Store {
        let mut s = Store::open_in_memory().unwrap();
        s.upsert_nodes(&[
            Node::new(NodeId::dir("."), NodeKind::DIRECTORY, ".", Source::WALKER).with_path("."),
            Node::new(
                NodeId::dir("src"),
                NodeKind::DIRECTORY,
                "src",
                Source::WALKER,
            )
            .with_path("src"),
            file("src/a.ts"),
            file("src/b.ts"),
            file("src/c.ts"),
            Node::new(
                NodeId::package("npm", "react"),
                NodeKind::PACKAGE,
                "react",
                Source::LANG,
            ),
        ])
        .unwrap();
        s.upsert_edges(&[
            Edge::new(
                EdgeKind::CONTAINS,
                NodeId::dir("."),
                NodeId::dir("src"),
                Source::WALKER,
            ),
            Edge::new(
                EdgeKind::CONTAINS,
                NodeId::dir("src"),
                NodeId::file("src/a.ts"),
                Source::WALKER,
            ),
            Edge::new(
                EdgeKind::CONTAINS,
                NodeId::dir("src"),
                NodeId::file("src/b.ts"),
                Source::WALKER,
            ),
            Edge::new(
                EdgeKind::CONTAINS,
                NodeId::dir("src"),
                NodeId::file("src/c.ts"),
                Source::WALKER,
            ),
            Edge::new(
                EdgeKind::IMPORTS,
                NodeId::file("src/a.ts"),
                NodeId::file("src/b.ts"),
                Source::LANG,
            )
            .with_origin("src/a.ts"),
            Edge::new(
                EdgeKind::IMPORTS,
                NodeId::file("src/b.ts"),
                NodeId::file("src/c.ts"),
                Source::LANG,
            )
            .with_origin("src/b.ts"),
            Edge::new(
                EdgeKind::IMPORTS,
                NodeId::file("src/a.ts"),
                NodeId::package("npm", "react"),
                Source::LANG,
            )
            .with_origin("src/a.ts"),
        ])
        .unwrap();
        s
    }

    #[test]
    fn reopen_keeps_schema() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("cache/index.db");
        {
            let mut s = Store::open(&p).unwrap();
            s.upsert_nodes(&[file("x.ts")]).unwrap();
        }
        let s = Store::open(&p).unwrap();
        assert!(s.get_node(&NodeId::file("x.ts")).unwrap().is_some());
    }

    #[test]
    fn query_and_edges() {
        let s = seeded();
        let files = s
            .query_nodes(&NodeQuery {
                kinds: vec![NodeKind::FILE.into()],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(files.len(), 3);
        let hits = s
            .query_nodes(&NodeQuery {
                text: Some("b.ts".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        let under = s
            .query_nodes(&NodeQuery {
                path_prefix: Some("src/".into()),
                limit: Some(2),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(under.len(), 2);
        let out = s
            .get_edges(&EdgeQuery {
                src: Some(NodeId::file("src/a.ts")),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(out.len(), 2);
        let imports = s
            .get_edges(&EdgeQuery {
                src: Some(NodeId::file("src/a.ts")),
                kinds: vec![EdgeKind::IMPORTS.into()],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(imports.len(), 2);
    }

    #[test]
    fn neighborhood_bfs() {
        let s = seeded();
        let q = NeighborhoodQuery {
            depth: 1,
            direction: Some(Direction::Out),
            edge_kinds: vec![EdgeKind::IMPORTS.into()],
            limit: None,
        };
        let g = s.neighborhood(&[NodeId::file("src/a.ts")], &q).unwrap();
        assert_eq!(g.nodes.len(), 3, "a.ts, b.ts and the react it imports");
        assert_eq!(g.edges.len(), 2);
        let q2 = NeighborhoodQuery {
            depth: 2,
            direction: Some(Direction::Out),
            edge_kinds: vec![EdgeKind::IMPORTS.into()],
            limit: None,
        };
        let g2 = s.neighborhood(&[NodeId::file("src/a.ts")], &q2).unwrap();
        assert_eq!(g2.nodes.len(), 4);
        let q3 = NeighborhoodQuery {
            depth: 3,
            direction: Some(Direction::Both),
            edge_kinds: vec![],
            limit: Some(3),
        };
        let g3 = s.neighborhood(&[NodeId::file("src/c.ts")], &q3).unwrap();
        assert!(g3.truncated);
        assert_eq!(g3.nodes.len(), 3);
        let g0 = s
            .neighborhood(
                &[NodeId::file("src/c.ts")],
                &NeighborhoodQuery {
                    depth: 0,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(g0.nodes.len(), 1);
        assert!(g0.edges.is_empty());
    }

    #[test]
    fn replace_origin_emits_delta_and_gc() {
        let mut s = seeded();
        let comment = Node::new(
            NodeId::comment("src/a.ts", "TODO x"),
            "Comment",
            "TODO x",
            "spore:comments",
        )
        .with_origin("src/a.ts");
        let ann = Edge::new(
            EdgeKind::ANNOTATES,
            comment.id.clone(),
            NodeId::file("src/a.ts"),
            "spore:comments",
        )
        .with_origin("src/a.ts");
        let d1 = s
            .replace_origin(
                "src/a.ts",
                std::slice::from_ref(&comment),
                std::slice::from_ref(&ann),
            )
            .unwrap();
        assert!(d1.removed_node_ids.is_empty());
        assert_eq!(
            d1.removed_edges.len(),
            2,
            "both old IMPORTS from a.ts are gone"
        );
        assert_eq!(d1.nodes.len(), 1);

        let gone = s.gc_orphan_packages().unwrap();
        assert_eq!(gone, vec![NodeId::package("npm", "react")]);

        let d2 = s.replace_origin("src/a.ts", &[], &[]).unwrap();
        assert_eq!(d2.removed_node_ids, vec![comment.id.clone()]);
        assert_eq!(d2.removed_edges.len(), 1);
        assert!(s.get_node(&comment.id).unwrap().is_none());
        assert!(
            s.get_edges(&EdgeQuery {
                dst: Some(NodeId::file("src/a.ts")),
                kinds: vec![EdgeKind::ANNOTATES.into()],
                ..Default::default()
            })
            .unwrap()
            .is_empty()
        );
    }

    /// A commit points outward at the files it changed, so the inbound-only
    /// rule that collects orphan packages would wrongly collect every commit.
    #[test]
    fn history_nodes_are_collected_only_when_nothing_is_left_of_them() {
        let mut s = seeded();
        let touched = Node::new(
            NodeId::commit(".", "abc123"),
            NodeKind::COMMIT,
            "Grow the graph",
            Source::GIT,
        );
        let stranded = Node::new(
            NodeId::commit(".", "def456"),
            NodeKind::COMMIT,
            "Touched nothing we index",
            Source::GIT,
        );
        let modifies = Edge::new(
            EdgeKind::MODIFIES,
            touched.id.clone(),
            NodeId::file("src/a.ts"),
            Source::GIT,
        );
        s.upsert_nodes(&[touched.clone(), stranded.clone()])
            .unwrap();
        s.upsert_edges(std::slice::from_ref(&modifies)).unwrap();

        assert!(
            s.gc_orphan_packages().unwrap().is_empty(),
            "the package rule must not reach commits at all"
        );
        assert_eq!(s.gc_disconnected_history().unwrap(), vec![stranded.id]);
        assert!(
            s.get_node(&touched.id).unwrap().is_some(),
            "a commit with an outgoing edge stays"
        );
    }

    #[test]
    fn origins_are_found_by_prefix_and_the_graph_lands_with_its_offset() {
        let mut s = seeded();
        let session = Node::new(
            NodeId::session("uuid-1"),
            NodeKind::SESSION,
            "A session",
            Source::CLAUDE,
        )
        .with_origin("claude://session/uuid-1");
        let edge = Edge::new(
            EdgeKind::MODIFIES,
            session.id.clone(),
            NodeId::file("src/a.ts"),
            Source::CLAUDE,
        )
        .with_origin("claude://session/uuid-1");

        let mut delta = GraphDelta::new(DeltaPhase::Live);
        delta.nodes = vec![session.clone()];
        delta.edges = vec![edge];
        s.apply_delta_with_meta(&delta, &[("claude.tail.uuid-1", "{\"offset\":42}")])
            .unwrap();

        assert!(s.get_node(&session.id).unwrap().is_some());
        assert_eq!(
            s.meta_get("claude.tail.uuid-1").unwrap().as_deref(),
            Some("{\"offset\":42}")
        );
        assert_eq!(
            s.origins_with_prefix("claude://").unwrap(),
            vec!["claude://session/uuid-1"]
        );
        assert!(
            s.origins_with_prefix("git://").unwrap().is_empty(),
            "a prefix that matches nothing finds nothing"
        );
    }

    #[test]
    fn apply_delta_and_counts() {
        let mut s = seeded();
        let mut d = GraphDelta::new(DeltaPhase::Live);
        d.removed_node_ids.push(NodeId::file("src/c.ts"));
        d.nodes.push(file("src/d.ts"));
        s.apply_delta(&d).unwrap();
        assert!(s.get_node(&NodeId::file("src/c.ts")).unwrap().is_none());
        assert!(
            s.get_edges(&EdgeQuery {
                dst: Some(NodeId::file("src/c.ts")),
                ..Default::default()
            })
            .unwrap()
            .is_empty()
        );
        let c = s.counts().unwrap();
        assert_eq!(c.nodes, 6);
        assert!(c.by_kind.iter().any(|(k, n)| k == "File" && *n == 3));
        let snap = s.snapshot().unwrap();
        assert_eq!(snap.nodes.len(), 6);
        assert!(snap.edges.iter().all(|e| e.dst != NodeId::file("src/c.ts")));
    }

    #[test]
    fn files_and_unresolved() {
        let s = Store::open_in_memory().unwrap();
        let rec = FileRecord {
            path: "a.ts".into(),
            mtime: 1,
            size: 2,
            fingerprint: "abc".into(),
            lang: Some("typescript".into()),
            indexed_at: 3,
        };
        s.upsert_file(&rec).unwrap();
        assert_eq!(s.file_record("a.ts").unwrap().unwrap(), rec);
        s.record_unresolved(&[Unresolved {
            origin: "a.ts".into(),
            specifier: "./nope".into(),
            line: 3,
            reason: "not found".into(),
        }])
        .unwrap();
        assert_eq!(s.list_unresolved(None).unwrap().len(), 1);
        s.clear_unresolved("a.ts").unwrap();
        assert!(s.list_unresolved(None).unwrap().is_empty());
        s.delete_file("a.ts").unwrap();
        assert!(s.file_record("a.ts").unwrap().is_none());
        s.meta_set("k", "v").unwrap();
        assert_eq!(s.meta_get("k").unwrap().as_deref(), Some("v"));
    }
}
