//! What is happening on this machine right now.
//!
//! The graph already knows a great deal about work in progress — it is told
//! within a second that a plan was approved, that a commit landed, that a
//! session touched four more files — and it says nothing, because a fact
//! arriving as one more dot among three thousand is not a fact anybody reads.
//! This module is the reading.
//!
//! Everything here is derived from metadata that already exists: the delta
//! stream the app is already draining ([`read_deltas`]), and a small sample of
//! the machine taken off the main thread ([`probe`]). Nothing is executed,
//! nothing is asked of the network, and no text is generated — every line a
//! portal shows is assembled from a prop or a process, so it can be wrong about
//! the world only by being out of date, never by being invented.
//!
//! The unit is a [`Signal`]: one thing that happened, the nodes it happened to,
//! two lines about it, and a single decaying number. That number drives the
//! node's glow, the portal's rim and the portal's life, so there is no state
//! machine to get stuck in and a dropped frame cannot strand a portal open.

pub mod portal;
pub mod probe;

use crate::graph::{GraphNode, GraphState, Stir};
use crate::workspace::WorkspaceRes;
use aneural_core::NodeId;
use aneural_core::kinds::{EdgeKind, NodeKind};
use bevy::ecs::entity::EntityHashSet;
use bevy::prelude::*;

/// How long a signal stays warm once whatever raised it has stopped raising it.
/// Long enough to look up from what you were doing, short enough that the
/// canvas is not a wall of history.
const LIFE: f32 = 40.0;

/// Files changing together in one batch before the batch itself is the news.
/// Below this, individual files changing is just typing.
const CHURN: usize = 6;

/// How long a dismissed subject stays dismissed. A signal you closed coming
/// straight back is the single fastest way to make this feature hated.
const SNOOZE: f64 = 300.0;

/// The most signals kept at once. Anything colder than the last of these is
/// dropped rather than queued: this is a view of now, not an inbox.
const KEEP: usize = 12;

/// What happened. The tag is what a portal writes before the name, so it is
/// short, upper case and ASCII — the canvas font has no glyph for anything
/// prettier, and two separate attempts to be prettier than this have already
/// shipped tofu boxes in this codebase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum What {
    /// A session has entered plan mode: it is thinking, not yet writing.
    Planning,
    /// A plan was approved and now exists.
    Planned,
    /// A session is working — busy, or its transcript just grew.
    Working,
    Committed,
    /// A merge or rebase is in flight in one of the repositories.
    Merging,
    /// A process this workspace knows about is alive now.
    Running,
    /// And has stopped.
    Finished,
    /// A great many files changed at once.
    Churn,
}

impl What {
    pub fn tag(self) -> &'static str {
        match self {
            What::Planning => "PLANNING",
            What::Planned => "PLAN",
            What::Working => "SESSION",
            What::Committed => "COMMIT",
            What::Merging => "MERGE",
            What::Running => "RUNNING",
            What::Finished => "FINISHED",
            What::Churn => "CHANGED",
        }
    }
}

/// Something that just happened, and how warm it still is.
#[derive(Clone, Debug)]
pub struct Signal {
    pub what: What,
    /// The nodes it is about, most important first.
    ///
    /// More than one so that a portal can frame a script *and* the session
    /// that started it in the same aperture — which is the whole reason the
    /// process sampler bothers walking the parent chain.
    pub subjects: Vec<NodeId>,
    /// `PLAN dreamy-soaring-gadget`.
    pub headline: String,
    /// `running 20m, pid 78425, started by plan-visualization`.
    pub detail: String,
    pub born: f64,
    /// 1.0 when raised, 0 when forgotten.
    pub heat: f32,
}

impl Signal {
    pub fn new(what: What, subjects: Vec<NodeId>, headline: String, detail: String) -> Self {
        Signal {
            what,
            subjects,
            headline,
            detail,
            born: 0.0,
            heat: 1.0,
        }
    }

    /// The node a portal is placed against and a click selects.
    pub fn primary(&self) -> &NodeId {
        &self.subjects[0]
    }
}

/// Everything warm, hottest first.
#[derive(Resource, Default)]
pub struct Signals {
    live: Vec<Signal>,
    /// Subject → when it stops being snoozed.
    snoozed: Vec<(NodeId, f64)>,
}

impl Signals {
    /// Raise one, or warm the one already standing for the same subject.
    ///
    /// Re-raising rather than stacking is what makes a process that is still
    /// running cost one portal instead of one per sample, and what stops a
    /// transcript appended to every second from producing sixty portals a
    /// minute. It also means a signal only ever needs to be *repeated* to stay
    /// alive, so nothing has to remember to turn it off.
    pub fn raise(&mut self, now: f64, signal: Signal) {
        if signal.subjects.is_empty() || self.snoozing(signal.primary(), now) {
            return;
        }
        if let Some(open) = self
            .live
            .iter_mut()
            .find(|s| s.primary() == signal.primary())
        {
            open.what = signal.what;
            open.subjects = signal.subjects;
            open.headline = signal.headline;
            open.detail = signal.detail;
            open.heat = 1.0;
            // `born` is deliberately kept: it is when this subject started
            // being interesting, which is what "running 20m" is counted from.
            return;
        }
        self.live.push(Signal {
            born: now,
            ..signal
        });
        self.live.sort_by(|a, b| b.heat.total_cmp(&a.heat));
        self.live.truncate(KEEP);
    }

    pub fn all(&self) -> &[Signal] {
        &self.live
    }

    pub fn newest(&self) -> Option<&Signal> {
        self.live.iter().max_by(|a, b| a.born.total_cmp(&b.born))
    }

    /// Stop showing this subject, and stop offering to.
    pub fn dismiss(&mut self, id: &NodeId, now: f64) {
        self.live.retain(|s| s.primary() != id);
        self.snoozed.retain(|(other, _)| other != id);
        self.snoozed.push((id.clone(), now + SNOOZE));
    }

    fn snoozing(&self, id: &NodeId, now: f64) -> bool {
        self.snoozed
            .iter()
            .any(|(other, until)| other == id && *until > now)
    }
}

/// How warm this node is. Read by the renderer to light a halo by day, which
/// is the one place the circadian rule is broken on purpose: activity is
/// information, not atmosphere. The same exception already exists for nodes
/// that float rather than sit in the tree.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Live(pub f32);

/// Nodes whose labels are drawn whatever the zoom, because something is
/// happening to them and sending a reader to an anonymous dot is worse than
/// sending them nowhere.
#[derive(Resource, Default)]
pub struct Named(pub EntityHashSet);

pub struct LivePlugin;

impl Plugin for LivePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Signals>()
            .init_resource::<Named>()
            .add_plugins((probe::ProbePlugin, portal::PortalPlugin))
            .add_systems(Update, (cool, read_deltas, mark_live).chain());
    }
}

/// Everything cools. A signal nobody is re-raising is gone in [`LIFE`].
fn cool(mut signals: ResMut<Signals>, time: Res<Time>, ws: Res<WorkspaceRes>) {
    if !ws.config.gui.live {
        if !signals.live.is_empty() {
            signals.live.clear();
        }
        return;
    }
    let step = time.delta_secs() / LIFE;
    for s in &mut signals.live {
        s.heat -= step;
    }
    signals.live.retain(|s| s.heat > 0.0);
    let now = time.elapsed_secs_f64();
    signals.snoozed.retain(|(_, until)| *until > now);
}

/// Turn the last live delta into signals.
///
/// This is the half of the feature that reads nothing: the engine re-reads
/// `~/.claude` every second while a session is being written to and already
/// emits the `Plan`, `Session` and `Commit` nodes that result. All that was
/// missing was noticing.
fn read_deltas(
    mut signals: ResMut<Signals>,
    graph: Res<GraphState>,
    nodes: Query<&GraphNode>,
    time: Res<Time>,
    ws: Res<WorkspaceRes>,
) {
    if !ws.config.gui.live || graph.stirred.is_empty() {
        return;
    }
    let now = time.elapsed_secs_f64();
    let node = |id: &NodeId| graph.by_id.get(id).and_then(|e| nodes.get(*e).ok());
    let mut churned: Vec<NodeId> = Vec::new();

    for (id, stir) in &graph.stirred {
        let Some(n) = node(id) else { continue };
        let fresh = *stir == Stir::Appeared;
        match n.kind.as_str() {
            NodeKind::PLAN if fresh => {
                let reading = crate::review::read(&graph, id);
                let named = reading.named.len();
                let done = reading.named.intersection(&reading.touched).count();
                signals.raise(
                    now,
                    Signal::new(
                        What::Planned,
                        vec![id.clone()],
                        format!("{} {}", What::Planned.tag(), n.label),
                        match named {
                            0 => "no files named yet".to_string(),
                            _ => format!("{named} files named, {done} touched"),
                        },
                    ),
                );
            }
            NodeKind::SESSION => {
                // A session in plan mode is thinking rather than editing, and
                // that is the moment worth reacting to — by the time a plan
                // node exists the thinking is over.
                let planning = n.prop_str("mode") == Some("plan");
                let what = if planning {
                    What::Planning
                } else {
                    What::Working
                };
                signals.raise(
                    now,
                    Signal::new(
                        what,
                        vec![id.clone()],
                        format!("{} {}", what.tag(), n.label),
                        session_detail(n),
                    ),
                );
            }
            NodeKind::COMMIT if fresh => {
                signals.raise(
                    now,
                    Signal::new(
                        What::Committed,
                        vec![id.clone()],
                        format!("{} {}", What::Committed.tag(), n.label),
                        commit_detail(n),
                    ),
                );
            }
            NodeKind::RUN => {
                let status = n.prop_str("status").unwrap_or("unknown");
                let what = match status {
                    "running" => What::Running,
                    _ => What::Finished,
                };
                signals.raise(
                    now,
                    Signal::new(
                        what,
                        vec![id.clone()],
                        format!("{} {}", what.tag(), n.label),
                        run_detail(n),
                    ),
                );
            }
            NodeKind::FILE => churned.push(id.clone()),
            _ => {}
        }
    }

    if churned.len() >= CHURN
        && let Some(dir) = common_dir(&graph, &churned)
        && let Some(n) = node(&dir)
    {
        let count = churned.len();
        signals.raise(
            now,
            Signal::new(
                What::Churn,
                vec![dir],
                format!("{} {}", What::Churn.tag(), n.label),
                format!("{count} files at once"),
            ),
        );
    }
}

/// Keep the [`Live`] component and the forced-label set in step with the
/// signals. Both are pure functions of heat, so neither can drift out of date.
fn mark_live(
    mut commands: Commands,
    signals: Res<Signals>,
    graph: Res<GraphState>,
    mut named: ResMut<Named>,
    warm: Query<Entity, With<Live>>,
) {
    let mut wanted: Vec<(Entity, f32)> = Vec::new();
    for s in signals.all() {
        for id in &s.subjects {
            if let Some(&e) = graph.by_id.get(id) {
                wanted.push((e, s.heat));
            }
        }
    }
    for e in &warm {
        if !wanted.iter().any(|(w, _)| *w == e) {
            commands.entity(e).remove::<Live>();
        }
    }
    for (e, heat) in &wanted {
        commands.entity(*e).insert(Live(*heat));
    }
    let next: EntityHashSet = wanted.iter().map(|(e, _)| *e).collect();
    if next != named.0 {
        named.0 = next;
    }
}

// ---------------------------------------------------------------- the wording

fn session_detail(n: &GraphNode) -> String {
    let num = |key: &str| n.props.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    let files = num("filesTouched");
    let prompts = num("prompts");
    match (files, prompts) {
        (0, 0) => "just started".to_string(),
        (0, p) => format!("{} so far", plural(p, "prompt")),
        (f, 0) => format!("{} touched", plural(f, "file")),
        (f, p) => format!("{} touched over {}", plural(f, "file"), plural(p, "prompt")),
    }
}

fn commit_detail(n: &GraphNode) -> String {
    let files = n.props.get("files").and_then(|v| v.as_u64()).unwrap_or(0);
    match n.prop_str("author") {
        Some(who) if !who.is_empty() => format!("{}, {who}", plural(files, "file")),
        _ => plural(files, "file"),
    }
}

fn run_detail(n: &GraphNode) -> String {
    let mut parts = Vec::new();
    if let Some(status) = n.prop_str("status") {
        parts.push(status.to_string());
    }
    if let Some(ms) = n.props.get("durationMs").and_then(|v| v.as_i64())
        && ms > 0
    {
        parts.push(elapsed((ms / 1000) as u64));
    }
    if let Some(bytes) = n.props.get("outBytes").and_then(|v| v.as_i64())
        && bytes > 0
    {
        parts.push(format!("{} out", bytes_of(bytes as u64)));
    }
    if parts.is_empty() {
        "no detail recorded".to_string()
    } else {
        parts.join(", ")
    }
}

/// `1 file` / `4 files`. Written out because every alternative reads as a
/// placeholder: `1 file(s)` in a line meant to be read aloud is worse than
/// three lines of code.
pub fn plural(n: u64, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// `9s` / `4m` / `3m12s` / `2h07m`. Seconds are dropped past the hour: nobody
/// reads them, and they make a caption jitter every frame.
pub fn elapsed(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => {
            let (m, r) = (s / 60, s % 60);
            if r == 0 {
                format!("{m}m")
            } else {
                format!("{m}m{r:02}s")
            }
        }
        s => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// `912 B` / `2.1 MB`. Decimal, because a log's size is compared against what
/// a shell reported, not against a page table.
pub fn bytes_of(n: u64) -> String {
    const STEP: f64 = 1000.0;
    let units = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= STEP && i + 1 < units.len() {
        v /= STEP;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", units[i])
    }
}

/// The shallowest directory that holds all of these, if it is on the canvas.
///
/// Files changing together are usually siblings, and when they are not the
/// answer worth showing is the fork above them — not one arbitrary file of the
/// batch, and not the workspace root, which tells the reader nothing.
fn common_dir(graph: &GraphState, files: &[NodeId]) -> Option<NodeId> {
    let mut shared: Option<Vec<&str>> = None;
    for f in files {
        let path = f.path_part();
        let dir = path.rsplit_once('/').map(|(d, _)| d).unwrap_or(".");
        let parts: Vec<&str> = dir.split('/').collect();
        shared = Some(match shared {
            None => parts,
            Some(prev) => prev
                .iter()
                .zip(parts.iter())
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| *a)
                .collect(),
        });
    }
    let parts = shared?;
    let rel = if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    };
    let id = NodeId::dir(rel);
    graph.by_id.contains_key(&id).then_some(id)
}

/// Kinds a portal follows outward from its subject. `CONTAINS` is left out
/// deliberately: a directory's whole listing is not context, it is a wall.
pub fn portal_edge(kind: &str) -> bool {
    matches!(
        kind,
        EdgeKind::ANNOTATES | EdgeKind::MODIFIES | EdgeKind::REALIZES | EdgeKind::REFERENCES
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(what: What, id: &str) -> Signal {
        Signal::new(
            what,
            vec![NodeId::new(id)],
            "HEAD x".into(),
            "detail".into(),
        )
    }

    #[test]
    fn the_same_subject_twice_is_one_signal() {
        let mut s = Signals::default();
        s.raise(0.0, sig(What::Working, "session:a"));
        s.raise(1.0, sig(What::Working, "session:a"));
        s.raise(2.0, sig(What::Working, "session:b"));
        assert_eq!(s.all().len(), 2);
    }

    #[test]
    fn re_raising_keeps_when_it_started_and_resets_the_heat() {
        let mut s = Signals::default();
        s.raise(10.0, sig(What::Running, "script:a.py"));
        s.live[0].heat = 0.2;
        s.raise(99.0, sig(What::Running, "script:a.py"));
        assert_eq!(s.all()[0].born, 10.0, "elapsed is counted from the first");
        assert_eq!(s.all()[0].heat, 1.0);
    }

    #[test]
    fn a_dismissed_subject_does_not_come_straight_back() {
        let mut s = Signals::default();
        s.raise(0.0, sig(What::Planned, "plan:p"));
        s.dismiss(&NodeId::new("plan:p"), 0.0);
        s.raise(1.0, sig(What::Planned, "plan:p"));
        assert!(s.all().is_empty());
        // ...but it is not banished for the session.
        s.raise(SNOOZE + 2.0, sig(What::Planned, "plan:p"));
        assert_eq!(s.all().len(), 1);
    }

    #[test]
    fn a_signal_with_no_subject_is_not_raised() {
        let mut s = Signals::default();
        s.raise(
            0.0,
            Signal::new(What::Churn, vec![], "X".into(), String::new()),
        );
        assert!(s.all().is_empty());
    }

    #[test]
    fn nothing_written_into_a_caption_is_outside_ascii() {
        // The canvas font and the egui font both lack the typographic
        // characters a caption wants, and two attempts to use them anyway have
        // already shipped tofu boxes. Checked rather than remembered.
        let mut text = String::new();
        for what in [
            What::Planning,
            What::Planned,
            What::Working,
            What::Committed,
            What::Merging,
            What::Running,
            What::Finished,
            What::Churn,
        ] {
            text.push_str(what.tag());
        }
        for n in [0u64, 1, 2, 42, 1_000_000] {
            text.push_str(&plural(n, "file"));
            text.push_str(&elapsed(n));
            text.push_str(&bytes_of(n));
        }
        assert!(text.is_ascii(), "non-ascii in {text}");
    }

    #[test]
    fn durations_read_the_way_a_person_says_them() {
        assert_eq!(elapsed(9), "9s");
        assert_eq!(elapsed(60), "1m");
        assert_eq!(elapsed(192), "3m12s");
        assert_eq!(elapsed(7620), "2h07m");
    }

    #[test]
    fn sizes_are_decimal_and_bytes_stay_bytes() {
        assert_eq!(bytes_of(912), "912 B");
        assert_eq!(bytes_of(2_100_000), "2.1 MB");
    }

    #[test]
    fn plurals_are_written_out() {
        assert_eq!(plural(1, "file"), "1 file");
        assert_eq!(plural(0, "file"), "0 files");
        assert_eq!(plural(3, "prompt"), "3 prompts");
    }

    #[test]
    fn only_what_is_warmest_is_kept() {
        let mut s = Signals::default();
        for i in 0..KEEP + 5 {
            s.raise(i as f64, sig(What::Working, &format!("session:{i}")));
        }
        assert_eq!(s.all().len(), KEEP);
    }
}
