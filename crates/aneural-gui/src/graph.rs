//! In-memory graph ECS state and delta application.

use crate::render::{Visuals, spawn_node_visuals};
use crate::workspace::WorkspaceRes;
use aneural_core::kinds::EdgeKind;
use aneural_core::{Edge, GraphDelta, Node, NodeId};
use bevy::ecs::entity::EntityHashMap;
use bevy::prelude::*;
use std::collections::{HashMap, HashSet};

#[derive(Component, Clone, Debug)]
pub struct GraphNode {
    pub id: NodeId,
    pub kind: String,
    pub label: String,
    pub path: Option<String>,
    pub repo_id: Option<NodeId>,
    pub props: serde_json::Value,
}

impl GraphNode {
    /// A string prop, as `aneural_core::Node::prop_str` reads one. The GUI
    /// keeps props as raw JSON, so every reader would otherwise repeat this.
    pub fn prop_str(&self, key: &str) -> Option<&str> {
        self.props.get(key)?.as_str()
    }
}

#[derive(Component, Clone, Debug)]
pub struct GraphEdge {
    pub kind: String,
    pub src: Entity,
    pub dst: Entity,
    pub seed: u32,
    /// The edge's own props. Kept because *when* a thing happened lives here
    /// and nowhere else: a session's `MODIFIES` carries `firstAt`/`lastAt`, and
    /// a commit's carries `at`, which is what a timeline is made of.
    pub props: serde_json::Value,
}

#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Pos(pub Vec2);

#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Vel(pub Vec2);

/// How far a node has wandered from the position the layout gave it, and the
/// seed its wander is drawn from. Written by [`crate::circadian`] and added
/// on top of [`Pos`] wherever a node is drawn or picked; the layout never
/// sees it, so the graph breathes without the simulation noticing.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Drift {
    pub seed: u32,
    pub offset: Vec2,
}

#[derive(Component)]
pub struct Pinned;

#[derive(Component)]
pub struct Hidden;

#[derive(Component, Clone, Copy, Debug)]
pub struct GrowIn {
    pub t: f32,
    pub dur: f32,
}

impl Default for GrowIn {
    fn default() -> Self {
        GrowIn { t: 0.0, dur: 0.45 }
    }
}

pub type EdgeKey = (String, NodeId, NodeId);

#[derive(Resource, Default)]
pub struct GraphState {
    pub by_id: HashMap<NodeId, Entity>,
    /// The other direction, for the places that hold an entity and need to ask
    /// something about the node — the renderer deciding whether an edge's ends
    /// float, for one.
    pub ids: EntityHashMap<NodeId>,
    pub edges: HashMap<EdgeKey, Entity>,
    pub pending_edges: Vec<Edge>,
    /// Adjacency (undirected) for BFS/focus mode: id → (neighbor id, edge kind).
    pub adjacency: HashMap<NodeId, Vec<(NodeId, String)>>,
    /// Parent (CONTAINS src) per node, for sprouting and the layout's tree.
    pub parent: HashMap<NodeId, NodeId>,
    /// How many edges of each kind touch each node. A hairball is made of
    /// nodes with a great many links, so the renderer reads this to hold back
    /// the strands between busy ones.
    pub degree: EntityHashMap<Degrees>,
    pub node_count: usize,
    pub edge_count: usize,
    /// What the last live delta did, for whoever wants to react to it.
    ///
    /// Filled only for [`DeltaPhase::Live`] and drained every frame by
    /// [`crate::live`]. The initial index is skipped deliberately: three
    /// thousand nodes arriving because the app just opened is not news.
    pub stirred: Vec<(NodeId, Stir)>,
}

/// What happened to one node in a live delta.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stir {
    /// It was not here a moment ago.
    Appeared,
    /// It was, and its label or its props are not what they were.
    Changed,
}

/// A node's link count, per kind. Density is a
/// property of one relation at a time: a file with three imports and twenty
/// mentions is crowded in mentions and perfectly clear in imports.
#[derive(Default, Debug)]
pub struct Degrees {
    /// A node has a handful of distinct edge kinds at most, so a short list
    /// beats a map here.
    by_kind: Vec<(String, u32)>,
}

impl Degrees {
    fn bump(&mut self, kind: &str, up: bool) {
        match self.by_kind.iter_mut().find(|(k, _)| k == kind) {
            Some((_, n)) => *n = if up { *n + 1 } else { n.saturating_sub(1) },
            None if up => self.by_kind.push((kind.to_string(), 1)),
            None => {}
        }
    }

    pub fn of_kind(&self, kind: &str) -> u32 {
        self.by_kind
            .iter()
            .find(|(k, _)| k == kind)
            .map(|(_, n)| *n)
            .unwrap_or(0)
    }

    /// Does any kind this node still has an edge of satisfy `want`?
    pub fn any(&self, want: impl Fn(&str) -> bool) -> bool {
        self.by_kind
            .iter()
            .any(|(kind, n)| *n > 0 && want(kind.as_str()))
    }
}

impl GraphState {
    /// Links of one kind touching a node.
    pub fn kind_degree(&self, e: Entity, kind: &str) -> u32 {
        self.degree.get(&e).map(|d| d.of_kind(kind)).unwrap_or(0)
    }

    /// Does this node float? One that only relates to others (a note, an
    /// idea, a plan) has no place in the folder tree, so rather than being
    /// strung to what it relates to it hovers nearby, like a spore.
    pub fn floats(&self, e: Entity, id: &NodeId) -> bool {
        if self.parent.contains_key(id) {
            return false;
        }
        self.degree
            .get(&e)
            .is_some_and(|d| d.any(|kind| !EdgeKind::is_strand(kind)))
    }

    /// [`Self::floats`] for a node held only as an entity.
    pub fn floats_entity(&self, e: Entity) -> bool {
        self.ids.get(&e).is_some_and(|id| self.floats(e, id))
    }

    /// Nodes this one points at with an edge of `kind`.
    pub fn out_of(&self, id: &NodeId, kind: &str) -> Vec<NodeId> {
        self.directed(id, kind, true)
    }

    /// Nodes that point at this one with an edge of `kind`.
    pub fn pointing_at(&self, id: &NodeId, kind: &str) -> Vec<NodeId> {
        self.directed(id, kind, false)
    }

    /// Adjacency is undirected, so direction is recovered by asking whether the
    /// edge exists the way round we want.
    fn directed(&self, id: &NodeId, kind: &str, outgoing: bool) -> Vec<NodeId> {
        self.neighbors(id)
            .iter()
            .filter(|(_, k)| k == kind)
            .map(|(other, _)| other.clone())
            .filter(|other| {
                let key = match outgoing {
                    true => (kind.to_string(), id.clone(), other.clone()),
                    false => (kind.to_string(), other.clone(), id.clone()),
                };
                self.edges.contains_key(&key)
            })
            .collect()
    }

    pub fn neighbors(&self, id: &NodeId) -> &[(NodeId, String)] {
        self.adjacency.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// BFS up to `depth` over edges whose kind passes `allow`.
    pub fn neighborhood(
        &self,
        roots: &[NodeId],
        depth: u32,
        allow: &dyn Fn(&str) -> bool,
    ) -> HashSet<NodeId> {
        let mut seen: HashSet<NodeId> = roots.iter().cloned().collect();
        let mut frontier: Vec<NodeId> = roots.to_vec();
        for _ in 0..depth {
            let mut next = Vec::new();
            for id in &frontier {
                for (n, kind) in self.neighbors(id) {
                    if allow(kind) && seen.insert(n.clone()) {
                        next.push(n.clone());
                    }
                }
            }
            frontier = next;
        }
        seen
    }
}

/// Deterministic pseudo-random in [0,1) from a string and a salt.
pub fn hash01(s: &str, salt: u32) -> f32 {
    let mut h: u32 = 2166136261 ^ salt.wrapping_mul(16777619);
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    h ^= h >> 13;
    h = h.wrapping_mul(0x5bd1e995);
    h ^= h >> 15;
    (h as f32) / (u32::MAX as f32)
}

pub fn seed_of(s: &str) -> u32 {
    (hash01(s, 7) * u32::MAX as f32) as u32
}

pub fn apply_delta(
    commands: &mut Commands,
    graph: &mut GraphState,
    ws: &WorkspaceRes,
    v: &mut Visuals,
    existing: &mut Query<(&mut GraphNode, &Pos)>,
    delta: GraphDelta,
) {
    // removals first
    for e in &delta.removed_edges {
        remove_edge(
            commands,
            graph,
            &(e.kind.clone(), e.src.clone(), e.dst.clone()),
        );
    }
    for id in &delta.removed_node_ids {
        remove_node(commands, graph, id);
    }
    // nodes
    let watch = delta.phase == aneural_core::graph::DeltaPhase::Live;
    for node in delta.nodes {
        let stir = upsert_node(commands, graph, ws, v, existing, node);
        if watch && let Some((id, stir)) = stir {
            graph.stirred.push((id, stir));
        }
    }
    // edges
    for edge in delta.edges {
        add_edge(commands, graph, edge);
    }
    // retry parked edges
    if !graph.pending_edges.is_empty() {
        let parked = std::mem::take(&mut graph.pending_edges);
        for e in parked {
            add_edge(commands, graph, e);
        }
    }
}

fn parent_pos(
    graph: &GraphState,
    id: &NodeId,
    existing: &Query<(&mut GraphNode, &Pos)>,
) -> Option<Vec2> {
    let parent = graph.parent.get(id).cloned().or_else(|| {
        // derive parent from the path when the CONTAINS edge hasn't arrived
        let p = id.path_part();
        if p == "." || id.prefix() == "pkg" {
            return None;
        }
        let parent = p.rsplit_once('/').map(|(d, _)| d).unwrap_or(".");
        Some(NodeId::dir(parent))
    })?;
    let e = graph.by_id.get(&parent)?;
    existing.get(*e).ok().map(|(_, p)| p.0)
}

/// Insert or update one node, saying what that turned out to be so a caller
/// watching a live delta can tell news from a re-emission.
fn upsert_node(
    commands: &mut Commands,
    graph: &mut GraphState,
    ws: &WorkspaceRes,
    v: &mut Visuals,
    existing: &mut Query<(&mut GraphNode, &Pos)>,
    node: Node,
) -> Option<(NodeId, Stir)> {
    if let Some(&e) = graph.by_id.get(&node.id) {
        let mut moved = None;
        if let Ok((mut gn, _)) = existing.get_mut(e) {
            // Compared before the overwrite, because afterwards there is
            // nothing left to compare against. A full index re-emits every
            // node it already knows, so without this every reindex would read
            // as the whole workspace changing at once.
            if gn.label != node.label || gn.props != node.props {
                moved = Some((node.id.clone(), Stir::Changed));
            }
            gn.label = node.label;
            gn.props = node.props;
            gn.kind = node.kind;
            gn.repo_id = node.repo_id;
            gn.path = node.path;
        }
        return moved;
    }
    let anchor = parent_pos(graph, &node.id, existing).unwrap_or(Vec2::ZERO);
    let angle = hash01(node.id.as_str(), 1) * std::f32::consts::TAU;
    let radius = 25.0 + 30.0 * hash01(node.id.as_str(), 2);
    let pos = anchor + Vec2::from_angle(angle) * radius;
    let gn = GraphNode {
        id: node.id.clone(),
        kind: node.kind.clone(),
        label: node.label.clone(),
        path: node.path.clone(),
        repo_id: node.repo_id.clone(),
        props: node.props.clone(),
    };
    let entity = commands
        .spawn((
            gn,
            Pos(pos),
            Vel::default(),
            Drift {
                seed: seed_of(node.id.as_str()),
                offset: Vec2::ZERO,
            },
            GrowIn::default(),
            Transform::from_translation(pos.extend(0.0)).with_scale(Vec3::splat(0.01)),
            Visibility::default(),
        ))
        .id();
    spawn_node_visuals(commands, entity, &node, ws, v);
    graph.ids.insert(entity, node.id.clone());
    graph.by_id.insert(node.id.clone(), entity);
    graph.node_count += 1;
    Some((node.id, Stir::Appeared))
}

fn remove_node(commands: &mut Commands, graph: &mut GraphState, id: &NodeId) {
    let Some(entity) = graph.by_id.remove(id) else {
        return;
    };
    let keys: Vec<EdgeKey> = graph
        .edges
        .keys()
        .filter(|(_, s, d)| s == id || d == id)
        .cloned()
        .collect();
    for k in keys {
        remove_edge(commands, graph, &k);
    }
    graph.degree.remove(&entity);
    graph.ids.remove(&entity);
    graph.adjacency.remove(id);
    graph.parent.remove(id);
    graph.pending_edges.retain(|e| &e.src != id && &e.dst != id);
    commands.entity(entity).despawn();
    graph.node_count = graph.node_count.saturating_sub(1);
}

fn add_edge(commands: &mut Commands, graph: &mut GraphState, edge: Edge) {
    let key: EdgeKey = (edge.kind.clone(), edge.src.clone(), edge.dst.clone());
    if let Some(&existing) = graph.edges.get(&key) {
        // The edge is already drawn, but its props may have moved on — a live
        // session touches the same file again and the time changes. Overwrite
        // the component without re-running the grow-in animation.
        if let (Some(&s), Some(&d)) = (graph.by_id.get(&edge.src), graph.by_id.get(&edge.dst)) {
            commands.entity(existing).insert(GraphEdge {
                seed: seed_of(&format!("{}{}{}", edge.kind, edge.src, edge.dst)),
                kind: edge.kind,
                props: edge.props,
                src: s,
                dst: d,
            });
        }
        return;
    }
    let (Some(&s), Some(&d)) = (graph.by_id.get(&edge.src), graph.by_id.get(&edge.dst)) else {
        if edge.src != edge.dst {
            graph.pending_edges.push(edge);
        }
        return;
    };
    if s == d {
        return;
    }
    let seed = seed_of(&format!("{}{}{}", edge.kind, edge.src, edge.dst));
    let entity = commands
        .spawn((
            GraphEdge {
                kind: edge.kind.clone(),
                src: s,
                dst: d,
                seed,
                props: edge.props.clone(),
            },
            GrowIn { t: 0.0, dur: 0.6 },
        ))
        .id();
    graph
        .adjacency
        .entry(edge.src.clone())
        .or_default()
        .push((edge.dst.clone(), edge.kind.clone()));
    graph
        .adjacency
        .entry(edge.dst.clone())
        .or_default()
        .push((edge.src.clone(), edge.kind.clone()));
    if edge.kind == "CONTAINS" {
        graph.parent.insert(edge.dst.clone(), edge.src.clone());
    }
    graph.edges.insert(key, entity);
    for end in [s, d] {
        graph.degree.entry(end).or_default().bump(&edge.kind, true);
    }
    graph.edge_count += 1;
}

fn remove_edge(commands: &mut Commands, graph: &mut GraphState, key: &EdgeKey) {
    let Some(entity) = graph.edges.remove(key) else {
        return;
    };
    let (kind, src, dst) = key;
    for id in [src, dst] {
        if let Some(e) = graph.by_id.get(id).copied()
            && let Some(d) = graph.degree.get_mut(&e)
        {
            d.bump(kind, false);
        }
    }
    if let Some(v) = graph.adjacency.get_mut(src) {
        v.retain(|(n, k)| !(n == dst && k == kind));
    }
    if let Some(v) = graph.adjacency.get_mut(dst) {
        v.retain(|(n, k)| !(n == src && k == kind));
    }
    if kind == "CONTAINS" && graph.parent.get(dst) == Some(src) {
        graph.parent.remove(dst);
    }
    commands.entity(entity).despawn();
    graph.edge_count = graph.edge_count.saturating_sub(1);
}

/// Advance grow-in tweens; apply scale to nodes.
pub fn tick_grow_in(
    time: Res<Time>,
    mut commands: Commands,
    mut q: Query<(Entity, &mut GrowIn, Option<&mut Transform>)>,
) {
    let dt = time.delta_secs();
    for (e, mut g, transform) in &mut q {
        g.t = (g.t + dt / g.dur).min(1.0);
        let done = g.t >= 1.0;
        if let Some(mut t) = transform {
            t.scale = if done {
                Vec3::ONE
            } else {
                Vec3::splat(ease_out_back(g.t).max(0.01))
            };
        }
        if done {
            commands.entity(e).remove::<GrowIn>();
        }
    }
}

pub fn ease_out_back(t: f32) -> f32 {
    let c1 = 1.70158;
    let c3 = c1 + 1.0;
    1.0 + c3 * (t - 1.0).powi(3) + c1 * (t - 1.0).powi(2)
}

/// Copy layout positions into transforms, plus whatever the night has added
/// to them.
pub fn sync_transforms(mut q: Query<(&Pos, Option<&Drift>, &mut Transform), With<GraphNode>>) {
    for (p, drift, mut t) in &mut q {
        let at = p.0 + drift.map(|d| d.offset).unwrap_or(Vec2::ZERO);
        t.translation.x = at.x;
        t.translation.y = at.y;
    }
}
