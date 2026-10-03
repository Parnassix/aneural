//! Engine thread + event drain with a per-frame growth budget.

use crate::graph::{GraphState, apply_delta};
use crate::workspace::WorkspaceRes;
use aneural_core::GraphDelta;
use aneural_core::graph::DeltaPhase;
use aneural_engine::{EngineCommand, EngineEvent};
use bevy::prelude::*;
use crossbeam_channel::{Receiver, Sender};
use std::collections::VecDeque;

#[derive(Resource)]
pub struct EngineRx(pub Receiver<EngineEvent>);

#[derive(Resource)]
pub struct EngineTx(pub Sender<EngineCommand>);

#[derive(Resource, Default, Debug)]
pub struct IndexStatus {
    pub phase: String,
    pub done: u64,
    pub total: u64,
    pub complete: bool,
    pub watching: bool,
    /// True while the engine is reading files: the whole initial index, and
    /// for a moment after each batch the watcher brings in.
    pub busy: bool,
    /// When `busy` may drop again, in seconds of elapsed app time.
    busy_until: f64,
    pub last_error: Option<String>,
    pub last_stats: Option<aneural_core::graph::IndexStats>,
    /// Bumped whenever graph content changes (layout wakes up, filters recompute).
    pub generation: u64,
}

#[derive(Resource, Default)]
pub struct PendingDeltas(pub VecDeque<GraphDelta>);

pub struct EnginePlugin;

impl Plugin for EnginePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<IndexStatus>()
            .init_resource::<PendingDeltas>()
            .add_systems(Startup, spawn_engine)
            .add_systems(PreUpdate, drain_events)
            .add_systems(Last, stop_on_exit);
    }
}

/// Grow `root` on its own thread; the pair of channels is how the app talks
/// to it. Used at startup and again whenever another workspace is opened.
pub fn start_engine(root: &std::path::Path) -> (EngineRx, EngineTx) {
    let (ev_tx, ev_rx) = crossbeam_channel::unbounded::<EngineEvent>();
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<EngineCommand>();
    let root = root.to_path_buf();
    std::thread::Builder::new()
        .name("aneural-engine".into())
        .spawn(move || {
            // The GUI is a host that may call out: the marketplace already
            // links a TLS stack, so handing the engine a fetcher here costs
            // nothing and is what makes tier-1 spores work.
            aneural_engine::run_with(
                &root,
                ev_tx,
                cmd_rx,
                Some(Box::new(aneural_registry::UreqFetcher::new())),
            )
        })
        .expect("spawn engine thread");
    (EngineRx(ev_rx), EngineTx(cmd_tx))
}

fn spawn_engine(mut commands: Commands, ws: Res<WorkspaceRes>) {
    let (rx, tx) = start_engine(ws.ws.root());
    commands.insert_resource(rx);
    commands.insert_resource(tx);
}

#[allow(clippy::too_many_arguments)]
fn drain_events(
    mut commands: Commands,
    rx: Option<Res<EngineRx>>,
    mut spores_res: Option<ResMut<crate::marketplace::SporesRes>>,
    mut status: ResMut<IndexStatus>,
    mut pending: ResMut<PendingDeltas>,
    mut graph: ResMut<GraphState>,
    ws: Res<WorkspaceRes>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    atlas: Res<crate::render::IconAtlas>,
    glow: Res<crate::render::GlowTexture>,
    vibe: Res<crate::circadian::Vibe>,
    mut nodes: Query<(&mut crate::graph::GraphNode, &crate::graph::Pos)>,
    time: Res<Time>,
) {
    // Last frame's news, read in `Update` and stale by now. Cleared here
    // rather than by the reader so that turning the reader off cannot leave a
    // list growing for the life of the session.
    if !graph.stirred.is_empty() {
        graph.stirred.clear();
    }
    let Some(rx) = rx else { return };
    let now = time.elapsed_secs_f64();
    while let Ok(ev) = rx.0.try_recv() {
        match ev {
            EngineEvent::Delta(d) => {
                // a live batch is the watcher reacting; hold `busy` briefly so
                // the status line has time to say so
                if d.phase == DeltaPhase::Live {
                    status.busy_until = now + 0.8;
                }
                pending.0.push_back(d);
            }
            EngineEvent::Progress { phase, done, total } => {
                status.phase = phase.to_string();
                status.done = done;
                status.total = total;
            }
            EngineEvent::IndexComplete(stats) => {
                status.complete = true;
                status.phase = "complete".into();
                status.last_stats = Some(stats);
            }
            EngineEvent::Watching => status.watching = true,
            EngineEvent::Spores { spores, errors } => {
                if let Some(res) = spores_res.as_mut() {
                    res.spores = spores;
                    res.errors = errors;
                }
            }
            EngineEvent::Error(e) => {
                warn!("engine: {e}");
                status.last_error = Some(e);
            }
        }
    }
    status.busy = !status.complete || status.busy_until > now;
    if pending.0.is_empty() {
        return;
    }
    let mut budget = ws.config.gui.growth_budget_per_frame.max(1) as usize;
    let mut visuals = crate::render::Visuals {
        meshes: &mut meshes,
        materials: &mut materials,
        atlas: &atlas,
        glow: &glow,
        vibe: &vibe,
    };
    while budget > 0 {
        let Some(mut delta) = pending.0.pop_front() else {
            break;
        };
        if delta.nodes.len() + delta.edges.len() > budget {
            // split: apply the first `budget` items, park the rest
            let mut rest = GraphDelta::new(delta.phase);
            rest.initial_complete = delta.initial_complete;
            delta.initial_complete = false;
            if delta.nodes.len() > budget {
                rest.nodes = delta.nodes.split_off(budget);
                rest.edges = std::mem::take(&mut delta.edges);
            } else {
                let keep = budget - delta.nodes.len();
                rest.edges = delta.edges.split_off(keep.min(delta.edges.len()));
            }
            budget = 0;
            pending.0.push_front(rest);
            apply_delta(
                &mut commands,
                &mut graph,
                &ws,
                &mut visuals,
                &mut nodes,
                delta,
            );
        } else {
            budget -= delta.nodes.len() + delta.edges.len();
            apply_delta(
                &mut commands,
                &mut graph,
                &ws,
                &mut visuals,
                &mut nodes,
                delta,
            );
        }
        status.generation += 1;
    }
}

fn stop_on_exit(mut exit: MessageReader<AppExit>, tx: Option<Res<EngineTx>>) {
    if exit.read().next().is_some()
        && let Some(tx) = tx
    {
        let _ = tx.0.send(EngineCommand::Stop);
    }
}
