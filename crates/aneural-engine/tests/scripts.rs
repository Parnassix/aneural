//! End-to-end: the runnable things in a workspace, and the schedules declared
//! for them, as they appear on the canvas.
//!
//! The fixture is deliberately two repositories in one workspace — scripts in
//! one, the document that declares their cadence in another — because that is
//! the shape this was written for, and a single-repo fixture cannot show that
//! the join crosses a repository boundary.

use aneural_core::kinds::{EdgeKind, NodeKind};
use aneural_core::{Node, NodeId, Workspace};
use aneural_engine::{Engine, EngineEvent};
use aneural_store::{EdgeQuery, NodeQuery};

const CADENCES: &str = "\
## The index

| source_id | cadence | last_refreshed | notes |
|---|---|---|---|
| `pubmed` | daily | 2026-09-20 | NLM publishes ~14:00 UTC; cron `0 4 * * *` picks them up. |
| `gdelt` | weekly-mon | 2026-09-01 | See [[Refresh Semantics]] for the `pageNumber\\|offset` walk. |

## Refresh shapes

| source_id | refresh_shape |
|---|---|
| `pubmed` | listing-diff |
";

/// A workspace spore doing the per-project half: reading this project's own
/// cadence table and wiring each row to the script its key names.
const CADENCE_SPORE: &str = r##"{
  "publisher": "acme", "name": "cadences", "version": "0.1.0",
  "displayName": "Acme cadences", "description": "Reads the refresh cadence table.",
  "harvesters": [
    {
      "id": "index", "kind": "markdown", "include": ["**/Cadences.md"],
      "granularity": "table-row",
      "table": { "requires": ["source_id", "cadence"] },
      "emit": {
        "node": {
          "kind": "Schedule",
          "id": "acme.cadences.schedule:{file}#{slug(col.source_id)}",
          "label": "{col.source_id}",
          "props": {
            "cadence": "{col.cadence}",
            "lastRefreshed": "{col.last_refreshed}",
            "declaredBy": "cadence-index"
          }
        },
        "edges": [
          { "kind": "ANNOTATES", "src": "$node",
            "dst": "script:ingest/scripts/download_{col.source_id}.py",
            "props": { "via": "schedule" } }
        ]
      }
    },
    {
      "id": "shapes", "kind": "markdown", "include": ["**/Cadences.md"],
      "granularity": "table-row",
      "table": { "requires": ["source_id", "refresh_shape"] },
      "emit": {
        "node": {
          "kind": "Schedule",
          "id": "acme.cadences.schedule:{file}#{slug(col.source_id)}",
          "label": "{col.source_id}",
          "props": { "refreshShape": "{col.refresh_shape}" }
        }
      }
    }
  ]
}"##;

fn workspace(with_cadence_spore: bool) -> (tempfile::TempDir, Workspace) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("ws");
    std::fs::create_dir_all(root.join("ingest/scripts")).unwrap();
    std::fs::create_dir_all(root.join("ingest/lib")).unwrap();
    std::fs::create_dir_all(root.join("vault/Data Sources")).unwrap();
    std::fs::create_dir_all(root.join("bin")).unwrap();

    // No shebang, no exec bit — the shape of the real repository.
    std::fs::write(
        root.join("ingest/scripts/download_pubmed.py"),
        "\"\"\"Download PubMed update files.\"\"\"\nSOURCE_ID = \"pubmed\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("ingest/scripts/download_gdelt.py"),
        "\"\"\"Download GDELT.\"\"\"\nSOURCE_ID = \"gdelt\"\n",
    )
    .unwrap();
    // A shebang outside any scripts directory is still a script.
    std::fs::write(root.join("bin/release"), "#!/usr/bin/env bash\nset -e\n").unwrap();
    // A library is not a script, and neither is a document beside the scripts.
    std::fs::write(root.join("ingest/lib/storage.py"), "ROOT = 1\n").unwrap();
    std::fs::write(root.join("ingest/scripts/README.md"), "# how to run\n").unwrap();
    std::fs::write(root.join("vault/Data Sources/Cadences.md"), CADENCES).unwrap();

    let root = root.canonicalize().unwrap();
    let ws = Workspace::at(&root);
    ws.init(None, true).unwrap();

    if with_cadence_spore {
        let dir = ws.spores_dir().join("cadences");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("spore.json"), CADENCE_SPORE).unwrap();
    }

    let mut config = ws.load_config().unwrap();
    config.spores.enabled.push("aneural.scripts".into());
    if with_cadence_spore {
        config.spores.enabled.push("acme.cadences".into());
    }
    ws.save_config(&config).unwrap();
    (tmp, ws)
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

fn labels(nodes: &[Node]) -> Vec<&str> {
    nodes.iter().map(|n| n.label.as_str()).collect()
}

#[test]
fn the_runnable_things_become_scripts_and_nothing_else_does() {
    let (_tmp, ws) = workspace(false);
    let scripts = nodes_of_kind(&index(&ws), NodeKind::SCRIPT);
    assert_eq!(
        labels(&scripts),
        vec!["release", "download_gdelt", "download_pubmed"],
        "{:?}",
        scripts.iter().map(|n| n.id.as_str()).collect::<Vec<_>>()
    );
    // a library module and a README next to the scripts are not runnable
    assert!(!scripts.iter().any(|n| n.label == "storage"));
    assert!(!scripts.iter().any(|n| n.label == "README"));
}

#[test]
fn a_script_is_tethered_to_the_file_it_was_found_in() {
    let (_tmp, ws) = workspace(false);
    let engine = index(&ws);
    let edges = engine
        .store()
        .get_edges(&EdgeQuery {
            src: Some(NodeId::script("ingest/scripts/download_pubmed.py", None)),
            ..Default::default()
        })
        .unwrap();
    let to_file: Vec<_> = edges
        .iter()
        .filter(|e| e.dst == NodeId::file("ingest/scripts/download_pubmed.py"))
        .collect();
    assert_eq!(to_file.len(), 1, "{edges:?}");
    assert_eq!(to_file[0].kind, EdgeKind::REFERENCES);
    assert_eq!(to_file[0].props["via"], "source");
    // REFERENCES is a strand, so the script is wired into the tree beside its
    // file rather than floating away from it.
    assert!(EdgeKind::is_strand(EdgeKind::REFERENCES));
}

#[test]
fn a_shebang_and_a_scripts_directory_describe_one_script_between_them() {
    let (_tmp, ws) = workspace(false);
    let scripts = nodes_of_kind(&index(&ws), NodeKind::SCRIPT);
    let release = scripts.iter().find(|n| n.label == "release").unwrap();
    assert_eq!(release.props["found"], "shebang");
    assert_eq!(release.props["interpreter"], "/usr/bin/env");
    // found by both harvesters, emitted once
    assert_eq!(scripts.iter().filter(|n| n.label == "release").count(), 1);
}

#[test]
fn a_cadence_table_in_another_repository_reaches_the_scripts_it_is_about() {
    let (_tmp, ws) = workspace(true);
    let engine = index(&ws);

    let schedules = nodes_of_kind(&engine, NodeKind::SCHEDULE);
    assert_eq!(labels(&schedules), vec!["gdelt", "pubmed"], "{schedules:?}");

    let pubmed = schedules.iter().find(|n| n.label == "pubmed").unwrap();
    assert_eq!(pubmed.props["cadence"], "daily");
    assert_eq!(pubmed.props["lastRefreshed"], "2026-09-20");
    // the second table's column landed on the same node as the first's
    assert_eq!(pubmed.props["refreshShape"], "listing-diff");
    let gdelt = schedules.iter().find(|n| n.label == "gdelt").unwrap();
    assert_eq!(gdelt.props["cadence"], "weekly-mon");
    assert!(
        gdelt.props.get("refreshShape").is_none(),
        "gdelt is not in the shapes table"
    );

    // and each row is wired across the repository boundary to its script
    let edges = engine
        .store()
        .get_edges(&EdgeQuery {
            src: Some(pubmed.id.clone()),
            ..Default::default()
        })
        .unwrap();
    assert!(
        edges.iter().any(
            |e| e.dst == NodeId::script("ingest/scripts/download_pubmed.py", None)
                && e.kind == EdgeKind::ANNOTATES
                && e.props["via"] == "schedule"
        ),
        "{edges:?}"
    );
}

#[test]
fn a_cron_written_in_prose_is_not_harvested_as_a_schedule() {
    // The notes column says "cron `0 4 * * *` picks them up". That is a note
    // about intent, and promoting it would let something schedule a job nobody
    // chose. Nothing may read it as a `cron` prop.
    let (_tmp, ws) = workspace(true);
    let schedules = nodes_of_kind(&index(&ws), NodeKind::SCHEDULE);
    assert!(!schedules.is_empty());
    for s in &schedules {
        assert!(
            s.props.get("cron").is_none(),
            "{} grew a cron from prose: {:?}",
            s.label,
            s.props
        );
    }
}

#[test]
fn turning_the_spore_off_retracts_every_script_it_found() {
    let (_tmp, ws) = workspace(false);
    assert!(!nodes_of_kind(&index(&ws), NodeKind::SCRIPT).is_empty());

    let mut config = ws.load_config().unwrap();
    config.spores.enabled.retain(|e| e != "aneural.scripts");
    ws.save_config(&config).unwrap();

    let engine = index(&ws);
    assert!(nodes_of_kind(&engine, NodeKind::SCRIPT).is_empty());
    // the files themselves are untouched
    assert!(
        engine
            .store()
            .get_node(&NodeId::file("ingest/scripts/download_pubmed.py"))
            .unwrap()
            .is_some()
    );
}

/// A launch agent shaped like a real one, pointed at a script in the fixture.
fn install_agent(dir: &std::path::Path, label: &str, ws_root: &std::path::Path, hour: u8) {
    std::fs::create_dir_all(dir).unwrap();
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <!-- no idle sleep -->
        <string>/usr/bin/caffeinate</string>
        <string>-i</string>
        <string>/opt/homebrew/bin/uv</string>
        <string>run</string>
        <string>python</string>
        <string>scripts/download_pubmed.py</string>
    </array>
    <key>WorkingDirectory</key>
    <string>{}/ingest</string>
    <key>StartCalendarInterval</key>
    <dict>
        <key>Hour</key>
        <integer>{hour}</integer>
        <key>Minute</key>
        <integer>0</integer>
    </dict>
    <key>RunAtLoad</key>
    <false/>
</dict>
</plist>
"#,
        ws_root.display()
    );
    std::fs::write(dir.join(format!("{label}.plist")), plist).unwrap();
}

fn with_runs(ws: &Workspace, agents: &std::path::Path) {
    let mut config = ws.load_config().unwrap();
    config.runs.enabled = true;
    config.runs.launch_agents_root = agents.display().to_string();
    ws.save_config(&config).unwrap();
}

#[test]
fn the_monitor_says_what_is_due_and_where_the_claims_disagree() {
    let (tmp, ws) = workspace(true);
    let agents = tmp.path().join("LaunchAgents");
    std::fs::create_dir_all(&agents).unwrap();
    with_runs(&ws, &agents);

    let engine = index(&ws);
    let verdicts: Vec<Node> = nodes_of_kind(&engine, NodeKind::SCHEDULE)
        .into_iter()
        .filter(|n| n.prop_str("declaredBy") == Some("verdict"))
        .collect();
    assert_eq!(labels(&verdicts), vec!["gdelt", "pubmed"], "{verdicts:?}");

    // Both cadences are past their last refresh in the fixture, and there is no
    // agent at all, which is the state the motivating project is in.
    //
    // Whether that reads `due` or `overdue` depends on today's date, and the
    // exact boundary is pinned by the unit tests with an injected clock; what
    // this test is for is that the verdict reaches the graph at all.
    for v in &verdicts {
        assert!(
            matches!(v.prop_str("due"), Some("due" | "overdue")),
            "{} is {:?}",
            v.label,
            v.prop_str("due")
        );
        assert!(
            v.prop_str("drift")
                .unwrap_or_default()
                .contains("nothing installed to run it"),
            "{:?}",
            v.props
        );
        assert!(v.prop_str("nextDue").is_some());
    }
}

#[test]
fn an_installed_agent_and_a_declaration_are_two_nodes_and_neither_wins() {
    let (tmp, ws) = workspace(true);
    let agents = tmp.path().join("LaunchAgents");
    install_agent(&agents, "dev.aneural.ws.pubmed", ws.root(), 4);
    with_runs(&ws, &agents);

    let engine = index(&ws);
    let schedules = nodes_of_kind(&engine, NodeKind::SCHEDULE);
    let by = |who: &str| -> Vec<Node> {
        schedules
            .iter()
            .filter(|n| n.prop_str("declaredBy") == Some(who))
            .cloned()
            .collect()
    };

    // three separate claims about pubmed: what the table says, what this machine
    // has, and what Aneural makes of the pair
    let installed = by("installed");
    assert_eq!(installed.len(), 1, "{installed:?}");
    assert_eq!(
        installed[0].id,
        NodeId::installed_schedule("dev.aneural.ws.pubmed")
    );
    assert_eq!(installed[0].prop_str("when"), Some("daily at 04:00"));
    assert_eq!(installed[0].prop_i64("everyDays"), Some(1));
    assert_eq!(
        by("cadence-index").len(),
        2,
        "the declarations are untouched"
    );
    assert_eq!(by("verdict").len(), 2);

    // the declaration node is the spore's and the producer has not written to it
    let declared = by("cadence-index");
    let pubmed = declared.iter().find(|n| n.label == "pubmed").unwrap();
    assert_eq!(pubmed.prop_str("cadence"), Some("daily"));
    assert!(pubmed.prop_str("due").is_none(), "not the spore's to say");
    assert!(pubmed.prop_str("drift").is_none());

    // and the installed agent is wired to the script it runs
    let edges = engine
        .store()
        .get_edges(&EdgeQuery {
            src: Some(installed[0].id.clone()),
            ..Default::default()
        })
        .unwrap();
    assert!(
        edges
            .iter()
            .any(|e| e.dst == NodeId::script("ingest/scripts/download_pubmed.py", None)),
        "{edges:?}"
    );
}

#[test]
fn an_agent_on_disk_that_launchd_has_not_loaded_is_the_first_thing_said() {
    // A directory that is not the one launchd reads holds agents launchd has
    // never seen, so nothing in it is loaded — which is the true state of the
    // one real agent on the machine this was written on, too. "Not running at
    // all" outranks "running at the wrong interval".
    let (tmp, ws) = workspace(true);
    let agents = tmp.path().join("LaunchAgents");
    install_agent(&agents, "dev.aneural.ws.pubmed", ws.root(), 4);
    with_runs(&ws, &agents);

    let engine = index(&ws);
    let pubmed = nodes_of_kind(&engine, NodeKind::SCHEDULE)
        .into_iter()
        .find(|n| n.prop_str("declaredBy") == Some("verdict") && n.label == "pubmed")
        .unwrap();
    assert_eq!(
        pubmed.prop_str("drift"),
        Some("dev.aneural.ws.pubmed is installed but not loaded")
    );
    // Note what is *not* said: the agent fires daily and the table says daily,
    // so no drift is reported over the wording. Comparing "04:00 every day"
    // against "daily" as text would report drift on that pair forever.
    let installed = nodes_of_kind(&engine, NodeKind::SCHEDULE)
        .into_iter()
        .find(|n| n.prop_str("declaredBy") == Some("installed"))
        .unwrap();
    assert_eq!(installed.prop_i64("everyDays"), Some(1));
    assert_eq!(installed.prop_str("when"), Some("daily at 04:00"));
    assert_eq!(installed.props["loaded"], false);
}

#[test]
fn turning_the_monitor_off_retracts_its_verdicts_but_not_the_declarations() {
    let (tmp, ws) = workspace(true);
    let agents = tmp.path().join("LaunchAgents");
    install_agent(&agents, "dev.aneural.ws.pubmed", ws.root(), 4);
    with_runs(&ws, &agents);
    assert_eq!(nodes_of_kind(&index(&ws), NodeKind::SCHEDULE).len(), 5);

    let mut config = ws.load_config().unwrap();
    config.runs.enabled = false;
    ws.save_config(&config).unwrap();

    let engine = index(&ws);
    let left = nodes_of_kind(&engine, NodeKind::SCHEDULE);
    assert_eq!(labels(&left), vec!["gdelt", "pubmed"], "{left:?}");
    assert!(
        left.iter()
            .all(|n| n.prop_str("declaredBy") == Some("cadence-index")),
        "only what the spore found is left"
    );
}
