//! Camera, pan/zoom, framing, hotkeys.
//!
//! Panning: left-drag on empty canvas (right/middle drag anywhere) via
//! `bevy_pancam`; left-drag on a node is handled by `picking` instead.
//! Framing: while the graph is first growing the camera follows the graph's
//! bounds so it stays centered; the follow stops on the first manual pan/zoom
//! or once indexing is complete and the layout has settled. `F` re-frames.

use crate::engine::{EngineTx, IndexStatus};
use crate::graph::{GraphNode, Hidden, Pos};
use crate::layout::LayoutParams;
use crate::picking::{DragState, Hovered};
use aneural_engine::EngineCommand;
use bevy::camera::visibility::RenderLayers;
use bevy::ecs::entity::EntityHashSet;
use bevy::input::mouse::MouseWheel;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy_egui::{EguiContexts, PrimaryEguiContext};
use bevy_pancam::{PanCam, PanCamPlugin, PanCamSystems};

const MIN_SCALE: f32 = 0.15;
/// Far enough out to hold a whole monorepo. The old ceiling of 12 was below
/// what a few thousand nodes need, so framing could not actually frame them
/// and the graph could only ever be looked at from inside.
const MAX_SCALE: f32 = 60.0;
/// Pixels the cursor must travel before a background press counts as a pan.
const PAN_DEADZONE: f32 = 4.0;

#[derive(Component)]
pub struct MainCamera;

/// An earlier pass over the same view as the main camera. 2D gizmos are
/// always queued last within a pass, so edges drawn alongside the nodes would
/// paint over them; stacking passes is how they go underneath instead. From
/// the bottom:
///
/// 1. [`HYPHAE_LAYER`]: every hypha not being followed.
/// 2. [`FADED_LAYER`]: nodes faded because another node is in focus.
/// 3. [`LIT_HYPHAE_LAYER`]: the focused node's own hyphae.
/// 4. The main pass: every other node, then egui's panels.
///
/// Nodes always sit over hyphae, except that the thread being followed runs
/// over the nodes that have stepped back.
#[derive(Component)]
pub struct Underlay;

pub const HYPHAE_LAYER: usize = 1;
pub const FADED_LAYER: usize = 2;
pub const LIT_HYPHAE_LAYER: usize = 3;

#[derive(Resource, Default)]
pub struct UiCapture {
    pub pointer: bool,
    pub keyboard: bool,
}

/// One-shot "frame this now".
#[derive(Resource, Default)]
pub struct FrameRequest {
    pub now: bool,
    /// Frame only these. Empty is everything visible, which is what `F` and
    /// the frame button ask for; a plan step asks for its own files instead.
    pub only: EntityHashSet,
}

impl FrameRequest {
    /// Frame everything visible.
    pub fn all(&mut self) {
        self.now = true;
        self.only.clear();
    }

    pub fn these(&mut self, only: EntityHashSet) {
        self.now = true;
        self.only = only;
    }
}

/// Smoothly keep the whole graph in view while it grows.
#[derive(Resource)]
pub struct AutoFollow {
    /// Whether the camera is still keeping the whole graph in view.
    pub on: bool,
    /// Set once the user pans or zooms: from then on the view is theirs and
    /// framing never takes it back on its own.
    pub released: bool,
}

impl Default for AutoFollow {
    fn default() -> Self {
        AutoFollow {
            on: true,
            released: false,
        }
    }
}

/// The window region not covered by egui panels, in logical pixels
/// (screen coordinates, y down). Written by `ui::panels`.
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct CanvasRect {
    pub min: Vec2,
    pub max: Vec2,
}

impl CanvasRect {
    fn size(&self) -> Vec2 {
        self.max - self.min
    }
    fn center(&self) -> Vec2 {
        (self.min + self.max) / 2.0
    }
    /// Is a window-space point (logical pixels, y down) on the canvas?
    pub fn contains(&self, p: Vec2) -> bool {
        p.x >= self.min.x && p.x <= self.max.x && p.y >= self.min.y && p.y <= self.max.y
    }
}

/// A left-button press that started on empty canvas (a pan, or a click that
/// clears the selection if the cursor never moved).
#[derive(Resource, Default, Debug)]
pub struct PanGrab {
    pub active: bool,
    pub moved: bool,
    start: Vec2,
}

pub struct CameraPlugin;

impl Plugin for CameraPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(PanCamPlugin)
            .init_resource::<UiCapture>()
            .init_resource::<FrameRequest>()
            .init_resource::<AutoFollow>()
            .init_resource::<PanGrab>()
            .init_resource::<CanvasRect>()
            .add_systems(Startup, spawn_camera)
            .add_systems(bevy_egui::EguiPrimaryContextPass, read_ui_capture)
            .add_systems(
                Update,
                (
                    (gate_pancam, hotkeys)
                        .after(crate::picking::pick)
                        .before(PanCamSystems),
                    (frame_all, follow_graph, sync_hyphae_camera)
                        .chain()
                        .after(PanCamSystems),
                ),
            );
    }
}

fn spawn_camera(mut commands: Commands) {
    // The node pass goes last and carries egui, so the panels stay on top of
    // everything; bevy_egui claims the first camera it sees, hence the order.
    commands.spawn((
        MainCamera,
        PrimaryEguiContext,
        Camera2d,
        Camera {
            clear_color: ClearColorConfig::None,
            ..default()
        },
        RenderLayers::layer(0),
        PanCam {
            grab_buttons: vec![MouseButton::Left, MouseButton::Right, MouseButton::Middle],
            zoom_to_cursor: true,
            min_scale: MIN_SCALE,
            max_scale: MAX_SCALE,
            ..default()
        },
    ));
    for (order, layer) in [
        (-3, HYPHAE_LAYER),
        (-2, FADED_LAYER),
        (-1, LIT_HYPHAE_LAYER),
    ] {
        commands.spawn((
            Underlay,
            Camera2d,
            Camera {
                order,
                // the bottom pass clears the frame, the rest paint over it
                clear_color: if layer == HYPHAE_LAYER {
                    ClearColorConfig::Default
                } else {
                    ClearColorConfig::None
                },
                ..default()
            },
            RenderLayers::layer(layer),
        ));
    }
}

/// Keep the underlay passes looking through the same lens as the main camera.
fn sync_hyphae_camera(
    main: Query<(&Transform, &Projection), (With<MainCamera>, Without<Underlay>)>,
    mut underlays: Query<(&mut Transform, &mut Projection), With<Underlay>>,
) {
    let Ok((t, p)) = main.single() else {
        return;
    };
    for (mut ot, mut op) in &mut underlays {
        *ot = *t;
        *op = p.clone();
    }
}

fn read_ui_capture(mut contexts: EguiContexts, mut capture: ResMut<UiCapture>) {
    if let Ok(ctx) = contexts.ctx_mut() {
        capture.pointer = ctx.egui_wants_pointer_input() || ctx.is_pointer_over_egui();
        capture.keyboard = ctx.egui_wants_keyboard_input();
    }
}

/// Decide each frame whether `bevy_pancam` may move the camera, and track
/// background left-presses so they pan instead of picking.
fn gate_pancam(
    capture: Res<UiCapture>,
    canvas: Res<CanvasRect>,
    mouse: Res<ButtonInput<MouseButton>>,
    mut wheel: MessageReader<MouseWheel>,
    hovered: Res<Hovered>,
    drag: Res<DragState>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut grab: ResMut<PanGrab>,
    mut follow: ResMut<AutoFollow>,
    mut cams: Query<&mut PanCam>,
) {
    let cursor = windows.single().ok().and_then(Window::cursor_position);
    // The panels are painted into egui's background layer, which egui does not
    // report as an area the pointer is over, so the canvas rectangle is what
    // actually says whether a wheel tick or a drag belongs to the graph.
    let on_canvas = cursor.is_some_and(|c| canvas.contains(c)) && !capture.pointer;

    if mouse.just_pressed(MouseButton::Left) {
        grab.active = on_canvas && hovered.0.is_none() && drag.entity.is_none();
        grab.moved = false;
        grab.start = cursor.unwrap_or_default();
    }
    if grab.active {
        if let Some(c) = cursor
            && c.distance(grab.start) > PAN_DEADZONE
        {
            grab.moved = true;
        }
        if !mouse.pressed(MouseButton::Left) {
            grab.active = false;
        }
    }

    let side_pan =
        (mouse.pressed(MouseButton::Right) || mouse.pressed(MouseButton::Middle)) && on_canvas;
    let scrolled = wheel.read().next().is_some() && on_canvas;
    if (grab.active && grab.moved) || side_pan || scrolled {
        *follow = AutoFollow {
            on: false,
            released: true,
        };
    }

    // Left button: only pan for a grab that began on empty canvas. Other
    // buttons and keys: pan unless egui owns the pointer or a node is being dragged.
    let enabled = drag.entity.is_none()
        && if mouse.pressed(MouseButton::Left) {
            grab.active
        } else {
            on_canvas
        };
    for mut cam in &mut cams {
        cam.enabled = enabled;
    }
}

fn hotkeys(
    keys: Res<ButtonInput<KeyCode>>,
    capture: Res<UiCapture>,
    mut frame: ResMut<FrameRequest>,
    mut follow: ResMut<AutoFollow>,
    mut layout: ResMut<LayoutParams>,
    tx: Option<Res<EngineTx>>,
) {
    if capture.keyboard {
        return;
    }
    if keys.just_pressed(KeyCode::KeyF) {
        frame.all();
    }
    if keys.just_pressed(KeyCode::Space) {
        layout.stir();
    }
    if keys.just_pressed(KeyCode::KeyR)
        && let Some(tx) = tx
    {
        let _ = tx.0.send(EngineCommand::Reindex { force: false });
    }
    if keys.any_pressed([
        KeyCode::ArrowLeft,
        KeyCode::ArrowRight,
        KeyCode::ArrowUp,
        KeyCode::ArrowDown,
        KeyCode::KeyW,
        KeyCode::KeyA,
        KeyCode::KeyS,
        KeyCode::KeyD,
    ]) {
        *follow = AutoFollow {
            on: false,
            released: true,
        };
    }
}

/// The centre these positions sit around, and the orthographic scale that
/// fits them into a target `size` screen points across.
///
/// No panel arithmetic: the caller owns the whole target. That is what lets a
/// portal — which renders to a texture of its own and has no panels to avoid —
/// frame a handful of nodes with the same code that frames the workspace.
pub fn fit_into(positions: impl Iterator<Item = Vec2>, size: Vec2) -> Option<(Vec2, f32)> {
    let mut min = Vec2::splat(f32::MAX);
    let mut max = Vec2::splat(f32::MIN);
    let mut count = 0;
    for p in positions {
        min = min.min(p);
        max = max.max(p);
        count += 1;
    }
    if count == 0 {
        return None;
    }
    // A little air, and a floor so that one node alone does not fill the frame.
    let world = (max - min).max(Vec2::splat(100.0)) + Vec2::splat(160.0);
    let size = size.max(Vec2::splat(120.0));
    let scale = (world.x / size.x)
        .max(world.y / size.y)
        .clamp(MIN_SCALE, MAX_SCALE);
    Some(((min + max) / 2.0, scale))
}

/// Camera translation and orthographic scale that fit every visible node
/// inside the canvas (the window minus egui panels).
fn fit(
    positions: impl Iterator<Item = Vec2>,
    windows: &Query<&Window, With<PrimaryWindow>>,
    canvas: &CanvasRect,
) -> Option<(Vec2, f32)> {
    // Room for the names. A node's label is drawn beneath it and reaches well
    // past it, so fitting the node positions alone puts the outermost label
    // half under a drawer. The margin is in screen points because that is what
    // a label's width is measured in — world padding shrinks as the camera
    // pulls back, exactly when the labels do not.
    const MARGIN: f32 = 80.0;
    let win = windows
        .single()
        .ok()
        .map(|w| Vec2::new(w.width(), w.height()))
        .unwrap_or(Vec2::new(1280.0, 800.0));
    let (canvas_size, canvas_center) = if canvas.size().x > 50.0 && canvas.size().y > 50.0 {
        (canvas.size(), canvas.center())
    } else {
        (win, win / 2.0)
    };
    let canvas_size = canvas_size - Vec2::splat(2.0 * MARGIN);
    let (center, scale) = fit_into(positions, canvas_size)?;
    // the camera looks at the window centre; shift it so the graph centre
    // lands on the canvas centre instead (screen y is down, world y is up)
    let offset = canvas_center - win / 2.0;
    Some((center - Vec2::new(offset.x, -offset.y) * scale, scale))
}

fn frame_all(
    mut frame: ResMut<FrameRequest>,
    nodes: Query<(Entity, &Pos, Has<Hidden>), With<GraphNode>>,
    mut cam: Query<(&mut Transform, &mut Projection), With<MainCamera>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    canvas: Res<CanvasRect>,
) {
    if !frame.now {
        return;
    }
    frame.now = false;
    let wanted = std::mem::take(&mut frame.only);
    // Asked-for nodes are framed whether or not a filter is hiding them this
    // frame: the ask came from the reader, and the filters catch up a frame
    // later. Framing everything still means everything *visible*.
    let seen = nodes
        .iter()
        .filter(|(e, _, hidden)| match wanted.is_empty() {
            true => !hidden,
            false => wanted.contains(e),
        })
        .map(|(_, p, _)| p.0);
    let Some((center, scale)) = fit(seen, &windows, &canvas) else {
        return;
    };
    let Ok((mut t, mut proj)) = cam.single_mut() else {
        return;
    };
    t.translation.x = center.x;
    t.translation.y = center.y;
    if let Projection::Orthographic(o) = &mut *proj {
        o.scale = scale;
    }
}

/// While `AutoFollow` is on, ease the camera toward the graph's bounds every
/// frame so the graph grows in the middle of the window. Stops (after a final
/// snap) once the index is complete and the layout has come to rest.
fn follow_graph(
    time: Res<Time>,
    mut follow: ResMut<AutoFollow>,
    status: Res<IndexStatus>,
    layout: Res<LayoutParams>,
    nodes: Query<&Pos, (With<GraphNode>, Without<Hidden>)>,
    mut cam: Query<(&mut Transform, &mut Projection), With<MainCamera>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    canvas: Res<CanvasRect>,
    mut steady: Local<f32>,
    mut generation: Local<u64>,
) {
    // A workspace of thousands takes a while to arrive and grows by a long
    // way as it does, so every new batch puts the camera back on the graph.
    // Only while it is first growing, though: once the index is in, changes
    // come a file at a time, and re-framing on each one would carry the view
    // off whatever was being looked at. Nor at all if the user has taken the
    // view over themselves.
    if status.generation != *generation {
        *generation = status.generation;
        *steady = 0.0;
        if !follow.released && !status.complete {
            follow.on = true;
        }
    }
    if !follow.on {
        return;
    }
    let Some((center, scale)) = fit(nodes.iter().map(|p| p.0), &windows, &canvas) else {
        return;
    };
    let Ok((mut t, mut proj)) = cam.single_mut() else {
        return;
    };
    // Still for a moment is not settled: the layout falls quiet between
    // batches while the rest of the workspace is still being indexed.
    if status.complete && layout.frozen {
        *steady += time.delta_secs();
    } else {
        *steady = 0.0;
    }
    let settled = *steady > 0.75;
    // exponential ease; snap on the final frame
    let k = if settled {
        1.0
    } else {
        1.0 - (-time.delta_secs() * 3.0).exp()
    };
    let cur = t.translation.truncate();
    let next = cur.lerp(center, k);
    t.translation.x = next.x;
    t.translation.y = next.y;
    if let Projection::Orthographic(o) = &mut *proj {
        o.scale += (scale - o.scale) * k;
    }
    if settled {
        follow.on = false;
    }
}
