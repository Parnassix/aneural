//! End-to-end: a workspace that is a real repository grows Commit nodes joined
//! to the files each commit changed.

use aneural_core::kinds::{EdgeKind, NodeKind};
use aneural_core::{NodeId, Workspace};
use aneural_engine::{Engine, EngineEvent};
use aneural_git::fixture::{commit, init};
use aneural_store::{EdgeQuery, NodeQuery};
use std::collections::BTreeSet;
use std::path::Path;

/// A workspace that is one repository, with two files on disk and two commits
/// behind them — one of which also touched a file we do not have.
fn workspace() -> (tempfile::TempDir, Workspace) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("ws");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.ts"), "export const a = 1;\n").unwrap();
    std::fs::write(root.join("src/b.ts"), "export const b = 2;\n").unwrap();

    let repo = init(&root);
    commit(&repo, "Add a", &[("src/a.ts", "one\n")]);
    commit(
        &repo,
        "Add b and something we do not index\n\n\
         Claude-Session: https://claude.ai/code/session_01HU8BExGG2vrVEe15dkroDa",
        &[
            ("src/a.ts", "one, edited\n"),
            ("src/b.ts", "two\n"),
            ("vendor/huge.min.js", "x\n"),
        ],
    );

    let root = root.canonicalize().unwrap();
    let ws = Workspace::at(&root);
    ws.init(None, true).unwrap();
    (tmp, ws)
}

fn index(ws: &Workspace) -> Engine {
    let mut engine = Engine::open_in_memory(ws.clone()).unwrap();
    engine.index_full(false, &mut |_: EngineEvent| {}).unwrap();
    engine
}

#[test]
fn commits_become_nodes_joined_to_the_files_they_changed() {
    let (_tmp, ws) = workspace();
    let engine = index(&ws);

    let commits = engine
        .store()
        .query_nodes(&NodeQuery {
            kinds: vec![NodeKind::COMMIT.into()],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(commits.len(), 2, "both commits are nodes");

    let head = commits
        .iter()
        .find(|n| n.label.starts_with("Add b"))
        .expect("the second commit");
    assert_eq!(
        head.prop_str("session"),
        Some("01HU8BExGG2vrVEe15dkroDa"),
        "the trailer is carried onto the node, ready to join to a session"
    );
    assert_eq!(head.repo_id, Some(NodeId::dir(".")));

    let touched = engine
        .store()
        .get_edges(&EdgeQuery {
            src: Some(head.id.clone()),
            kinds: vec![EdgeKind::MODIFIES.into()],
            ..Default::default()
        })
        .unwrap();
    let mut paths: Vec<&str> = touched.iter().map(|e| e.dst.path_part()).collect();
    paths.sort();
    assert_eq!(
        paths,
        vec!["src/a.ts", "src/b.ts"],
        "vendor/huge.min.js is not in this graph, so no edge points at it"
    );
    assert_eq!(
        head.props.get("files").and_then(|v| v.as_i64()),
        Some(2),
        "the count matches the edges drawn"
    );
    assert_eq!(
        head.props.get("filesInCommit").and_then(|v| v.as_i64()),
        Some(3),
        "while the commit's real size is still recorded"
    );
}

#[test]
fn a_workspace_that_is_not_a_repository_grows_no_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("plain");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("a.ts"), "export const a = 1;\n").unwrap();
    let root = root.canonicalize().unwrap();
    let ws = Workspace::at(&root);
    ws.init(None, true).unwrap();
    let engine = index(&ws);

    assert!(
        engine
            .store()
            .query_nodes(&NodeQuery {
                kinds: vec![NodeKind::COMMIT.into()],
                ..Default::default()
            })
            .unwrap()
            .is_empty()
    );
}

/// Turning the producer off has to *retract* what it wrote, not merely stop
/// adding to it — the same rule disabling a spore follows.
#[test]
fn history_can_be_switched_off_and_the_commits_go_with_it() {
    let (_tmp, ws) = workspace();
    let engine = index(&ws);
    assert!(!nodes_of_kind(&engine, NodeKind::COMMIT).is_empty());

    let mut config = ws.load_config().unwrap();
    config.history.git = false;
    ws.save_config(&config).unwrap();

    let engine = index(&ws);
    assert!(
        nodes_of_kind(&engine, NodeKind::COMMIT).is_empty(),
        "a fresh index with history off writes no commits"
    );
}

fn nodes_of_kind(engine: &Engine, kind: &str) -> Vec<aneural_core::Node> {
    engine
        .store()
        .query_nodes(&NodeQuery {
            kinds: vec![kind.into()],
            ..Default::default()
        })
        .unwrap()
}

/// The `.git` directory itself must never become nodes: it is excluded from the
/// walker in three places and the producer reads it out of band.
#[test]
fn the_git_directory_is_not_indexed_as_files() {
    let (_tmp, ws) = workspace();
    let engine = index(&ws);
    let leaked: Vec<_> = engine
        .store()
        .query_nodes(&NodeQuery::default())
        .unwrap()
        .into_iter()
        .filter(|n| {
            n.path
                .as_deref()
                .is_some_and(|p| Path::new(p).components().any(|c| c.as_os_str() == ".git"))
        })
        .collect();
    assert!(leaked.is_empty(), "{leaked:?}");
}

// ---- sessions ------------------------------------------------------------

/// A transcript, in the shape Claude Code writes them.
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

/// Turn on the Claude producer and point it at a state directory we control.
fn with_claude(ws: &Workspace, state_dir: &Path) {
    let mut config = ws.load_config().unwrap();
    config.history.claude = true;
    config.history.claude_root = state_dir.display().to_string();
    ws.save_config(&config).unwrap();
}

fn session_fixture(ws: &Workspace, state: &Path) {
    transcript(
        &state.join("projects/-ws"),
        "11111111-2222-3333-4444-555555555555",
        ws.root(),
        &[
            serde_json::json!({
                "type": "user", "timestamp": "2026-09-17T10:00:00.000Z",
                "gitBranch": "main", "version": "2.1.272", "promptSource": "typed",
                "message": { "role": "user", "content": "Add b" }
            }),
            serde_json::json!({
                "type": "bridge-session", "sessionId": "x",
                "bridgeSessionId": "cse_01HU8BExGG2vrVEe15dkroDa"
            }),
            serde_json::json!({ "type": "ai-title", "aiTitle": "Adding b" }),
            serde_json::json!({
                "type": "user", "timestamp": "2026-09-17T10:05:00.000Z",
                "toolUseResult": {
                    "filePath": ws.root().join("src/b.ts").display().to_string(),
                    "structuredPatch": [{ "oldStart": 1, "newStart": 1, "lines": ["+b"] }]
                }
            }),
        ],
    );
    with_claude(ws, state);
}

#[test]
fn sessions_become_nodes_joined_to_the_files_they_edited() {
    let (tmp, ws) = workspace();
    let state = tmp.path().join("claude-state");
    session_fixture(&ws, &state);
    let engine = index(&ws);

    let sessions = nodes_of_kind(&engine, NodeKind::SESSION);
    assert_eq!(sessions.len(), 1);
    let s = &sessions[0];
    assert_eq!(s.label, "Adding b");
    assert_eq!(s.prop_str("branch"), Some("main"));
    assert_eq!(s.prop_str("bridge"), Some("01HU8BExGG2vrVEe15dkroDa"));
    assert_eq!(s.props.get("prompts").and_then(|v| v.as_i64()), Some(1));

    let edges = engine
        .store()
        .get_edges(&EdgeQuery {
            src: Some(s.id.clone()),
            kinds: vec![EdgeKind::MODIFIES.into()],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].dst, NodeId::file("src/b.ts"));
    assert_eq!(edges[0].props.get("via").unwrap(), "tool");
}

/// The whole point: a commit's trailer reaches the session that wrote it.
#[test]
fn a_commit_is_joined_to_the_session_its_trailer_names() {
    let (tmp, ws) = workspace();
    let state = tmp.path().join("claude-state");
    session_fixture(&ws, &state);
    let engine = index(&ws);

    let commit = nodes_of_kind(&engine, NodeKind::COMMIT)
        .into_iter()
        .find(|n| n.label.starts_with("Add b"))
        .expect("the commit with the trailer");
    let links = engine
        .store()
        .get_edges(&EdgeQuery {
            src: Some(commit.id.clone()),
            kinds: vec![EdgeKind::RELATES_TO.into()],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(links.len(), 1, "one session, not a guess between several");
    assert_eq!(
        links[0].dst,
        NodeId::session("11111111-2222-3333-4444-555555555555")
    );

    let untrailered = nodes_of_kind(&engine, NodeKind::COMMIT)
        .into_iter()
        .find(|n| n.label == "Add a")
        .expect("the first commit");
    assert!(
        engine
            .store()
            .get_edges(&EdgeQuery {
                src: Some(untrailered.id),
                kinds: vec![EdgeKind::RELATES_TO.into()],
                ..Default::default()
            })
            .unwrap()
            .is_empty(),
        "a commit with no trailer is joined to nothing, which is the normal case"
    );
}

#[test]
fn sessions_are_off_until_the_workspace_asks_for_them() {
    let (tmp, ws) = workspace();
    let state = tmp.path().join("claude-state");
    session_fixture(&ws, &state);

    let mut config = ws.load_config().unwrap();
    config.history.claude = false;
    ws.save_config(&config).unwrap();

    let engine = index(&ws);
    assert!(nodes_of_kind(&engine, NodeKind::SESSION).is_empty());
}

/// Transcripts from other workspaces must not leak in, however many there are.
#[test]
fn another_workspaces_sessions_are_not_ours() {
    let (tmp, ws) = workspace();
    let state = tmp.path().join("claude-state");
    session_fixture(&ws, &state);
    transcript(
        &state.join("projects/-elsewhere"),
        "99999999-9999-9999-9999-999999999999",
        Path::new("/somewhere/else"),
        &[serde_json::json!({
            "type": "user", "timestamp": "2026-09-17T10:00:00.000Z",
            "promptSource": "typed", "message": { "content": "not yours" }
        })],
    );

    let engine = index(&ws);
    let sessions = nodes_of_kind(&engine, NodeKind::SESSION);
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].id,
        NodeId::session("11111111-2222-3333-4444-555555555555")
    );
}

// ---- plans ---------------------------------------------------------------

fn plan_file(state: &Path, name: &str, body: &str) {
    let dir = state.join("plans");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), body).unwrap();
}

/// A session that approved a plan naming one file we have and one we do not.
fn plan_fixture(ws: &Workspace, state: &Path) {
    plan_file(
        state,
        "wise-phoenix.md",
        "# Rewrite the layout\n\n\
         Rewrite `src/b.ts` from scratch, and leave `vendor/huge.min.js` alone.\n",
    );
    transcript(
        &state.join("projects/-ws"),
        "11111111-2222-3333-4444-555555555555",
        ws.root(),
        &[
            serde_json::json!({
                "type": "user", "timestamp": "2026-09-17T10:00:00.000Z",
                "gitBranch": "main", "promptSource": "typed",
                "message": { "role": "user", "content": "Plan it" }
            }),
            serde_json::json!({
                "type": "bridge-session", "sessionId": "x",
                "bridgeSessionId": "cse_01HU8BExGG2vrVEe15dkroDa"
            }),
            serde_json::json!({ "type": "ai-title", "aiTitle": "Rewriting the layout" }),
            serde_json::json!({
                "type": "assistant", "timestamp": "2026-09-17T10:01:00.000Z",
                "message": { "content": [{
                    "type": "tool_use", "name": "ExitPlanMode",
                    "input": { "plan": "# Rewrite the layout", "planFilePath": "/x/plans/wise-phoenix.md" }
                }]}
            }),
            serde_json::json!({
                "type": "user", "timestamp": "2026-09-17T10:05:00.000Z",
                "toolUseResult": {
                    "filePath": ws.root().join("src/b.ts").display().to_string(),
                    "structuredPatch": [{ "oldStart": 1, "newStart": 1, "lines": ["+b"] }]
                }
            }),
        ],
    );
    with_claude(ws, state);
}

#[test]
fn a_plan_becomes_a_node_annotating_the_files_it_named() {
    let (tmp, ws) = workspace();
    let state = tmp.path().join("claude-state");
    plan_fixture(&ws, &state);
    let engine = index(&ws);

    let plans: Vec<_> = nodes_of_kind(&engine, NodeKind::PLAN)
        .into_iter()
        .filter(|n| n.id.path_part().starts_with("~/"))
        .collect();
    assert_eq!(plans.len(), 1);
    let plan = &plans[0];
    assert_eq!(plan.id, NodeId::home_plan("wise-phoenix.md"));
    assert_eq!(plan.label, "Rewrite the layout");
    assert_eq!(plan.props.get("names").and_then(|v| v.as_i64()), Some(2));
    assert_eq!(
        plan.props.get("namesInGraph").and_then(|v| v.as_i64()),
        Some(1),
        "it named a file this workspace does not have"
    );

    let annotates = engine
        .store()
        .get_edges(&EdgeQuery {
            src: Some(plan.id.clone()),
            kinds: vec![EdgeKind::ANNOTATES.into()],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(annotates.len(), 1);
    assert_eq!(annotates[0].dst, NodeId::file("src/b.ts"));
    assert_eq!(
        annotates[0].props.get("via").unwrap(),
        "mentioned",
        "marked as read out of prose, not from a frontmatter list"
    );
}

#[test]
fn the_session_and_the_commit_both_realize_the_plan() {
    let (tmp, ws) = workspace();
    let state = tmp.path().join("claude-state");
    plan_fixture(&ws, &state);
    let engine = index(&ws);

    let plan = NodeId::home_plan("wise-phoenix.md");
    let realizes = engine
        .store()
        .get_edges(&EdgeQuery {
            dst: Some(plan.clone()),
            kinds: vec![EdgeKind::REALIZES.into()],
            ..Default::default()
        })
        .unwrap();

    let by_session: Vec<_> = realizes
        .iter()
        .filter(|e| e.src.prefix() == "session")
        .collect();
    assert_eq!(by_session.len(), 1, "the session that approved it");
    assert_eq!(by_session[0].props.get("via").unwrap(), "plan-mode");

    let by_commit: Vec<_> = realizes
        .iter()
        .filter(|e| e.src.prefix() == "commit")
        .collect();
    assert_eq!(
        by_commit.len(),
        1,
        "and the commit that session produced, through the chain"
    );
    assert_eq!(by_commit[0].props.get("via").unwrap(), "session");
}

/// The three-way split the whole feature exists to show, straight off the graph.
#[test]
fn plan_against_reality_is_a_difference_of_two_edge_sets() {
    let (tmp, ws) = workspace();
    let state = tmp.path().join("claude-state");
    plan_fixture(&ws, &state);
    let engine = index(&ws);

    let plan = NodeId::home_plan("wise-phoenix.md");
    let named: BTreeSet<String> = engine
        .store()
        .get_edges(&EdgeQuery {
            src: Some(plan),
            kinds: vec![EdgeKind::ANNOTATES.into()],
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .map(|e| e.dst.path_part().to_string())
        .collect();
    let session = NodeId::session("11111111-2222-3333-4444-555555555555");
    let touched: BTreeSet<String> = engine
        .store()
        .get_edges(&EdgeQuery {
            src: Some(session),
            kinds: vec![EdgeKind::MODIFIES.into()],
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .map(|e| e.dst.path_part().to_string())
        .collect();

    assert_eq!(
        named.intersection(&touched).collect::<Vec<_>>(),
        vec!["src/b.ts"],
        "named and touched"
    );
    assert!(
        named.difference(&touched).next().is_none(),
        "nothing was named and then left alone"
    );
    assert!(
        touched.difference(&named).next().is_none(),
        "and nothing was touched that the plan did not name"
    );
}

#[test]
fn a_plan_reports_that_commits_carried_it_out() {
    let (tmp, ws) = workspace();
    let state = tmp.path().join("claude-state");
    plan_fixture(&ws, &state);
    let engine = index(&ws);

    let plan = engine
        .store()
        .get_node(&NodeId::home_plan("wise-phoenix.md"))
        .unwrap()
        .unwrap();
    assert_eq!(plan.prop_str("state"), Some("built"));
    assert_eq!(
        plan.prop_str("approvedAt"),
        Some("2026-09-17T10:01:00.000Z"),
        "when the plan was approved, not when the session began"
    );
}

/// The global plans directory holds plans from every project on the machine.
/// Only the ones a session of *this* workspace named may be read.
#[test]
fn a_plan_no_session_here_named_is_not_ours_to_read() {
    let (tmp, ws) = workspace();
    let state = tmp.path().join("claude-state");
    plan_fixture(&ws, &state);
    plan_file(
        &state,
        "someone-elses.md",
        "# Not our plan\n\nTouch `src/a.ts`.\n",
    );

    let engine = index(&ws);
    let plans: Vec<_> = nodes_of_kind(&engine, NodeKind::PLAN)
        .into_iter()
        .map(|n| n.id)
        .collect();
    assert_eq!(plans, vec![NodeId::home_plan("wise-phoenix.md")]);
}

#[test]
fn turning_sessions_off_takes_the_plans_with_them() {
    let (tmp, ws) = workspace();
    let state = tmp.path().join("claude-state");
    plan_fixture(&ws, &state);
    assert!(!nodes_of_kind(&index(&ws), NodeKind::PLAN).is_empty());

    let mut config = ws.load_config().unwrap();
    config.history.claude = false;
    ws.save_config(&config).unwrap();

    assert!(nodes_of_kind(&index(&ws), NodeKind::PLAN).is_empty());
}

/// A session runs for days and approves several plans along the way, so being
/// attributed to the session is not enough — found on real data, where a plan
/// approved on the 23rd was credited with commits from the 17th.
#[test]
fn a_commit_made_before_the_plan_existed_does_not_realize_it() {
    let (tmp, ws) = workspace();
    let state = tmp.path().join("claude-state");
    plan_fixture(&ws, &state);
    // Approved after every commit in the fixture repository.
    plan_file(&state, "later.md", "# Much later\n\nTouch `src/a.ts`.\n");
    transcript(
        &state.join("projects/-ws"),
        "22222222-2222-2222-2222-222222222222",
        ws.root(),
        &[
            serde_json::json!({
                "type": "user", "timestamp": "2026-09-23T10:00:00.000Z",
                "promptSource": "typed", "message": { "content": "Plan it later" }
            }),
            serde_json::json!({
                "type": "bridge-session", "sessionId": "y",
                "bridgeSessionId": "cse_01HU8BExGG2vrVEe15dkroDa"
            }),
            serde_json::json!({
                "type": "assistant", "timestamp": "2026-09-23T10:01:00.000Z",
                "message": { "content": [{
                    "type": "tool_use", "name": "ExitPlanMode",
                    "input": { "planFilePath": "/x/plans/later.md" }
                }]}
            }),
        ],
    );

    let engine = index(&ws);
    let later = engine
        .store()
        .get_node(&NodeId::home_plan("later.md"))
        .unwrap()
        .expect("the later plan is in the graph");
    assert_eq!(
        later.prop_str("state"),
        Some("proposed"),
        "nothing has been built for it yet"
    );
    let realized = engine
        .store()
        .get_edges(&EdgeQuery {
            dst: Some(later.id.clone()),
            kinds: vec![EdgeKind::REALIZES.into()],
            ..Default::default()
        })
        .unwrap();
    assert!(
        realized.iter().all(|e| e.src.prefix() == "session"),
        "only the session that approved it, never an earlier commit: {realized:?}"
    );
}

/// A full index must *emit* everything it knows, not merely have it in the
/// store — a fresh consumer builds its whole graph from the deltas. Caught by
/// screenshot: the second run of the GUI showed one session instead of seven,
/// because only the transcript still being written to had anything new to read.
#[test]
fn a_second_index_still_sends_the_sessions_to_a_fresh_consumer() {
    let (tmp, ws) = workspace();
    let state = tmp.path().join("claude-state");
    plan_fixture(&ws, &state);

    // `Engine::open` uses the workspace's own on-disk cache, which is what a
    // second run of the GUI gets.
    let mut first = Engine::open(ws.clone()).unwrap();
    first.index_full(false, &mut |_: EngineEvent| {}).unwrap();
    drop(first);

    // Second run, warm cache, nothing appended to any transcript.
    let mut seen = Vec::new();
    let mut second = Engine::open(ws.clone()).unwrap();
    second
        .index_full(false, &mut |e: EngineEvent| {
            if let EngineEvent::Delta(d) = e {
                seen.extend(d.nodes.iter().map(|n| (n.kind.clone(), n.id.clone())));
            }
        })
        .unwrap();

    for kind in [NodeKind::SESSION, NodeKind::COMMIT, NodeKind::PLAN] {
        assert!(
            seen.iter().any(|(k, _)| k == kind),
            "a warm index emitted no {kind}: {:?}",
            seen.iter().map(|(k, _)| k).collect::<BTreeSet<_>>()
        );
    }
    assert!(
        seen.iter()
            .any(|(_, id)| *id == NodeId::session("11111111-2222-3333-4444-555555555555")),
        "and it is the session we already had, replayed from the cache"
    );
}
