//! End-to-end: a run that happened, recorded so it survives a cache rebuild and
//! a pull from another machine — and nothing executed to get it.
//!
//! Everything here is observation. The launch agent in the fixture never runs;
//! what exists is the evidence a run leaves behind, which is all launchd keeps:
//! a log file with a modification time and a size. That is deliberately the
//! whole of this phase, so the cross-machine story can be shown working with no
//! executor anywhere near it.

use aneural_core::kinds::{EdgeKind, NodeKind};
use aneural_core::{Node, NodeId, Workspace};
use aneural_engine::runs;
use aneural_engine::{Engine, EngineEvent};
use aneural_store::{EdgeQuery, NodeQuery};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const MACHINE: &str = "test-box";
const SCRIPT: &str = "ingest/scripts/download_pubmed.py";
const LABEL: &str = "com.example.download-pubmed";

/// Where the agent's logs go: outside the workspace, as a real one's are, which
/// is also what lets the "writes only under its own directories" test be exact.
struct Fixture {
    tmp: tempfile::TempDir,
    ws: Workspace,
}

impl Fixture {
    fn logs(&self) -> PathBuf {
        self.tmp.path().join("logs")
    }
    fn agents(&self) -> PathBuf {
        self.tmp.path().join("LaunchAgents")
    }
    fn journal(&self) -> PathBuf {
        self.ws.runs_dir()
    }
    fn segment(&self) -> PathBuf {
        let found = runs::journal::segments(&self.journal());
        assert_eq!(found.len(), 1, "{found:?}");
        found.into_iter().next().unwrap()
    }
}

/// A workspace with one script, one launch agent pointed at it, and the logs
/// that agent would have left behind if it had run.
fn fixture(scripts_spore: bool, ran: bool) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("ws");
    std::fs::create_dir_all(root.join("ingest/scripts")).unwrap();
    std::fs::write(
        root.join(SCRIPT),
        "\"\"\"Download PubMed.\"\"\"\nSOURCE_ID = \"pubmed\"\n",
    )
    .unwrap();

    let root = root.canonicalize().unwrap();
    let ws = Workspace::at(&root);
    ws.init(None, true).unwrap();

    let logs = tmp.path().join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    if ran {
        // The normal shape for anything logging through Python: stdout empty,
        // stderr carrying the output.
        std::fs::write(logs.join("pubmed_stdout.log"), "").unwrap();
        std::fs::write(logs.join("pubmed_stderr.log"), "fetched 412 files\n").unwrap();
    }

    let agents = tmp.path().join("LaunchAgents");
    install_agent(&agents, &root, &logs);

    let mut config = ws.load_config().unwrap();
    if scripts_spore {
        config.spores.enabled.push("aneural.scripts".into());
    }
    config.runs.enabled = true;
    config.runs.machine = MACHINE.into();
    config.runs.launch_agents_root = agents.display().to_string();
    ws.save_config(&config).unwrap();
    Fixture { tmp, ws }
}

fn install_agent(dir: &Path, ws_root: &Path, logs: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>/opt/homebrew/bin/uv</string>
        <string>run</string>
        <string>python</string>
        <string>scripts/download_pubmed.py</string>
    </array>
    <key>WorkingDirectory</key>
    <string>{ingest}</string>
    <key>StartCalendarInterval</key>
    <dict>
        <key>Hour</key>
        <integer>4</integer>
    </dict>
    <key>StandardOutPath</key>
    <string>{out}</string>
    <key>StandardErrorPath</key>
    <string>{err}</string>
    <key>RunAtLoad</key>
    <false/>
</dict>
</plist>
"#,
        ingest = ws_root.join("ingest").display(),
        out = logs.join("pubmed_stdout.log").display(),
        err = logs.join("pubmed_stderr.log").display(),
    );
    std::fs::write(dir.join(format!("{LABEL}.plist")), plist).unwrap();
}

fn index(ws: &Workspace) -> Engine {
    let mut engine = Engine::open_in_memory(ws.clone()).unwrap();
    engine.index_full(false, &mut |_: EngineEvent| {}).unwrap();
    engine
}

fn nodes_of_kind(engine: &Engine, kind: &str) -> Vec<Node> {
    let mut v = engine
        .store()
        .query_nodes(&NodeQuery {
            kinds: vec![kind.into()],
            ..Default::default()
        })
        .unwrap();
    v.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    v
}

/// Every file under `root`, by content, ignoring `.aneural/` — which is the one
/// place Aneural is allowed to write.
fn tree(root: &Path) -> BTreeMap<String, String> {
    fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<String, String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let rel = path.strip_prefix(base).unwrap().display().to_string();
            if rel.starts_with(".aneural") {
                continue;
            }
            if path.is_dir() {
                walk(&path, base, out);
            } else {
                let bytes = std::fs::read(&path).unwrap_or_default();
                out.insert(
                    rel,
                    aneural_core::short_hash(&String::from_utf8_lossy(&bytes)),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[test]
fn a_run_that_happened_becomes_the_scripts_latest_run() {
    let f = fixture(true, true);
    let engine = index(&f.ws);

    let runs = nodes_of_kind(&engine, NodeKind::RUN);
    assert_eq!(runs.len(), 1, "{runs:?}");
    let run = &runs[0];
    assert_eq!(run.id, NodeId::run(SCRIPT, None));
    // Unloaded, so launchd has forgotten how it went. `unknown` and not `ok`:
    // guessing would turn a job that died into one that succeeded.
    assert_eq!(run.label, "download_pubmed: unknown");
    assert_eq!(run.prop_str("fidelity"), Some("observed"));
    assert_eq!(run.prop_str("trigger"), Some("launchd"));
    assert_eq!(run.prop_str("machine"), Some(MACHINE));
    assert_eq!(run.prop_i64("errBytes"), Some(18));
    assert_eq!(run.prop_str("tail"), Some("fetched 412 files\n"));
    // and nothing is claimed about how long it took
    assert_eq!(run.prop_i64("durationMs"), None);
}

#[test]
fn a_run_is_tethered_to_the_script_and_says_which_schedule_fired_it() {
    let f = fixture(true, true);
    let engine = index(&f.ws);
    let edges = engine
        .store()
        .get_edges(&EdgeQuery {
            src: Some(NodeId::run(SCRIPT, None)),
            ..Default::default()
        })
        .unwrap();

    let to_script: Vec<_> = edges
        .iter()
        .filter(|e| e.dst == NodeId::script(SCRIPT, None))
        .collect();
    assert_eq!(to_script.len(), 1, "{edges:?}");
    assert_eq!(to_script[0].kind, EdgeKind::ANNOTATES);
    assert_eq!(to_script[0].props["via"], "run");
    // A strand, so the run grows off its script rather than floating.
    assert!(EdgeKind::is_strand(EdgeKind::ANNOTATES));

    // And which declaration actually fired, which is what makes the drift
    // verdict beside it falsifiable rather than merely asserted.
    assert!(
        edges
            .iter()
            .any(|e| e.kind == EdgeKind::REALIZES && e.dst == NodeId::installed_schedule(LABEL)),
        "{edges:?}"
    );
}

#[test]
fn the_journal_is_committed_and_everything_derived_from_it_is_not() {
    // The whole reason the durable form is text: `.aneural/`'s own gitignore
    // already splits derived from durable, and the journal has to land on the
    // committed side of it with nothing to configure.
    let f = fixture(true, true);
    index(&f.ws);

    let ignored = std::fs::read_to_string(f.ws.aneural_dir().join(".gitignore")).unwrap();
    assert!(ignored.contains("cache/"), "{ignored}");
    assert!(!ignored.contains("runs/"), "the journal is not ignored");

    let segment = f.segment();
    assert!(
        segment.starts_with(f.ws.runs_dir().join(MACHINE)),
        "{segment:?}"
    );
    assert!(f.ws.runs_db_path().starts_with(f.ws.cache_dir()));

    // And the merge rule that makes two machines' appends resolve themselves.
    let attributes = std::fs::read_to_string(f.ws.gitattributes_path()).unwrap();
    assert!(
        attributes
            .lines()
            .any(|l| l.trim() == "runs/**/*.jsonl merge=union"),
        "{attributes}"
    );
}

#[test]
fn observing_the_same_unchanged_agent_again_appends_nothing() {
    // It runs on a timer against a file git tracks, so an idempotent second
    // look is not a nicety.
    let f = fixture(true, true);
    index(&f.ws);
    let after_first = std::fs::read_to_string(f.segment()).unwrap();
    assert_eq!(after_first.lines().count(), 1);

    index(&f.ws);
    assert_eq!(std::fs::read_to_string(f.segment()).unwrap(), after_first);
}

#[test]
fn a_run_survives_losing_both_caches() {
    // The point of the journal. The graph store is dropped and rebuilt whenever
    // its version changes, and the run index is a cache of the journal — so
    // deleting both must cost nothing.
    //
    // It must also cost nothing *in the committed file*: asking the empty cache
    // whether it has seen a run answers no, and appending on that answer would
    // add a duplicate line to git history every time the cache was lost. The
    // byte-for-byte assertion at the end is what pins that.
    let f = fixture(true, true);
    index(&f.ws);
    let committed = std::fs::read_to_string(f.segment()).unwrap();

    std::fs::remove_file(f.ws.runs_db_path()).unwrap();
    // `open_in_memory` already gives a fresh graph store, which is the other
    // half of the rebuild.
    let engine = index(&f.ws);

    assert_eq!(nodes_of_kind(&engine, NodeKind::RUN).len(), 1);
    assert_eq!(
        std::fs::read_to_string(f.segment()).unwrap(),
        committed,
        "rebuilt from the journal without rewriting it"
    );
}

#[test]
fn a_run_pulled_from_another_machine_is_one_row_and_can_be_the_latest() {
    // What a `git pull` delivers: a directory this machine never wrote, holding
    // a run of a script this machine knows. Both copies of the same record
    // collapse, because the id is derived rather than random.
    let f = fixture(true, true);
    index(&f.ws);

    let millis = 1_800_000_000_000_i64;
    let theirs = runs::journal::Record {
        run_id: runs::journal::run_id("build-box", millis, SCRIPT),
        machine: "build-box".into(),
        script_key: SCRIPT.into(),
        trigger: runs::journal::trigger::SCHEDULER.into(),
        started_at: runs::journal::rfc3339(millis),
        status: runs::journal::status::FAILED.into(),
        exit_code: Some(2),
        duration_ms: Some(42_000),
        fidelity: runs::journal::fidelity::EXACT.into(),
        seq: 1,
        ..Default::default()
    };
    // Written twice, as a union merge of two clones that both had it would.
    let path = runs::journal::append(&f.journal(), &theirs).unwrap();
    runs::journal::append(&f.journal(), &theirs).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);

    let engine = index(&f.ws);
    let runs = nodes_of_kind(&engine, NodeKind::RUN);
    assert_eq!(runs.len(), 1, "still one node per script");
    // Theirs is the newer run, so it is the one shown — with its measured
    // duration, because they watched it and we only saw traces.
    assert_eq!(runs[0].label, "download_pubmed: failed 42s");
    assert_eq!(runs[0].prop_str("machine"), Some("build-box"));
    assert_eq!(runs[0].prop_str("fidelity"), Some("exact"));
    assert_eq!(runs[0].prop_i64("exitCode"), Some(2));
}

#[test]
fn a_rewritten_segment_is_read_again_rather_than_resumed() {
    let f = fixture(true, true);
    index(&f.ws);
    let ours = f.segment();

    // A rebase reorders the file. The offset remembered against it now points
    // into the middle of a different line.
    let millis = 1_800_000_000_000_i64;
    let extra = runs::journal::Record {
        run_id: runs::journal::run_id(MACHINE, millis, SCRIPT),
        machine: MACHINE.into(),
        script_key: SCRIPT.into(),
        trigger: runs::journal::trigger::MANUAL.into(),
        started_at: runs::journal::rfc3339(millis),
        status: runs::journal::status::OK.into(),
        duration_ms: Some(1_500),
        fidelity: runs::journal::fidelity::EXACT.into(),
        seq: 1,
        ..Default::default()
    };
    let mut lines: Vec<String> = std::fs::read_to_string(&ours)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    lines.insert(0, serde_json::to_string(&extra).unwrap());
    std::fs::write(&ours, lines.join("\n") + "\n").unwrap();

    let engine = index(&f.ws);
    let runs = nodes_of_kind(&engine, NodeKind::RUN);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].label, "download_pubmed: ok 1.5s", "the newer run");
}

#[test]
fn a_run_whose_script_is_not_on_the_canvas_grows_no_node() {
    // With the spore off there is no Script node, so a Run node would have
    // nothing to grow off. The record is still written — the run happened —
    // which is the difference between withholding a node and losing history.
    let f = fixture(false, true);
    let engine = index(&f.ws);
    assert!(nodes_of_kind(&engine, NodeKind::SCRIPT).is_empty());
    assert!(nodes_of_kind(&engine, NodeKind::RUN).is_empty());
    assert_eq!(
        std::fs::read_to_string(f.segment())
            .unwrap()
            .lines()
            .count(),
        1,
        "recorded anyway"
    );
}

#[test]
fn an_agent_that_has_left_no_trace_is_not_reported_as_having_run() {
    let f = fixture(true, false);
    let engine = index(&f.ws);
    // The agent is on the canvas, because it is installed.
    assert!(
        nodes_of_kind(&engine, NodeKind::SCHEDULE)
            .iter()
            .any(|n| n.prop_str("declaredBy") == Some("installed"))
    );
    // But nothing claims it ever fired, and no journal was started.
    assert!(nodes_of_kind(&engine, NodeKind::RUN).is_empty());
    assert!(runs::journal::segments(&f.journal()).is_empty());
}

#[test]
fn turning_the_monitor_off_retracts_the_run_and_keeps_the_journal() {
    let f = fixture(true, true);
    index(&f.ws);
    let committed = std::fs::read_to_string(f.segment()).unwrap();

    let mut config = f.ws.load_config().unwrap();
    config.runs.enabled = false;
    f.ws.save_config(&config).unwrap();

    let engine = index(&f.ws);
    assert!(nodes_of_kind(&engine, NodeKind::RUN).is_empty());
    assert!(nodes_of_kind(&engine, NodeKind::SCHEDULE).is_empty());
    // Switching a producer off is not a reason to throw away what a machine
    // already recorded and committed.
    assert_eq!(std::fs::read_to_string(f.segment()).unwrap(), committed);
}

#[test]
fn nothing_is_executed_to_produce_any_of_this() {
    // The most important test in the phase. Everything above is read from files
    // that already existed; if a process were ever started, the script would
    // leave this marker behind.
    let f = fixture(true, true);
    let marker = f.tmp.path().join("ws").join("ingest").join("RAN");
    std::fs::write(
        f.ws.root().join(SCRIPT),
        format!(
            "import pathlib\npathlib.Path(r\"{}\").write_text(\"ran\")\n",
            marker.display()
        ),
    )
    .unwrap();

    index(&f.ws);
    assert!(!marker.exists(), "something ran a script");
    // and the gate that would allow it is still shut
    assert!(!f.ws.load_config().unwrap().runs.execute);
}

#[test]
fn aneural_writes_only_under_its_own_directory() {
    let f = fixture(true, true);
    let before = tree(f.ws.root());
    assert!(before.contains_key(SCRIPT), "{before:?}");

    index(&f.ws);
    index(&f.ws);

    assert_eq!(tree(f.ws.root()), before, "the workspace was modified");
    // Nor the logs it read, nor the launch agents it looked at.
    assert_eq!(
        std::fs::read_to_string(f.logs().join("pubmed_stderr.log")).unwrap(),
        "fetched 412 files\n"
    );
    assert_eq!(std::fs::read_dir(f.agents()).unwrap().count(), 1);
}
