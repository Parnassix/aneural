//! Claude Code's own record of what it did in this workspace.
//!
//! The transcripts are append-only JSONL, one file per session, and they get
//! large — 25 MB and 2600 lines for one session on the machine this was
//! written on, with single lines over 1 MB. So they are read by tailing from a
//! stored byte offset, and most lines never reach a JSON parser at all: a
//! substring test rules them out first.
//!
//! What is *not* here matters as much as what is. A session's tool calls are
//! authoritative for intent and only partial for change, because an edit made
//! through the shell — a heredoc, `sed`, a script — leaves no record. Git is
//! the source of truth for what changed; these edges say "this session had its
//! hands on this file", and carry `via` to say how that was known.

use aneural_core::kinds::{EdgeKind, NodeKind, Source};
use aneural_core::{Edge, Node, NodeId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Every origin this producer writes starts with this.
pub const PREFIX: &str = "claude://";

/// Just the per-transcript origins. The sweep that retracts sessions whose
/// transcript is gone keys on this rather than on [`PREFIX`]: the plans and the
/// commit-to-session links are `claude://` too, and sweeping them every pass
/// would delete and rebuild them on every tick.
pub const SESSION_PREFIX: &str = "claude://session/";

/// One transcript, one origin — a file is the unit of rotation and deletion.
pub fn session_origin(uuid: &str) -> String {
    format!("{SESSION_PREFIX}{uuid}")
}

/// The `meta` key holding how far a transcript was read and what was found.
pub fn state_key(uuid: &str) -> String {
    format!("claude.session.{uuid}")
}

/// A single line longer than this is skipped rather than held in memory. One
/// tool result can be over a megabyte, and nothing that big is a record we read.
const MAX_LINE: u64 = 2 * 1024 * 1024;

/// Substrings that make a line worth parsing. Everything else — assistant
/// prose, thinking, the bulk of tool results — is skipped without touching
/// serde. `promptSource` is the precise marker of a *typed* message: a
/// transcript with 464 `"type":"user"` entries had 20 of these, and the 444
/// others were tool results.
const NEEDLES: [&str; 8] = [
    "promptSource",
    "bridge-session",
    "ai-title",
    "file-history-delta",
    "ExitPlanMode",
    "structuredPatch",
    // Every assistant message carries a usage block, and it is the only place
    // the count exists — Claude Code writes no running total anywhere.
    "output_tokens",
    // The only record that a session has *entered* plan mode. `ExitPlanMode`
    // says a plan was approved, which is the end of the thinking, not the
    // start of it.
    "permission-mode",
];

/// Where Claude Code keeps its state, honouring a config override.
pub fn state_dir(configured: &str) -> Option<PathBuf> {
    if !configured.is_empty() {
        return Some(PathBuf::from(configured));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude"))
}

/// How far a transcript has been read, and enough identity to notice that the
/// file underneath was replaced rather than appended to.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tail {
    pub offset: u64,
    /// Length when last read. A file shorter than the offset was truncated.
    pub len: u64,
    #[serde(default)]
    pub ino: u64,
    #[serde(default)]
    pub dev: u64,
    /// Hash of the first line, which catches a same-length replacement.
    #[serde(default)]
    pub head: String,
}

/// What a stretch of work cost.
///
/// The four counts are kept apart because they are not interchangeable: a
/// cached input token and a fresh one differ by an order of magnitude in
/// price, and adding them up is how a tally becomes a number nobody can
/// check. `thinking` is a *part of* `output`, never added to it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// Assistant messages counted. One typed prompt is many of these.
    pub messages: u64,
    pub input: u64,
    pub output: u64,
    /// Reasoning tokens, already inside `output`.
    pub thinking: u64,
    pub cache_write: u64,
    pub cache_read: u64,
}

impl Usage {
    /// Every token that actually moved. Not a price: the four are worth
    /// different amounts, which is why they stay on the node beside this.
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_write + self.cache_read
    }

    pub fn add(&mut self, other: &Usage) {
        self.messages += other.messages;
        self.input += other.input;
        self.output += other.output;
        self.thinking += other.thinking;
        self.cache_write += other.cache_write;
        self.cache_read += other.cache_read;
    }

    pub fn is_zero(&self) -> bool {
        self.messages == 0 && self.total() == 0
    }

    /// Put the counts on a node under a common prefix. `tokens` is the sum and
    /// the rest are the parts, so a reader can take the headline or the truth.
    pub fn write_props(&self, mut node: Node) -> Node {
        node = node
            .with_prop("tokens", self.total() as i64)
            .with_prop("tokensIn", self.input as i64)
            .with_prop("tokensOut", self.output as i64)
            .with_prop("cacheWrite", self.cache_write as i64)
            .with_prop("cacheRead", self.cache_read as i64)
            .with_prop("messages", self.messages as i64);
        if self.thinking > 0 {
            node = node.with_prop("tokensThinking", self.thinking as i64);
        }
        node
    }
}

/// A tally split by the model that ran it.
///
/// The split is the point, not a detail: a token's worth depends entirely on
/// which model produced it, so one number spanning several models answers no
/// question you would actually ask. Everything that carries a cost — a
/// session, a plan, a repository — carries it in this shape.
pub type ByModel = BTreeMap<String, Usage>;

/// Every model's spend added together.
pub fn total(by_model: &ByModel) -> Usage {
    let mut all = Usage::default();
    for used in by_model.values() {
        all.add(used);
    }
    all
}

/// Fold `other` into `into`, model by model.
pub fn merge(into: &mut ByModel, other: &ByModel) {
    for (model, used) in other {
        into.entry(model.clone()).or_default().add(used);
    }
}

/// The models, busiest first. Ties break by name so the order is stable
/// rather than however the map happened to be walked.
pub fn busiest_first(by_model: &ByModel) -> Vec<(&String, &Usage)> {
    let mut out: Vec<(&String, &Usage)> = by_model.iter().collect();
    out.sort_by_key(|(name, used)| (std::cmp::Reverse(used.total()), (*name).clone()));
    out
}

/// Put a tally and its breakdown on a node.
///
/// The one place any of this is written, so a Session, a Plan and a Usage
/// node cannot drift into describing the same thing three different ways.
pub fn write_usage(mut node: Node, by_model: &ByModel) -> Node {
    let all = total(by_model);
    if all.is_zero() {
        return node;
    }
    node = all.write_props(node);
    let order = busiest_first(by_model);
    node = node.with_prop(
        "models",
        order
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    );
    node.with_prop(
        "byModel",
        serde_json::Value::Array(
            order
                .iter()
                .map(|(name, used)| {
                    serde_json::json!({
                        "model": name,
                        "tokens": used.total(),
                        "in": used.input,
                        "out": used.output,
                        "thinking": used.thinking,
                        "cacheWrite": used.cache_write,
                        "cacheRead": used.cache_read,
                        "messages": used.messages,
                    })
                })
                .collect(),
        ),
    )
}

/// One file a session had its hands on.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Touch {
    pub first: String,
    pub last: String,
    pub count: u64,
    /// How the touch was known: `tool` (an Edit or Write) or `backup` (a
    /// file-history entry). Neither sees an edit made through the shell.
    pub via: String,
}

/// Everything read out of one transcript so far.
///
/// Persisted whole beside the offset, so reopening the engine needs no re-read
/// and the counters cannot drift — they are recomputed from this, never
/// incremented in place on a node.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub uuid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The identifier a commit's `Claude-Session` trailer would name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub started: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ended: String,
    /// Messages the user actually typed, as opposed to tool results.
    #[serde(default)]
    pub prompts: u64,
    /// Plan documents approved in this session: file name to when it was
    /// approved. The time matters — a session runs for days and approves
    /// several plans, so its start time says nothing about any one of them.
    #[serde(default)]
    pub plans: BTreeMap<String, String>,
    /// Workspace-relative path to what is known about the touch.
    #[serde(default)]
    pub touched: BTreeMap<String, Touch>,
    /// Tokens spent, by model. Per model because a session routinely runs on
    /// more than one — a subagent on Haiku, the main thread on Opus — and one
    /// number across both would answer no question worth asking.
    #[serde(default)]
    pub usage: BTreeMap<String, Usage>,
    /// Tokens spent under each plan, by plan file name and then by model. A
    /// session runs for days and approves several plans, so its total says
    /// nothing about any one of them; this is the stretch between one
    /// approval and the next.
    #[serde(default)]
    pub plan_usage: BTreeMap<String, ByModel>,
    /// The plan in force. Persisted, because a transcript is read in pieces
    /// and the plan approved before the last read is still the one running.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub under_plan: String,
    /// Tokens spent before any plan was approved.
    #[serde(default)]
    pub unplanned: Usage,
    /// The permission mode last latched in the transcript: `plan`, `auto`,
    /// `normal`, and whatever else Claude Code writes there.
    ///
    /// Deliberately **not** backfilled by a [`STATE_VERSION`] bump. The only
    /// useful value of this field is the current one, and a bump would re-read
    /// every transcript on the machine — four hundred megabytes on this one —
    /// to learn what mode sessions that ended weeks ago were in. A session
    /// still being written to latches it on the next line, which is the only
    /// session anyone is asking about.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mode: String,
}

/// Bumped whenever what is read out of a transcript changes, so that a
/// workspace indexed by an older build reads its transcripts again.
///
/// This is not housekeeping — it is the only way a new field is ever filled
/// in on a workspace that has been open before. The bytes have already been
/// consumed: the tail sits at the end of the file, `advance` finds nothing
/// new and returns, and the field stays empty forever while the graph looks
/// perfectly healthy. Only a transcript still being written to would pick it
/// up, which is the most misleading possible outcome — the newest session
/// answers and every older one quietly says nothing.
///
/// 2: tokens, by model and by plan.
/// 3: a plan's tokens split by model too, not just totalled.
pub const STATE_VERSION: u32 = 3;

/// A transcript's persisted state: where we stopped, and what we had found.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// The [`STATE_VERSION`] this was parsed under. Absent — so zero — on
    /// anything written before versioning, which is exactly the case that
    /// has to be re-read.
    #[serde(default)]
    pub version: u32,
    pub tail: Tail,
    pub session: Session,
}

impl Session {
    fn note_time(&mut self, at: &str) {
        if at.is_empty() {
            return;
        }
        if self.started.is_empty() || at < self.started.as_str() {
            self.started = at.to_string();
        }
        if at > self.ended.as_str() {
            self.ended = at.to_string();
        }
    }

    /// Add one message's cost to the model that produced it, and to whatever
    /// plan was running when it did.
    fn spend(&mut self, model: &str, used: Usage) {
        self.usage.entry(model.to_string()).or_default().add(&used);
        match self.under_plan.is_empty() {
            true => self.unplanned.add(&used),
            false => self
                .plan_usage
                .entry(self.under_plan.clone())
                .or_default()
                .entry(model.to_string())
                .or_default()
                .add(&used),
        }
    }

    /// Every model's spend added together.
    pub fn spent(&self) -> Usage {
        total(&self.usage)
    }

    fn touch(&mut self, rel: String, at: &str, via: &str) {
        let entry = self.touched.entry(rel).or_insert_with(|| Touch {
            first: at.to_string(),
            last: at.to_string(),
            count: 0,
            via: via.to_string(),
        });
        entry.count += 1;
        if at > entry.last.as_str() {
            entry.last = at.to_string();
        }
        if !at.is_empty() && (entry.first.is_empty() || at < entry.first.as_str()) {
            entry.first = at.to_string();
        }
    }
}

/// Every transcript under `state_dir` whose session ran inside `workspace`.
///
/// The directory names are an escaped form of the working directory, but the
/// escaping is undocumented and lossy — every line of every transcript carries
/// its own `cwd`, so that is read instead of the name being reconstructed.
pub fn transcripts_for(state_dir: &Path, workspace: &Path) -> Vec<PathBuf> {
    let projects = state_dir.join("projects");
    let Ok(dirs) = std::fs::read_dir(&projects) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for dir in dirs.flatten() {
        let Ok(files) = std::fs::read_dir(dir.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if cwd_of(&path).is_some_and(|cwd| cwd.starts_with(workspace)) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// The working directory a transcript records, from its first usable line.
fn cwd_of(path: &Path) -> Option<PathBuf> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    for _ in 0..40 {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
            && let Some(cwd) = v.get("cwd").and_then(|c| c.as_str())
        {
            return Some(PathBuf::from(cwd));
        }
    }
    None
}

/// The session id a transcript file is named after.
pub fn uuid_of(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
}

/// Read everything appended since `state.tail` and fold it in.
///
/// Returns `Ok(None)` when there is nothing new, so a caller can skip the write
/// entirely. A file that was truncated, rotated or replaced is re-read whole
/// and its accumulated session discarded, which is the only way the counters
/// can stay honest.
pub fn advance(path: &Path, state: &mut State) -> std::io::Result<Option<()>> {
    let meta = std::fs::metadata(path)?;
    let len = meta.len();
    let (ino, dev) = identity(&meta);
    let head = first_line_hash(path)?;

    let replaced = state.version != STATE_VERSION
        || len < state.tail.offset
        || (state.tail.ino != 0 && ino != 0 && state.tail.ino != ino)
        || (state.tail.dev != 0 && dev != 0 && state.tail.dev != dev)
        || (!state.tail.head.is_empty() && !head.is_empty() && state.tail.head != head);
    if replaced {
        let uuid = state.session.uuid.clone();
        state.session = Session {
            uuid,
            ..Default::default()
        };
        state.tail = Tail::default();
    } else if len == state.tail.offset {
        state.tail.len = len;
        return Ok(None);
    }

    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(state.tail.offset))?;

    let mut consumed = state.tail.offset;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = read_capped(&mut reader, &mut line)?;
        if read == 0 {
            break;
        }
        // Only a `\n`-terminated line is complete; a half-written tail waits
        // for the next pass rather than being parsed as if it were whole.
        if line.last() != Some(&b'\n') {
            break;
        }
        consumed += read;
        apply_line(&line, &mut state.session);
    }

    state.version = STATE_VERSION;
    state.tail.offset = consumed;
    state.tail.len = len;
    state.tail.ino = ino;
    state.tail.dev = dev;
    state.tail.head = head;
    Ok(Some(()))
}

/// Read one line, giving up on anything absurdly long rather than buffering it.
/// The bytes are still consumed, so an oversized line cannot wedge the file.
fn read_capped(reader: &mut impl BufRead, out: &mut Vec<u8>) -> std::io::Result<u64> {
    let mut total = 0u64;
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            return Ok(total);
        }
        match chunk.iter().position(|b| *b == b'\n') {
            Some(at) => {
                let take = at + 1;
                if total + take as u64 <= MAX_LINE {
                    out.extend_from_slice(&chunk[..take]);
                } else {
                    out.clear();
                    out.push(b'\n');
                }
                reader.consume(take);
                return Ok(total + take as u64);
            }
            None => {
                let take = chunk.len();
                if total + take as u64 <= MAX_LINE {
                    out.extend_from_slice(chunk);
                }
                reader.consume(take);
                total += take as u64;
            }
        }
    }
}

/// Fold one transcript line into the session, if it is one of the few that say
/// anything. Anything unparseable is skipped, never fatal: these files are an
/// undocumented format that changes with the version stamped on every line.
fn apply_line(line: &[u8], session: &mut Session) {
    if !NEEDLES.iter().any(|n| contains(line, n.as_bytes())) {
        return;
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else {
        return;
    };
    let at = v.get("timestamp").and_then(|t| t.as_str()).unwrap_or("");
    let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

    if let Some(cwd) = v.get("cwd").and_then(|c| c.as_str()) {
        session.cwd.get_or_insert_with(|| cwd.to_string());
    }
    if let Some(branch) = v.get("gitBranch").and_then(|b| b.as_str())
        && !branch.is_empty()
    {
        session.branch = Some(branch.to_string());
    }
    if let Some(version) = v.get("version").and_then(|b| b.as_str()) {
        session.version = Some(version.to_string());
    }

    match kind {
        "bridge-session" => {
            if let Some(id) = v.get("bridgeSessionId").and_then(|b| b.as_str()) {
                session.bridge = Some(aneural_git::trailer::identifier(id));
            }
            return;
        }
        "ai-title" => {
            if let Some(t) = v.get("aiTitle").and_then(|b| b.as_str())
                && !t.is_empty()
            {
                session.title = Some(t.to_string());
            }
            return;
        }
        "file-history-delta" => {
            if let Some(p) = v.get("trackingPath").and_then(|b| b.as_str()) {
                session.note_time(at);
                session.touch(p.to_string(), at, "backup");
            }
            return;
        }
        // A latch line, not a message: it carries no timestamp and must not
        // move the session's start or end.
        "permission-mode" => {
            if let Some(mode) = v.get("permissionMode").and_then(|b| b.as_str())
                && !mode.is_empty()
            {
                session.mode = mode.to_string();
            }
            return;
        }
        _ => {}
    }

    session.note_time(at);

    // A message the user actually typed, rather than a tool result wearing the
    // same `"type": "user"`.
    if v.get("promptSource").is_some() {
        session.prompts += 1;
    }

    // An Edit or Write that landed: the result carries the path it wrote.
    if let Some(result) = v.get("toolUseResult")
        && result.get("structuredPatch").is_some()
        && let Some(p) = result.get("filePath").and_then(|b| b.as_str())
    {
        session.touch(p.to_string(), at, "tool");
    }

    // What this message cost. Read before the plan marker below, so the
    // message that *proposes* a plan is charged to the work that argued for
    // it rather than to the plan it is asking for.
    if kind == "assistant"
        && let Some(message) = v.get("message")
        && let Some(used) = usage_of(message)
    {
        let model = message
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown");
        session.spend(model, used);
    }

    // A plan approved here.
    if let Some(content) = v.get("message").and_then(|m| m.get("content")) {
        for block in content.as_array().into_iter().flatten() {
            if block.get("name").and_then(|n| n.as_str()) == Some("ExitPlanMode")
                && let Some(file) = block
                    .get("input")
                    .and_then(|i| i.get("planFilePath"))
                    .and_then(|p| p.as_str())
                && let Some(name) = Path::new(file).file_name().and_then(|n| n.to_str())
            {
                // Approved more than once: the first time is when it began.
                let seen = session.plans.entry(name.to_string()).or_default();
                if seen.is_empty() || (!at.is_empty() && at < seen.as_str()) {
                    *seen = at.to_string();
                }
                // Everything from here is that plan's, until another is
                // approved. This is the whole reason a plan can be given a
                // cost at all: the transcript says when it started running.
                session.under_plan = name.to_string();
            }
        }
    }
}

/// One assistant message's usage block, if it has one.
///
/// The shape is the API's, not Claude Code's, so the field names are stable in
/// a way the rest of the transcript is not. A message replayed from cache
/// still reports what it read, which is why `cache_read` is counted and not
/// treated as free.
fn usage_of(message: &serde_json::Value) -> Option<Usage> {
    let u = message.get("usage")?;
    let n = |key: &str| u.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    let used = Usage {
        messages: 1,
        input: n("input_tokens"),
        output: n("output_tokens"),
        thinking: u
            .get("output_tokens_details")
            .and_then(|d| d.get("thinking_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_write: n("cache_creation_input_tokens"),
        cache_read: n("cache_read_input_tokens"),
    };
    // A synthetic message carries a usage block of all zeros. Counting it
    // would inflate the message tally with entries that cost nothing, so the
    // test is on the tokens rather than on the whole record — which always
    // has a message in it by the time it is built.
    (used.total() > 0).then_some(used)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(unix)]
fn identity(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.ino(), meta.dev())
}

#[cfg(not(unix))]
fn identity(_meta: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

fn first_line_hash(path: &Path) -> std::io::Result<String> {
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    read_capped(&mut reader, &mut line)?;
    Ok(blake3::hash(&line).to_hex().to_string()[..16].to_string())
}

/// Turn an accumulated session into its node and edges.
///
/// `rel` maps a path the transcript recorded — absolute, or already relative —
/// to a workspace-relative one, returning `None` for anything outside. `known`
/// says whether a file id is in the graph, because a session routinely edits
/// files that have since been deleted.
pub fn to_graph(
    session: &Session,
    rel: &dyn Fn(&str) -> Option<String>,
    known: &dyn Fn(&NodeId) -> bool,
) -> (Node, Vec<Edge>) {
    let origin = session_origin(&session.uuid);
    let id = NodeId::session(&session.uuid);

    let mut edges = Vec::new();
    for (path, touch) in &session.touched {
        let Some(rel) = rel(path) else { continue };
        let file = NodeId::file(&rel);
        if !known(&file) {
            continue;
        }
        edges.push(
            Edge::new(EdgeKind::MODIFIES, id.clone(), file, Source::CLAUDE)
                .with_origin(&origin)
                .with_prop("via", touch.via.clone())
                .with_prop("firstAt", touch.first.clone())
                .with_prop("lastAt", touch.last.clone()),
        );
    }

    let label = session
        .title
        .clone()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| short(&session.uuid));

    // Counters live here, never on the edges: an edge is unique per
    // (kind, src, dst, source) and its props are replaced wholesale, so a tally
    // kept there could not survive being written twice.
    let mut node = Node::new(id, NodeKind::SESSION, label, Source::CLAUDE)
        .with_origin(&origin)
        .with_prop("uuid", session.uuid.clone())
        .with_prop("prompts", session.prompts as i64)
        .with_prop("files", edges.len() as i64)
        .with_prop("filesTouched", session.touched.len() as i64);
    node = write_usage(node, &session.usage);
    for (key, value) in [
        ("bridge", &session.bridge),
        ("branch", &session.branch),
        ("version", &session.version),
    ] {
        if let Some(v) = value {
            node = node.with_prop(key, v.clone());
        }
    }
    for (key, value) in [
        ("startedAt", &session.started),
        ("endedAt", &session.ended),
        ("mode", &session.mode),
    ] {
        if !value.is_empty() {
            node = node.with_prop(key, value.clone());
        }
    }
    if !session.plans.is_empty() {
        let names: Vec<&str> = session.plans.keys().map(String::as_str).collect();
        node = node.with_prop("plans", names.join(", "));
    }
    (node, edges)
}

fn short(uuid: &str) -> String {
    uuid.split('-').next().unwrap_or(uuid).to_string()
}

#[cfg(test)]
mod tests;
