//! Opening another workspace without restarting, and the list of recent ones.

use crate::camera::{AutoFollow, FrameRequest};
use crate::engine::{EngineTx, IndexStatus, PendingDeltas, start_engine};
use crate::filters::Filters;
use crate::focus::FocusState;
use crate::graph::{GraphEdge, GraphNode, GraphState};
use crate::layout::LayoutParams;
use crate::picking::{DragState, Hovered, Selection};
use crate::workspace::WorkspaceRes;
use aneural_engine::EngineCommand;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use std::path::{Path, PathBuf};

/// How many workspaces the menu remembers.
const RECENT_LIMIT: usize = 8;

/// A directory the user asked for, applied on the next frame.
#[derive(Resource, Default)]
pub struct OpenRequest(pub Option<PathBuf>);

/// What belongs to the user rather than to any one workspace: kept in their
/// config directory, never in a `.aneural/`.
pub(crate) fn user_config_dir() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(config.join("aneural"))
}

/// Workspaces opened before, most recent first. Shared by every window of the
/// app, so it lives beside the user's home rather than in any one workspace.
#[derive(Resource, Default)]
pub struct Recents(pub Vec<PathBuf>);

impl Recents {
    fn path() -> Option<PathBuf> {
        Some(user_config_dir()?.join("recent.json"))
    }

    fn load() -> Self {
        let paths = Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<Vec<PathBuf>>(&s).ok())
            .unwrap_or_default();
        Recents(paths.into_iter().filter(|p| p.is_dir()).collect())
    }

    /// Put `root` at the top, without a second entry for it further down.
    fn remember(&mut self, root: &Path) {
        self.0.retain(|p| p != root);
        self.0.insert(0, root.to_path_buf());
        self.0.truncate(RECENT_LIMIT);
        let Some(path) = Self::path() else { return };
        if let Some(dir) = path.parent()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            warn!("recent workspaces: {e}");
            return;
        }
        match serde_json::to_string_pretty(&self.0) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    warn!("recent workspaces: {e}");
                }
            }
            Err(e) => warn!("recent workspaces: {e}"),
        }
    }
}

pub struct SwitchPlugin;

impl Plugin for SwitchPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<OpenRequest>()
            .insert_resource(Recents::load())
            .add_systems(Startup, remember_initial)
            .add_systems(Update, open_requested);
    }
}

fn remember_initial(ws: Res<WorkspaceRes>, mut recents: ResMut<Recents>) {
    let root = ws.ws.root().to_path_buf();
    recents.remember(&root);
}

/// Swap the whole graph over to another directory: everything derived from the
/// old workspace goes, and a fresh engine grows the new one in its place.
///
/// Exclusive because it replaces a dozen resources at once; it does nothing at
/// all on the frames where no one has asked for a workspace.
fn open_requested(world: &mut World) {
    let Some(root) = world.resource_mut::<OpenRequest>().0.take() else {
        return;
    };
    if world.resource::<WorkspaceRes>().ws.root() == root {
        return;
    }

    // Open the new one first: a directory that cannot be read leaves the
    // current workspace untouched rather than tearing it down for nothing.
    let ws = aneural_core::Workspace::at(&root);
    let (config, node_types) = match aneural_engine::Engine::open(ws.clone()) {
        Ok(engine) => {
            let types = engine
                .node_types()
                .unwrap_or_else(|_| aneural_core::config::builtin_node_types());
            (engine.config().clone(), types)
        }
        Err(e) => {
            warn!("could not open {}: {e}", root.display());
            world.resource_mut::<IndexStatus>().last_error =
                Some(format!("could not open {}: {e}", root.display()));
            return;
        }
    };

    if let Some(tx) = world.get_resource::<EngineTx>() {
        let _ = tx.0.send(EngineCommand::Stop);
    }
    let mut graph_entities =
        world.query_filtered::<Entity, Or<(With<GraphNode>, With<GraphEdge>)>>();
    for entity in graph_entities.iter(world).collect::<Vec<_>>() {
        world.entity_mut(entity).despawn();
    }

    let workspace = WorkspaceRes::new(ws, config, node_types);
    let title = format!("Aneural — {}", workspace.name());
    world
        .resource_mut::<crate::circadian::Vibe>()
        .adopt(&workspace.config.gui.circadian);
    world.insert_resource(workspace);
    world.insert_resource(GraphState::default());
    world.insert_resource(PendingDeltas::default());
    world.insert_resource(IndexStatus::default());
    world.insert_resource(Filters::default());
    world.insert_resource(Selection::default());
    // Signals are about the workspace that was open; nothing that happened in
    // it is news in this one. The probe is told the new one on its next pass,
    // so the thread itself keeps running.
    world.insert_resource(crate::live::Signals::default());
    world.insert_resource(crate::live::Named::default());
    world.insert_resource(Hovered::default());
    world.insert_resource(DragState::default());
    world.insert_resource(FocusState::default());
    world.insert_resource(AutoFollow::default());
    world.insert_resource(FrameRequest {
        now: true,
        ..Default::default()
    });
    world.resource_mut::<LayoutParams>().stir();

    let (rx, tx) = start_engine(&root);
    world.insert_resource(rx);
    world.insert_resource(tx);

    let mut windows = world.query_filtered::<&mut Window, With<PrimaryWindow>>();
    if let Ok(mut window) = windows.single_mut(world) {
        window.title = title;
    }
    world.resource_mut::<Recents>().remember(&root);
    info!("opened {}", root.display());
}
