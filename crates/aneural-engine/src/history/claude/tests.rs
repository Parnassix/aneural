use super::*;
use std::io::Write;

/// A transcript line, in the shape the real files use.
fn line(json: serde_json::Value) -> String {
    format!("{json}\n")
}

fn write(path: &Path, lines: &[String]) {
    let mut f = std::fs::File::create(path).unwrap();
    for l in lines {
        f.write_all(l.as_bytes()).unwrap();
    }
}

fn append(path: &Path, lines: &[String]) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    for l in lines {
        f.write_all(l.as_bytes()).unwrap();
    }
}

fn prompt(at: &str, text: &str) -> String {
    line(serde_json::json!({
        "type": "user", "timestamp": at, "cwd": "/ws", "gitBranch": "main",
        "version": "2.1.272", "promptSource": "typed",
        "message": { "role": "user", "content": text }
    }))
}

fn tool_result(at: &str, file: &str) -> String {
    line(serde_json::json!({
        "type": "user", "timestamp": at, "cwd": "/ws",
        "toolUseResult": {
            "type": "update", "filePath": file,
            "structuredPatch": [{ "oldStart": 1, "newStart": 1, "lines": ["-a", "+b"] }]
        }
    }))
}

/// A line that is neither a prompt nor an edit: the bulk of a real transcript.
fn noise(at: &str, size: usize) -> String {
    line(serde_json::json!({
        "type": "assistant", "timestamp": at, "cwd": "/ws",
        "message": { "content": [{ "type": "text", "text": "x".repeat(size) }] }
    }))
}

/// An assistant message that cost something, in the shape the API reports.
fn spent(at: &str, model: &str, out: u64, cache_read: u64) -> String {
    line(serde_json::json!({
        "type": "assistant", "timestamp": at, "cwd": "/ws",
        "message": {
            "role": "assistant", "model": model,
            "content": [{ "type": "text", "text": "ok" }],
            "usage": {
                "input_tokens": 2,
                "output_tokens": out,
                "output_tokens_details": { "thinking_tokens": out / 2 },
                "cache_creation_input_tokens": 100,
                "cache_read_input_tokens": cache_read
            }
        }
    }))
}

/// The tool call Claude Code writes when a plan is approved.
fn approve(at: &str, plan: &str) -> String {
    line(serde_json::json!({
        "type": "assistant", "timestamp": at, "cwd": "/ws",
        "message": { "role": "assistant", "model": "claude-opus-5", "content": [{
            "type": "tool_use", "name": "ExitPlanMode",
            "input": { "planFilePath": format!("/home/u/.claude/plans/{plan}") }
        }]}
    }))
}

fn fixture(dir: &Path) -> PathBuf {
    let path = dir.join("11111111-2222-3333-4444-555555555555.jsonl");
    write(
        &path,
        &[
            prompt("2026-09-17T10:00:00.000Z", "Do the thing"),
            noise("2026-09-17T10:00:01.000Z", 500),
            line(serde_json::json!({
                "type": "bridge-session", "sessionId": "s",
                "bridgeSessionId": "cse_01HU8BExGG2vrVEe15dkroDa"
            })),
            line(serde_json::json!({ "type": "ai-title", "aiTitle": "Doing the thing" })),
            tool_result("2026-09-17T10:05:00.000Z", "/ws/src/a.ts"),
            line(serde_json::json!({
                "type": "file-history-delta", "timestamp": "2026-09-17T10:06:00.000Z",
                "trackingPath": "src/b.ts",
                "backup": { "backupFileName": "abc@v1", "version": 1 }
            })),
        ],
    );
    path
}

fn read(path: &Path) -> State {
    let mut state = State {
        session: Session {
            uuid: uuid_of(path).unwrap(),
            ..Default::default()
        },
        ..Default::default()
    };
    advance(path, &mut state).unwrap();
    state
}

#[test]
fn a_transcript_yields_the_session_its_prompts_and_the_files_it_touched() {
    let tmp = tempfile::tempdir().unwrap();
    let path = fixture(tmp.path());
    let s = read(&path).session;

    assert_eq!(s.title.as_deref(), Some("Doing the thing"));
    assert_eq!(s.branch.as_deref(), Some("main"));
    assert_eq!(s.cwd.as_deref(), Some("/ws"));
    assert_eq!(s.version.as_deref(), Some("2.1.272"));
    assert_eq!(
        s.bridge.as_deref(),
        Some("01HU8BExGG2vrVEe15dkroDa"),
        "reduced to what a commit trailer would name"
    );
    assert_eq!(s.prompts, 1, "the assistant's turn is not a prompt");
    assert_eq!(s.started, "2026-09-17T10:00:00.000Z");
    assert_eq!(s.ended, "2026-09-17T10:06:00.000Z");

    let touched: Vec<_> = s.touched.keys().cloned().collect();
    assert_eq!(touched, vec!["/ws/src/a.ts", "src/b.ts"]);
    assert_eq!(s.touched["/ws/src/a.ts"].via, "tool");
    assert_eq!(s.touched["src/b.ts"].via, "backup");
}

/// Entering plan mode is a thing worth reacting to, and the latch line is the
/// only record of it: `ExitPlanMode` marks the *end* of the thinking.
#[test]
fn the_permission_mode_latch_says_what_a_session_is_doing_now() {
    let tmp = tempfile::tempdir().unwrap();
    let path = fixture(tmp.path());
    assert_eq!(read(&path).session.mode, "", "nothing latched yet");

    append(
        &path,
        &[line(serde_json::json!({
            "type": "permission-mode", "permissionMode": "plan", "sessionId": "x"
        }))],
    );
    let s = read(&path).session;
    assert_eq!(s.mode, "plan");
    // A latch line carries no timestamp, so it must not move the session's ends.
    assert_eq!(s.ended, "2026-09-17T10:06:00.000Z");

    // The last one wins: this is a state log, not a history.
    append(
        &path,
        &[
            line(serde_json::json!({
                "type": "permission-mode", "permissionMode": "auto", "sessionId": "x"
            })),
            line(serde_json::json!({
                "type": "permission-mode", "permissionMode": "", "sessionId": "x"
            })),
        ],
    );
    assert_eq!(
        read(&path).session.mode,
        "auto",
        "an empty latch is not a mode"
    );
}

/// The whole point of the offset: appending must cost only the new bytes, and
/// must not double anything already counted.
#[test]
fn appending_reads_only_what_is_new() {
    let tmp = tempfile::tempdir().unwrap();
    let path = fixture(tmp.path());
    let mut state = read(&path);
    let first_offset = state.tail.offset;
    assert_eq!(state.session.prompts, 1);

    assert_eq!(
        advance(&path, &mut state).unwrap(),
        None,
        "an unchanged file reports nothing to do"
    );
    assert_eq!(state.tail.offset, first_offset);
    assert_eq!(state.session.prompts, 1, "and nothing is counted twice");

    append(
        &path,
        &[
            prompt("2026-09-17T11:00:00.000Z", "And another"),
            tool_result("2026-09-17T11:01:00.000Z", "/ws/src/a.ts"),
        ],
    );
    assert!(advance(&path, &mut state).unwrap().is_some());
    assert!(state.tail.offset > first_offset);
    assert_eq!(state.session.prompts, 2);
    assert_eq!(
        state.session.touched["/ws/src/a.ts"].count, 2,
        "the same file touched twice is one entry with a count"
    );
    assert_eq!(
        state.session.touched["/ws/src/a.ts"].last,
        "2026-09-17T11:01:00.000Z"
    );
    assert_eq!(state.session.ended, "2026-09-17T11:01:00.000Z");
}

/// Reading from a stale offset must land on exactly the same graph. This is the
/// test that catches a counter kept somewhere it cannot be rewritten.
#[test]
fn replaying_from_a_stale_offset_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let path = fixture(tmp.path());
    let once = read(&path);

    let mut twice = State {
        session: Session {
            uuid: once.session.uuid.clone(),
            ..Default::default()
        },
        ..Default::default()
    };
    advance(&path, &mut twice).unwrap();
    // Wind the offset back to the start and read the whole file again.
    twice.tail.offset = 0;
    twice.session = Session {
        uuid: once.session.uuid.clone(),
        ..Default::default()
    };
    advance(&path, &mut twice).unwrap();

    assert_eq!(once, twice, "a re-read is not a double count");
}

#[test]
fn a_half_written_line_waits_for_the_rest_of_itself() {
    let tmp = tempfile::tempdir().unwrap();
    let path = fixture(tmp.path());
    let mut state = read(&path);
    let settled = state.tail.offset;

    // A line arriving in two pieces, as a writer flushing mid-record would.
    let whole = prompt("2026-09-17T12:00:00.000Z", "Half now");
    let (head, tail) = whole.split_at(whole.len() / 2);
    append(&path, &[head.to_string()]);
    advance(&path, &mut state).unwrap();
    assert_eq!(
        state.tail.offset, settled,
        "nothing is consumed until the newline arrives"
    );
    assert_eq!(state.session.prompts, 1);

    append(&path, &[tail.to_string()]);
    advance(&path, &mut state).unwrap();
    assert!(state.tail.offset > settled);
    assert_eq!(state.session.prompts, 2, "and then it counts exactly once");
}

#[test]
fn a_replaced_file_is_read_from_the_beginning_again() {
    let tmp = tempfile::tempdir().unwrap();
    let path = fixture(tmp.path());
    let mut state = read(&path);
    assert_eq!(state.session.prompts, 1);

    // Rotated: same name, different content, shorter.
    write(&path, &[prompt("2026-09-18T09:00:00.000Z", "Fresh start")]);
    advance(&path, &mut state).unwrap();

    assert_eq!(
        state.session.prompts, 1,
        "counted from scratch, not added to"
    );
    assert!(
        state.session.touched.is_empty(),
        "and the old file's touches are gone"
    );
    assert_eq!(state.session.started, "2026-09-18T09:00:00.000Z");
}

#[test]
fn an_unparseable_line_is_stepped_over_rather_than_wedging_the_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = fixture(tmp.path());
    let mut state = read(&path);

    append(
        &path,
        &[
            "{ this is not json but it mentions promptSource\n".to_string(),
            prompt("2026-09-17T13:00:00.000Z", "After the mess"),
        ],
    );
    advance(&path, &mut state).unwrap();
    assert_eq!(
        state.session.prompts, 2,
        "the good line after the bad one is still read"
    );
}

#[test]
fn an_enormous_line_is_skipped_without_being_held_in_memory() {
    let tmp = tempfile::tempdir().unwrap();
    let path = fixture(tmp.path());
    let mut state = read(&path);
    let before = state.tail.offset;

    append(
        &path,
        &[
            noise("2026-09-17T14:00:00.000Z", (MAX_LINE as usize) + 1024),
            prompt("2026-09-17T14:01:00.000Z", "After the monster"),
        ],
    );
    advance(&path, &mut state).unwrap();

    assert!(state.tail.offset > before + MAX_LINE);
    assert_eq!(state.session.prompts, 2, "the line after it still lands");
}

#[test]
fn a_plan_approved_in_the_session_is_recorded() {
    let tmp = tempfile::tempdir().unwrap();
    let path = fixture(tmp.path());
    let mut state = read(&path);

    let exit = line(serde_json::json!({
        "type": "assistant", "timestamp": "2026-09-17T15:00:00.000Z", "cwd": "/ws",
        "message": { "content": [{
            "type": "tool_use", "name": "ExitPlanMode",
            "input": { "plan": "# Do it", "planFilePath": "/Users/someone/.claude/plans/wise-phoenix.md" }
        }]}
    }));
    append(&path, &[exit.clone(), exit]);
    advance(&path, &mut state).unwrap();

    assert_eq!(
        state.session.plans.keys().collect::<Vec<_>>(),
        vec!["wise-phoenix.md"],
        "named once however many times it was approved"
    );
    assert_eq!(
        state.session.plans["wise-phoenix.md"], "2026-09-17T15:00:00.000Z",
        "and dated from the first approval, not the session's start"
    );
}

#[test]
fn only_transcripts_from_this_workspace_are_picked_up() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join(".claude");
    let mine = state_dir.join("projects/-ws");
    let theirs = state_dir.join("projects/-elsewhere");
    std::fs::create_dir_all(&mine).unwrap();
    std::fs::create_dir_all(&theirs).unwrap();

    write(
        &mine.join("a.jsonl"),
        &[prompt("2026-09-17T10:00:00Z", "hi")],
    );
    write(
        &theirs.join("b.jsonl"),
        &[line(serde_json::json!({
            "type": "user", "cwd": "/somewhere/else", "timestamp": "2026-09-17T10:00:00Z",
            "promptSource": "typed", "message": {"content": "hi"}
        }))],
    );
    // A stray file that is not a transcript at all.
    std::fs::write(mine.join("notes.txt"), "not jsonl").unwrap();

    let found = transcripts_for(&state_dir, Path::new("/ws"));
    assert_eq!(found.len(), 1);
    assert!(found[0].ends_with("a.jsonl"));
}

#[test]
fn a_missing_state_directory_is_simply_no_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(transcripts_for(&tmp.path().join("nope"), Path::new("/ws")).is_empty());
}

#[test]
fn a_session_becomes_a_node_and_an_edge_per_file_we_have() {
    let tmp = tempfile::tempdir().unwrap();
    let path = fixture(tmp.path());
    let session = read(&path).session;

    let rel = |p: &str| {
        p.strip_prefix("/ws/")
            .map(String::from)
            .or_else(|| (!p.starts_with('/')).then(|| p.to_string()))
    };
    let (node, edges) = to_graph(&session, &rel, &|id| id != &NodeId::file("src/b.ts"));

    assert_eq!(node.kind, NodeKind::SESSION);
    assert_eq!(node.source, Source::CLAUDE);
    assert_eq!(node.label, "Doing the thing");
    assert_eq!(node.prop_str("bridge"), Some("01HU8BExGG2vrVEe15dkroDa"));
    assert_eq!(node.props.get("prompts").and_then(|v| v.as_i64()), Some(1));

    assert_eq!(edges.len(), 1, "b.ts is not in the graph");
    assert_eq!(edges[0].dst, NodeId::file("src/a.ts"));
    assert_eq!(edges[0].kind, EdgeKind::MODIFIES);
    assert_eq!(edges[0].props.get("via").unwrap(), "tool");
    assert_eq!(
        node.props.get("files").and_then(|v| v.as_i64()),
        Some(1),
        "the count follows the edges actually drawn"
    );
    assert_eq!(
        node.props.get("filesTouched").and_then(|v| v.as_i64()),
        Some(2),
        "while what the session really touched is kept"
    );
}

#[test]
fn a_session_with_no_title_falls_back_to_its_id() {
    let session = Session {
        uuid: "11111111-2222-3333".into(),
        ..Default::default()
    };
    let (node, edges) = to_graph(&session, &|_| None, &|_| true);
    assert_eq!(node.label, "11111111");
    assert!(edges.is_empty());
}

#[test]
fn state_round_trips_through_json() {
    let tmp = tempfile::tempdir().unwrap();
    let path = fixture(tmp.path());
    let state = read(&path);
    let json = serde_json::to_string(&state).unwrap();
    let back: State = serde_json::from_str(&json).unwrap();
    assert_eq!(state, back);
}

#[test]
fn tokens_are_tallied_per_model() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp
        .path()
        .join("aaaaaaaa-0000-0000-0000-000000000000.jsonl");
    write(
        &path,
        &[
            prompt("2026-09-20T10:00:00.000Z", "go"),
            spent("2026-09-20T10:00:01.000Z", "claude-opus-5", 100, 5_000),
            spent("2026-09-20T10:00:02.000Z", "claude-opus-5", 300, 9_000),
            spent(
                "2026-09-20T10:00:03.000Z",
                "claude-haiku-4-5-20251001",
                40,
                80,
            ),
            // a synthetic message with an all-zero usage block is not a message
            line(serde_json::json!({
                "type": "assistant", "timestamp": "2026-09-20T10:00:04.000Z", "cwd": "/ws",
                "message": { "role": "assistant", "model": "<synthetic>", "content": [],
                             "usage": { "input_tokens": 0, "output_tokens": 0 } }
            })),
        ],
    );
    let s = read(&path).session;

    let opus = s.usage.get("claude-opus-5").copied().unwrap();
    assert_eq!(opus.messages, 2);
    assert_eq!(opus.output, 400);
    assert_eq!(
        opus.thinking, 200,
        "thinking is a part of output, not an extra"
    );
    assert_eq!(opus.cache_read, 14_000);
    assert_eq!(opus.cache_write, 200);
    assert_eq!(opus.input, 4);
    assert_eq!(opus.total(), 4 + 400 + 200 + 14_000);

    assert_eq!(
        s.usage.get("claude-haiku-4-5-20251001").unwrap().messages,
        1
    );
    assert!(!s.usage.contains_key("<synthetic>"), "{:?}", s.usage);
    assert_eq!(s.spent().messages, 3);
}

#[test]
fn a_plan_is_charged_for_the_stretch_of_work_that_ran_under_it() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp
        .path()
        .join("bbbbbbbb-0000-0000-0000-000000000000.jsonl");
    write(
        &path,
        &[
            // before any plan: this belongs to nobody
            spent("2026-09-20T09:00:00.000Z", "claude-opus-5", 10, 0),
            approve("2026-09-20T10:00:00.000Z", "first-plan.md"),
            spent("2026-09-20T10:01:00.000Z", "claude-opus-5", 100, 0),
            spent("2026-09-20T10:02:00.000Z", "claude-opus-5", 200, 0),
            approve("2026-09-20T11:00:00.000Z", "second-plan.md"),
            spent("2026-09-20T11:01:00.000Z", "claude-opus-5", 50, 0),
        ],
    );
    let s = read(&path).session;

    assert_eq!(total(&s.plan_usage["first-plan.md"]).output, 300);
    assert_eq!(total(&s.plan_usage["second-plan.md"]).output, 50);
    // the message that proposed a plan is charged to the work that argued for
    // it, never to the plan it is asking for
    assert_eq!(s.unplanned.output, 10);
    assert_eq!(
        s.unplanned.output + s.plan_usage.values().map(|m| total(m).output).sum::<u64>(),
        s.spent().output,
        "every token lands in exactly one bucket"
    );
    assert_eq!(s.under_plan, "second-plan.md");
    // and it is split by model, not just totalled
    assert_eq!(s.plan_usage["first-plan.md"]["claude-opus-5"].output, 300);
}

#[test]
fn a_plan_still_running_keeps_its_bill_across_a_resumed_read() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp
        .path()
        .join("cccccccc-0000-0000-0000-000000000000.jsonl");
    write(
        &path,
        &[
            approve("2026-09-20T10:00:00.000Z", "long-plan.md"),
            spent("2026-09-20T10:01:00.000Z", "claude-opus-5", 100, 0),
        ],
    );
    let mut state = read(&path);
    assert_eq!(total(&state.session.plan_usage["long-plan.md"]).output, 100);

    // the session runs on; the next pass starts mid-plan with no approval in
    // the bytes it reads
    append(
        &path,
        &[spent("2026-09-20T12:00:00.000Z", "claude-opus-5", 25, 0)],
    );
    advance(&path, &mut state).unwrap();
    assert_eq!(total(&state.session.plan_usage["long-plan.md"]).output, 125);
    assert_eq!(state.session.unplanned.output, 0);
}

#[test]
fn a_session_node_carries_what_it_spent() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp
        .path()
        .join("dddddddd-0000-0000-0000-000000000000.jsonl");
    write(
        &path,
        &[
            spent(
                "2026-09-20T10:00:01.000Z",
                "claude-haiku-4-5-20251001",
                10,
                10,
            ),
            spent("2026-09-20T10:00:02.000Z", "claude-opus-5", 500, 90_000),
        ],
    );
    let s = read(&path).session;
    let (node, _) = to_graph(&s, &|p| Some(p.to_string()), &|_| true);

    assert_eq!(node.prop_i64("tokensOut"), Some(510));
    assert_eq!(node.prop_i64("cacheRead"), Some(90_010));
    assert_eq!(node.prop_i64("messages"), Some(2));
    assert_eq!(node.prop_i64("tokens"), Some(s.spent().total() as i64));
    // busiest model first, so the headline is the one that did the work
    assert_eq!(
        node.prop_str("models"),
        Some("claude-opus-5, claude-haiku-4-5-20251001")
    );
}

#[test]
fn a_node_carries_the_breakdown_by_model_not_just_the_total() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp
        .path()
        .join("ffffffff-0000-0000-0000-000000000000.jsonl");
    write(
        &path,
        &[
            spent(
                "2026-09-20T10:00:01.000Z",
                "claude-haiku-4-5-20251001",
                10,
                10,
            ),
            spent("2026-09-20T10:00:02.000Z", "claude-opus-5", 500, 90_000),
            spent("2026-09-20T10:00:03.000Z", "claude-opus-5", 100, 1_000),
        ],
    );
    let s = read(&path).session;
    let (node, _) = to_graph(&s, &|p| Some(p.to_string()), &|_| true);

    let rows = node.props.get("byModel").unwrap().as_array().unwrap();
    assert_eq!(rows.len(), 2);
    // busiest first, so the headline row is the one that did the work
    assert_eq!(rows[0]["model"], "claude-opus-5");
    assert_eq!(rows[0]["out"], 600);
    assert_eq!(rows[0]["cacheRead"], 91_000);
    assert_eq!(rows[0]["messages"], 2);
    assert_eq!(rows[1]["model"], "claude-haiku-4-5-20251001");
    assert_eq!(rows[1]["out"], 10);

    // the rows add up to the headline, which is the only reason to trust it
    let summed: i64 = rows.iter().map(|r| r["tokens"].as_i64().unwrap()).sum();
    assert_eq!(node.prop_i64("tokens"), Some(summed));
    assert_eq!(
        node.prop_str("models"),
        Some("claude-opus-5, claude-haiku-4-5-20251001")
    );
}

#[test]
fn a_session_that_spent_nothing_carries_no_token_props() {
    let tmp = tempfile::tempdir().unwrap();
    let path = fixture(tmp.path());
    let s = read(&path).session;
    let (node, _) = to_graph(&s, &|p| Some(p.to_string()), &|_| true);
    assert_eq!(node.prop_i64("tokens"), None);
    assert_eq!(node.prop_str("models"), None);
}

#[test]
fn a_transcript_read_by_an_older_build_is_read_again() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp
        .path()
        .join("eeeeeeee-0000-0000-0000-000000000000.jsonl");
    write(
        &path,
        &[spent("2026-09-20T10:00:01.000Z", "claude-opus-5", 100, 0)],
    );

    // What an older build left behind: read to the end, and knowing nothing
    // about tokens because it had no idea to look for them.
    let mut state = read(&path);
    let at_eof = state.tail.clone();
    state.version = 0;
    state.session.usage.clear();
    assert_eq!(state.tail.offset, at_eof.offset, "already at the end");

    // Nothing has been appended, so without the version this would find
    // nothing to do and the counts would stay empty forever.
    advance(&path, &mut state).unwrap();
    assert_eq!(state.version, STATE_VERSION);
    assert_eq!(state.session.spent().output, 100, "re-read from the start");

    // and having caught up it goes back to reading only what is new
    assert_eq!(advance(&path, &mut state).unwrap(), None);
}
