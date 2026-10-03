//! What a workspace's scripts are declared to run on, and whether that matches
//! what is actually installed.
//!
//! The `scripts` spore finds the runnable things and reads the declarations
//! written down beside them, which is all a spore can ever do: the launch agents
//! that actually fire live in `~/Library/LaunchAgents`, outside the workspace,
//! and reaching them is `fs-read` — native tier, and never something a
//! downloadable manifest may have. So this is an engine producer, the same
//! arrangement as the `claude` producer reading `~/.claude`.
//!
//! What it adds to what the spore already said:
//!
//! - **a due status**, which needs a clock the template language does not have;
//! - **the schedules this machine actually has**, which are out of a spore's reach;
//! - **drift**, which needs to see the declared and the installed at once.
//!
//! Drift is deliberately *several nodes disagreeing*, not a verdict prop on one.
//! A node has exactly one origin and `replace_origin` is the only way to publish,
//! so this producer could not amend the spore's node even if it wanted to — the
//! next re-harvest of the declaring file would wipe whatever it wrote. One node
//! per claim, each saying who claimed it, is the only arrangement that survives,
//! and it is also the honest one: nothing here picks a winner.

pub mod journal;
pub mod launchd;
pub mod schedule;
pub mod store;

use aneural_core::kinds::{EdgeKind, NodeKind, Source};
use aneural_core::{Edge, Node, NodeId};
use std::collections::BTreeMap;
use time::Date;

/// What this machine's launchd or crontab actually has.
pub const INSTALLED_ORIGIN: &str = "runs://installed";
/// What Aneural itself is set to manage, from config.
pub const MANAGED_ORIGIN: &str = "runs://managed";
/// The due status and drift verdicts, recomputed together from everything above.
pub const VERDICT_ORIGIN: &str = "runs://verdicts";
/// One origin per script's latest run: `runs://latest/<script key>`.
///
/// Per script rather than one origin for all of them, so a run finishing
/// converges one node and leaves every other Run node alone. The same reason
/// each repository's log gets its own `git://` origin.
pub const LATEST_PREFIX: &str = "runs://latest/";
/// Every origin this producer owns.
pub const PREFIX: &str = "runs://";

/// The origin holding one script's most recent run.
pub fn latest_origin(script_key: &str) -> String {
    format!("{LATEST_PREFIX}{script_key}")
}

/// A `Script` node's id without its prefix.
///
/// This is the key a run is recorded against, and it is workspace-relative on
/// purpose: an absolute path would make a journal unreadable on the machine that
/// pulls it.
pub fn script_key(id: &NodeId) -> String {
    id.as_str()
        .split_once(':')
        .map(|(_, rest)| rest)
        .unwrap_or_default()
        .to_string()
}

/// The `Script` node a key belongs to — the inverse of [`script_key`].
pub fn script_id(key: &str) -> NodeId {
    NodeId::new(format!("script:{key}"))
}

/// A schedule as it was declared somewhere in the workspace, read back off the
/// graph. The spore owns these nodes; this producer only reads them.
#[derive(Clone, Debug)]
pub struct Declared {
    pub id: NodeId,
    pub label: String,
    pub cadence: String,
    pub last_refreshed: String,
    pub cron: String,
    /// Which scripts the declaration reaches, from its own edges.
    pub scripts: Vec<NodeId>,
}

impl Declared {
    /// Read one out of a `Schedule` node the spore emitted.
    pub fn of(node: &Node, scripts: Vec<NodeId>) -> Self {
        let prop = |k: &str| node.prop_str(k).unwrap_or_default().to_string();
        Declared {
            id: node.id.clone(),
            label: node.label.clone(),
            cadence: prop("cadence"),
            last_refreshed: prop("lastRefreshed"),
            cron: prop("cron"),
            scripts,
        }
    }
}

/// A launch agent or crontab entry this machine actually has.
#[derive(Clone, Debug, PartialEq)]
pub struct Installed {
    /// launchd label, or the crontab line's own identity.
    pub label: String,
    /// `launchd` | `crontab`.
    pub kind: String,
    /// The command, as it will actually be run.
    pub command: String,
    /// `StartCalendarInterval` rendered, or the cron expression.
    pub when: String,
    /// Roughly how often it fires, in days, when that can be told. This is what
    /// a declared cadence can be compared against; the strings never match.
    pub every_days: Option<u32>,
    /// Whether it is loaded and enabled right now.
    pub loaded: bool,
    /// The script it runs, when one in this workspace can be recognised.
    pub script: Option<NodeId>,
    /// `StandardOutPath` and `StandardErrorPath`, when the plist names them.
    ///
    /// These are the only first-hand evidence that a foreign agent ever ran:
    /// launchd keeps no history, so a file's modification time and size are what
    /// there is. Absent, an agent is installed and nothing can be said about
    /// whether it has fired.
    pub log_out: Option<String>,
    pub log_err: Option<String>,
    /// The running process, if it is running right now.
    pub pid: Option<u32>,
    /// How it exited last time, as launchd reports it. Negative means a signal.
    pub last_exit: Option<i64>,
}

/// Turn what was found into nodes and edges.
///
/// Pure, and `now` is passed in, so every due and drift decision is testable
/// without a clock.
pub fn to_graph(
    declared: &[Declared],
    installed: &[Installed],
    now: Date,
    cadences: &BTreeMap<String, u32>,
) -> (Vec<Node>, Vec<Edge>) {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    // One node per thing this machine actually has, whether or not anything
    // declared it. An agent nobody wrote down is exactly as interesting as a
    // declaration nobody installed.
    for one in installed {
        let id = NodeId::installed_schedule(&one.label);
        let mut node = Node::new(id.clone(), NodeKind::SCHEDULE, &one.label, Source::RUNS)
            .with_origin(INSTALLED_ORIGIN)
            .with_prop("declaredBy", "installed")
            .with_prop("mechanism", one.kind.clone())
            .with_prop("when", one.when.clone())
            .with_prop("loaded", one.loaded);
        if !one.command.is_empty() {
            node = node.with_prop("command", one.command.clone());
        }
        if let Some(days) = one.every_days {
            node = node.with_prop("everyDays", days as i64);
        }
        if let Some(script) = &one.script {
            edges.push(
                Edge::new(
                    EdgeKind::ANNOTATES,
                    id.clone(),
                    script.clone(),
                    Source::RUNS,
                )
                .with_origin(INSTALLED_ORIGIN)
                .with_prop("via", "schedule"),
            );
        }
        nodes.push(node);
    }

    // The verdicts, on nodes of this producer's own, because the declaration's
    // node belongs to the spore that read it.
    let mut verdicts = Vec::new();
    for d in declared {
        let status = schedule::status(&d.cadence, &d.last_refreshed, now, cadences);
        let drift = drift_of(d, installed);
        if status == schedule::Due::Unknown && drift.is_none() {
            // Nothing to say: no cadence anyone recognises and nothing to
            // compare it against. A node saying "unknown" is worse than none.
            continue;
        }
        let id = NodeId::managed_schedule(&format!("verdict/{}", aneural_core::slug(&d.label)));
        let mut node = Node::new(id.clone(), NodeKind::SCHEDULE, &d.label, Source::RUNS)
            .with_origin(VERDICT_ORIGIN)
            .with_prop("declaredBy", "verdict")
            .with_prop("due", status.label())
            .with_prop("cadence", d.cadence.clone());
        if let Some(next) = schedule::next_due(&d.last_refreshed, &d.cadence, cadences) {
            node = node.with_prop("nextDue", next.to_string());
        }
        if let Some(why) = &drift {
            node = node.with_prop("drift", why.clone());
        }
        // Tethered to the declaration it judges, and to the script if the
        // declaration reached one.
        edges.push(
            Edge::new(EdgeKind::ANNOTATES, id.clone(), d.id.clone(), Source::RUNS)
                .with_origin(VERDICT_ORIGIN)
                .with_prop("via", "verdict"),
        );
        for script in &d.scripts {
            edges.push(
                Edge::new(
                    EdgeKind::ANNOTATES,
                    id.clone(),
                    script.clone(),
                    Source::RUNS,
                )
                .with_origin(VERDICT_ORIGIN)
                .with_prop("via", "schedule"),
            );
        }
        verdicts.push(node);
    }
    nodes.extend(verdicts);
    (nodes, edges)
}

/// The `Run` node a script's latest run is published as.
pub fn run_node_id(script_key: &str) -> NodeId {
    NodeId::new(format!("run:{script_key}"))
}

/// One `Run` node per script, from the most recent run of each.
///
/// **One stable node per script, ever**, carrying a `runId` saying which run it
/// currently describes. A content-derived id would despawn and respawn a canvas
/// entity every time a schedule fired — 288 of them a day for a five-minute cron
/// — losing its position and its pin each time. Durable run identity lives in
/// the journal; the graph is a cache of the latest state.
///
/// The cost of that, stated rather than hidden: a pinned `Run` node quietly
/// becomes a different run. `runId` and `startedAt` are on the node so a reader
/// can see which.
pub fn runs_to_graph(latest: &[journal::Record]) -> (Vec<Node>, Vec<Edge>) {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for r in latest {
        if r.script_key.is_empty() {
            continue;
        }
        let id = run_node_id(&r.script_key);
        let script = script_id(&r.script_key);
        let origin = latest_origin(&r.script_key);
        let mut node = Node::new(id.clone(), NodeKind::RUN, run_label(r), Source::RUNS)
            .with_origin(&origin)
            .with_prop("runId", r.run_id.clone())
            .with_prop("status", r.status.clone())
            .with_prop("trigger", r.trigger.clone())
            // On every run node, not only the observed ones: a reader asking
            // "how do you know?" should never have to infer the answer from
            // which other props happen to be missing.
            .with_prop("fidelity", r.fidelity.clone())
            .with_prop("machine", r.machine.clone())
            .with_prop("startedAt", r.started_at.clone());
        for (key, value) in [("endedAt", &r.ended_at), ("logPath", &r.log_path)] {
            if let Some(v) = value {
                node = node.with_prop(key, v.clone());
            }
        }
        for (key, value) in [
            ("exitCode", r.exit_code),
            ("durationMs", r.duration_ms),
            ("outBytes", r.out_bytes),
            ("errBytes", r.err_bytes),
        ] {
            if let Some(v) = value {
                node = node.with_prop(key, v);
            }
        }
        if let Some(tail) = &r.tail {
            node = node.with_prop("tail", tail.clone());
        }
        nodes.push(node);

        // A strand, so the run grows off the script it is a run of rather than
        // floating. This is the tether the whole feature was asked for.
        edges.push(
            Edge::new(EdgeKind::ANNOTATES, id.clone(), script, Source::RUNS)
                .with_origin(&origin)
                .with_prop("via", "run"),
        );
        // Which declaration actually fired, when that is known. `REALIZES` is
        // the "this carried it out" relation, and it is what makes a drift
        // verdict falsifiable rather than merely asserted.
        if let Some(schedule) = &r.schedule_id {
            edges.push(
                Edge::new(
                    EdgeKind::REALIZES,
                    id.clone(),
                    NodeId::new(schedule.clone()),
                    Source::RUNS,
                )
                .with_origin(&origin),
            );
        }
    }
    (nodes, edges)
}

/// `download_pubmed: ok 3m12s`.
///
/// The outcome goes in the label because there is no colour-by-prop in the GUI
/// and this feature is not the place to add one. A duration nobody measured is
/// left off rather than written as zero.
///
/// **ASCII only, and that is not a style preference.** The canvas font has no
/// glyph for `·`, so the separator the plan asked for rendered as a tofu box on
/// a real node; every other producer in the engine emits ASCII labels for the
/// same reason. Checked by a test rather than by eye.
fn run_label(r: &journal::Record) -> String {
    let stem = std::path::Path::new(&r.script_key)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| r.script_key.clone());
    match r.duration_ms {
        Some(ms) if ms > 0 => format!("{stem}: {} {}", r.status, duration(ms)),
        _ => format!("{stem}: {}", r.status),
    }
}

/// `1.2s`, `3m12s`, `2h07m` — short enough for a node label.
fn duration(ms: i64) -> String {
    let secs = ms / 1_000;
    match secs {
        s if s < 10 => format!("{:.1}s", ms as f64 / 1_000.0),
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{}h{:02}m", s / 3_600, (s % 3_600) / 60),
    }
}

/// How a declaration and what is installed disagree, in one line, or `None` when
/// they do not.
///
/// Compared by **interval**, never by string: a launch agent says "06:00 every
/// day" and a table says "daily", and no amount of text comparison makes those
/// equal. The two would then report drift forever, which is the same as
/// reporting nothing.
fn drift_of(d: &Declared, installed: &[Installed]) -> Option<String> {
    let mine: Vec<&Installed> = installed
        .iter()
        .filter(|i| d.scripts.iter().any(|s| Some(s) == i.script.as_ref()))
        .collect();

    let wants = schedule::interval_days(&d.cadence, &BTreeMap::new()).flatten();
    match (mine.first(), wants) {
        // Declared a real cadence and nothing installed fires it. The common
        // case in a project whose crons live in a document.
        (None, Some(days)) => Some(format!(
            "declared every {days} day(s), nothing installed to run it"
        )),
        // Something fires it that nobody wrote down.
        (Some(first), None) => Some(format!(
            "{} installed, but no cadence is declared for it",
            first.label
        )),
        (None, None) => None,
        (Some(_), Some(days)) => {
            // Installed but switched off is more urgent than installed at the
            // wrong interval: it is not running at all.
            if let Some(off) = mine.iter().find(|i| !i.loaded) {
                return Some(format!("{} is installed but not loaded", off.label));
            }
            let disagree = mine
                .iter()
                .find(|i| i.every_days.is_some_and(|got| got != days))?;
            Some(format!(
                "declared every {days} day(s), {} fires every {} day(s)",
                disagree.label,
                disagree.every_days.unwrap_or(0)
            ))
        }
    }
}

/// The `Schedule` nodes a spore declared, with the scripts each one reaches.
///
/// Read off the graph rather than re-parsed, for the reason `refresh_plans`
/// reads sessions off the graph: the spore has already done the work, and a
/// second reading of the same file could disagree with the first.
pub fn declared_from(nodes: &[Node], edges_of: &dyn Fn(&NodeId) -> Vec<NodeId>) -> Vec<Declared> {
    nodes
        .iter()
        .filter(|n| n.kind == NodeKind::SCHEDULE && Source::is_spore(&n.source))
        .map(|n| Declared::of(n, edges_of(&n.id)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u8, d: u8) -> Date {
        Date::from_calendar_date(y, time::Month::try_from(m).unwrap(), d).unwrap()
    }

    fn script() -> NodeId {
        NodeId::script("ingest/scripts/download_pubmed.py", None)
    }

    fn declared(cadence: &str, last: &str) -> Declared {
        Declared {
            id: NodeId::parse("acme.cadences.schedule:Cadences.md#pubmed").unwrap(),
            label: "pubmed".into(),
            cadence: cadence.into(),
            last_refreshed: last.into(),
            cron: String::new(),
            scripts: vec![script()],
        }
    }

    fn agent(label: &str, every: Option<u32>, loaded: bool) -> Installed {
        Installed {
            label: label.into(),
            kind: "launchd".into(),
            command: "uv run python scripts/download_pubmed.py".into(),
            when: "daily at 04:00".into(),
            every_days: every,
            loaded,
            script: Some(script()),
            log_out: None,
            log_err: None,
            pid: None,
            last_exit: None,
        }
    }

    fn only(nodes: &[Node], declared_by: &str) -> Vec<Node> {
        nodes
            .iter()
            .filter(|n| n.prop_str("declaredBy") == Some(declared_by))
            .cloned()
            .collect()
    }

    #[test]
    fn a_declaration_with_nothing_installed_to_run_it_is_drift() {
        // The state the motivating project is actually in: two hundred cadences
        // written down and one launch agent installed.
        let (nodes, _) = to_graph(
            &[declared("daily", "2026-09-20")],
            &[],
            day(2026, 9, 27),
            &BTreeMap::new(),
        );
        let v = only(&nodes, "verdict");
        assert_eq!(v.len(), 1);
        // a week after a daily refresh is six days late: due, not yet overdue
        assert_eq!(v[0].prop_str("due"), Some("due"));
        assert_eq!(v[0].prop_str("nextDue"), Some("2026-09-21"));
        assert_eq!(
            v[0].prop_str("drift"),
            Some("declared every 1 day(s), nothing installed to run it")
        );

        // and past the week it is overdue
        let (later, _) = to_graph(
            &[declared("daily", "2026-09-20")],
            &[],
            day(2026, 9, 29),
            &BTreeMap::new(),
        );
        assert_eq!(only(&later, "verdict")[0].prop_str("due"), Some("overdue"));
    }

    #[test]
    fn a_declaration_and_an_agent_that_agree_are_not_drift() {
        // They agree on the *interval*, not on the words: the agent says
        // "daily at 04:00" and the table says "daily". Comparing the strings
        // would report drift on a pair that matches, forever.
        let (nodes, _) = to_graph(
            &[declared("daily", "2026-09-27")],
            &[agent("dev.aneural.ws.pubmed", Some(1), true)],
            day(2026, 9, 27),
            &BTreeMap::new(),
        );
        let v = only(&nodes, "verdict");
        assert_eq!(v[0].prop_str("due"), Some("fresh"));
        assert_eq!(v[0].prop_str("drift"), None, "{:?}", v[0].props);
    }

    #[test]
    fn an_agent_at_the_wrong_interval_says_which_way_round() {
        let (nodes, _) = to_graph(
            &[declared("weekly-mon", "2026-09-27")],
            &[agent("dev.aneural.ws.pubmed", Some(1), true)],
            day(2026, 9, 27),
            &BTreeMap::new(),
        );
        assert_eq!(
            only(&nodes, "verdict")[0].prop_str("drift"),
            Some("declared every 7 day(s), dev.aneural.ws.pubmed fires every 1 day(s)")
        );
    }

    #[test]
    fn installed_but_not_loaded_is_the_more_urgent_thing_to_say() {
        // Both are true — wrong interval and switched off — but one means it is
        // not running at all.
        let (nodes, _) = to_graph(
            &[declared("weekly-mon", "2026-09-27")],
            &[agent("dev.aneural.ws.pubmed", Some(1), false)],
            day(2026, 9, 27),
            &BTreeMap::new(),
        );
        assert_eq!(
            only(&nodes, "verdict")[0].prop_str("drift"),
            Some("dev.aneural.ws.pubmed is installed but not loaded")
        );
    }

    #[test]
    fn something_firing_that_nobody_wrote_down_is_also_drift() {
        let (nodes, _) = to_graph(
            &[declared("on-demand", "—")],
            &[agent("dev.aneural.ws.pubmed", Some(1), true)],
            day(2026, 9, 27),
            &BTreeMap::new(),
        );
        assert_eq!(
            only(&nodes, "verdict")[0].prop_str("drift"),
            Some("dev.aneural.ws.pubmed installed, but no cadence is declared for it")
        );
    }

    #[test]
    fn nothing_is_overwritten_to_make_the_claims_agree() {
        // The declaration, what is installed, and the verdict are three nodes.
        // None of them says which one won, because that is the reader's call.
        let (nodes, edges) = to_graph(
            &[declared("weekly-mon", "2026-09-20")],
            &[agent("dev.aneural.ws.pubmed", Some(1), true)],
            day(2026, 9, 27),
            &BTreeMap::new(),
        );
        assert_eq!(only(&nodes, "installed").len(), 1);
        assert_eq!(only(&nodes, "verdict").len(), 1);
        for n in &nodes {
            assert!(n.props.get("wins").is_none());
            assert!(n.props.get("effective").is_none());
        }
        // and the verdict is tethered both to the declaration it judges and to
        // the script the whole thing is about
        let from_verdict: Vec<_> = edges
            .iter()
            .filter(|e| e.origin.as_deref() == Some(VERDICT_ORIGIN))
            .collect();
        assert!(from_verdict.iter().any(|e| e.props["via"] == "verdict"));
        assert!(
            from_verdict
                .iter()
                .any(|e| e.dst == script() && e.props["via"] == "schedule")
        );
    }

    #[test]
    fn the_two_origins_stay_separate_so_one_re_read_does_not_disturb_the_other() {
        let (nodes, edges) = to_graph(
            &[declared("daily", "2026-09-20")],
            &[agent("dev.aneural.ws.pubmed", Some(1), true)],
            day(2026, 9, 27),
            &BTreeMap::new(),
        );
        for n in &nodes {
            let origin = n.origin.as_deref().unwrap();
            assert!(origin.starts_with(PREFIX), "{origin}");
            assert!(origin == INSTALLED_ORIGIN || origin == VERDICT_ORIGIN);
        }
        assert!(edges.iter().all(|e| e.source == Source::RUNS));
    }

    fn ran(script: &str, status: &str, duration: Option<i64>) -> journal::Record {
        journal::Record {
            run_id: journal::run_id("mbp", 1_789_430_400_000, script),
            machine: "mbp".into(),
            script_key: script.into(),
            trigger: journal::trigger::LAUNCHD.into(),
            started_at: "2026-09-15T00:00:00Z".into(),
            status: status.into(),
            duration_ms: duration,
            fidelity: journal::fidelity::OBSERVED.into(),
            seq: 1,
            ..Default::default()
        }
    }

    #[test]
    fn a_run_grows_off_the_script_it_is_a_run_of() {
        let key = "ingest/scripts/download_pubmed.py";
        let (nodes, edges) = runs_to_graph(&[ran(key, journal::status::OK, Some(192_000))]);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].id, NodeId::run(key, None));
        assert_eq!(nodes[0].kind, NodeKind::RUN);
        assert_eq!(nodes[0].label, "download_pubmed: ok 3m12s");
        assert_eq!(
            nodes[0].origin.as_deref(),
            Some("runs://latest/ingest/scripts/download_pubmed.py")
        );
        // ANNOTATES and not REALIZES: REALIZES is not a strand, and a Run that
        // floated away from its script is the opposite of what was asked for.
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, EdgeKind::ANNOTATES);
        assert_eq!(edges[0].dst, script());
        assert_eq!(edges[0].props["via"], "run");
        assert!(EdgeKind::is_strand(&edges[0].kind));
    }

    #[test]
    fn a_duration_nobody_measured_is_left_off_rather_than_written_as_zero() {
        // Every observed run is in this state, and ": ok 0.0s" would be a claim
        // about how long something took that nothing supports.
        let latest = ran("a/b.py", journal::status::UNKNOWN, None);
        let (nodes, _) = runs_to_graph(&[latest]);
        assert_eq!(nodes[0].label, "b: unknown");
        assert_eq!(nodes[0].prop_i64("durationMs"), None);
        assert_eq!(nodes[0].prop_str("fidelity"), Some("observed"));
    }

    #[test]
    fn which_schedule_fired_is_a_realizes_edge_when_it_is_known() {
        // What makes a drift verdict falsifiable: not "these two disagree" but
        // "this one is the one that actually ran".
        let schedule = NodeId::installed_schedule("dev.aneural.ws.pubmed");
        let (_, edges) = runs_to_graph(&[journal::Record {
            schedule_id: Some(schedule.as_str().to_string()),
            ..ran(
                "ingest/scripts/download_pubmed.py",
                journal::status::OK,
                None,
            )
        }]);
        let realizes: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == EdgeKind::REALIZES)
            .collect();
        assert_eq!(realizes.len(), 1);
        assert_eq!(realizes[0].dst, schedule);
        // still tethered to the script by the strand, so it does not float
        assert!(edges.iter().any(|e| e.kind == EdgeKind::ANNOTATES));
    }

    #[test]
    fn each_script_gets_its_own_origin_so_one_run_finishing_disturbs_no_other() {
        let (nodes, _) = runs_to_graph(&[
            ran("a/one.py", journal::status::OK, None),
            ran("a/two.py", journal::status::FAILED, None),
        ]);
        let origins: Vec<&str> = nodes.iter().filter_map(|n| n.origin.as_deref()).collect();
        assert_eq!(
            origins,
            ["runs://latest/a/one.py", "runs://latest/a/two.py"]
        );
        assert!(origins.iter().all(|o| o.starts_with(PREFIX)));
    }

    #[test]
    fn there_is_one_run_node_per_script_however_often_it_fires() {
        // The reason the id is not derived from the run: a five-minute cron
        // would churn a canvas entity 288 times a day, losing its position and
        // its pin each time.
        let key = "a/one.py";
        let first = runs_to_graph(&[ran(key, journal::status::OK, None)]).0;
        let later = runs_to_graph(&[journal::Record {
            run_id: journal::run_id("mbp", 1_789_430_460_000, key),
            started_at: "2026-09-15T00:01:00Z".into(),
            ..ran(key, journal::status::FAILED, None)
        }])
        .0;
        assert_eq!(first[0].id, later[0].id, "the same entity");
        // and it says which run it is currently describing, so the change is
        // visible rather than silent
        assert_ne!(first[0].prop_str("runId"), later[0].prop_str("runId"));
        assert_eq!(later[0].prop_str("status"), Some("failed"));
    }

    #[test]
    fn a_label_is_ascii_because_the_canvas_font_has_nothing_else() {
        // Found by looking at a real node: the `·` the plan asked for rendered
        // as a tofu box. Every other producer in the engine emits ASCII, and an
        // eye is the wrong thing to be checking this with.
        let (nodes, _) = runs_to_graph(&[
            ran("a/one.py", journal::status::OK, Some(192_000)),
            ran("a/two.py", journal::status::SKIPPED_PRECONDITION, None),
        ]);
        for n in &nodes {
            assert!(n.label.is_ascii(), "{:?}", n.label);
        }
    }

    #[test]
    fn a_duration_reads_as_a_person_would_say_it() {
        assert_eq!(duration(1_200), "1.2s");
        assert_eq!(duration(42_000), "42s");
        assert_eq!(duration(192_000), "3m12s");
        assert_eq!(duration(7_620_000), "2h07m");
    }

    #[test]
    fn a_script_key_survives_the_round_trip() {
        // It is what a run is filed under in a committed journal, so it has to
        // mean the same thing on the machine that pulls it.
        for id in [
            NodeId::script("ingest/scripts/a.py", None),
            NodeId::script("package.json", Some("build")),
        ] {
            assert_eq!(script_id(&script_key(&id)), id);
        }
    }

    #[test]
    fn a_declaration_nothing_can_be_said_about_grows_no_verdict() {
        // No cadence anyone recognises and nothing installed to compare it to.
        // A node reading `due: unknown, drift: none` is worse than no node.
        let (nodes, _) = to_graph(
            &[Declared {
                scripts: vec![],
                ..declared("", "")
            }],
            &[],
            day(2026, 9, 27),
            &BTreeMap::new(),
        );
        assert!(only(&nodes, "verdict").is_empty(), "{nodes:?}");
    }
}
