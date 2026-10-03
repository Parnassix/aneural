//! Apertures onto somewhere else in the graph.
//!
//! A portal is a **second view of the same world**, not a redrawing of it. A
//! camera of its own renders the nodes a signal is about into a small texture,
//! and that texture is shown on the canvas. Everything comes along for free and
//! correctly: the node colours, the rasterised icons, the hyphae, the night
//! palette, the halos, the breath. A hand-painted panel would need every one of
//! those reimplemented in egui and would still look like a diagram of the graph
//! rather than the graph.
//!
//! The camera never moves for you. That is the whole point of a portal: the
//! view you arranged stays arranged, and the thing that happened comes to you
//! instead. Clicking one is how you go there, and that is a decision, not a
//! reaction.
//!
//! Three costs are dealt with rather than hoped about:
//!
//! - **The cameras are pooled.** Both the textures and the cameras are made
//!   once at startup and switched with `Camera::is_active`. An inactive camera
//!   renders nothing, and nothing is allocated when something happens.
//! - **The zoom is capped, not fitted.** A tight fit around two distant nodes
//!   would magnify past what the labels were baked for. Capping it keeps names
//!   sharp and makes a portal read as "over there" rather than as a microscope;
//!   a neighbourhood too wide to fit is cropped, which is what a window does.
//! - **They stay inside the canvas.** The rect is clamped to
//!   [`CanvasRect`](crate::camera::CanvasRect) — the window minus the egui
//!   panels — so a portal can never cover the inspector or the filters.

use super::{Named, Signals};
use crate::camera::{CanvasRect, FrameRequest, MainCamera, fit_into};
use crate::circadian::Vibe;
use crate::graph::{GraphState, Pos};
use crate::picking::Selection;
use crate::theme;
use crate::workspace::{WorkspaceRes, node_radius};
use aneural_core::NodeId;
use bevy::camera::{RenderTarget, visibility::RenderLayers};
use bevy::prelude::*;
use bevy::render::render_resource::{
    Extent3d, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
};
use bevy_egui::{EguiContexts, EguiTextureHandle, EguiUserTextures, egui};

/// A portal, in logical points. Big enough for a name and three or four
/// neighbours, small enough that three of them do not become the interface.
const SIZE: egui::Vec2 = egui::vec2(236.0, 148.0);
/// How tall the caption under the image is: a headline and up to three
/// wrapped lines of detail. Three, because two truncated the one line that
/// makes a task worth reporting -- which session started it.
const CAPTION: f32 = 58.0;
/// Render-target pixels per logical point. 2 is Retina; going higher buys
/// nothing on a 236-point window and costs four times the fill.
const OVERSAMPLE: u32 = 2;
/// However many the config asks for, never more than this.
pub const MAX: usize = 4;
/// Seconds to open and to close.
const SWING: f32 = 0.28;
/// How many times a second a portal's *contents* are redrawn.
///
/// Each open portal is a whole extra render pass, and at the window's own rate
/// three of them measured at about eight percent of a core — for a view of a
/// few nodes that barely moves. A camera switched off keeps the last thing it
/// drew in its texture, so slowing this down costs nothing but smoothness
/// inside a 236-point window, and the frames are staggered so three portals
/// never redraw on the same one.
const REDRAW: f64 = 12.0;
/// The tightest a portal ever zooms. Labels are baked against the main
/// camera's zoom, so magnifying much past 1:1 is where they go soft.
const TIGHTEST: f32 = 1.15;

/// A pooled camera and the texture it draws into.
struct Slot {
    cam: Entity,
    tex: egui::TextureId,
}

/// One portal currently on screen.
struct Open {
    subject: NodeId,
    /// 0 closed, 1 fully open.
    t: f32,
    closing: bool,
    /// App time its camera last rendered, for the rate cap.
    drawn: f64,
}

#[derive(Resource, Default)]
pub struct Portals {
    slots: Vec<Slot>,
    /// Parallel to `slots`: what each is showing, if anything.
    open: Vec<Option<Open>>,
}

impl Portals {
    /// Which slot is already showing this subject, so that a signal being
    /// re-raised does not make its portal jump to a different place on screen.
    fn slot_of(&self, id: &NodeId) -> Option<usize> {
        self.open
            .iter()
            .position(|o| o.as_ref().is_some_and(|o| &o.subject == id && !o.closing))
    }
}

#[derive(Component)]
struct PortalCamera;

pub struct PortalPlugin;

impl Plugin for PortalPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Portals>()
            .add_systems(Startup, build)
            .add_systems(Update, aim)
            // After `ui::panels`, which is what writes `CanvasRect` — a portal
            // that placed itself before the panels were measured would spend
            // its first frame under one.
            .add_systems(
                bevy_egui::EguiPrimaryContextPass,
                draw.after(crate::ui::panels),
            );
    }
}

/// The render target for one portal: a plain RGBA texture the camera is
/// pointed at and egui reads back.
fn target(size: Extent3d) -> Image {
    let mut image = Image {
        texture_descriptor: TextureDescriptor {
            label: Some("aneural portal"),
            size,
            dimension: TextureDimension::D2,
            format: TextureFormat::Bgra8UnormSrgb,
            mip_level_count: 1,
            sample_count: 1,
            usage: TextureUsages::TEXTURE_BINDING
                | TextureUsages::COPY_DST
                | TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        },
        ..default()
    };
    image.resize(size);
    image
}

fn build(
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut textures: ResMut<EguiUserTextures>,
    ws: Res<WorkspaceRes>,
) {
    let want = match ws.config.gui.live {
        true => (ws.config.gui.portals as usize).min(MAX),
        false => 0,
    };
    let size = Extent3d {
        width: SIZE.x as u32 * OVERSAMPLE,
        height: SIZE.y as u32 * OVERSAMPLE,
        depth_or_array_layers: 1,
    };
    let mut portals = Portals::default();
    for i in 0..want {
        let image = images.add(target(size));
        let tex = textures.add_image(EguiTextureHandle::Strong(image.clone()));
        let cam = commands
            .spawn((
                PortalCamera,
                Camera2d,
                Camera {
                    // Its own target, so the order only has to be distinct.
                    order: -20 - i as isize,
                    // Transparent, so the portal's own background shows through
                    // the gaps and it reads as a window rather than a sticker.
                    clear_color: ClearColorConfig::Custom(Color::NONE),
                    is_active: false,
                    ..default()
                },
                RenderTarget::Image(image.into()),
                // Node bodies, both hypha groups, and the layer the spotlight
                // moves everything it is not lighting to — otherwise a portal
                // opened while focus mode is on would be full of holes.
                RenderLayers::from_layers(&[
                    0,
                    crate::camera::HYPHAE_LAYER,
                    crate::camera::FADED_LAYER,
                    crate::camera::LIT_HYPHAE_LAYER,
                ]),
            ))
            .id();
        portals.slots.push(Slot { cam, tex });
        portals.open.push(None);
    }
    commands.insert_resource(portals);
}

/// Decide what each portal is looking at, and point its camera there.
fn aim(
    mut portals: ResMut<Portals>,
    signals: Res<Signals>,
    graph: Res<GraphState>,
    positions: Query<&Pos>,
    mut cams: Query<(&mut Camera, &mut Transform, &mut Projection), With<PortalCamera>>,
    mut named: ResMut<Named>,
    ws: Res<WorkspaceRes>,
    time: Res<Time>,
) {
    if portals.slots.is_empty() {
        return;
    }
    let step = time.delta_secs() / SWING;
    let now = time.elapsed_secs_f64();

    // Who deserves one: the warmest signals that name a node we can place.
    let mut wanted: Vec<NodeId> = Vec::new();
    for s in signals.all() {
        if wanted.len() == portals.slots.len() {
            break;
        }
        if graph.by_id.contains_key(s.primary()) {
            wanted.push(s.primary().clone());
        }
    }

    // Close anything no longer wanted, and keep everything else where it is:
    // a portal that moved slots every time its signal was re-raised would
    // shuffle around the screen for as long as the thing kept happening.
    for slot in portals.open.iter_mut() {
        if let Some(open) = slot
            && !wanted.contains(&open.subject)
        {
            open.closing = true;
        }
    }
    for id in &wanted {
        if portals.slot_of(id).is_some() {
            continue;
        }
        let free = portals
            .open
            .iter()
            .position(|o| o.as_ref().is_none_or(|o| o.closing && o.t <= 0.0));
        if let Some(i) = free {
            // Staggered at birth and kept that way by a fixed period, so
            // three open portals never redraw on the same frame.
            portals.open[i] = Some(Open {
                subject: id.clone(),
                t: 0.0,
                closing: false,
                drawn: now - (1.0 / REDRAW) * (i as f64 / MAX as f64),
            });
        }
    }

    // Swing, then aim.
    let cap = (ws.config.gui.label_zoom_threshold * 0.9).min(TIGHTEST);
    let eyes: Vec<Entity> = portals.slots.iter().map(|s| s.cam).collect();
    let mut shown: Vec<Entity> = Vec::new();
    for (i, slot) in portals.open.iter_mut().enumerate() {
        let mut looking = None;
        let mut due = false;
        if let Some(open) = slot {
            open.t = match open.closing {
                true => (open.t - step).max(0.0),
                false => (open.t + step).min(1.0),
            };
            if open.closing && open.t <= 0.0 {
                *slot = None;
            } else {
                due = now - open.drawn >= 1.0 / REDRAW;
                if due {
                    open.drawn = now;
                }
                looking = Some(open.subject.clone());
            }
        }
        let Ok((mut cam, mut transform, mut projection)) = cams.get_mut(eyes[i]) else {
            continue;
        };
        // A camera with nothing to look at is switched off rather than
        // despawned: off, it renders nothing at all, and there is no asset or
        // entity churn when the next thing happens.
        let Some(subject) = looking else {
            if cam.is_active {
                cam.is_active = false;
            }
            continue;
        };

        // Every subject, not only the one the signal is filed under: a job an
        // agent started is about the place it runs *and* the session running
        // it, and framing one without the other shows an aperture onto a
        // repository with nothing in it -- most of what a directory is
        // connected by is `Contains`, which is deliberately not a portal edge.
        let about = signals
            .all()
            .iter()
            .find(|s| s.primary() == &subject)
            .map(|s| s.subjects.clone())
            .unwrap_or_else(|| vec![subject.clone()]);
        // Depth 1 over the kinds that mean "about": a directory's whole
        // listing is not context, it is a wall.
        let near = graph.neighborhood(&about, 1, &super::portal_edge);
        let entities: Vec<Entity> = near
            .iter()
            .filter_map(|id| graph.by_id.get(id).copied())
            .collect();
        let Some((center, scale)) = fit_into(
            entities
                .iter()
                .filter_map(|e| positions.get(*e).ok())
                .map(|p| p.0),
            Vec2::new(SIZE.x, SIZE.y),
        ) else {
            continue;
        };
        // Recorded whether or not the camera runs this frame: which labels are
        // forced must not flicker at the redraw rate.
        shown.extend(entities);
        if !due {
            // The texture still holds the last frame, so an idle camera costs
            // nothing and shows the same thing.
            if cam.is_active {
                cam.is_active = false;
            }
            continue;
        }
        cam.is_active = true;
        transform.translation = center.extend(transform.translation.z);
        if let Projection::Orthographic(ortho) = &mut *projection {
            ortho.scale = scale.min(cap);
        }
    }
    // Everything a portal is looking at is named, whatever the main camera's
    // zoom — a portal full of anonymous dots answers nothing.
    let next: bevy::ecs::entity::EntityHashSet = shown.into_iter().collect();
    if next != named.0 {
        named.0 = next;
    }
}

/// Paint the apertures, and take the two clicks they offer.
#[allow(clippy::too_many_arguments)]
fn draw(
    mut contexts: EguiContexts,
    portals: Res<Portals>,
    mut signals: ResMut<Signals>,
    graph: Res<GraphState>,
    canvas: Res<CanvasRect>,
    vibe: Res<Vibe>,
    time: Res<Time>,
    cam: Query<(&Camera, &GlobalTransform), With<MainCamera>>,
    positions: Query<&Pos>,
    mut selection: ResMut<Selection>,
    mut frame: ResMut<FrameRequest>,
) {
    if portals.slots.is_empty() {
        return;
    }
    let Ok(ctx) = contexts.ctx_mut() else { return };
    let room = egui::Rect::from_min_max(
        egui::pos2(canvas.min.x, canvas.min.y),
        egui::pos2(canvas.max.x, canvas.max.y),
    );
    if room.width() < SIZE.x + 24.0 || room.height() < SIZE.y + CAPTION + 24.0 {
        return;
    }
    let now = time.elapsed_secs_f64();
    let breath = vibe.breath();
    let mut taken: Vec<egui::Rect> = Vec::new();
    let mut dismissed: Vec<NodeId> = Vec::new();
    let mut go: Option<NodeId> = None;

    for (i, slot) in portals.open.iter().enumerate() {
        let Some(open) = slot else { continue };
        let Some(signal) = signals.all().iter().find(|s| s.primary() == &open.subject) else {
            continue;
        };
        let anchor = screen_of(&cam, &graph, &positions, &open.subject);
        let rect = place(anchor, &room, &taken);
        taken.push(rect);

        // Ease the way a node sprouts, so a portal arriving reads as part of
        // the same organism rather than as a dialog.
        let t = crate::graph::ease_out_back(open.t);
        let fade = open.t.clamp(0.0, 1.0);
        let body =
            egui::Rect::from_min_size(rect.center() - (rect.size() * t) / 2.0, rect.size() * t);

        let area = egui::Area::new(egui::Id::new(("aneural-portal", i)))
            .order(egui::Order::Middle)
            .fixed_pos(body.min);
        let response = area.show(ctx, |ui| {
            ui.set_min_size(body.size());
            let painter = ui.painter();
            let accent = vibe.palette.accent;
            // Bright enough to find against a black canvas, with the breath as
            // a pulse on top rather than as the whole of it: a portal that
            // faded towards invisible at the bottom of its breath was hard to
            // read for half of every cycle.
            let rim_a = fade * (0.82 + 0.18 * breath);

            // A soft bloom, drawn as a few rings rather than a shader: at this
            // size nobody can tell, and it costs no pipeline.
            for ring in 1..=3 {
                let grow = 3.0 * ring as f32;
                painter.rect_stroke(
                    body.expand(grow),
                    10.0 + grow,
                    egui::Stroke::new(
                        2.0,
                        theme::egui_color(accent).gamma_multiply(0.16 * rim_a / ring as f32),
                    ),
                    egui::StrokeKind::Outside,
                );
            }
            painter.rect_filled(
                body,
                10.0,
                theme::egui_color(vibe.palette.panel).gamma_multiply(0.96 * fade),
            );

            let view = egui::Rect::from_min_size(
                body.min + egui::vec2(1.0, 1.0),
                egui::vec2(body.width() - 2.0, (body.height() - CAPTION * t).max(1.0)),
            );
            if let Some(tex) = portals.slots.get(i).map(|s| s.tex) {
                painter.image(
                    tex,
                    view,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE.gamma_multiply(fade),
                );
            }
            painter.rect_stroke(
                body,
                10.0,
                egui::Stroke::new(1.3, theme::egui_color(accent).gamma_multiply(rim_a)),
                egui::StrokeKind::Inside,
            );

            if t > 0.9 {
                let room = body.width() - 20.0;
                let head = fit_text(
                    painter,
                    &signal.headline,
                    egui::FontId::proportional(11.5),
                    theme::egui_color(vibe.palette.text).gamma_multiply(fade),
                    room,
                    1,
                );
                let tail = fit_text(
                    painter,
                    &signal.detail,
                    egui::FontId::monospace(9.5),
                    theme::egui_color(vibe.palette.dim).gamma_multiply(fade),
                    room,
                    3,
                );
                let at = egui::pos2(body.left() + 10.0, view.bottom() + 6.0);
                let below = at + egui::vec2(0.0, head.rect.height() + 2.0);
                painter.galley(at, head, egui::Color32::WHITE);
                painter.galley(below, tail, egui::Color32::WHITE);
            }

            // The image is the way there; the corner is the way out.
            let shut = egui::Rect::from_center_size(
                egui::pos2(body.right() - 11.0, body.top() + 11.0),
                egui::Vec2::splat(16.0),
            );
            let on_shut = ui.allocate_rect(shut, egui::Sense::click());
            if on_shut.hovered() {
                ui.painter().circle_filled(
                    shut.center(),
                    8.0,
                    theme::egui_color(vibe.palette.hover),
                );
            }
            let ink = theme::egui_color(if on_shut.hovered() {
                vibe.palette.text
            } else {
                vibe.palette.dim
            });
            for (a, b) in [((-3.5, -3.5), (3.5, 3.5)), ((3.5, -3.5), (-3.5, 3.5))] {
                ui.painter().line_segment(
                    [
                        shut.center() + egui::vec2(a.0, a.1),
                        shut.center() + egui::vec2(b.0, b.1),
                    ],
                    egui::Stroke::new(1.2, ink),
                );
            }
            let on_body = ui.allocate_rect(view, egui::Sense::click());
            (on_shut.clicked(), on_body.clicked())
        });
        let (shut, went) = response.inner;
        if shut {
            dismissed.push(open.subject.clone());
        } else if went {
            go = Some(open.subject.clone());
        }
    }

    for id in dismissed {
        signals.dismiss(&id, now);
    }
    if let Some(id) = go {
        // Selecting *and* framing: a portal answers "what happened", and the
        // only useful next question is "show me where".
        selection.primary = Some(id.clone());
        let about = signals
            .all()
            .iter()
            .find(|s| s.primary() == &id)
            .map(|s| s.subjects.clone())
            .unwrap_or_else(|| vec![id.clone()]);
        let near = graph.neighborhood(&about, 1, &super::portal_edge);
        let wanted: bevy::ecs::entity::EntityHashSet = near
            .iter()
            .filter_map(|n| graph.by_id.get(n).copied())
            .collect();
        if !wanted.is_empty() {
            frame.these(wanted);
        }
    }
}

/// Lay text out to fit the portal, cut rather than spilling.
///
/// egui's own overflow marker is an ellipsis, which the font here has no glyph
/// for — the same reason the canvas draws `...` — so it is set explicitly.
fn fit_text(
    painter: &egui::Painter,
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
    width: f32,
    rows: usize,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple(text.to_owned(), font, color, width);
    job.wrap = egui::text::TextWrapping {
        max_width: width,
        max_rows: rows,
        break_anywhere: rows == 1,
        overflow_character: Some('.'),
    };
    painter.layout_job(job)
}

/// Where this node is on screen, if the main camera can say.
fn screen_of(
    cam: &Query<(&Camera, &GlobalTransform), With<MainCamera>>,
    graph: &GraphState,
    positions: &Query<&Pos>,
    id: &NodeId,
) -> Option<egui::Pos2> {
    let (camera, transform) = cam.single().ok()?;
    let entity = graph.by_id.get(id)?;
    let at = positions.get(*entity).ok()?.0;
    let p = camera.world_to_viewport(transform, at.extend(0.0)).ok()?;
    Some(egui::pos2(p.x, p.y))
}

/// Put the portal near its subject, wholly inside the canvas, and clear of the
/// ones already placed.
///
/// Pure so the awkward cases — a subject off the side of the screen, a subject
/// with no position at all, four portals at once in a corner — are a test
/// rather than something to drag a window around and squint at.
fn place(anchor: Option<egui::Pos2>, room: &egui::Rect, taken: &[egui::Rect]) -> egui::Rect {
    const GAP: f32 = 12.0;
    let size = egui::vec2(SIZE.x, SIZE.y + CAPTION);
    // No anchor means the node is not on screen: the portal is then the only
    // sight of it there is, so it goes in a corner and stays out of the way.
    let Some(at) = anchor else {
        return clamp_into(
            egui::Rect::from_min_size(
                egui::pos2(room.right() - size.x - GAP, room.top() + GAP),
                size,
            ),
            room,
            GAP,
        );
    };
    // Around the node rather than only above it. Up and to the right first,
    // because that is where a reader's eye already is after a click, then the
    // other three corners, then further out. Without the sideways options a
    // second portal near the bottom of the canvas had nowhere to go but on top
    // of the first.
    let r = node_radius("File") + GAP;
    let away = size.x + GAP;
    let down = size.y + GAP;
    let corners: [egui::Vec2; 8] = [
        egui::vec2(r, -size.y - r),
        egui::vec2(-size.x - r, -size.y - r),
        egui::vec2(r, r),
        egui::vec2(-size.x - r, r),
        egui::vec2(r + away, -size.y - r),
        egui::vec2(-size.x - r - away, -size.y - r),
        egui::vec2(r, -size.y - r - down),
        egui::vec2(r, r + down),
    ];
    let mut first = None;
    for off in corners {
        let rect = clamp_into(egui::Rect::from_min_size(at + off, size), room, GAP);
        first.get_or_insert(rect);
        if !taken.iter().any(|t| t.expand(GAP / 2.0).intersects(rect)) {
            return rect;
        }
    }
    // Nowhere clear. Overlapping is better than hanging off the edge, and the
    // cap on how many are open at once keeps this rare.
    first.unwrap_or_else(|| clamp_into(egui::Rect::from_min_size(at, size), room, GAP))
}

fn clamp_into(rect: egui::Rect, room: &egui::Rect, gap: f32) -> egui::Rect {
    let room = room.shrink(gap);
    let x = rect
        .min
        .x
        .clamp(room.min.x, (room.max.x - rect.width()).max(room.min.x));
    let y = rect
        .min
        .y
        .clamp(room.min.y, (room.max.y - rect.height()).max(room.min.y));
    egui::Rect::from_min_size(egui::pos2(x, y), rect.size())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room() -> egui::Rect {
        egui::Rect::from_min_max(egui::pos2(220.0, 40.0), egui::pos2(1400.0, 860.0))
    }

    #[test]
    fn a_portal_never_leaves_the_canvas() {
        let r = room();
        for at in [
            egui::pos2(0.0, 0.0),
            egui::pos2(5000.0, 5000.0),
            egui::pos2(-5000.0, 400.0),
            egui::pos2(230.0, 50.0),
            egui::pos2(1399.0, 859.0),
        ] {
            let got = place(Some(at), &r, &[]);
            assert!(
                r.contains_rect(got),
                "anchor {at:?} put the portal at {got:?}, outside {r:?}"
            );
        }
    }

    #[test]
    fn a_subject_off_screen_still_gets_a_portal_out_of_the_way() {
        let r = room();
        let got = place(None, &r, &[]);
        assert!(r.contains_rect(got));
        assert!(got.right() > r.center().x, "expected it on the right");
    }

    #[test]
    fn portals_do_not_land_on_top_of_each_other() {
        let r = room();
        let a = place(Some(egui::pos2(700.0, 500.0)), &r, &[]);
        let b = place(Some(egui::pos2(705.0, 505.0)), &r, &[a]);
        assert!(!a.intersects(b), "{a:?} overlaps {b:?}");
        assert!(r.contains_rect(b));
    }

    #[test]
    fn a_canvas_only_just_big_enough_still_holds_one_whole() {
        // `draw` refuses a canvas smaller than this, so the smallest one it
        // will ever hand over has to work.
        let r = egui::Rect::from_min_max(
            egui::pos2(0.0, 0.0),
            egui::pos2(SIZE.x + 24.0, SIZE.y + CAPTION + 24.0),
        );
        let got = place(Some(egui::pos2(10.0, 10.0)), &r, &[]);
        assert!(r.contains_rect(got), "{got:?} does not fit {r:?}");
    }

    #[test]
    fn a_fourth_portal_with_nowhere_to_go_stays_on_the_canvas() {
        // Pushing down runs out of room before it runs out of portals, and the
        // fallback is to overlap rather than to hang off the edge: half a
        // portal off screen is worse than two touching.
        let r = room();
        let mut taken = Vec::new();
        for _ in 0..MAX + 2 {
            let next = place(Some(egui::pos2(700.0, 120.0)), &r, &taken);
            assert!(r.contains_rect(next), "{next:?} left {r:?}");
            taken.push(next);
        }
    }
}
