//! End-to-end: what the sessions in a workspace cost, rolled up to each
//! repository and to the workspace as a whole.

use aneural_core::kinds::{EdgeKind, NodeKind};
use aneural_core::{Node, NodeId, Workspace};
use aneural_engine::{Engine, EngineEvent};
use aneural_git::fixture::{commit, init};
use aneural_store::{EdgeQuery, NodeQuery};
use std::path::Path;

/// A workspace holding two repositories: the root, and one nested inside it.
/// The case a single-repo workspace cannot show — tokens have to land in one
/// of them and not both.
fn workspace() -> (tempfile::TempDir, Workspace) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("ws");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("packages/lib/src")).unwrap();
    std::fs::write(root.join("src/a.ts"), "export const a = 1;\n").unwrap();
    std::fs::write(root.join("packages/lib/src/b.ts"), "export const b = 2;\n").unwrap();

    let outer = init(&root);
    commit(&outer, "Add a", &[("src/a.ts", "one\n")]);
    let inner = init(&root.join("packages/lib"));
    commit(&inner, "Add b", &[("src/b.ts", "two\n")]);

    let root = root.canonicalize().unwrap();
    let ws = Workspace::at(&root);
    ws.init(None, true).unwrap();
    (tmp, ws)
}

fn transcript(dir: &Path, uuid: &str, cwd: &Path, lines: &[serde_json::Value]) {
    std::fs::create_dir_all(dir).unwrap();
    let mut body = String::new();
    for line in lines {
        let mut line = line.clone();
        line.as_object_mut()
            .unwrap()
            .insert("cwd".into(), cwd.display().to_string().into());
        body.push_str(&format!("{line}\n"));
    }
    std::fs::write(dir.join(format!("{uuid}.jsonl")), body).unwrap();
}

/// An assistant message that cost `out` output tokens and read `cache` cached.
fn spent(at: &str, model: &str, out: u64, cache: u64) -> serde_json::Value {
    serde_json::json!({
        "type": "assistant", "timestamp": at,
        "message": {
            "role": "assistant", "model": model,
            "content": [{ "type": "text", "text": "ok" }],
            "usage": {
                "input_tokens": 1, "output_tokens": out,
                "cache_creation_input_tokens": 10, "cache_read_input_tokens": cache
            }
        }
    })
}

fn touched(at: &str, file: &Path) -> serde_json::Value {
    serde_json::json!({
        "type": "user", "timestamp": at,
        "toolUseResult": {
            "filePath": file.display().to_string(),
            "structuredPatch": [{ "oldStart": 1, "newStart": 1, "lines": ["+x"] }]
        }
    })
}

fn index(ws: &Workspace) -> Engine {
    let mut engine = Engine::open_in_memory(ws.clone()).unwrap();
    engine.index_full(false, &mut |_: EngineEvent| {}).unwrap();
    engine
}

fn nodes_of_kind(engine: &Engine, kind: &str) -> Vec<Node> {
    engine
        .store()
        .query_nodes(&NodeQuery {
            kinds: vec![kind.into()],
            ..Default::default()
        })
        .unwrap()
}

fn by_id<'a>(nodes: &'a [Node], id: &NodeId) -> &'a Node {
    nodes.iter().find(|n| &n.id == id).unwrap_or_else(|| {
        panic!(
            "no {id}, have {:?}",
            nodes.iter().map(|n| &n.id).collect::<Vec<_>>()
        )
    })
}

/// Two sessions: one rooted at the workspace, one inside the vendored
/// repository, and the second also reaches out and edits a file in the first.
fn sessions(ws: &Workspace, state: &Path) {
    transcript(
        &state.join("projects/-ws"),
        "11111111-0000-0000-0000-000000000000",
        ws.root(),
        &[
            serde_json::json!({
                "type": "user", "timestamp": "2026-09-20T10:00:00.000Z",
                "promptSource": "typed", "message": { "role": "user", "content": "go" }
            }),
            spent("2026-09-20T10:00:01.000Z", "claude-opus-5", 1_000, 50_000),
            touched("2026-09-20T10:01:00.000Z", &ws.root().join("src/a.ts")),
        ],
    );
    transcript(
        &state.join("projects/-ws-packages-lib"),
        "22222222-0000-0000-0000-000000000000",
        &ws.root().join("packages/lib"),
        &[
            serde_json::json!({
                "type": "user", "timestamp": "2026-09-20T11:00:00.000Z",
                "promptSource": "typed", "message": { "role": "user", "content": "go" }
            }),
            spent("2026-09-20T11:00:01.000Z", "claude-opus-5", 200, 1_000),
            spent(
                "2026-09-20T11:00:02.000Z",
                "claude-haiku-4-5-20251001",
                50,
                0,
            ),
            touched(
                "2026-09-20T11:01:00.000Z",
                &ws.root().join("packages/lib/src/b.ts"),
            ),
            // reaching up into the outer repository
            touched("2026-09-20T11:02:00.000Z", &ws.root().join("src/a.ts")),
        ],
    );
    let mut config = ws.load_config().unwrap();
    config.history.claude = true;
    config.history.claude_root = state.display().to_string();
    ws.save_config(&config).unwrap();
}

#[test]
fn every_repository_and_the_workspace_get_a_tally() {
    let (tmp, ws) = workspace();
    let state = tmp.path().join("claude-state");
    sessions(&ws, &state);
    let engine = index(&ws);

    let tallies = nodes_of_kind(&engine, NodeKind::USAGE);
    assert_eq!(tallies.len(), 3, "the workspace and both repositories");

    let whole = by_id(&tallies, &NodeId::workspace_usage());
    let outer = by_id(&tallies, &NodeId::repo_usage("."));
    let inner = by_id(&tallies, &NodeId::repo_usage("packages/lib"));

    // Session one: 1 in + 1000 out + 10 written + 50000 read.
    assert_eq!(outer.prop_i64("tokens"), Some(51_011));
    assert_eq!(outer.prop_i64("sessions"), Some(1));
    // Session two, both its messages: 2 in + 250 out + 20 written + 1000 read.
    assert_eq!(inner.prop_i64("tokens"), Some(1_272));
    assert_eq!(inner.prop_i64("sessions"), Some(1));
    // The workspace is the two of them, counted once each.
    assert_eq!(whole.prop_i64("tokens"), Some(51_011 + 1_272));
    assert_eq!(whole.prop_i64("sessions"), Some(2));
    assert_eq!(whole.prop_i64("messages"), Some(3));
}

#[test]
fn a_session_that_reached_into_another_repository_says_so_rather_than_being_split() {
    let (tmp, ws) = workspace();
    let state = tmp.path().join("claude-state");
    sessions(&ws, &state);
    let engine = index(&ws);

    let tallies = nodes_of_kind(&engine, NodeKind::USAGE);
    let outer = by_id(&tallies, &NodeId::repo_usage("."));
    // The vendored session edited a file up here, but its tokens stayed with
    // the repository it was actually working in. The footnote is what says so.
    assert_eq!(outer.prop_i64("sessionsElsewhere"), Some(1));
    assert_eq!(
        by_id(&tallies, &NodeId::repo_usage("packages/lib")).prop_i64("sessionsElsewhere"),
        None
    );
}

#[test]
fn a_tally_hangs_off_the_place_it_is_about() {
    let (tmp, ws) = workspace();
    let state = tmp.path().join("claude-state");
    sessions(&ws, &state);
    let engine = index(&ws);

    let edges = engine
        .store()
        .get_edges(&EdgeQuery {
            kinds: vec![EdgeKind::ANNOTATES.into()],
            ..Default::default()
        })
        .unwrap();
    let from_tally: Vec<&aneural_core::Edge> = edges
        .iter()
        .filter(|e| e.src.as_str().starts_with("usage:"))
        .collect();
    assert_eq!(from_tally.len(), 3);
    assert!(from_tally.iter().any(
        |e| e.src == NodeId::repo_usage("packages/lib") && e.dst == NodeId::dir("packages/lib")
    ));
    assert!(
        from_tally
            .iter()
            .any(|e| e.src == NodeId::workspace_usage() && e.dst == NodeId::dir("."))
    );
}

#[test]
fn turning_the_producer_off_retracts_the_tallies() {
    let (tmp, ws) = workspace();
    let state = tmp.path().join("claude-state");
    sessions(&ws, &state);
    let engine = index(&ws);
    assert_eq!(nodes_of_kind(&engine, NodeKind::USAGE).len(), 3);

    let mut config = ws.load_config().unwrap();
    config.history.claude = false;
    ws.save_config(&config).unwrap();
    let mut engine = Engine::open_in_memory(ws.clone()).unwrap();
    engine.index_full(false, &mut |_: EngineEvent| {}).unwrap();
    assert!(nodes_of_kind(&engine, NodeKind::USAGE).is_empty());
}

/// A workspace that is one repository would otherwise grow two tallies saying
/// exactly the same thing, sitting on top of each other on the canvas.
#[test]
fn one_repository_at_the_root_gets_one_tally_not_two() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("ws");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.ts"), "export const a = 1;\n").unwrap();
    let repo = init(&root);
    commit(&repo, "Add a", &[("src/a.ts", "one\n")]);
    let root = root.canonicalize().unwrap();
    let ws = Workspace::at(&root);
    ws.init(None, true).unwrap();

    let state = tmp.path().join("claude-state");
    transcript(
        &state.join("projects/-ws"),
        "33333333-0000-0000-0000-000000000000",
        ws.root(),
        &[spent("2026-09-20T10:00:01.000Z", "claude-opus-5", 100, 0)],
    );
    let mut config = ws.load_config().unwrap();
    config.history.claude = true;
    config.history.claude_root = state.display().to_string();
    ws.save_config(&config).unwrap();

    let engine = index(&ws);
    let tallies = nodes_of_kind(&engine, NodeKind::USAGE);
    assert_eq!(tallies.len(), 1, "{tallies:?}");
    assert_eq!(tallies[0].id, NodeId::repo_usage("."));
}
