//! Where every node goes: a mycelium of clusters within clusters.
//!
//! The shape is decided, not simulated. Every folder is a disc holding its own
//! node, a head packed with its files, and a disc for each of its subfolders,
//! spiralled in around it so the space is filled rather than ringed. Discs
//! never overlap, so each branch keeps its own patch of the canvas and a
//! workspace of thousands reads as clusters with room between them instead of
//! one mat. Nodes outside the folder tree (packages, comments, notes, tables)
//! grow from the file they belong to, or from the deepest folder shared by
//! everything they touch. The simulation only eases nodes to their places and
//! nudges apart any that would land on top of each other.

use crate::engine::IndexStatus;
use crate::graph::{GraphNode, GraphState, Hidden, Pinned, Pos, Vel, hash01};
use aneural_core::NodeId;
use bevy::prelude::*;
use std::collections::{HashMap, HashSet};

/// The shortest a hypha grows from its parent, and the room a node keeps to
/// itself. Several times a node's own width: the space between nodes is what
/// makes a shape readable once there are thousands of them.
const STEP: f32 = 70.0;
/// Clear space around every disc, so clusters read as separate things.
const PAD: f32 = 34.0;
/// How widely a folder's files are packed into their head, as a share of
/// [`STEP`] per file. Above about 0.7 the seats keep a full gap apart.
const SEAT: f32 = 0.78;
/// How far to either side of the way it is already growing a folder may put
/// its growth. Short of a half turn, so nothing is ever packed back over the
/// branch it came from and the whole mesh keeps spreading outward.
const FORWARD: f32 = 1.35;
/// Nodes closer than this push apart.
const PERSONAL_SPACE: f32 = 34.0;

#[derive(Resource)]
pub struct LayoutParams {
    /// How hard a node is drawn to its place.
    pub pull: f32,
    /// How hard two nodes inside each other's personal space push apart.
    pub push: f32,
    pub damping: f32,
    pub max_speed: f32,
    pub epsilon: f32,
    /// How much the nodes are still allowed to jostle, 1 down to nothing.
    /// Every tick cools it a little, so the layout always comes to rest.
    pub alpha: f32,
    pub alpha_decay: f32,
    pub frozen: bool,
    pub last_generation: u64,
    /// Set to forget where everything settled and work the shape out from
    /// nothing. Read and cleared by [`step`].
    pub afresh: bool,
}

impl LayoutParams {
    /// Deal the whole graph again: the shape is worked out from nothing and
    /// every node walks to wherever it lands this time. The only way the
    /// canvas is ever rearranged under someone, so it is asked for by hand —
    /// `Space`, or opening another workspace — and never by a file changing.
    pub fn stir(&mut self) {
        self.frozen = false;
        self.alpha = 1.0;
        self.afresh = true;
    }

    /// Just warm enough to carry a dragged node's branch along with it.
    pub fn nudge(&mut self) {
        self.frozen = false;
        self.alpha = self.alpha.max(0.3);
    }
}

impl Default for LayoutParams {
    fn default() -> Self {
        LayoutParams {
            pull: 0.08,
            push: 3.0,
            damping: 0.8,
            // a workspace of thousands is tens of thousands of units across,
            // and a node that crawls there takes a minute to arrive
            max_speed: 150.0,
            epsilon: 0.05,
            alpha: 1.0,
            alpha_decay: 0.02,
            frozen: false,
            last_generation: 0,
            afresh: false,
        }
    }
}

pub struct LayoutPlugin;

impl Plugin for LayoutPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LayoutParams>()
            .insert_resource(Time::<Fixed>::from_hz(64.0))
            .add_systems(FixedUpdate, step);
    }
}

/// A node's place in the mycelium.
#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    /// The node it grows from; `None` for a root.
    pub host: Option<NodeId>,
    /// Where it sits while nothing has been dragged.
    pub at: Vec2,
}

/// Where the last layout put everything, so the next one can leave it there.
///
/// A workspace that is watched changes all day: a file saved here, a folder
/// added there. Working the shape out afresh each time deals the whole hand
/// again — every seat in a head shuffles along by one, sibling clusters swap
/// sides — and anyone looking closely at a corner of the canvas watches it
/// slide out from under them. So the shape is worked out against this
/// instead: whatever was placed before keeps its place, and only what is new,
/// or has outgrown where it sat, is placed afresh.
#[derive(Default)]
pub struct Settled {
    /// The seat each file took in its folder's head.
    seats: HashMap<NodeId, usize>,
    /// Where each folder put its growth, in the folder's own frame: (the
    /// folder, what it placed) to the offset. `None` stands for the head of
    /// files, and for the folder when it is the roots being spread.
    spots: HashMap<(Option<NodeId>, Option<NodeId>), Vec2>,
}

/// Lay `nodes` out as a radial tree. `parent` is the folder tree and
/// `neighbors` everything a node is linked to. Hosts come before the nodes
/// that grow from them. `settled` is what the last layout decided: pass a
/// fresh one to lay the graph out from nothing, or the one the last call
/// filled in to keep what is already on the canvas where it is.
pub fn radial(
    nodes: &[NodeId],
    parent: &HashMap<NodeId, NodeId>,
    neighbors: &dyn Fn(&NodeId) -> Vec<NodeId>,
    settled: &mut Settled,
) -> Vec<(NodeId, Place)> {
    let was_seated = std::mem::take(&mut settled.seats);
    let was_spotted = std::mem::take(&mut settled.spots);
    let present: HashSet<&NodeId> = nodes.iter().collect();
    let in_tree = |id: &NodeId| id.is_dir() || id.is_file();

    // The folder tree first: a node's parent, or failing that (its CONTAINS
    // edge has not arrived yet) the nearest folder above it by path.
    let mut host: HashMap<NodeId, Option<NodeId>> = HashMap::new();
    for id in nodes.iter().filter(|id| in_tree(id)) {
        let h = parent
            .get(id)
            .filter(|p| present.contains(p))
            .cloned()
            .or_else(|| folder_above(id.path_part(), &present));
        host.insert(id.clone(), h);
    }
    let chain = |id: &NodeId| -> Vec<NodeId> {
        let mut out = vec![id.clone()];
        while let Some(Some(h)) = host.get(out.last().unwrap()) {
            if out.len() > host.len() || out.contains(h) {
                break;
            }
            out.push(h.clone());
        }
        out
    };

    // Everything else grows from the deepest folder-tree node shared by all
    // it touches: the file for a comment, the folder two importers share for
    // a package.
    let mut extra: Vec<(NodeId, Option<NodeId>)> = Vec::new();
    for id in nodes.iter().filter(|id| !in_tree(id)) {
        let touched: Vec<NodeId> = neighbors(id)
            .into_iter()
            .filter(|n| in_tree(n) && present.contains(n))
            .collect();
        let h = match touched.split_first() {
            Some((first, rest)) => {
                let mut common = chain(first);
                for other in rest {
                    let theirs: HashSet<NodeId> = chain(other).into_iter().collect();
                    let keep = common.iter().position(|a| theirs.contains(a));
                    common = keep.map(|i| common.split_off(i)).unwrap_or_default();
                }
                common.into_iter().next()
            }
            None => {
                let own = NodeId::file(id.path_part());
                if present.contains(&own) {
                    Some(own)
                } else {
                    folder_above(id.path_part(), &present)
                }
            }
        };
        extra.push((id.clone(), h));
    }
    host.extend(extra);

    let mut children: HashMap<Option<NodeId>, Vec<NodeId>> = HashMap::new();
    for (id, h) in &host {
        children.entry(h.clone()).or_default().push(id.clone());
    }
    for list in children.values_mut() {
        list.sort();
    }
    let kids = |id: &Option<NodeId>| children.get(id).map(Vec::as_slice).unwrap_or(&[]);

    let branches_of = |id: &NodeId| -> Vec<NodeId> {
        kids(&Some(id.clone()))
            .iter()
            .filter(|k| !kids(&Some((*k).clone())).is_empty())
            .cloned()
            .collect()
    };
    let leaves_of = |id: &NodeId| -> Vec<NodeId> {
        kids(&Some(id.clone()))
            .iter()
            .filter(|k| kids(&Some((*k).clone())).is_empty())
            .cloned()
            .collect()
    };

    // Seats first, because how wide a head packs depends on them. A file
    // that already had a seat keeps it, so a newcomer sorting in ahead of it
    // no longer pushes the whole head along by one; newcomers take the lowest
    // seat going, so the seats a folder uses stay packed together.
    let mut seats: HashMap<NodeId, usize> = HashMap::new();
    let mut span: HashMap<NodeId, usize> = HashMap::new();
    for (h, list) in &children {
        let Some(h) = h else { continue };
        let leaves = list.iter().filter(|k| kids(&Some((*k).clone())).is_empty());
        let (mut taken, mut newcomers) = (HashSet::new(), Vec::new());
        for k in leaves {
            match was_seated.get(k) {
                Some(&seat) if taken.insert(seat) => {
                    seats.insert(k.clone(), seat);
                }
                _ => newcomers.push(k),
            }
        }
        let mut free = 0;
        for k in newcomers {
            while !taken.insert(free) {
                free += 1;
            }
            seats.insert(k.clone(), free);
        }
        span.insert(h.clone(), taken.iter().max().map_or(0, |seat| seat + 1));
    }

    // Bottom up: pack each folder's growth into a disc around it, and
    // remember where everything sits inside that disc.
    let mut order: Vec<NodeId> = Vec::new();
    let mut stack: Vec<NodeId> = kids(&None).to_vec();
    while let Some(id) = stack.pop() {
        stack.extend(kids(&Some(id.clone())).iter().cloned());
        order.push(id);
    }
    // The root grows every way at once; everything else grows away from what
    // it came from, so it packs into the half of the circle facing outward.
    let root = match kids(&None) {
        [only] => Some(only.clone()),
        _ => None,
    };
    // A cluster is measured from its own middle, not from the folder it hangs
    // off. Measured from the folder, a chain of folders would double in size
    // at every step down it, and a deep tree would be astronomical.
    let mut disc: HashMap<NodeId, Disc> = HashMap::new();
    let mut inside: HashMap<NodeId, Vec<(Option<NodeId>, Vec2, f32)>> = HashMap::new();
    let mut spots: HashMap<(Option<NodeId>, Option<NodeId>), Vec2> = HashMap::new();
    for id in order.iter().rev() {
        // its own room first, at the middle, then the biggest growth first so
        // the small growth can tuck into what is left
        let mut want: Vec<(Option<NodeId>, f32)> = branches_of(id)
            .into_iter()
            .map(|b| (Some(b.clone()), disc[&b].radius))
            .collect();
        let head = head_width(span.get(id).copied().unwrap_or(0)) * 0.5;
        if head > 0.0 {
            want.push((None, head));
        }
        want.sort_by(|a, b| b.1.total_cmp(&a.1));
        let facing = if root.as_ref() == Some(id) {
            std::f32::consts::PI
        } else {
            FORWARD
        };
        let packed = spiral_pack(&want, facing, &|what| {
            was_spotted.get(&(Some(id.clone()), what.clone())).copied()
        });
        for (what, at, _) in &packed {
            spots.insert((Some(id.clone()), what.clone()), *at);
        }
        // where the whole cluster sits, relative to the folder itself
        let mut circles: Vec<(Vec2, f32)> = vec![(Vec2::ZERO, STEP * 0.5)];
        circles.extend(packed.iter().map(|(_, at, r)| (*at, *r)));
        disc.insert(id.clone(), enclosing(&circles));
        inside.insert(id.clone(), packed);
    }

    // Each cluster is packed facing along its own x axis, so on the way down
    // it is simply turned to face away from whatever it grew from.
    let mut out: Vec<(NodeId, Place)> = Vec::with_capacity(host.len());
    // (node, where it sits, which way its growth should face)
    let mut todo: Vec<(NodeId, Vec2, f32)> = Vec::new();
    match kids(&None) {
        [root] => {
            out.push((
                root.clone(),
                Place {
                    host: None,
                    at: Vec2::ZERO,
                },
            ));
            todo.push((root.clone(), Vec2::ZERO, 0.0));
        }
        // more than one root: spread them the same way, around nothing
        roots => {
            let want: Vec<(Option<NodeId>, f32)> = roots
                .iter()
                .map(|r| (Some(r.clone()), disc[r].radius))
                .collect();
            let packed = spiral_pack(&want, std::f32::consts::PI, &|what| {
                was_spotted.get(&(None, what.clone())).copied()
            });
            for (id, at, _) in packed {
                spots.insert((None, id.clone()), at);
                let Some(id) = id else { continue };
                // `at` is where the cluster goes; the folder itself sits off
                // the middle of it by however its growth is packed
                let node = at - disc[&id].middle;
                out.push((
                    id.clone(),
                    Place {
                        host: None,
                        at: node,
                    },
                ));
                todo.push((id, node, at.to_angle()));
            }
        }
    }
    while let Some((id, at, face)) = todo.pop() {
        let leaves = leaves_of(&id);
        let packed = inside.get(&id).cloned().unwrap_or_default();
        let turn = Rot2::radians(face);
        for (what, offset, _) in packed {
            let spot = at + turn * offset;
            match what {
                // a subfolder: its own disc, laid out the same way
                Some(k) => {
                    let face = (spot - at).to_angle();
                    // `spot` holds the child's whole cluster; the child itself
                    // sits off the middle of it
                    let node = spot - Rot2::radians(face) * disc[&k].middle;
                    out.push((
                        k.clone(),
                        Place {
                            host: Some(id.clone()),
                            at: node,
                        },
                    ));
                    todo.push((k, node, face));
                }
                // the head of files
                None => {
                    for k in &leaves {
                        let seat = seats.get(k).copied().unwrap_or(0);
                        out.push((
                            k.clone(),
                            Place {
                                host: Some(id.clone()),
                                at: spot + seat_at(seat, k),
                            },
                        ));
                    }
                }
            }
        }
    }
    settled.seats = seats;
    settled.spots = spots;
    out
}

/// Pack discs around a node, spiralling outward and taking the first spot
/// each one fits, and say how much room the lot of them takes. The node's own
/// space is held at the middle, so nothing is packed on top of it. Anything
/// `known` remembers goes down first and stays exactly where it was, unless
/// it has grown too big for the spot: new growth settles around what is
/// already growing rather than the cluster being dealt again.
fn spiral_pack(
    want: &[(Option<NodeId>, f32)],
    facing: f32,
    known: &dyn Fn(&Option<NodeId>) -> Option<Vec2>,
) -> Vec<(Option<NodeId>, Vec2, f32)> {
    let mut placed: Vec<(Vec2, f32)> = vec![(Vec2::ZERO, STEP * 0.5)];
    let mut out: Vec<(Option<NodeId>, Vec2, f32)> = Vec::with_capacity(want.len());
    // a stable sort, so the biggest-first order survives inside each group
    let mut order: Vec<usize> = (0..want.len()).collect();
    order.sort_by_key(|&i| known(&want[i].0).is_none());
    let floor = |room: f32| STEP * 0.5 + PAD + room;
    for i in order {
        let (what, room) = (&want[i].0, want[i].1);
        let at = match known(what) {
            // It was here before, so it keeps the bearing it grew on and is
            // given only as much more room as it now needs: a cluster that
            // has filled out eases straight outward rather than being dealt
            // somewhere else entirely.
            Some(spot) => {
                let bearing = Vec2::from_angle(spot.to_angle().clamp(-facing, facing));
                let mut reach = spot.length().max(floor(room));
                while !clears(bearing * reach, room, &placed) {
                    reach += PAD * 0.3;
                }
                bearing * reach
            }
            None => {
                let mut reach = floor(room);
                loop {
                    // a golden turn between tries, so they spread out rather
                    // than marching around in step with what is already there
                    let found = (0..24).find_map(|turn| {
                        let angle = wrap(turn as f32 * 2.399_963_2 + reach * 0.02);
                        if angle.abs() > facing {
                            return None;
                        }
                        let spot = Vec2::from_angle(angle) * reach;
                        clears(spot, room, &placed).then_some(spot)
                    });
                    if let Some(spot) = found {
                        break spot;
                    }
                    reach += PAD * 0.6;
                }
            }
        };
        placed.push((at, room));
        out.push((what.clone(), at, room));
    }
    out
}

/// Whether a disc of `room` at `spot` is clear of everything already packed.
fn clears(spot: Vec2, room: f32, placed: &[(Vec2, f32)]) -> bool {
    placed
        .iter()
        .all(|(p, r)| p.distance(spot) >= r + room + PAD)
}

/// A cluster: where its middle sits relative to the node it grew from, and how
/// far it reaches from that middle.
#[derive(Clone, Copy, Debug, Default)]
pub struct Disc {
    pub middle: Vec2,
    pub radius: f32,
}

/// A circle holding all of these. Starts at their middle and walks towards
/// whichever sticks out furthest, which closes on the smallest such circle
/// quickly enough for a few dozen of them.
fn enclosing(circles: &[(Vec2, f32)]) -> Disc {
    let mut middle = circles.iter().map(|(at, _)| *at).sum::<Vec2>() / circles.len() as f32;
    let mut radius = 0.0;
    for step in 0..48 {
        let (out, far) = circles
            .iter()
            .fold((Vec2::ZERO, 0.0_f32), |(out, far), (at, r)| {
                let reach = middle.distance(*at) + r;
                if reach > far {
                    (*at - middle, reach)
                } else {
                    (out, far)
                }
            });
        radius = far;
        middle += out.normalize_or_zero() * (far * 0.5 / (step + 2) as f32);
    }
    Disc { middle, radius }
}

/// An angle folded into -π..π.
fn wrap(a: f32) -> f32 {
    let t = std::f32::consts::TAU;
    (a + std::f32::consts::PI).rem_euclid(t) - std::f32::consts::PI
}

/// How wide a head of `n` seats packs: the seats are spread over a disc, so
/// its width goes by the square root of how many there are.
fn head_width(n: usize) -> f32 {
    if n == 0 {
        0.0
    } else {
        STEP * SEAT * (n as f32).sqrt() * 2.0
    }
}

/// Where seat `n` sits, from the middle of the head. Sunflower packing: each
/// seat a golden angle on from the last and out by the square root of its
/// number, so the head fills evenly however many files there are — and so a
/// seat is where it is regardless of how many other seats there turn out to
/// be, which is what lets a file stay put while its neighbours come and go.
fn seat_at(n: usize, id: &NodeId) -> Vec2 {
    let seat = n as f32 + 0.5;
    let a = seat * 2.399_963_2 + hash01(id.as_str(), 6) * 0.12;
    Vec2::from_angle(a) * (STEP * SEAT * seat.sqrt())
}

/// The nearest folder above `path` that is in the graph.
fn folder_above(path: &str, present: &HashSet<&NodeId>) -> Option<NodeId> {
    let mut path = path;
    while path != "." && !path.is_empty() {
        path = path.rsplit_once('/').map(|(d, _)| d).unwrap_or(".");
        let dir = NodeId::dir(path);
        if present.contains(&dir) {
            return Some(dir);
        }
    }
    None
}

/// The places last worked out, and the shape of the graph they were worked
/// out for.
#[derive(Default)]
struct Plan {
    /// What the plan was worked out for: the set of nodes, and how many edges
    /// there are. Not the generation, which also counts a file merely being
    /// edited — that changes what a node says, never where it belongs.
    shape: (u64, usize),
    /// (node, the node it grows from, where it sits) in host-first order.
    places: Vec<(Entity, Option<Entity>, Vec2)>,
    /// Where this plan put everything, so the next one leaves it there.
    settled: Settled,
}

/// A number that changes when the set of nodes does: each id hashed, and the
/// lot exclusive-or'd together so the order the map hands them over in does
/// not matter.
fn node_set(graph: &GraphState) -> u64 {
    graph.by_id.keys().fold(0, |all, id| {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in id.as_str().bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        all ^ h
    })
}

fn step(
    mut params: ResMut<LayoutParams>,
    status: Res<IndexStatus>,
    graph: Res<GraphState>,
    mut plan: Local<Plan>,
    mut nodes: Query<(Entity, &mut Pos, &mut Vel, Has<Pinned>, Has<Hidden>), With<GraphNode>>,
) {
    let afresh = std::mem::take(&mut params.afresh);
    if afresh {
        plan.settled = Settled::default();
    }
    if afresh || status.generation != params.last_generation {
        params.last_generation = status.generation;
        let shape = (node_set(&graph), graph.edge_count);
        if afresh || plan.shape != shape {
            plan.shape = shape;
            let ids: Vec<NodeId> = graph.by_id.keys().cloned().collect();
            let neighbors =
                |id: &NodeId| graph.neighbors(id).iter().map(|(n, _)| n.clone()).collect();
            let places = radial(&ids, &graph.parent, &neighbors, &mut plan.settled);
            plan.places = places
                .into_iter()
                .filter_map(|(id, p)| {
                    let e = *graph.by_id.get(&id)?;
                    let host = p.host.and_then(|h| graph.by_id.get(&h).copied());
                    Some((e, host, p.at))
                })
                .collect();
            // Warm enough to walk the new growth over to its place and part
            // anything that landed on top of something, and no warmer: the
            // graph is not dealt again now, so reheating the whole of it
            // every time a file is saved would only make it flinch.
            params.nudge();
        }
    }
    if params.frozen {
        return;
    }

    // A dragged node stays where it was left and takes its branch with it:
    // everything growing from it shifts by however far it was moved.
    let mut shift: HashMap<Entity, Vec2> = HashMap::with_capacity(plan.places.len());
    let mut target: HashMap<Entity, Vec2> = HashMap::with_capacity(plan.places.len());
    for (e, host, at) in &plan.places {
        let inherited = host
            .and_then(|h| shift.get(&h).copied())
            .unwrap_or(Vec2::ZERO);
        let own = match nodes.get(*e) {
            Ok((_, pos, _, true, _)) => pos.0 - *at,
            _ => inherited,
        };
        shift.insert(*e, own);
        target.insert(*e, *at + inherited);
    }

    let snapshot: Vec<(Entity, Vec2, bool)> = nodes
        .iter()
        .filter(|(.., hidden)| !hidden)
        .map(|(e, p, _, pinned, _)| (e, p.0, pinned))
        .collect();
    let n = snapshot.len();
    if n == 0 {
        return;
    }
    let mut force: Vec<Vec2> = snapshot
        .iter()
        .map(|(e, p, _)| (target.get(e).copied().unwrap_or(*p) - *p) * params.pull)
        .collect();

    // Personal space, found through a grid so only near neighbours are compared.
    let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
    let cell = |p: Vec2| {
        (
            (p.x / PERSONAL_SPACE).floor() as i32,
            (p.y / PERSONAL_SPACE).floor() as i32,
        )
    };
    for (i, (_, p, _)) in snapshot.iter().enumerate() {
        grid.entry(cell(*p)).or_default().push(i);
    }
    for (i, (_, p, _)) in snapshot.iter().enumerate() {
        let (cx, cy) = cell(*p);
        for dx in -1..=1 {
            for dy in -1..=1 {
                for &j in grid.get(&(cx + dx, cy + dy)).into_iter().flatten() {
                    if i == j {
                        continue;
                    }
                    let d = *p - snapshot[j].1;
                    let dist = d.length();
                    if dist >= PERSONAL_SPACE {
                        continue;
                    }
                    // Two nodes on the very same spot part in directions of
                    // their own rather than not at all.
                    let away = if dist > 1e-3 {
                        d / dist
                    } else {
                        Vec2::from_angle((i as f32 - j as f32) * 2.399)
                    };
                    force[i] += away * params.push * params.alpha * (1.0 - dist / PERSONAL_SPACE);
                }
            }
        }
    }

    let mut energy = 0.0;
    for (i, (e, _, pinned)) in snapshot.iter().enumerate() {
        let Ok((_, mut pos, mut vel, ..)) = nodes.get_mut(*e) else {
            continue;
        };
        if *pinned {
            vel.0 = Vec2::ZERO;
            continue;
        }
        let v = ((vel.0 + force[i]) * params.damping).clamp_length_max(params.max_speed);
        pos.0 += v;
        vel.0 = v;
        energy += v.length_squared();
    }
    params.alpha *= 1.0 - params.alpha_decay;
    // Still and cool: it has arrived.
    if params.alpha < 0.5 && energy / (n as f32) < params.epsilon {
        params.frozen = true;
        debug!("layout settled ({n} nodes, alpha {:.3})", params.alpha);
        for (_, _, mut vel, ..) in &mut nodes {
            vel.0 = Vec2::ZERO;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Links = HashMap<NodeId, Vec<NodeId>>;

    /// A workspace shaped like the ones people have: a few top-level folders,
    /// one deep and busy, files at every level, and a package and a comment
    /// hanging off the tree.
    fn workspace() -> (Vec<NodeId>, HashMap<NodeId, NodeId>, Links) {
        let dirs = [
            "apps",
            "apps/web",
            "apps/web/src",
            "apps/web/src/lib",
            "services",
            "services/api",
            "docs",
        ];
        let files = [
            "README.md",
            "apps/web/package.json",
            "apps/web/src/index.ts",
            "apps/web/src/app.ts",
            "apps/web/src/types.ts",
            "apps/web/src/styles.css",
            "apps/web/src/lib/util.ts",
            "apps/web/src/lib/format.ts",
            "services/api/main.py",
            "services/api/db.py",
            "docs/overview.md",
        ];
        let up = |p: &str| {
            p.rsplit_once('/')
                .map(|(d, _)| d)
                .unwrap_or(".")
                .to_string()
        };
        let mut nodes = vec![NodeId::dir(".")];
        let mut parent = HashMap::new();
        for d in dirs {
            nodes.push(NodeId::dir(d));
            parent.insert(NodeId::dir(d), NodeId::dir(up(d)));
        }
        for f in files {
            nodes.push(NodeId::file(f));
            parent.insert(NodeId::file(f), NodeId::dir(up(f)));
        }
        let react = NodeId::package("npm", "react");
        let todo = NodeId::new("comment:apps/web/src/app.ts#abc");
        nodes.push(react.clone());
        nodes.push(todo.clone());
        let mut links = Links::new();
        links.insert(
            react,
            vec![
                NodeId::file("apps/web/src/index.ts"),
                NodeId::file("apps/web/src/lib/util.ts"),
            ],
        );
        links.insert(todo, vec![NodeId::file("apps/web/src/app.ts")]);
        (nodes, parent, links)
    }

    fn lay_out() -> HashMap<NodeId, Place> {
        let (nodes, parent, links) = workspace();
        radial(
            &nodes,
            &parent,
            &|id| links.get(id).cloned().unwrap_or_default(),
            &mut Settled::default(),
        )
        .into_iter()
        .collect()
    }

    /// What nesting guarantees in place of a strict ring order: a folder's
    /// whole cluster keeps its own patch of canvas.
    #[test]
    fn clusters_never_overlap() {
        let places = lay_out();
        let mut kids: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        for (id, p) in &places {
            if let Some(h) = &p.host {
                kids.entry(h.clone()).or_default().push(id.clone());
            }
        }
        // the circle a whole subtree actually occupies, from where its nodes
        // ended up rather than from where the packing meant to put them
        fn spread(
            id: &NodeId,
            places: &HashMap<NodeId, Place>,
            kids: &HashMap<NodeId, Vec<NodeId>>,
            into: &mut Vec<Vec2>,
        ) {
            into.push(places[id].at);
            for k in kids.get(id).into_iter().flatten() {
                spread(k, places, kids, into);
            }
        }
        let circle = |id: &NodeId| {
            let mut points = Vec::new();
            spread(id, &places, &kids, &mut points);
            enclosing(&points.iter().map(|p| (*p, 0.0)).collect::<Vec<_>>())
        };
        for family in kids.values() {
            for (i, a) in family.iter().enumerate() {
                for b in &family[i + 1..] {
                    let (ca, cb) = (circle(a), circle(b));
                    let apart = ca.middle.distance(cb.middle);
                    // a shade of slack: the circles are found by walking
                    // towards whatever sticks out, not solved exactly
                    assert!(
                        apart >= (ca.radius + cb.radius) * 0.97,
                        "{a} and {b} overlap: {apart} < {} + {}",
                        ca.radius,
                        cb.radius
                    );
                }
            }
        }
    }

    #[test]
    fn growth_faces_away_from_what_it_grew_from() {
        let places = lay_out();
        let root = places[&NodeId::dir(".")].at;
        let (mut outward, mut total) = (0, 0);
        for p in places.values() {
            let Some(h) = &p.host else { continue };
            if places[h].host.is_none() {
                continue; // the root's own growth has no way it came from
            }
            total += 1;
            if p.at.distance(root) > places[h].at.distance(root) {
                outward += 1;
            }
        }
        assert!(
            outward * 4 >= total * 3,
            "only {outward} of {total} grew outward"
        );
    }

    #[test]
    fn hyphae_never_cross() {
        let places = lay_out();
        let segments: Vec<(&NodeId, Vec2, Vec2)> = places
            .iter()
            .filter_map(|(id, p)| Some((id, places[p.host.as_ref()?].at, p.at)))
            .collect();
        for (i, (a, a0, a1)) in segments.iter().enumerate() {
            for (b, b0, b1) in &segments[i + 1..] {
                // hyphae that share an end meet there, which is not a crossing
                let shared = [a0, a1]
                    .iter()
                    .any(|p| p.distance(*b0) < 1e-3 || p.distance(*b1) < 1e-3);
                if !shared {
                    assert!(!crosses(*a0, *a1, *b0, *b1), "{a} crosses {b}");
                }
            }
        }
    }

    /// The case that turned a real workspace into a white smear: one folder
    /// holding hundreds of files, with a couple of subfolders beside it.
    #[test]
    fn a_folder_of_hundreds_packs_into_a_head() {
        let mut nodes = vec![NodeId::dir("."), NodeId::dir("scripts")];
        let mut parent = HashMap::new();
        parent.insert(NodeId::dir("scripts"), NodeId::dir("."));
        for d in ["src", "docs"] {
            nodes.push(NodeId::dir(d));
            parent.insert(NodeId::dir(d), NodeId::dir("."));
            for i in 0..20 {
                let f = NodeId::file(format!("{d}/f{i}.rs"));
                parent.insert(f.clone(), NodeId::dir(d));
                nodes.push(f);
            }
        }
        for i in 0..400 {
            let f = NodeId::file(format!("scripts/s{i}.py"));
            parent.insert(f.clone(), NodeId::dir("scripts"));
            nodes.push(f);
        }
        let places: HashMap<NodeId, Place> =
            radial(&nodes, &parent, &|_| Vec::new(), &mut Settled::default())
                .into_iter()
                .collect();

        let spots: Vec<(&NodeId, Vec2)> = places.iter().map(|(id, p)| (id, p.at)).collect();
        let mut tightest = f32::MAX;
        for (i, (a, at)) in spots.iter().enumerate() {
            for (b, other) in &spots[i + 1..] {
                let d = at.distance(*other);
                if d < tightest {
                    tightest = d;
                }
                assert!(d > STEP * 0.5, "{a} and {b} are {d} apart");
            }
        }
        // the files are a head, not a cone: none of them is further from the
        // folder than the head is wide, so no strand reaches across the graph
        let folder = places[&NodeId::dir("scripts")].at;
        let out: Vec<f32> = (0..400)
            .map(|i| {
                places[&NodeId::file(format!("scripts/s{i}.py"))]
                    .at
                    .distance(folder)
            })
            .collect();
        let far = out.iter().fold(0.0f32, |hi, r| hi.max(*r));
        let head = STEP * SEAT * 400.0_f32.sqrt();
        assert!(far < head * 2.5, "files trail {far} from a head of {head}");
        assert!(tightest > STEP * 0.5, "{tightest}");
    }

    /// A monorepo has to fit on a screen. This is the guard on the mistake
    /// that made it not: measuring a cluster from the folder it hangs off
    /// rather than from its own middle, which doubled the size of everything
    /// at every step down a chain of folders and put a real workspace two
    /// hundred thousand units across.
    #[test]
    fn a_monorepo_lays_out_at_a_size_that_fits_on_a_screen() {
        // 6 repos, folders nested four deep, 1249 files
        let mut nodes = vec![NodeId::dir(".")];
        let mut parent = HashMap::new();
        let mut dirs = vec![];
        for r in 0..6 {
            let repo = format!("r{r}");
            nodes.push(NodeId::dir(&repo));
            parent.insert(NodeId::dir(&repo), NodeId::dir("."));
            dirs.push(repo.clone());
            // nested the way real source trees are, several levels deep
            let mut frontier = vec![repo.clone()];
            for _ in 0..4 {
                let mut next = vec![];
                for base in &frontier {
                    for d in 0..2 {
                        let sub = format!("{base}/d{d}");
                        nodes.push(NodeId::dir(&sub));
                        parent.insert(NodeId::dir(&sub), NodeId::dir(base));
                        dirs.push(sub.clone());
                        next.push(sub);
                    }
                }
                frontier = next;
            }
        }
        let mut n = 0;
        while n < 1249 {
            let d = &dirs[n % dirs.len()];
            let f = NodeId::file(format!("{d}/f{n}.rs"));
            parent.insert(f.clone(), NodeId::dir(d));
            nodes.push(f);
            n += 1;
        }
        let places = radial(&nodes, &parent, &|_| Vec::new(), &mut Settled::default());
        let mut far = 0.0f32;
        let mut bad = 0;
        for (_, p) in &places {
            if !p.at.is_finite() {
                bad += 1;
            }
            far = far.max(p.at.length());
        }
        assert_eq!(bad, 0, "{bad} nodes landed nowhere");
        assert!(far < 20_000.0, "{} nodes reach {far:.0} out", places.len());
    }

    #[test]
    fn strays_grow_from_what_they_touch() {
        let places = lay_out();
        let host = |id: NodeId| places[&id].host.clone();
        assert_eq!(
            host(NodeId::new("comment:apps/web/src/app.ts#abc")),
            Some(NodeId::file("apps/web/src/app.ts"))
        );
        assert_eq!(
            host(NodeId::package("npm", "react")),
            Some(NodeId::dir("apps/web/src")),
            "the deepest folder both importers share"
        );
    }

    #[test]
    fn hosts_come_before_what_grows_from_them() {
        let (nodes, parent, links) = workspace();
        let order = radial(
            &nodes,
            &parent,
            &|id| links.get(id).cloned().unwrap_or_default(),
            &mut Settled::default(),
        );
        assert_eq!(order.len(), nodes.len());
        let mut seen = HashSet::new();
        for (id, p) in &order {
            if let Some(h) = &p.host {
                assert!(seen.contains(h), "{id} placed before its host {h}");
            }
            seen.insert(id.clone());
        }
    }

    /// The whole point of remembering: a workspace is watched, so files
    /// arrive all day, and what is already on the canvas has to stay where
    /// the user last saw it. The case this guards is the one that used to
    /// shuffle a whole head along by one seat — a file whose name sorts in
    /// ahead of everything already in its folder.
    #[test]
    fn what_is_already_placed_stays_where_it_is() {
        let (mut nodes, mut parent, links) = workspace();
        let neighbors = |id: &NodeId| links.get(id).cloned().unwrap_or_default();
        let mut settled = Settled::default();
        let before: HashMap<NodeId, Place> = radial(&nodes, &parent, &neighbors, &mut settled)
            .into_iter()
            .collect();

        for name in [
            "apps/web/src/aaa.ts",
            "apps/web/src/lib/aaa.ts",
            "docs/aaa.md",
            "services/api/aaa.py",
        ] {
            let f = NodeId::file(name);
            parent.insert(f.clone(), NodeId::dir(name.rsplit_once('/').unwrap().0));
            nodes.push(f);
        }
        let after: HashMap<NodeId, Place> = radial(&nodes, &parent, &neighbors, &mut settled)
            .into_iter()
            .collect();

        // A head never reshuffles. Every file in a folder has moved by the
        // same small amount relative to that folder — the head easing outward
        // as one thing to make room for the newcomer — rather than the seats
        // being dealt again.
        let hosts: HashSet<&NodeId> = after.values().filter_map(|p| p.host.as_ref()).collect();
        let mut head_moved: HashMap<NodeId, Vec2> = HashMap::new();
        for (id, was) in &before {
            let now = &after[id];
            assert_eq!(now.host, was.host, "{id} changed what it grows from");
            // a folder carries the cluster it has grown along with it
            let Some(h) = &was.host.clone().filter(|_| !hosts.contains(id)) else {
                continue;
            };
            let moved = (now.at - after[h].at) - (was.at - before[h].at);
            assert!(
                moved.length() < STEP,
                "{h}'s head moved {moved} out of place"
            );
            match head_moved.get(h) {
                Some(head) => assert!(
                    moved.distance(*head) < 1e-3,
                    "{id} took a different seat in {h}: its head moved {head}, it moved {moved}"
                ),
                None => {
                    head_moved.insert(h.clone(), moved);
                }
            }
        }
        // and nothing anywhere is flung across the canvas
        for (id, was) in &before {
            let moved = after[id].at.distance(was.at);
            assert!(moved < STEP, "{id} moved {moved} across the canvas");
        }
    }

    /// A day of being watched: files land one at a time, all over the tree,
    /// and after each one the canvas still has to look like the one that was
    /// there a second ago. Also the guard on the other way this could go
    /// wrong — every arrival easing a cluster a little further out, until an
    /// afternoon's editing has pushed the workspace off to infinity.
    #[test]
    fn a_day_of_files_arriving_never_pulls_the_canvas_around() {
        let (mut nodes, mut parent, links) = workspace();
        let neighbors = |id: &NodeId| links.get(id).cloned().unwrap_or_default();
        let mut settled = Settled::default();
        let mut places: HashMap<NodeId, Place> = radial(&nodes, &parent, &neighbors, &mut settled)
            .into_iter()
            .collect();
        let started = places.clone();
        let dirs = [
            "apps/web/src",
            "apps/web/src/lib",
            "services/api",
            "docs",
            "apps/web",
        ];
        for n in 0..60 {
            let d = dirs[n % dirs.len()];
            // named so it sorts in ahead of everything already there, which
            // is the order that used to renumber every seat in the head
            let f = NodeId::file(format!("{d}/a{:02}.ts", 60 - n));
            parent.insert(f.clone(), NodeId::dir(d));
            nodes.push(f);
            let next: HashMap<NodeId, Place> = radial(&nodes, &parent, &neighbors, &mut settled)
                .into_iter()
                .collect();
            for (id, was) in &places {
                let moved = next[id].at.distance(was.at);
                assert!(moved < STEP, "file {n} landed and {id} moved {moved}");
            }
            places = next;
        }
        // 60 arrivals into five folders: the tree has spread, but by the room
        // the new files actually take up rather than a step per arrival
        let reach = |places: &HashMap<NodeId, Place>| {
            places
                .values()
                .fold(0.0f32, |far, p| far.max(p.at.length()))
        };
        let (was, now) = (reach(&started), reach(&places));
        assert!(
            now < was * 3.0,
            "the canvas went from {was:.0} out to {now:.0}"
        );
        // and nothing has ended up sitting on top of anything else
        let spots: Vec<Vec2> = places.values().map(|p| p.at).collect();
        for (i, a) in spots.iter().enumerate() {
            for b in &spots[i + 1..] {
                assert!(
                    a.distance(*b) > STEP * 0.5,
                    "two nodes {} apart",
                    a.distance(*b)
                );
            }
        }
    }

    fn crosses(a: Vec2, b: Vec2, c: Vec2, d: Vec2) -> bool {
        let side = |p: Vec2, q: Vec2, r: Vec2| (q - p).perp_dot(r - p);
        side(a, b, c) * side(a, b, d) < 0.0 && side(c, d, a) * side(c, d, b) < 0.0
    }
}
