//! Aneural desktop app: a living mycelium graph of your workspace.

#![allow(clippy::type_complexity, clippy::too_many_arguments)]

mod camera;
mod circadian;
mod engine;
mod filters;
mod focus;
mod graph;
mod layout;
mod live;
mod markdown;
mod marketplace;
mod picking;
mod render;
mod review;
mod switch;
mod theme;
mod ui;
mod workspace;

use bevy::prelude::*;
use bevy::window::PresentMode;
use std::path::PathBuf;

fn pick_root() -> Option<PathBuf> {
    let arg = std::env::args().skip(1).find(|a| !a.starts_with('-'));
    let path = match arg {
        Some(a) => PathBuf::from(a),
        None => rfd::FileDialog::new()
            .set_title("Open a directory to grow")
            .pick_folder()?,
    };
    path.canonicalize().ok().filter(|p| p.is_dir())
}

fn main() {
    let Some(root) = pick_root() else {
        eprintln!("usage: aneural-gui <directory>");
        std::process::exit(2);
    };
    let ws = aneural_core::Workspace::at(&root);
    let (config, node_types) = match aneural_engine::Engine::open(ws.clone()) {
        Ok(engine) => {
            let types = engine
                .node_types()
                .unwrap_or_else(|_| aneural_core::config::builtin_node_types());
            (engine.config().clone(), types)
        }
        Err(e) => {
            eprintln!("aneural: could not open workspace {}: {e}", root.display());
            std::process::exit(1);
        }
    };
    let workspace = workspace::WorkspaceRes::new(ws, config, node_types);
    let title = format!("Aneural — {}", workspace.name());

    App::new()
        .insert_resource(ClearColor(theme::hex(theme::BACKGROUND)))
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title,
                present_mode: PresentMode::AutoVsync,
                ..default()
            }),
            ..default()
        }))
        .insert_resource(workspace)
        .init_resource::<graph::GraphState>()
        .add_plugins((
            circadian::CircadianPlugin,
            render::RenderPlugin,
            engine::EnginePlugin,
            layout::LayoutPlugin,
            camera::CameraPlugin,
            picking::PickingPlugin,
            filters::FiltersPlugin,
            live::LivePlugin,
            ui::UiPlugin,
            focus::FocusPlugin,
            switch::SwitchPlugin,
            marketplace::MarketplacePlugin,
        ))
        .add_systems(
            Update,
            (graph::tick_grow_in, graph::sync_transforms).chain(),
        )
        .run();
}
