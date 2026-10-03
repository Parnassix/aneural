//! Node visuals (circle mesh + icon sprite + label), hyphae edge drawing, and
//! the bioluminescence that comes over all of it after dark.

use crate::camera::{FADED_LAYER, HYPHAE_LAYER, LIT_HYPHAE_LAYER, MainCamera};
use crate::circadian::Vibe;
use crate::graph::{Drift, GraphEdge, GraphNode, GraphState, GrowIn, Hidden, Pos};
use crate::picking::{Hovered, Selection};
use crate::theme;
use crate::workspace::{WorkspaceRes, node_radius};
use aneural_core::Node;
use aneural_core::kinds::EdgeKind;
use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::RenderLayers;
use bevy::ecs::entity::EntityHashSet;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use std::collections::HashMap;
use std::path::Path;

#[derive(Resource, Default)]
pub struct IconAtlas {
    pub by_name: HashMap<&'static str, Handle<Image>>,
}

/// One soft radial falloff, shared by every node's halo, so the whole glow
/// costs a single texture and batches as a single draw.
#[derive(Resource, Default)]
pub struct GlowTexture(pub Handle<Image>);

#[derive(Resource, Default)]
pub struct MeshCache {
    pub materials: HashMap<String, Handle<ColorMaterial>>,
}

#[derive(Component)]
pub struct NodeLabel;

/// The icon punched into a node's disc. It darkens as the disc brightens, so
/// it stays legible against a glowing node.
#[derive(Component)]
pub struct NodeIcon;

/// The node's disc, carrying the colour it is drawn in so that
/// [`node_colors`] can put it back after fading or lighting it.
#[derive(Component)]
pub struct NodeBody(pub Color);

/// The halo behind a node: invisible by day, and after dark the light the
/// node is giving off. Carries the node's daylight colour, so the halo can be
/// relit from it as the night comes on.
#[derive(Component)]
pub struct GlowHalo(pub Color);

#[derive(Component)]
pub struct SelectionRing;

/// Everything a node needs in order to be drawn for the first time. Bundled
/// because a sprouting node wants five unrelated things and threading them
/// one by one through the delta path buries the code that matters.
pub struct Visuals<'a> {
    pub meshes: &'a mut Assets<Mesh>,
    pub materials: &'a mut Assets<ColorMaterial>,
    pub atlas: &'a IconAtlas,
    pub glow: &'a GlowTexture,
    pub vibe: &'a Vibe,
}

pub struct RenderPlugin;

impl Plugin for RenderPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<IconAtlas>()
            .init_resource::<GlowTexture>()
            .init_resource::<MeshCache>()
            .init_resource::<Spotlight>()
            .init_gizmo_group::<Skeleton>()
            .init_gizmo_group::<LitHyphae>()
            .add_systems(
                Startup,
                (build_icon_atlas, build_glow_texture, hyphae_behind_nodes),
            )
            .add_systems(
                Update,
                (
                    (thin_hyphae, draw_spore_motes, draw_edges).chain(),
                    hidden_visibility,
                    label_visibility,
                    selection_ring,
                    hover_scale,
                    node_colors,
                    fade_layers,
                    night_chrome,
                    breathe_halos,
                ),
            )
            .add_systems(
                Update,
                aim_spotlight
                    .before(draw_edges)
                    .before(node_colors)
                    .before(fade_layers),
            );
    }
}

/// The folder tree's hyphae: the body of the mycelium, drawn thicker than
/// the links between files so the growth reads first and the wiring second.
#[derive(Default, Reflect, GizmoConfigGroup)]
pub struct Skeleton;

/// The hyphae of the node in focus. They are drawn in a pass of their own, over
/// the nodes that have faded back but under the ones still lit.
#[derive(Default, Reflect, GizmoConfigGroup)]
pub struct LitHyphae;

/// Stroke widths in screen pixels, close in. Pulled back over a whole
/// monorepo the same strokes would merge into one lump of ink, so they thin
/// and fade with the distance: see [`thinning`].
const SKELETON_WIDTH: f32 = 2.4;
const LINK_WIDTH: f32 = 1.0;
const LIT_WIDTH: f32 = 2.0;
/// The zoom at which strands start thinning, and the one past which they are
/// as fine and faint as they go (world units per screen pixel).
const THIN_FROM: f32 = 2.0;
const THIN_TO: f32 = 24.0;
/// What a strand is reduced to at [`THIN_TO`]: a little under half its width,
/// and a little over half its ink.
const THINNEST: f32 = 0.45;
const FAINTEST: f32 = 0.42;

/// How far the view has pulled back, 0 (close in) to 1 (a whole monorepo).
fn thinning(zoom: f32) -> f32 {
    ((zoom - THIN_FROM) / (THIN_TO - THIN_FROM)).clamp(0.0, 1.0)
}

/// Thin every stroke as the view pulls back. A separate system because the
/// gizmos themselves hold the config store while they are being drawn.
fn thin_hyphae(mut store: ResMut<GizmoConfigStore>, camera: Query<&Projection, With<MainCamera>>) {
    let Ok(Projection::Orthographic(ortho)) = camera.single() else {
        return;
    };
    let thinner = 1.0 - (1.0 - THINNEST) * thinning(ortho.scale.max(1e-3));
    store.config_mut::<DefaultGizmoConfigGroup>().0.line.width = LINK_WIDTH * thinner;
    store.config_mut::<Skeleton>().0.line.width = SKELETON_WIDTH * thinner;
    store.config_mut::<LitHyphae>().0.line.width = LIT_WIDTH * thinner;
}

/// 2D gizmos are always queued last, whatever their depth, so the hyphae would
/// paint over the nodes. Park them on render layers of their own, drawn by the
/// underlay cameras before the main pass (see [`crate::camera::Underlay`]).
fn hyphae_behind_nodes(mut store: ResMut<GizmoConfigStore>) {
    let (config, _) = store.config_mut::<DefaultGizmoConfigGroup>();
    config.render_layers = RenderLayers::layer(HYPHAE_LAYER);
    config.line.width = LINK_WIDTH;
    let (config, _) = store.config_mut::<Skeleton>();
    config.render_layers = RenderLayers::layer(HYPHAE_LAYER);
    config.line.width = SKELETON_WIDTH;
    // round joints, or a thick bowed strand shows notches at every bend
    config.line.joints = GizmoLineJoint::Round(4);
    let (config, _) = store.config_mut::<LitHyphae>();
    config.render_layers = RenderLayers::layer(LIT_HYPHAE_LAYER);
    config.line.width = LIT_WIDTH;
    config.line.joints = GizmoLineJoint::Round(4);
}

/// What is lit right now; everything else fades back. Worked out once a frame
/// so the hyphae, the node colours and the render layers always agree.
#[derive(Resource, Default, PartialEq)]
pub struct Spotlight {
    /// Nodes that stay lit. `None` when nothing is singled out.
    pub keep: Option<EntityHashSet>,
    pub hyphae: LitHyphaeRule,
    /// Bumped whenever the spotlight moves.
    pub moves: u64,
}

#[derive(Default, PartialEq)]
pub enum LitHyphaeRule {
    #[default]
    None,
    /// Every hypha touching this node: a node in hand.
    Touching(Entity),
    /// Hyphae with both ends lit: a node kind pointed at in the drawer.
    Between,
    /// Every hypha of this kind: an edge kind pointed at in the drawer.
    Kind(String),
}

impl Spotlight {
    pub fn on(&self) -> bool {
        self.keep.is_some()
    }

    pub fn lights(&self, e: &GraphEdge) -> bool {
        match &self.hyphae {
            LitHyphaeRule::None => false,
            LitHyphaeRule::Touching(f) => *f == e.src || *f == e.dst,
            LitHyphaeRule::Between => self
                .keep
                .as_ref()
                .is_some_and(|k| k.contains(&e.src) && k.contains(&e.dst)),
            LitHyphaeRule::Kind(kind) => e.kind == *kind,
        }
    }
}

/// A kind pointed at in the Filters drawer outranks the node in hand: the
/// pointer is on the drawer, so that is what the user is asking about.
fn aim_spotlight(
    mut spot: ResMut<Spotlight>,
    selection: Res<Selection>,
    hovered: Res<Hovered>,
    graph: Res<GraphState>,
    filters: Res<crate::filters::Filters>,
    review: Res<crate::review::Review>,
    nodes: Query<(Entity, &GraphNode, Has<Hidden>)>,
    edges: Query<&GraphEdge>,
) {
    use crate::filters::PointedKind;
    // A plan open in the drawer outranks everything: the reader has asked, in
    // as many words, to be shown this one thing. Pointing at a row in the
    // Filters drawer still wins over hovering, and hovering over selection,
    // as before.
    if let Some(plan) = &review.plan {
        let reading = crate::review::read(&graph, plan);
        if !reading.is_empty() {
            let keep: EntityHashSet = review
                .lit(&reading)
                .iter()
                .filter_map(|id| graph.by_id.get(id).copied())
                .collect();
            let keep = Some(keep);
            // `Between` rather than `Touching`: a plan's own threads are not
            // strands, so what should light is the wiring *among* the files it
            // reaches — which is the shape of the work it describes.
            if spot.keep != keep || spot.hyphae != LitHyphaeRule::Between {
                spot.keep = keep;
                spot.hyphae = LitHyphaeRule::Between;
                spot.moves += 1;
            }
            return;
        }
    }
    let (keep, hyphae) = match &filters.pointed {
        Some(PointedKind::Node(kind)) => (
            Some(
                nodes
                    .iter()
                    .filter(|(_, n, hidden)| !hidden && n.kind == *kind)
                    .map(|(e, ..)| e)
                    .collect(),
            ),
            LitHyphaeRule::Between,
        ),
        Some(PointedKind::Edge(kind)) => {
            let mut keep = EntityHashSet::default();
            if filters.edge_visible(kind) {
                for e in edges.iter().filter(|e| e.kind == *kind) {
                    keep.insert(e.src);
                    keep.insert(e.dst);
                }
            }
            (Some(keep), LitHyphaeRule::Kind(kind.clone()))
        }
        None => {
            let focus = selection
                .primary
                .as_ref()
                .or(hovered.0.as_ref())
                .and_then(|id| Some((id, *graph.by_id.get(id)?)));
            match focus {
                Some((id, f)) => {
                    let mut keep = EntityHashSet::default();
                    keep.insert(f);
                    for (n, _) in graph.neighbors(id) {
                        if let Some(&e) = graph.by_id.get(n) {
                            keep.insert(e);
                        }
                    }
                    (Some(keep), LitHyphaeRule::Touching(f))
                }
                None => (None, LitHyphaeRule::None),
            }
        }
    };
    if spot.keep != keep || spot.hyphae != hyphae {
        spot.keep = keep;
        spot.hyphae = hyphae;
        spot.moves += 1;
    }
}

/// Send the faded nodes to the pass under the focused node's hyphae, and bring
/// them back once nothing is in focus.
#[allow(clippy::type_complexity)]
fn fade_layers(
    mut commands: Commands,
    spot: Res<Spotlight>,
    parts: Query<
        (Entity, &ChildOf, Has<RenderLayers>),
        Or<(
            With<NodeBody>,
            With<NodeIcon>,
            With<NodeLabel>,
            With<GlowHalo>,
        )>,
    >,
) {
    for (part, node, layered) in &parts {
        let faded = spot
            .keep
            .as_ref()
            .is_some_and(|k| !k.contains(&node.parent()));
        // Parts of a node that stays put are left alone, so moving the pointer
        // costs a write only for the nodes whose place actually changes.
        match (faded, layered) {
            (true, false) => {
                commands
                    .entity(part)
                    .insert(RenderLayers::layer(FADED_LAYER));
            }
            (false, true) => {
                commands.entity(part).remove::<RenderLayers>();
            }
            _ => {}
        }
    }
}

/// One hypha's strokes, for whichever pass it is drawn in.
enum Stroke {
    Flat(Vec<Vec2>, Color),
    Graded(Vec<(Vec2, Color)>),
}

fn paint<C: GizmoConfigGroup>(gizmos: &mut Gizmos<C>, strokes: Vec<Stroke>) {
    for s in strokes {
        match s {
            Stroke::Flat(pts, color) => gizmos.linestrip_2d(pts, color),
            Stroke::Graded(pts) => gizmos.linestrip_gradient_2d(pts),
        }
    }
}

fn build_icon_atlas(mut atlas: ResMut<IconAtlas>, mut images: ResMut<Assets<Image>>) {
    for name in aneural_icons::names() {
        let Ok(rgba) = aneural_icons::rasterize_named(name, 64) else {
            continue;
        };
        let image = Image::new(
            Extent3d {
                width: rgba.width,
                height: rgba.height,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            rgba.data,
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD,
        );
        atlas.by_name.insert(name, images.add(image));
    }
    info!("rasterised {} icons", atlas.by_name.len());
}

/// White, with the alpha falling off from the middle. A sprite tinted with
/// the node's colour then reads as that colour's light.
fn build_glow_texture(mut glow: ResMut<GlowTexture>, mut images: ResMut<Assets<Image>>) {
    const SIZE: usize = 96;
    let mut data = vec![0u8; SIZE * SIZE * 4];
    let centre = (SIZE as f32 - 1.0) / 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let d = Vec2::new(x as f32 - centre, y as f32 - centre).length() / centre;
            // A steep-ish falloff: a small bright core and a wide, very faint
            // skirt, which is what a light source in fog actually looks like.
            let a = (1.0 - d).clamp(0.0, 1.0).powf(2.6);
            let i = (y * SIZE + x) * 4;
            data[i..i + 3].fill(255);
            data[i + 3] = (a * 255.0) as u8;
        }
    }
    glow.0 = images.add(Image::new(
        Extent3d {
            width: SIZE as u32,
            height: SIZE as u32,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD,
    ));
}

pub fn icon_for(node: &Node, ws: &WorkspaceRes) -> &'static str {
    if node.kind == "File"
        && let Some(p) = &node.path
    {
        return aneural_icons::default_icon("File", Some(Path::new(p)));
    }
    ws.style(&node.kind).icon
}

pub fn spawn_node_visuals(
    commands: &mut Commands,
    entity: Entity,
    node: &Node,
    ws: &WorkspaceRes,
    v: &mut Visuals,
) {
    let style = ws.style(&node.kind);
    let r = node_radius(&node.kind);
    let palette = &v.vibe.palette;
    let mesh = v.meshes.add(shape_mesh(&style.shape, r));
    let mat = v
        .materials
        .add(ColorMaterial::from_color(palette.bioluminesce(style.color)));
    let icon = icon_for(node, ws);
    // Behind the disc, so the node sits in its own light rather than under it.
    let halo = r * 4.0;
    commands.spawn((
        GlowHalo(style.color),
        Sprite {
            image: v.glow.0.clone(),
            color: palette.glow(style.color).with_alpha(0.0),
            custom_size: Some(Vec2::splat(halo)),
            ..default()
        },
        Transform::from_xyz(0.0, 0.0, 0.25),
        Visibility::Hidden,
        ChildOf(entity),
    ));
    let mut body = commands.spawn((
        NodeBody(style.color),
        Mesh2d(mesh),
        MeshMaterial2d(mat),
        Transform::from_xyz(0.0, 0.0, 1.0),
    ));
    body.insert(ChildOf(entity));
    if let Some(img) = v.atlas.by_name.get(icon) {
        let size = r * 1.2;
        commands.spawn((
            NodeIcon,
            Sprite {
                image: img.clone(),
                color: palette.icon_ink,
                custom_size: Some(Vec2::splat(size)),
                ..default()
            },
            Transform::from_xyz(0.0, 0.0, 2.0),
            ChildOf(entity),
        ));
    }
    // `...` and not `…`: the canvas font has no glyph for the ellipsis, so
    // every label long enough to be cut — a quarter of them in a real
    // workspace — ended in a tofu box.
    let label = if node.label.chars().count() > 28 {
        format!("{}...", node.label.chars().take(27).collect::<String>())
    } else {
        node.label.clone()
    };
    commands.spawn((
        NodeLabel,
        Text2d(label),
        TextFont {
            font_size: FontSize::Px(LABEL_PX),
            ..default()
        },
        TextColor(palette.text),
        Transform::from_xyz(0.0, -(r + 9.0), 1.5),
        Visibility::Hidden,
        ChildOf(entity),
    ));
}

/// Mesh for a node shape name (`circle` | `hexagon` | `pill` | `square` | `diamond`).
pub fn shape_mesh(shape: &str, r: f32) -> Mesh {
    match shape {
        "hexagon" => RegularPolygon::new(r * 1.1, 6).into(),
        "square" => Rectangle::new(r * 1.8, r * 1.8).into(),
        "pill" => Capsule2d::new(r * 0.55, r * 1.2).into(),
        "diamond" => Rhombus::new(r * 2.2, r * 1.8).into(),
        _ => Circle::new(r).into(),
    }
}

/// Cubic Bézier hyphae sample points between two positions, sampled into
/// `segments` pieces. A hypha only a few pixels long needs two. `sway` pushes
/// the two control points sideways, in world units, on top of the bend the
/// seed gives it.
pub fn hypha_points(
    a: Vec2,
    b: Vec2,
    seed: u32,
    sway: (f32, f32),
    t_max: f32,
    segments: usize,
) -> Vec<Vec2> {
    let chord = b - a;
    let len = chord.length().max(1.0);
    let perp = chord.perp() / len;
    let n1 = ((seed % 1000) as f32 / 1000.0) * 2.0 - 1.0;
    let n2 = (((seed / 1000) % 1000) as f32 / 1000.0) * 2.0 - 1.0;
    let c1 = a + chord * 0.3 + perp * (len * BOW * n1 + sway.0);
    let c2 = a + chord * 0.7 + perp * (len * BOW * n2 + sway.1);
    let segments = segments.max(1);
    let count = ((segments as f32 * t_max).ceil() as usize).clamp(1, segments);
    let mut pts = Vec::with_capacity(count + 1);
    for i in 0..=count {
        let t = (i as f32 / segments as f32).min(t_max);
        let u = 1.0 - t;
        let p = a * (u * u * u) + c1 * (3.0 * u * u * t) + c2 * (3.0 * u * t * t) + b * (t * t * t);
        pts.push(p);
    }
    pts
}

/// How far a hypha's control points swing off the straight chord, as a
/// fraction of its length. Also the padding the cull box needs.
const BOW: f32 = 0.18;

/// No hypha is ever quite still: each one sways a little, day and night, as if
/// something were moving through it. At most this many *screen* pixels at a
/// control point — measured in world units it would vanish the moment the graph
/// was zoomed out to fit — and never more than [`SWAY_SHARE`] of the strand's
/// own length, so a short hypha does not flap.
const SWAY_REACH: f32 = 5.5;
const SWAY_SHARE: f32 = 0.07;
/// The quickest and slowest a strand takes to sway once, in seconds. Slow
/// enough that the mesh is never seen to move, only noticed to have moved.
const SWAY_SECONDS: (f32, f32) = (9.0, 16.0);
/// The folder tree is the living body of the mesh rather than a link drawn
/// across it, so it moves a little more than the rest.
const TREE_SWAY: f32 = 1.5;

/// After dark a strand now and then warbles: a quick shiver that runs through
/// it and dies away, as if something had passed along it. How far it throws the
/// strand, in screen pixels, on top of its sway.
const WARBLE_REACH: f32 = 3.6;
/// How long one warble lasts, and how many shivers it fits into that.
const WARBLE_SECONDS: f32 = 1.6;
const WARBLE_SHIVERS: f32 = 4.5;
/// The shortest and longest a strand waits between warbles. Minutes apart, and
/// each strand keeps its own clock, so they never come in a chorus.
const WARBLE_APART: (f32, f32) = (300.0, 700.0);

/// How far a strand's two control points lean right now, `scale` times the
/// usual reach. They move out of step, so the strand undulates rather than
/// swinging stiffly like a rope.
fn sway(clock: f32, seed: u32, len: f32, scale: f32, zoom: f32) -> (f32, f32) {
    let (fast, slow) = SWAY_SECONDS;
    let period = fast + (slow - fast) * ((seed >> 11) % 1000) as f32 / 1000.0;
    let phase = ((seed >> 3) % 1000) as f32 / 1000.0 * std::f32::consts::TAU;
    let w = clock / period * std::f32::consts::TAU + phase;
    let reach = (SWAY_REACH * scale * zoom).min(len * SWAY_SHARE * scale);
    (w.sin() * reach, (w * 1.3 + 2.1).sin() * reach)
}

/// Roughly how many screen pixels of hypha each sample covers. Curves read as
/// curves at this rate, and a graph of thousands stops paying for the rest.
const PIXELS_PER_SAMPLE: f32 = 26.0;

/// What a hypha fades to when something else has the focus.
const GHOST: f32 = 0.05;

/// How long one pulse takes to travel a hypha, at the fast and slow ends.
/// Each hypha picks its own from its seed, so the mesh never flashes in time.
const PULSE_SECONDS: (f32, f32) = (7.0, 14.0);
/// How much of a hypha one pulse lights at a time.
const PULSE_WIDTH: f32 = 0.16;

/// A number in 0..1 from some bits of a strand's seed.
fn seeded(bits: u32) -> f32 {
    (bits % 1000) as f32 / 1000.0
}

/// How hard a strand is shivering this instant: nothing at all almost always,
/// and for a second or so at a time, rarely, a damped quiver. Night only, and
/// it fades in and out with the night itself.
fn warble(clock: f32, seed: u32, night: f32, len: f32, zoom: f32) -> f32 {
    if night < 0.05 {
        return 0.0;
    }
    let (soon, later) = WARBLE_APART;
    let apart = soon + (later - soon) * seeded(seed >> 17);
    // its own moment in its own cycle
    let since = (clock - seeded(seed >> 5) * apart).rem_euclid(apart);
    if since > WARBLE_SECONDS {
        return 0.0;
    }
    // a bell over the shiver, so it arrives and leaves rather than switching on
    let envelope = (std::f32::consts::PI * since / WARBLE_SECONDS)
        .sin()
        .powi(2);
    let reach = (WARBLE_REACH * zoom).min(len * 0.12);
    (std::f32::consts::TAU * WARBLE_SHIVERS * since / WARBLE_SECONDS).sin()
        * envelope
        * reach
        * night
}

/// Where a hypha's pulse is along its length right now, in a cycle that
/// carries it off the far end and leaves a dark gap before the next.
fn pulse_head(clock: f32, seed: u32) -> f32 {
    let (fast, slow) = PULSE_SECONDS;
    let cycle = fast + (slow - fast) * ((seed >> 7) % 1000) as f32 / 1000.0;
    let phase = ((clock / cycle) + (seed % 1000) as f32 / 1000.0).fract();
    // 0..1 of the cycle maps past both ends, so for most of it the hypha is dark
    phase * 2.6 - 0.8
}

#[allow(clippy::too_many_arguments)]
fn draw_edges(
    mut gizmos: Gizmos,
    mut skeleton: Gizmos<Skeleton>,
    mut lit_gizmos: Gizmos<LitHyphae>,
    edges: Query<(&GraphEdge, Option<&GrowIn>)>,
    nodes: Query<(&Pos, Option<&Drift>, Has<Hidden>), With<GraphNode>>,
    camera: Query<(&Transform, &Projection), With<MainCamera>>,
    graph: Res<GraphState>,
    filters: Res<crate::filters::Filters>,
    spot: Res<Spotlight>,
    vibe: Res<Vibe>,
) {
    let Ok((cam, Projection::Orthographic(ortho))) = camera.single() else {
        return;
    };
    let eye = cam.translation.truncate();
    let half = ortho.area.size() * 0.5 * 1.1;
    let (view_min, view_max) = (eye - half, eye + half);
    // World units per screen pixel: what keeps the strands' width and their
    // sway the same size on screen however far the graph is zoomed out.
    let scale_px = ortho.scale.max(1e-3);
    // Thousands of strands at full width are a wall of ink from far enough
    // back, so the whole mesh is drawn finer the further out the view goes.
    let fainter = 1.0 - (1.0 - FAINTEST) * thinning(scale_px);
    let palette = &vibe.palette;
    let night = palette.night;
    // After dark the whole mesh dims and lifts together with the breath, and
    // each hypha carries a slow pulse of light along itself.
    let swell = 1.0 + 0.10 * night * vibe.breath();
    let pulsing = night > 0.04;

    // Focus and context: with a node in hand (or a kind pointed at) its
    // hyphae stay lit and the rest of the mesh drops to a whisper, so a single
    // thread can be followed across a crowd instead of vanishing into it.

    for (e, grow) in &edges {
        if !filters.edge_visible(&e.kind) {
            continue;
        }
        let lit = spot.lights(e);
        let structural = e.kind == "CONTAINS";
        // A relation is shown by where its node floats, not by a strand. Only
        // with one end in hand does a faint thread say exactly what to.
        //
        // That covers two cases. Kinds that are never strands, and any edge
        // hanging off a node that floats: a plan naming twenty-five files by
        // name would otherwise fan twenty-five lines across the whole tree, the
        // same hairball a commit makes, just in a different colour.
        let relation = !EdgeKind::is_strand(&e.kind)
            || graph.floats_entity(e.src)
            || graph.floats_entity(e.dst);
        if relation && !lit {
            continue;
        }
        // Hairball control. A link between two nodes that are each already
        // tangled in dozens says very little on its own and costs a stroke
        // through the middle of everything, so it waits until one of its ends
        // is picked up. The folder tree is never cut: it is the skeleton.
        let crowd = graph
            .kind_degree(e.src, &e.kind)
            .min(graph.kind_degree(e.dst, &e.kind));
        if !lit && filters.decluttered(&e.kind, crowd) {
            continue;
        }
        let (Ok((a, da, ha)), Ok((b, db, hb))) = (nodes.get(e.src), nodes.get(e.dst)) else {
            continue;
        };
        if ha || hb {
            continue;
        }
        // The ends wander with the nodes, so a hypha stays rooted in both.
        let drift = |d: Option<&Drift>| d.map(|d| d.offset).unwrap_or(Vec2::ZERO);
        let (a, b) = (a.0 + drift(da), b.0 + drift(db));

        let len = (b - a).length();
        let pad = Vec2::splat(len * BOW + (SWAY_REACH * TREE_SWAY + WARBLE_REACH) * scale_px);
        let (lo, hi) = (a.min(b) - pad, a.max(b) + pad);
        if hi.x < view_min.x || lo.x > view_max.x || hi.y < view_min.y || lo.y > view_max.y {
            continue;
        }

        let t = grow.map(|g| g.t).unwrap_or(1.0);
        let segments = ((len / scale_px / PIXELS_PER_SAMPLE).ceil() as usize).clamp(2, 20);
        let scale = if structural { TREE_SWAY } else { 1.0 };
        let (s1, s2) = sway(vibe.clock, e.seed, len, scale, scale_px);
        // The two ends of the shiver lean opposite ways, so it travels through
        // the strand rather than swinging the whole thing sideways.
        let w = warble(vibe.clock, e.seed, night, len, scale_px);
        let bend = (s1 + w, s2 - w * 0.8);
        let pts = hypha_points(a, b, e.seed, bend, t, segments);
        let base = palette.edge(&e.kind);
        let alpha = match (lit, spot.on(), structural) {
            (true, _, _) if relation => 0.45,
            (true, _, _) => 1.0,
            // with a node in hand the tree stays as a faint outline to find
            // your way by, and the other links all but vanish
            (false, true, true) => 0.12,
            (false, true, false) => GHOST,
            (false, false, true) => 0.55,
            (false, false, false) => 0.22,
        } * swell
            * if lit { 1.0 } else { fainter };
        let mut strokes = Vec::with_capacity(3);
        // The glow doubles a hypha's ink, so it is spent only on the thread
        // the user is actually looking at.
        if lit && !relation {
            let off = (b - a).perp().normalize_or_zero() * 1.5;
            let glow = base.with_alpha(0.3);
            strokes.push(Stroke::Flat(pts.iter().map(|p| *p + off).collect(), glow));
            strokes.push(Stroke::Flat(pts.iter().map(|p| *p - off).collect(), glow));
        }
        let head = pulse_head(vibe.clock, e.seed);
        // Most hyphae are between pulses at any moment; those cost nothing
        // extra and are drawn flat.
        if !pulsing || !(-PULSE_WIDTH * 3.0..1.0 + PULSE_WIDTH * 3.0).contains(&head) {
            strokes.push(Stroke::Flat(pts, base.with_alpha(alpha.min(1.0))));
        } else {
            let last = (pts.len() - 1).max(1) as f32;
            strokes.push(Stroke::Graded(
                pts.iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let u = (i as f32 / last) * t;
                        let g = (-((u - head) / PULSE_WIDTH).powi(2)).exp() * night;
                        (
                            *p,
                            theme::mix(base, palette.accent, g * 0.55)
                                .with_alpha((alpha * (1.0 + g * 1.3)).min(1.0)),
                        )
                    })
                    .collect(),
            ));
        }
        if lit {
            paint(&mut lit_gizmos, strokes);
        } else if structural {
            paint(&mut skeleton, strokes);
        } else {
            paint(&mut gizmos, strokes);
        }
    }
}

/// How many motes drift across the canvas at the deepest point of the night.
/// They are kept small and faint on purpose: a mote the size of a node is not
/// atmosphere, it is a node the user cannot click.
const MOTES: usize = 30;

/// splitmix64's finaliser over an index and a salt, as a float in 0..1: a
/// mote's whole character, without keeping anything between frames.
fn rand01(i: usize, salt: u64) -> f32 {
    let mut h = (i as u64 + 1)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(salt.wrapping_mul(0xBF58_476D_1CE4_E5B9));
    h ^= h >> 29;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 32;
    (h >> 11) as f32 / (1u64 << 53) as f32
}

/// Spores in the air between the viewer and the mesh. They hang in front of
/// the lens rather than in the world, so panning the graph does not shake
/// them, and there is nothing to keep track of between frames.
fn draw_spore_motes(
    mut gizmos: Gizmos,
    camera: Query<(&Transform, &Projection), With<MainCamera>>,
    vibe: Res<Vibe>,
) {
    let night = vibe.palette.night;
    if night < 0.06 {
        return;
    }
    let Ok((cam, Projection::Orthographic(ortho))) = camera.single() else {
        return;
    };
    let size = ortho.area.size();
    let origin = cam.translation.truncate() - size * 0.5;
    let count = (MOTES as f32 * night) as usize;
    let t = vibe.clock;
    for i in 0..count {
        let (x0, y0, speed, size_seed) = (rand01(i, 1), rand01(i, 2), rand01(i, 3), rand01(i, 4));
        // A slow rise with a lazy sideways sway, wrapped in the view: a spore
        // that leaves the top comes back in at the bottom.
        let rise = (y0 + t * (0.004 + 0.010 * speed)).fract();
        let sway = (x0 + (t * 0.05 + y0 * std::f32::consts::TAU).sin() * 0.02).rem_euclid(1.0);
        let p = origin + size * Vec2::new(sway, rise);
        // Constant size on screen, whatever the zoom.
        let r = (0.5 + 1.0 * size_seed) * ortho.scale;
        let twinkle = 0.45 + 0.55 * ((t * 0.6 + size_seed * 20.0).sin() * 0.5 + 0.5);
        let alpha = night * 0.12 * twinkle;
        gizmos
            .circle_2d(p, r, vibe.palette.accent.with_alpha(alpha))
            .resolution(8);
    }
}

/// Fade every node that is not the focused one or one of its neighbours, and
/// light the rest by however much of the night there is. The canvas is black,
/// so darkening towards it reads as a fade without paying for transparency on
/// a thousand meshes.
fn node_colors(
    spot: Res<Spotlight>,
    vibe: Res<Vibe>,
    bodies: Query<(&ChildOf, &NodeBody, &MeshMaterial2d<ColorMaterial>)>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    mut last: Local<Option<(u64, u8)>>,
) {
    // The night moves too slowly to repaint a thousand materials every frame;
    // a step of it is a fine grain to notice.
    let step = quantized_night(&vibe);
    if *last == Some((spot.moves, step)) {
        return;
    }
    *last = Some((spot.moves, step));

    for (parent, body, mat) in &bodies {
        let lit = vibe.palette.bioluminesce(body.0);
        let faded = spot
            .keep
            .as_ref()
            .is_some_and(|k| !k.contains(&parent.parent()));
        let want = if faded { dimmed(lit) } else { lit };
        // Moving from one node to its neighbour leaves almost every other node
        // exactly as it was, so look before writing: an untouched material is
        // not re-uploaded.
        if materials.get(&mat.0).is_some_and(|m| m.color == want) {
            continue;
        }
        if let Some(mut m) = materials.get_mut(&mat.0) {
            m.color = want;
        }
    }
}

/// The night in 1/48ths: the grain at which the palette is worth repainting.
fn quantized_night(vibe: &Vibe) -> u8 {
    (vibe.palette.night * 48.0).round() as u8
}

/// Labels, icons and the selection ring follow the palette. Like the node
/// bodies, only when it has actually moved.
fn night_chrome(
    vibe: Res<Vibe>,
    mut icons: Query<&mut Sprite, With<NodeIcon>>,
    mut labels: Query<&mut TextColor, With<NodeLabel>>,
    cache: Res<MeshCache>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    mut last: Local<Option<u8>>,
) {
    let step = quantized_night(&vibe);
    if *last == Some(step) {
        return;
    }
    *last = Some(step);
    for mut sprite in &mut icons {
        sprite.color = vibe.palette.icon_ink;
    }
    for mut label in &mut labels {
        label.0 = vibe.palette.text;
    }
    if let Some(handle) = cache.materials.get("selection")
        && let Some(mut m) = materials.get_mut(handle)
    {
        m.color = vibe.palette.selection;
    }
}

/// The halo behind each node: dark and hidden by day, and after dark a glow
/// that swells and settles on the breath rippling out across the graph.
///
/// With one exception, and it is deliberate: a node something is **happening
/// to** is lit at noon as well. Activity is information, not atmosphere, and
/// the same exception is already made for the nodes that float rather than sit
/// in the tree. It costs a daylit workspace the per-frame pass this system
/// already runs every frame after dark, and only for as long as anything is
/// still warm.
fn breathe_halos(
    vibe: Res<Vibe>,
    positions: Query<&Pos>,
    warm: Query<&crate::live::Live>,
    mut halos: Query<(
        &ChildOf,
        &GlowHalo,
        &mut Sprite,
        &mut Transform,
        &mut Visibility,
    )>,
    mut lit: Local<bool>,
) {
    let night = vibe.palette.night;
    if night < 0.02 && warm.is_empty() {
        if !*lit {
            return;
        }
        *lit = false;
        for (_, _, _, _, mut visibility) in &mut halos {
            *visibility = Visibility::Hidden;
        }
        return;
    }
    *lit = true;
    for (parent, halo, mut sprite, mut transform, mut visibility) in &mut halos {
        // A halo is spawned dark, and nodes keep sprouting long after the
        // first frame, so this cannot be done once on the way into the night.
        if *visibility != Visibility::Inherited {
            *visibility = Visibility::Inherited;
        }
        let at = positions
            .get(parent.parent())
            .map(|p| p.0)
            .unwrap_or_default();
        let breath = vibe.breath_at(at);
        let heat = warm.get(parent.parent()).map(|l| l.0).unwrap_or(0.0);
        // Heat is added rather than blended in, so a live node after dark is
        // brighter than the graph around it and not merely differently lit.
        // It also swells: at noon there is no glow anywhere else to compare it
        // against, and a halo the size of the node it sits behind is not
        // something anyone catches out of the corner of an eye.
        let glow = night * (0.30 + 0.13 * breath) + heat * 0.70 * (0.78 + 0.22 * breath);
        sprite.color = vibe.palette.bioluminesce(halo.0).with_alpha(glow.min(0.95));
        transform.scale =
            Vec3::splat(1.0 + 0.07 * night * breath + 0.45 * heat * (0.8 + 0.2 * breath));
    }
}

/// Towards the black canvas, which on this background reads as a fade.
fn dimmed(c: Color) -> Color {
    let s = c.to_srgba();
    Color::srgba(s.red * 0.22, s.green * 0.22, s.blue * 0.22, s.alpha)
}

/// `Hidden` is the marker the filters put on a node; this is what stops it
/// being drawn — the disc, its icon, its label and its halo with it.
fn hidden_visibility(mut nodes: Query<(&mut Visibility, Has<Hidden>), With<GraphNode>>) {
    for (mut visibility, hidden) in &mut nodes {
        let want = if hidden {
            Visibility::Hidden
        } else {
            Visibility::Inherited
        };
        if *visibility != want {
            *visibility = want;
        }
    }
}

/// A label's height in world units, whatever the camera is doing.
const LABEL_PX: f32 = 11.0;

/// The largest a label is ever baked at. Past this the atlas costs more than
/// the sharpness is worth, and no zoom the camera allows gets there anyway.
const MAX_LABEL_PX: f32 = 96.0;

/// The font size to bake a label at for the `want` pixels it covers on screen.
/// Sizes climb in quarter-octave steps, so a pinch rebakes every label a
/// handful of times instead of once a frame, and always round up: a name drawn
/// smaller than it was baked stays sharp, one drawn larger is the blur this is
/// here to avoid.
fn label_bake_px(want: f32) -> f32 {
    let step = (want.max(LABEL_PX).log2() * 4.0).ceil() / 4.0;
    step.exp2().min(MAX_LABEL_PX)
}

fn label_visibility(
    camera: Query<&Projection, With<MainCamera>>,
    ws: Res<WorkspaceRes>,
    nodes: Query<(Entity, &Children, Has<Hidden>), With<GraphNode>>,
    mut labels: Query<(&mut Visibility, &mut Transform, &mut TextFont), With<NodeLabel>>,
    selection: Res<Selection>,
    hovered: Res<Hovered>,
    review: Res<crate::review::Review>,
    named: Res<crate::live::Named>,
    graph: Res<crate::graph::GraphState>,
) {
    let Ok(Projection::Orthographic(ortho)) = camera.single() else {
        return;
    };
    let show = ortho.scale < ws.config.gui.label_zoom_threshold;
    let selected = selection
        .primary
        .as_ref()
        .and_then(|id| graph.by_id.get(id))
        .copied();
    let hovered = hovered
        .0
        .as_ref()
        .and_then(|id| graph.by_id.get(id))
        .copied();
    // The files of a step being walked are named whatever the zoom: a step is
    // a handful of nodes the reader was just sent to, and sending someone to
    // anonymous dots is most of what made the plan view unreadable.
    let walked: EntityHashSet = review
        .step_files()
        .map(|files| {
            files
                .iter()
                .filter_map(|id| graph.by_id.get(id).copied())
                .collect()
        })
        .unwrap_or_default();
    // A label is drawn in the world, so it shrinks as the camera pulls back.
    // A step whose files sit at opposite ends of the repo has to be framed
    // from far away, and a name too small to read is the same as no name at
    // all — so the ones the walk forces are scaled back up to the size they
    // would have had at the threshold.
    let legible = (ortho.scale / ws.config.gui.label_zoom_threshold).max(1.0);
    for (entity, children, hidden) in &nodes {
        let walking = walked.contains(&entity);
        // A node something is happening to is named too, but *not* grown: its
        // name is read inside a portal, which frames it from close up, and the
        // up-scaling below would fill that little window with one word.
        let forced = selected == Some(entity)
            || hovered == Some(entity)
            || walking
            || named.0.contains(&entity);
        for c in children.iter() {
            if let Ok((mut v, mut t, mut font)) = labels.get_mut(c) {
                let want = !hidden && (show || forced);
                *v = if want {
                    Visibility::Inherited
                } else {
                    Visibility::Hidden
                };
                let grown = if walking { legible } else { 1.0 };
                // Glyphs are baked into an atlas once, at their font size, and
                // the camera only stretches that picture — so a name read from
                // up close has to be baked bigger. How big it lands on screen
                // is how big it is in the world over the zoom.
                let px = label_bake_px(LABEL_PX * grown / ortho.scale);
                let scale = LABEL_PX * grown / px;
                if t.scale.x != scale {
                    t.scale = Vec3::splat(scale);
                }
                if !matches!(font.font_size, FontSize::Px(p) if p == px) {
                    font.font_size = FontSize::Px(px);
                }
            }
        }
    }
}

fn selection_ring(
    mut commands: Commands,
    selection: Res<Selection>,
    graph: Res<crate::graph::GraphState>,
    nodes: Query<(&Pos, &GraphNode)>,
    ring: Query<Entity, With<SelectionRing>>,
    vibe: Res<Vibe>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    mut cache: ResMut<MeshCache>,
) {
    if !selection.is_changed() && !ring.is_empty() {
        // keep ring following the node
        return;
    }
    for r in &ring {
        commands.entity(r).despawn();
    }
    let Some(id) = &selection.primary else { return };
    let Some(&e) = graph.by_id.get(id) else {
        return;
    };
    let Ok((_, gn)) = nodes.get(e) else { return };
    let r = node_radius(&gn.kind) + 4.0;
    let mesh = meshes.add(Circle::new(r));
    let mat = cache
        .materials
        .entry("selection".into())
        .or_insert_with(|| materials.add(ColorMaterial::from_color(vibe.palette.selection)))
        .clone();
    commands.spawn((
        SelectionRing,
        Mesh2d(mesh),
        MeshMaterial2d(mat),
        Transform::from_xyz(0.0, 0.0, 0.5),
        ChildOf(e),
    ));
}

/// A node grows under the cursor, and after dark every node rises and falls a
/// little on the breath travelling out across the graph.
fn hover_scale(
    hovered: Res<Hovered>,
    graph: Res<crate::graph::GraphState>,
    vibe: Res<Vibe>,
    positions: Query<&Pos>,
    mut bodies: Query<(&ChildOf, &mut Transform), With<NodeBody>>,
) {
    let target = hovered
        .0
        .as_ref()
        .and_then(|id| graph.by_id.get(id))
        .copied();
    let swell = 0.035 * vibe.palette.night;
    for (parent, mut t) in &mut bodies {
        let want = if Some(parent.parent()) == target {
            1.25
        } else {
            1.0
        };
        let breath = if swell > 0.0005 {
            let at = positions
                .get(parent.parent())
                .map(|p| p.0)
                .unwrap_or_default();
            1.0 + swell * vibe.breath_at(at)
        } else {
            1.0
        };
        t.scale = Vec3::splat(want * breath);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_spotlight_lights_its_own_hyphae() {
        let mut world = World::new();
        let [a, b, c] = [(); 3].map(|_| world.spawn_empty().id());
        let edge = |kind: &str, src, dst| GraphEdge {
            kind: kind.into(),
            src,
            dst,
            seed: 0,
            props: serde_json::Value::Null,
        };
        let (ab, bc) = (edge("IMPORTS", a, b), edge("CONTAINS", b, c));

        let off = Spotlight::default();
        assert!(!off.on() && !off.lights(&ab));

        let in_hand = Spotlight {
            keep: Some([a, b].into_iter().collect()),
            hyphae: LitHyphaeRule::Touching(a),
            moves: 1,
        };
        assert!(in_hand.lights(&ab) && !in_hand.lights(&bc));

        let node_kind = Spotlight {
            keep: Some([b, c].into_iter().collect()),
            hyphae: LitHyphaeRule::Between,
            moves: 1,
        };
        assert!(node_kind.lights(&bc) && !node_kind.lights(&ab));

        let edge_kind = Spotlight {
            keep: Some([a, b].into_iter().collect()),
            hyphae: LitHyphaeRule::Kind("IMPORTS".into()),
            moves: 1,
        };
        assert!(edge_kind.lights(&ab) && !edge_kind.lights(&bc));
    }

    #[test]
    fn every_hypha_sways_gently_and_keeps_its_roots() {
        let (a, b) = (Vec2::ZERO, Vec2::new(120.0, 0.0));
        for seed in [0u32, 99, 123_456_789, u32::MAX] {
            for scale in [1.0, TREE_SWAY] {
                let short = sway(3.0, seed, 20.0, scale, 1.0);
                assert!(short.0.abs() <= 20.0 * SWAY_SHARE * scale + 1e-4);
                let moved = (0..40).any(|i| {
                    sway(i as f32 * 0.5, seed, 120.0, scale, 1.0)
                        != sway(0.0, seed, 120.0, scale, 1.0)
                });
                assert!(moved, "seed {seed} never sways");
                for i in 0..400 {
                    let s = sway(i as f32 * 0.1, seed, 120.0, scale, 1.0);
                    let most = SWAY_REACH * scale + 1e-4;
                    assert!(s.0.abs() <= most && s.1.abs() <= most);
                    let pts = hypha_points(a, b, seed, s, 1.0, 12);
                    assert_eq!(pts[0], a);
                    assert!(pts.last().unwrap().distance(b) < 1e-3);
                }
            }
        }
    }

    #[test]
    fn zooming_out_keeps_the_sway_the_same_size_on_screen() {
        // Four times as far out: four times the world units, the same pixels.
        let close = sway(1.7, 77, 4000.0, 1.0, 1.0);
        let far = sway(1.7, 77, 4000.0, 1.0, 4.0);
        assert!((far.0 - close.0 * 4.0).abs() < 1e-3);
        assert!((far.1 - close.1 * 4.0).abs() < 1e-3);
    }

    #[test]
    fn warbles_are_rare_after_dark_and_never_come_by_day() {
        for seed in [0u32, 5, 77_777, u32::MAX] {
            let (mut shivering, mut biggest) = (0, 0.0f32);
            let (steps, step) = (200_000, 0.02);
            for i in 0..steps {
                let clock = i as f32 * step;
                assert_eq!(warble(clock, seed, 0.0, 200.0, 1.0), 0.0, "daylight");
                let w = warble(clock, seed, 1.0, 200.0, 1.0);
                if w != 0.0 {
                    shivering += 1;
                }
                biggest = biggest.max(w.abs());
            }
            // it happens, it stays small, and it is over almost all of the time
            assert!(biggest > 0.5, "seed {seed} never warbles");
            assert!(biggest <= WARBLE_REACH + 1e-4, "{biggest}");
            let share = shivering as f32 / steps as f32;
            assert!(share < 0.01, "seed {seed} warbles {share} of the time");
        }
    }

    #[test]
    fn the_tree_moves_more_than_the_links() {
        let tree = sway(2.0, 4242, 300.0, TREE_SWAY, 1.0);
        let link = sway(2.0, 4242, 300.0, 1.0, 1.0);
        assert!(tree.0.abs() > link.0.abs() && tree.1.abs() > link.1.abs());
    }
}
