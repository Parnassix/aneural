//! Reading a plan against what became of it.
//!
//! The comparison the whole feature exists for is a difference of two edge
//! sets, and needs no bookkeeping of its own:
//!
//! - what the plan **named** — its `ANNOTATES` edges;
//! - what its sessions and commits **touched** — their `MODIFIES` edges.
//!
//! Named and touched is work that went as written. Named and untouched is what
//! was promised and skipped. Touched and unnamed is what happened anyway. A
//! ticket tracker would ask someone to keep those three lists by hand.

use crate::graph::GraphState;
use aneural_core::NodeId;
use aneural_core::kinds::EdgeKind;
use bevy::prelude::*;
use std::collections::BTreeSet;
use std::path::PathBuf;

/// The plan being read, and the text of it.
#[derive(Resource, Default)]
pub struct Review {
    /// The plan under review. `None` when the drawer is closed.
    pub plan: Option<NodeId>,
    /// Body text once it has been read, keyed by which plan it belongs to.
    pub body: Option<(NodeId, String)>,
    /// A read the panel asked for; performed by [`load`] on the next update,
    /// because the render pass never touches the filesystem.
    pub wanted: Option<(NodeId, PathBuf)>,
    /// Show only this plan's neighbourhood in the graph.
    pub only: bool,
    /// A file the reader clicked in the plan's text, to be selected.
    pub go_to: Option<NodeId>,
    /// Every touch the plan's work made, oldest first. Rebuilt by the drawer,
    /// which is the one place holding both the graph and the edges.
    pub touches: Vec<Touch>,
    /// Which step of the plan is being walked. `None` is the whole plan, which
    /// is what a reader who never touches the control gets.
    pub step: Option<usize>,
    /// The plan's steps, rebuilt by the drawer alongside the touches for the
    /// same reason: it is the one place holding both the node and the edges.
    pub walk: Vec<Walk>,
    /// Set when the step changed and the camera has not been told yet.
    pub frame: bool,
    /// How far along the timeline the reader has pulled it back to.
    /// `None` is the live end: new work extends the view rather than being
    /// hidden behind a slider nobody moved.
    pub at: Option<usize>,
}

/// A heading of a plan, as the engine recorded it on the node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heading {
    pub text: String,
    pub level: u8,
    pub line: u32,
}

impl Heading {
    /// Read the `steps` prop the `claude` producer writes. A plan indexed
    /// before this existed has no such prop and simply does not walk.
    pub fn all(prop: Option<&serde_json::Value>) -> Vec<Heading> {
        prop.and_then(|v| v.as_array())
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| {
                        Some(Heading {
                            text: r.get("heading")?.as_str()?.to_string(),
                            level: r.get("level").and_then(|v| v.as_u64()).unwrap_or(2) as u8,
                            line: r.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// One step of a plan, with what it promised and what became of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Walk {
    pub heading: String,
    pub level: u8,
    /// Line of the heading in the document, for slicing its prose out.
    pub line: u32,
    pub files: Vec<(Standing, NodeId)>,
}

impl Walk {
    /// How the step stands, from its files alone. A step that named nothing is
    /// waiting: there is no evidence either way, and claiming it is done
    /// because nothing contradicts that would be the tracker's lie.
    pub fn status(&self) -> &'static str {
        let touched = self
            .files
            .iter()
            .filter(|(s, _)| *s != Standing::Untouched)
            .count();
        match (touched, self.files.len()) {
            (0, _) => "waiting",
            (done, all) if done == all => "done",
            _ => "started",
        }
    }
}

/// One moment in a plan's execution: a file, when it was touched, and by what.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Touch {
    pub at: String,
    pub file: NodeId,
    pub actor: NodeId,
}

impl Review {
    /// Open `plan` for reading, and ask for its text if we do not have it.
    pub fn open(&mut self, plan: NodeId, file: Option<&str>) {
        if self.body.as_ref().is_none_or(|(id, _)| *id != plan)
            && let Some(file) = file
        {
            self.wanted = Some((plan.clone(), PathBuf::from(file)));
        }
        if self.plan.as_ref() != Some(&plan) {
            self.at = None;
            self.touches.clear();
            self.step = None;
            self.walk.clear();
        }
        self.plan = Some(plan);
    }

    pub fn close(&mut self) {
        self.plan = None;
        self.only = false;
        self.at = None;
        self.touches.clear();
        self.step = None;
        self.walk.clear();
    }

    /// Move to `step`, or to the whole plan with `None`, and ask the camera to
    /// go there. Out-of-range is ignored rather than clamped: a step that is
    /// not there is a bug in the caller, not a request to show a different one.
    pub fn go_to_step(&mut self, step: Option<usize>) {
        if step.is_some_and(|i| i >= self.walk.len()) {
            return;
        }
        self.step = step;
        self.frame = true;
    }

    /// The files of the step being walked, if one is.
    pub fn step_files(&self) -> Option<BTreeSet<NodeId>> {
        let step = self.walk.get(self.step?)?;
        Some(step.files.iter().map(|(_, id)| id.clone()).collect())
    }

    /// Everything to light up right now: the plan, its actors, the files it
    /// named, and — depending on where the timeline is parked — either every
    /// file touched or only those touched by that moment.
    pub fn lit(&self, reading: &Reading) -> BTreeSet<NodeId> {
        let Some(plan) = &self.plan else {
            return BTreeSet::new();
        };
        let mut out = reading.everything(plan);
        if let Some(files) = self.step_files() {
            // Walking one step: its own files, and the plan and actors that
            // hold it in place. A step with no files lights nothing but them,
            // which is the honest picture of a step nothing has happened to.
            out.retain(|id| !id.is_file() || files.contains(id));
            return out;
        }
        if let Some(by_now) = self.touched_by_now() {
            // Rewound: files the work had not reached yet step out of the
            // light, but what the plan *named* stays — that is the promise the
            // reader is watching being kept.
            out.retain(|id| !id.is_file() || reading.named.contains(id) || by_now.contains(id));
        }
        out
    }

    /// Files touched by the moment the timeline is parked at. `None` means
    /// the whole of it, which is also what an untouched slider means.
    pub fn touched_by_now(&self) -> Option<BTreeSet<NodeId>> {
        let at = self.at?;
        Some(
            self.touches
                .iter()
                .take(at + 1)
                .map(|s| s.file.clone())
                .collect(),
        )
    }

    /// Where the timeline stands, as a moment to show beside the slider.
    pub fn moment(&self) -> Option<&str> {
        self.touches.get(self.at?).map(|s| s.at.as_str())
    }

    /// The text of the plan currently open, if it has been read.
    pub fn text(&self) -> Option<&str> {
        let plan = self.plan.as_ref()?;
        self.body
            .as_ref()
            .filter(|(id, _)| id == plan)
            .map(|(_, text)| text.as_str())
    }
}

/// How a file stands in relation to the plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Standing {
    /// The plan named it and the work touched it.
    Done,
    /// The plan named it and nothing has touched it.
    Untouched,
    /// The work touched it and the plan never mentioned it.
    Unplanned,
}

impl Standing {
    pub fn label(self) -> &'static str {
        match self {
            Standing::Done => "named and touched",
            Standing::Untouched => "named, not touched",
            Standing::Unplanned => "touched, not named",
        }
    }
}

/// A plan read against the graph.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reading {
    pub named: BTreeSet<NodeId>,
    pub touched: BTreeSet<NodeId>,
    /// The sessions that approved it, and the commits that carried it out.
    pub sessions: Vec<NodeId>,
    pub commits: Vec<NodeId>,
}

impl Reading {
    /// Every file involved, with how it stands. Named-and-untouched first:
    /// it is the part of a plan a reader most needs to be shown, and the part
    /// nothing else in the graph points at.
    pub fn files(&self) -> Vec<(Standing, NodeId)> {
        let mut out: Vec<(Standing, NodeId)> = Vec::new();
        for id in &self.named {
            out.push((
                match self.touched.contains(id) {
                    true => Standing::Done,
                    false => Standing::Untouched,
                },
                id.clone(),
            ));
        }
        for id in self.touched.difference(&self.named) {
            out.push((Standing::Unplanned, id.clone()));
        }
        out.sort_by(|a, b| {
            let rank = |s: Standing| match s {
                Standing::Untouched => 0,
                Standing::Done => 1,
                Standing::Unplanned => 2,
            };
            rank(a.0)
                .cmp(&rank(b.0))
                .then_with(|| a.1.path_part().cmp(b.1.path_part()))
        });
        out
    }

    /// Everything the plan reaches, for lighting it up in the graph.
    pub fn everything(&self, plan: &NodeId) -> BTreeSet<NodeId> {
        let mut out: BTreeSet<NodeId> = self.named.union(&self.touched).cloned().collect();
        out.extend(self.sessions.iter().cloned());
        out.extend(self.commits.iter().cloned());
        out.insert(plan.clone());
        out
    }

    pub fn is_empty(&self) -> bool {
        self.named.is_empty() && self.touched.is_empty()
    }

    /// The plan's own steps, joined to the files each one named.
    ///
    /// The headings come off the node and the membership off the edges, which
    /// is why a step that named no file is still a step: "prompts into session
    /// props" is real work and names nothing, and a walk that skipped it would
    /// be lying about the shape of the plan.
    ///
    /// A last step, **not in the plan**, collects what the work touched and the
    /// plan never mentioned. That is the part a ticket tracker cannot show at
    /// all, so it gets a stop of its own rather than a footnote.
    pub fn steps(&self, headings: &[Heading], of: &dyn Fn(&NodeId) -> Option<usize>) -> Vec<Walk> {
        let mut out: Vec<Walk> = headings
            .iter()
            .map(|h| Walk {
                heading: h.text.clone(),
                level: h.level,
                line: h.line,
                files: Vec::new(),
            })
            .collect();
        // Files the plan named, under the step that named them. A mention above
        // every heading belongs to the plan itself and is left for the "all"
        // view rather than forced into step one.
        for id in &self.named {
            let Some(i) = of(id).filter(|i| *i < out.len()) else {
                continue;
            };
            let standing = match self.touched.contains(id) {
                true => Standing::Done,
                false => Standing::Untouched,
            };
            out[i].files.push((standing, id.clone()));
        }
        let unplanned: Vec<(Standing, NodeId)> = self
            .touched
            .difference(&self.named)
            .map(|id| (Standing::Unplanned, id.clone()))
            .collect();
        if !unplanned.is_empty() {
            out.push(Walk {
                heading: "Not in the plan".into(),
                level: 2,
                line: u32::MAX,
                files: unplanned,
            });
        }
        for step in &mut out {
            step.files.sort_by(|a, b| {
                a.0.cmp(&b.0)
                    .then_with(|| a.1.path_part().cmp(b.1.path_part()))
            });
        }
        out
    }

    /// The file a path written in the plan's prose refers to.
    ///
    /// Matched against what the engine already resolved rather than resolved
    /// again: a plan writes `ui.rs:607` and `crates/aneural-gui/src/ui.rs` for
    /// the same file, and the `ANNOTATES` edges are the answer it settled on.
    /// An ambiguous tail matches nothing, because guessing which of two files
    /// a sentence meant is worse than leaving the words unlinked.
    pub fn resolve(&self, mentioned: &str) -> Option<NodeId> {
        let wanted = mentioned.trim_start_matches("./");
        let exact = NodeId::file(wanted);
        if self.named.contains(&exact) || self.touched.contains(&exact) {
            return Some(exact);
        }
        let tail = format!("/{wanted}");
        let mut hits = self
            .named
            .iter()
            .chain(self.touched.iter())
            .filter(|id| id.path_part().ends_with(&tail));
        match (hits.next(), hits.next()) {
            (Some(only), None) => Some(only.clone()),
            _ => None,
        }
    }
}

/// Read a plan against the graph.
pub fn read(graph: &GraphState, plan: &NodeId) -> Reading {
    let named: BTreeSet<NodeId> = graph
        .out_of(plan, EdgeKind::ANNOTATES)
        .into_iter()
        .filter(|id| id.is_file())
        .collect();

    let mut sessions = Vec::new();
    let mut commits = Vec::new();
    for actor in graph.pointing_at(plan, EdgeKind::REALIZES) {
        match actor.prefix() {
            "session" => sessions.push(actor),
            "commit" => commits.push(actor),
            _ => {}
        }
    }
    sessions.sort();
    commits.sort();

    let touched: BTreeSet<NodeId> = sessions
        .iter()
        .chain(commits.iter())
        .flat_map(|actor| graph.out_of(actor, EdgeKind::MODIFIES))
        .filter(|id| id.is_file())
        .collect();

    Reading {
        named,
        touched,
        sessions,
        commits,
    }
}

/// Every touch the plan's sessions and commits made, oldest first.
///
/// `props` yields an edge's own props, which is where the times live. A
/// session's `MODIFIES` carries `firstAt` — when it first had hands on the
/// file — and a commit's carries `at`. Touches with no time at all are dropped
/// rather than guessed at: a timeline with invented positions on it is worse
/// than a short one.
pub fn timeline(
    graph: &GraphState,
    reading: &Reading,
    props: &dyn Fn(&NodeId, &NodeId) -> Option<serde_json::Value>,
) -> Vec<Touch> {
    let mut out = Vec::new();
    for actor in reading.sessions.iter().chain(reading.commits.iter()) {
        for file in graph.out_of(actor, EdgeKind::MODIFIES) {
            if !file.is_file() {
                continue;
            }
            let Some(p) = props(actor, &file) else {
                continue;
            };
            let at = p
                .get("firstAt")
                .or_else(|| p.get("at"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if at.is_empty() {
                continue;
            }
            out.push(Touch {
                at,
                file,
                actor: actor.clone(),
            });
        }
    }
    // RFC 3339 sorts correctly as text, which is why it is what gets stored.
    out.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.file.cmp(&b.file)));
    out.dedup_by(|a, b| a.file == b.file && a.actor == b.actor);
    out
}

/// Take the camera to the step just opened.
///
/// A step is a handful of files somewhere in a workspace of hundreds, so
/// without this the reader is told where to look and left to find it. Framing a
/// step also ends auto-follow: the view belongs to whoever is reading now, and
/// a later index batch must not drag it back to the whole graph.
pub fn frame_step(
    mut review: ResMut<Review>,
    graph: Res<GraphState>,
    mut frame: ResMut<crate::camera::FrameRequest>,
    mut follow: ResMut<crate::camera::AutoFollow>,
) {
    if !std::mem::take(&mut review.frame) {
        return;
    }
    let Some(files) = review.step_files() else {
        return;
    };
    let wanted: bevy::ecs::entity::EntityHashSet = files
        .iter()
        .chain(review.plan.iter())
        .filter_map(|id| graph.by_id.get(id).copied())
        .collect();
    if wanted.is_empty() {
        return;
    }
    frame.these(wanted);
    *follow = crate::camera::AutoFollow {
        on: false,
        released: true,
    };
}

/// Read the plan the panel asked for. An `Update` system: the render pass must
/// never do IO, which is the rule the marketplace set.
pub fn load(mut review: ResMut<Review>) {
    let Some((plan, path)) = review.wanted.take() else {
        return;
    };
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| format!("Could not read {}.\n\n{e}", path.display()));
    review.body = Some((plan, text));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(named: &[&str], touched: &[&str]) -> Reading {
        Reading {
            named: named.iter().map(NodeId::file).collect(),
            touched: touched.iter().map(NodeId::file).collect(),
            sessions: vec![NodeId::session("s")],
            commits: vec![],
        }
    }

    #[test]
    fn the_three_way_split_is_a_difference_of_two_sets() {
        let r = reading(&["a.ts", "b.ts"], &["b.ts", "c.ts"]);
        let files = r.files();
        assert_eq!(
            files,
            vec![
                (Standing::Untouched, NodeId::file("a.ts")),
                (Standing::Done, NodeId::file("b.ts")),
                (Standing::Unplanned, NodeId::file("c.ts")),
            ],
            "promised-and-skipped first: it is what a reader needs shown"
        );
    }

    #[test]
    fn a_plan_nothing_came_of_is_all_promises() {
        let r = reading(&["a.ts"], &[]);
        assert_eq!(r.files(), vec![(Standing::Untouched, NodeId::file("a.ts"))]);
        assert!(!r.is_empty());
    }

    #[test]
    fn everything_it_reaches_includes_the_plan_and_its_actors() {
        let r = reading(&["a.ts"], &["b.ts"]);
        let plan = NodeId::home_plan("p.md");
        let all = r.everything(&plan);
        assert!(all.contains(&plan));
        assert!(all.contains(&NodeId::session("s")));
        assert!(all.contains(&NodeId::file("a.ts")));
        assert!(all.contains(&NodeId::file("b.ts")));
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn opening_a_plan_asks_for_its_text_once() {
        let mut review = Review::default();
        let plan = NodeId::home_plan("p.md");
        review.open(plan.clone(), Some("/plans/p.md"));
        assert_eq!(review.wanted.as_ref().unwrap().0, plan);

        review.wanted = None;
        review.body = Some((plan.clone(), "# Read".into()));
        review.open(plan.clone(), Some("/plans/p.md"));
        assert!(
            review.wanted.is_none(),
            "already read, so not asked for again"
        );
        assert_eq!(review.text(), Some("# Read"));

        // A different plan is a different read.
        let other = NodeId::home_plan("q.md");
        review.open(other.clone(), Some("/plans/q.md"));
        assert_eq!(review.wanted.as_ref().unwrap().0, other);
        assert_eq!(review.text(), None, "and its text is not the old one");
    }

    #[test]
    fn a_path_written_in_prose_finds_the_file_the_engine_resolved() {
        let r = reading(&["crates/aneural-gui/src/ui.rs"], &["src/b.ts"]);
        assert_eq!(
            r.resolve("ui.rs"),
            Some(NodeId::file("crates/aneural-gui/src/ui.rs")),
            "a bare name, as a plan writes it"
        );
        assert_eq!(
            r.resolve("crates/aneural-gui/src/ui.rs"),
            Some(NodeId::file("crates/aneural-gui/src/ui.rs"))
        );
        assert_eq!(r.resolve("./src/b.ts"), Some(NodeId::file("src/b.ts")));
        assert_eq!(r.resolve("nothing.rs"), None);
    }

    #[test]
    fn an_ambiguous_name_is_left_unlinked_rather_than_guessed() {
        let r = reading(&["one/mod.rs", "two/mod.rs"], &[]);
        assert_eq!(r.resolve("mod.rs"), None);
        assert_eq!(r.resolve("one/mod.rs"), Some(NodeId::file("one/mod.rs")));
    }

    fn heads(names: &[(&str, u32)]) -> Vec<Heading> {
        names
            .iter()
            .map(|(text, line)| Heading {
                text: (*text).into(),
                level: 2,
                line: *line,
            })
            .collect()
    }

    #[test]
    fn a_walk_is_the_plans_own_headings_with_what_each_one_named() {
        let r = reading(&["a.ts", "b.ts"], &["b.ts"]);
        let steps = r.steps(&heads(&[("One", 1), ("Two", 9)]), &|id| {
            (id.path_part() == "a.ts").then_some(0).or(Some(1))
        });
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].heading, "One");
        assert_eq!(
            steps[0].files,
            vec![(Standing::Untouched, NodeId::file("a.ts"))]
        );
        assert_eq!(steps[0].status(), "waiting");
        assert_eq!(steps[1].files, vec![(Standing::Done, NodeId::file("b.ts"))]);
        assert_eq!(steps[1].status(), "done");
    }

    /// The step of this very plan that reads "prompts into session props"
    /// names no file at all, and skipping it would misreport the work left.
    #[test]
    fn a_step_that_named_no_file_is_still_a_step() {
        let r = reading(&["a.ts"], &[]);
        let steps = r.steps(&heads(&[("Build", 1), ("Docs", 9)]), &|_| Some(0));
        assert_eq!(steps.len(), 2);
        assert!(steps[1].files.is_empty());
        assert_eq!(steps[1].status(), "waiting");
    }

    #[test]
    fn work_the_plan_never_named_gets_a_stop_of_its_own_at_the_end() {
        let r = reading(&["a.ts"], &["a.ts", "surprise.ts"]);
        let steps = r.steps(&heads(&[("Build", 1)]), &|_| Some(0));
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[1].heading, "Not in the plan");
        assert_eq!(
            steps[1].files,
            vec![(Standing::Unplanned, NodeId::file("surprise.ts"))]
        );
    }

    #[test]
    fn nothing_unplanned_means_no_extra_stop() {
        let r = reading(&["a.ts"], &["a.ts"]);
        assert_eq!(r.steps(&heads(&[("Build", 1)]), &|_| Some(0)).len(), 1);
    }

    #[test]
    fn a_mention_that_belongs_to_no_heading_is_left_for_the_whole_plan_view() {
        let r = reading(&["preamble.ts"], &[]);
        let steps = r.steps(&heads(&[("Build", 9)]), &|_| None);
        assert!(
            steps[0].files.is_empty(),
            "it is not forced into step one just to be somewhere"
        );
    }

    #[test]
    fn walking_a_step_lights_only_that_steps_files() {
        let r = reading(&["a.ts", "b.ts"], &["b.ts"]);
        let plan = NodeId::home_plan("p.md");
        let mut review = Review {
            plan: Some(plan.clone()),
            ..Default::default()
        };
        review.walk = r.steps(&heads(&[("One", 1), ("Two", 9)]), &|id| {
            (id.path_part() == "a.ts").then_some(0).or(Some(1))
        });

        review.go_to_step(Some(0));
        let lit = review.lit(&r);
        assert!(lit.contains(&NodeId::file("a.ts")));
        assert!(!lit.contains(&NodeId::file("b.ts")), "another step's work");
        assert!(lit.contains(&plan), "and the plan it belongs to");
        assert!(review.frame, "the camera is asked to go there");

        review.go_to_step(None);
        assert!(
            review.lit(&r).contains(&NodeId::file("b.ts")),
            "all of it again"
        );
    }

    /// The step's files are what the reader was just sent to look at, so a
    /// search left in the filter box must not send them to an empty canvas.
    #[test]
    fn the_files_of_the_step_being_walked_are_the_ones_to_keep_visible() {
        let r = reading(&["a.ts", "b.ts"], &[]);
        let mut review = Review {
            plan: Some(NodeId::home_plan("p.md")),
            ..Default::default()
        };
        review.walk = r.steps(&heads(&[("One", 1), ("Two", 9)]), &|id| {
            (id.path_part() == "a.ts").then_some(0).or(Some(1))
        });
        assert_eq!(review.step_files(), None, "not walking: nothing is forced");

        review.go_to_step(Some(1));
        assert_eq!(
            review.step_files(),
            Some([NodeId::file("b.ts")].into_iter().collect())
        );
    }

    #[test]
    fn a_step_that_is_not_there_is_not_walked_to() {
        let mut review = Review::default();
        review.go_to_step(Some(3));
        assert_eq!(review.step, None);
        assert!(!review.frame);
    }

    #[test]
    fn opening_another_plan_starts_its_walk_from_the_top() {
        let mut review = Review {
            step: Some(2),
            walk: vec![Walk {
                heading: "One".into(),
                level: 2,
                line: 1,
                files: Vec::new(),
            }],
            ..Default::default()
        };
        review.open(NodeId::home_plan("p.md"), None);
        assert_eq!(review.step, None);
        assert!(review.walk.is_empty());
    }

    fn touch(at: &str, file: &str) -> Touch {
        Touch {
            at: at.into(),
            file: NodeId::file(file),
            actor: NodeId::session("s"),
        }
    }

    #[test]
    fn the_timeline_starts_at_the_live_end() {
        let review = Review {
            plan: Some(NodeId::home_plan("p.md")),
            touches: vec![
                touch("2026-09-17T10:00Z", "a.ts"),
                touch("2026-09-17T11:00Z", "b.ts"),
            ],
            ..Default::default()
        };
        assert_eq!(review.at, None);
        assert_eq!(
            review.touched_by_now(),
            None,
            "an untouched slider hides nothing"
        );
        assert_eq!(review.moment(), None);
    }

    #[test]
    fn rewinding_shows_only_what_had_happened_by_then() {
        let review = Review {
            plan: Some(NodeId::home_plan("p.md")),
            at: Some(0),
            touches: vec![
                touch("2026-09-17T10:00Z", "a.ts"),
                touch("2026-09-17T11:00Z", "b.ts"),
            ],
            ..Default::default()
        };
        let by_now = review.touched_by_now().unwrap();
        assert_eq!(by_now, [NodeId::file("a.ts")].into_iter().collect());
        assert_eq!(review.moment(), Some("2026-09-17T10:00Z"));
    }

    /// Rewinding hides work that had not happened yet — but never the files the
    /// plan named. Watching a promise be kept means seeing the promise.
    #[test]
    fn what_the_plan_named_stays_lit_however_far_back_it_is_wound() {
        let reading = Reading {
            named: [NodeId::file("named.ts")].into_iter().collect(),
            touched: [NodeId::file("early.ts"), NodeId::file("late.ts")]
                .into_iter()
                .collect(),
            sessions: vec![NodeId::session("s")],
            commits: vec![],
        };
        let plan = NodeId::home_plan("p.md");
        let review = Review {
            plan: Some(plan.clone()),
            at: Some(0),
            touches: vec![
                touch("2026-09-17T10:00Z", "early.ts"),
                touch("2026-09-17T11:00Z", "late.ts"),
            ],
            ..Default::default()
        };
        let lit = review.lit(&reading);
        assert!(lit.contains(&NodeId::file("named.ts")), "the promise");
        assert!(
            lit.contains(&NodeId::file("early.ts")),
            "and what was done by then"
        );
        assert!(
            !lit.contains(&NodeId::file("late.ts")),
            "but not what came later"
        );
        assert!(lit.contains(&plan));
        assert!(
            lit.contains(&NodeId::session("s")),
            "the actors always stay"
        );
    }

    #[test]
    fn opening_another_plan_rewinds_to_its_own_live_end() {
        let mut review = Review {
            at: Some(3),
            touches: vec![touch("2026-09-17T10:00Z", "a.ts")],
            ..Default::default()
        };
        review.open(NodeId::home_plan("p.md"), None);
        assert_eq!(review.at, None);
        assert!(
            review.touches.is_empty(),
            "and does not inherit the old one's"
        );
    }

    #[test]
    fn closing_stops_narrowing_the_graph() {
        let mut review = Review {
            only: true,
            ..Default::default()
        };
        review.open(NodeId::home_plan("p.md"), None);
        review.at = Some(2);
        review.touches = vec![touch("2026-09-17T10:00Z", "a.ts")];
        review.close();
        assert!(review.plan.is_none());
        assert!(!review.only, "or the graph would stay filtered to nothing");
        assert_eq!(review.at, None);
        assert!(review.touches.is_empty());
    }
}
