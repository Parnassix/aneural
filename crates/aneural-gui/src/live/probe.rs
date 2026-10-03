//! The one thing that polls.
//!
//! Most of what the command centre reacts to is already in the delta stream
//! (see [`super::read_deltas`]). Three things are not, and none of them can be
//! learned by watching files:
//!
//! - **Which Claude sessions are alive, and what they are doing.**
//!   `~/.claude/sessions/<pid>.json` is a purpose-built status file — pid,
//!   sessionId, cwd, a human name, and `status` of `busy` | `idle` | `shell`.
//!   Four files of about 700 bytes on a busy machine; listing the directory
//!   takes a quarter of a millisecond. Entries go **stale rather than being
//!   deleted**, so a file alone is not evidence the session exists and the pid
//!   is checked against the process table.
//! - **Which processes are running.** A script Aneural knows about may be
//!   started by anything — a shell, a launch agent, an agent's own scratchpad —
//!   so the only general answer is to look at the process table. And because
//!   the scratchpad path a session runs things from *contains that session's
//!   id*, the parent chain says which session started a script. That is the one
//!   line nothing else on the machine draws.
//! - **Whether git is mid-operation.** Commits arrive as nodes; a merge in
//!   flight is four `stat`s and is invisible to the graph.
//!
//! This runs on its own thread, on the shape `marketplace/worker.rs`
//! established: a `std::thread` plus a pair of channels. Not the engine loop,
//! which is serial and already the busiest thread in the process, and not the
//! render thread, which must not touch the filesystem. It sleeps on
//! `recv_timeout`, so a change of attention wakes it immediately and an idle
//! machine costs one blocked thread.
//!
//! Nothing here starts a process, and nothing is reported about a process that
//! has no node on the canvas — which is both the design rule and the privacy
//! answer.
//!
//! **The process table is read by running `ps`, and `sysinfo` is deliberately
//! not used.** It is already in the tree and would have cost no build time,
//! which is exactly the trap: it is there because `bevy_diagnostic` depends on
//! it, and `bevy_diagnostic` turns on its `apple-app-store` feature, which
//! turns on `apple-sandbox`, under which sysinfo reports **no processes at all
//! on macOS**. Cargo unifies features across a build, so that cannot be turned
//! off from here — measured, not assumed: a refresh returns 0 processes out of
//! the thousand this machine is running. `ps` costs one fork (40 ms for 1050
//! processes, 17 ms for a named handful), which is why the full sweep is rate
//! limited by [`DISCOVER`] and the fast lane only re-reads pids already known.

use super::{Signal, Signals, What, bytes_of, elapsed, plural};
use crate::graph::{GraphNode, GraphState};
use crate::workspace::WorkspaceRes;
use aneural_core::NodeId;
use aneural_core::kinds::NodeKind;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use crossbeam_channel::{Receiver, Sender};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Sampling as fast as this feature ever goes: something is running and
/// somebody is watching it.
const EAGER: Duration = Duration::from_millis(700);
/// The unhurried default. A process list and a handful of `stat`s at this rate
/// is not a cost worth optimising.
const CALM: Duration = Duration::from_secs(5);
/// Nothing live and nobody looking.
const DROWSY: Duration = Duration::from_secs(30);
/// Load per core above which the machine has better things to do. Chosen
/// below 1.0 on purpose: by the time every core is saturated the damage is
/// done, and this feature exists to watch heavy work, not to compete with it.
const BUSY: f32 = 0.7;
/// Samples with nothing to report before the slow lane, when nothing is live.
const QUIET: u32 = 12;
/// How often the process table is swept. This is the only fork this module
/// makes, and everything expensive hangs off it.
const DISCOVER: Duration = Duration::from_secs(3);
/// How much of the previous reading of how busy the machine is to keep.
/// Runnable processes is an instantaneous count where a load average is a
/// minute's worth, so it needs smoothing or the interval would jump about.
const SMOOTH: f32 = 0.7;
/// How long a chain must have been running before it is a task. Everything an
/// agent does is a process, so without this every `ls` is news.
const YOUNG: u64 = 20;
/// Programs that are never the work, only the plumbing around it.
const INERT: [&str; 16] = [
    "sleep", "ps", "grep", "cat", "head", "tail", "awk", "sed", "pwd", "true", "wc", "tr", "cut",
    "sort", "uniq", "env",
];
/// The marker that identifies a shell an agent opened to run a tool call. These
/// are chaff as work, but they are evidence of *waiting* -- see [`Task::watched`].
const TOOL_SHELL: &str = "shell-snapshots/snapshot-";

/// How long to sleep before the next sample.
///
/// `busy` is how many processes are runnable, smoothed — the thing a load
/// average is an average of, and available from the same `ps` the rest of this
/// module already pays for.
///
/// Pure, so the whole policy is a table in a test rather than a thing that has
/// to be observed on a loaded machine to be believed.
pub fn pace(busy: f32, cores: usize, focused: bool, live: bool, quiet: u32) -> Duration {
    if !focused && !live {
        return DROWSY;
    }
    let base = match (live, focused) {
        (true, true) => EAGER,
        (true, false) => CALM,
        (false, _) if quiet >= QUIET => CALM.max(DROWSY / 3),
        (false, _) => CALM,
    };
    // Between [`BUSY`] and a fully saturated machine, give way in proportion:
    // at saturation this is as slow as it ever gets, whatever it was doing.
    // A plain multiplier was tried first and had to be absurdly steep to reach
    // the slow lane at all, which put a cliff just above the threshold.
    let per = busy / cores.max(1) as f32;
    let over = ((per - BUSY) / (1.0 - BUSY)).clamp(0.0, 1.0);
    let out = base + (DROWSY - base).mul_f32(over);
    out.clamp(EAGER, DROWSY)
}

/// What to look for, and whether anyone is looking.
///
/// The workspace travels in the message rather than being fixed when the thread
/// starts, so opening another workspace is a new message and not a new thread.
#[derive(Clone, Debug, Default)]
pub struct Attention {
    pub focused: bool,
    pub root: PathBuf,
    /// `history.claudeRoot`, or `$HOME/.claude`. `None` turns the session half
    /// off entirely, which is what an empty override means.
    pub claude: Option<PathBuf>,
    /// Every `Script` node, with the workspace-relative path to match in a
    /// command line.
    pub scripts: Vec<(NodeId, String)>,
    /// Workspace-relative paths of the repositories.
    pub repos: Vec<String>,
}

pub enum Ask {
    Look(Box<Attention>),
    Stop,
}

/// A Claude session alive in this workspace.
#[derive(Clone, Debug)]
pub struct Seen {
    pub uuid: String,
    pub name: String,
    /// `busy` | `idle` | `shell`, as written by Claude Code.
    pub status: String,
}

/// Work a live session started, collapsed from the process chain that does it.
///
/// The unit to report is not a process. An agent starting a long job spawns a
/// shell, which spawns a runner, which spawns the interpreter that does the
/// work -- three pids the user thinks of as one thing, and naming any one of
/// them alone is either uninformative (`python`) or wrong (a `uv` wrapper
/// credited with 300 MB its child is holding). So a chain is found, summed and
/// named once.
///
/// A task exists only because a *live session owns it*. That is what makes this
/// reportable at all: it is not an enumeration of what is running on the
/// machine, it is the work this workspace's agents have in flight.
#[derive(Clone, Debug)]
pub struct Task {
    /// The owning session. Never optional -- ownership is what defines a task.
    pub session: String,
    /// The top of the chain. A task keeps its identity while children come and
    /// go, so this is what is matched between samples.
    pub pid: u32,
    /// Every pid in the chain, so the caption can say how many there are.
    pub pids: Vec<u32>,
    /// The script at the top of the chain, else the busiest program's name.
    pub name: String,
    /// Elapsed for the top of the chain, which is the age of the whole job.
    pub secs: u64,
    /// Summed across the chain.
    pub rss: u64,
    /// Workspace-relative repository the chain names, when it names one. A
    /// better portal subject than the workspace root.
    pub repo: Option<String>,
    /// The owning session is sitting in a poll loop waiting for this, so it is
    /// blocked rather than working -- which is the more useful of the two facts.
    pub watched: bool,
}

/// A process running one of this workspace's scripts.
#[derive(Clone, Debug)]
pub struct Ran {
    pub script: NodeId,
    pub pid: u32,
    pub secs: u64,
    pub rss: u64,
    /// The session whose scratchpad started it, when the parent chain says so.
    pub session: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Sample {
    pub sessions: Vec<Seen>,
    pub running: Vec<Ran>,
    /// Running at the last sample and gone now.
    pub ended: Vec<Ran>,
    /// Work owned by a live session, whether or not a `Script` node names it.
    pub tasks: Vec<Task>,
    /// A task in the last sample whose chain is gone now.
    pub done: Vec<Task>,
    /// Repository path → the operation in flight (`merge`, `rebase`, `commit`).
    pub busy: Vec<(String, String)>,
}

#[derive(Resource)]
pub struct ProbeTx(pub Sender<Ask>);

#[derive(Resource)]
pub struct ProbeRx(pub Receiver<Sample>);

/// The last thing sent, so attention is only re-sent when it changed.
#[derive(Resource, Default)]
struct Sent(Option<Attention>);

pub struct ProbePlugin;

impl Plugin for ProbePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Sent>()
            .add_systems(Startup, spawn)
            .add_systems(Update, (attend, drain).chain())
            .add_systems(Last, stop_on_exit);
    }
}

fn spawn(mut commands: Commands, ws: Res<WorkspaceRes>) {
    if !ws.config.gui.live || ws.config.gui.live_pace == "off" {
        return;
    }
    let (ask_tx, ask_rx) = crossbeam_channel::unbounded::<Ask>();
    let (sample_tx, sample_rx) = crossbeam_channel::unbounded::<Sample>();
    let pinned = match ws.config.gui.live_pace.as_str() {
        "eager" => Some(EAGER),
        "calm" => Some(CALM),
        _ => None,
    };
    std::thread::Builder::new()
        .name("aneural-live".into())
        .spawn(move || watch(ask_rx, sample_tx, pinned))
        .expect("spawn live probe thread");
    commands.insert_resource(ProbeTx(ask_tx));
    commands.insert_resource(ProbeRx(sample_rx));
}

/// Tell the probe what to look for, when and only when that changed.
fn attend(
    tx: Option<Res<ProbeTx>>,
    mut sent: ResMut<Sent>,
    ws: Res<WorkspaceRes>,
    graph: Res<GraphState>,
    nodes: Query<&GraphNode>,
    window: Query<&Window, With<PrimaryWindow>>,
) {
    let Some(tx) = tx else { return };
    let focused = window.iter().any(|w| w.focused);
    let mut scripts: Vec<(NodeId, String)> = Vec::new();
    let mut repos: Vec<String> = Vec::new();
    for n in &nodes {
        match n.kind.as_str() {
            NodeKind::SCRIPT => {
                // `file` is the prop the scripts spore declares; `path` is
                // never set on a spore's node, and the id is the fallback that
                // works for any producer.
                let rel = n
                    .prop_str("file")
                    .map(str::to_string)
                    .or_else(|| n.path.clone())
                    .unwrap_or_else(|| n.id.path_part().to_string());
                scripts.push((n.id.clone(), rel));
            }
            NodeKind::REPO => repos.push(n.id.path_part().to_string()),
            _ => {}
        }
    }
    scripts.sort();
    repos.sort();
    let _ = graph.node_count;
    let next = Attention {
        focused,
        root: ws.ws.root().to_path_buf(),
        claude: claude_dir(&ws.config.history.claude_root),
        scripts,
        repos,
    };
    if sent.0.as_ref().is_some_and(|prev| same(prev, &next)) {
        return;
    }
    sent.0 = Some(next.clone());
    let _ = tx.0.send(Ask::Look(Box::new(next)));
}

fn same(a: &Attention, b: &Attention) -> bool {
    a.focused == b.focused && a.root == b.root && a.scripts == b.scripts && a.repos == b.repos
}

/// `history.claudeRoot`, or `$HOME/.claude`. Mirrors
/// `aneural_engine::history::claude::state_dir`, which is not public.
fn claude_dir(configured: &str) -> Option<PathBuf> {
    if !configured.trim().is_empty() {
        return Some(PathBuf::from(configured));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude"))
}

/// Turn the newest sample into signals.
fn drain(
    rx: Option<Res<ProbeRx>>,
    mut signals: ResMut<Signals>,
    graph: Res<GraphState>,
    nodes: Query<&GraphNode>,
    time: Res<Time>,
    ws: Res<WorkspaceRes>,
) {
    let Some(rx) = rx else { return };
    if !ws.config.gui.live {
        while rx.0.try_recv().is_ok() {}
        return;
    }
    let mut latest = None;
    while let Ok(s) = rx.0.try_recv() {
        latest = Some(s);
    }
    let Some(sample) = latest else { return };
    let now = time.elapsed_secs_f64();
    let node = |id: &NodeId| graph.by_id.get(id).and_then(|e| nodes.get(*e).ok());
    let label = |id: &NodeId| node(id).map(|n| n.label.clone());

    // A session's own name is better than anything that could be assembled
    // from its transcript, and Claude Code writes one.
    for seen in &sample.sessions {
        let id = NodeId::session(&seen.uuid);
        let Some(n) = node(&id) else { continue };
        // Which mode it is in comes off the node, not the status file, which
        // does not carry one. Both halves of this feature then read the same
        // prop, so whichever of them runs first this frame they agree.
        let planning = n.prop_str("mode") == Some("plan");
        let what = match (planning, seen.status.as_str()) {
            (true, _) => What::Planning,
            (false, "busy") => What::Working,
            // Idle or dropped to a shell is not news; leave whatever the delta
            // stream already said standing and let it cool on its own.
            _ => continue,
        };
        signals.raise(
            now,
            Signal::new(
                what,
                vec![id],
                format!("{} {}", what.tag(), seen.name),
                match planning {
                    true => "in plan mode".to_string(),
                    false => "working".to_string(),
                },
            ),
        );
    }

    for ran in &sample.running {
        let Some(name) = label(&ran.script) else {
            continue;
        };
        let mut subjects = vec![ran.script.clone()];
        let mut detail = format!("running {}, pid {}", elapsed(ran.secs), ran.pid);
        if ran.rss > 0 {
            detail.push_str(&format!(", {}", bytes_of(ran.rss)));
        }
        if let Some(uuid) = &ran.session {
            let session = NodeId::session(uuid);
            if let Some(who) = label(&session) {
                detail.push_str(&format!(", started by {who}"));
                subjects.push(session);
            }
        }
        signals.raise(
            now,
            Signal::new(
                What::Running,
                subjects,
                format!("{} {name}", What::Running.tag()),
                detail,
            ),
        );
    }

    for ran in &sample.ended {
        let Some(name) = label(&ran.script) else {
            continue;
        };
        signals.raise(
            now,
            Signal::new(
                What::Finished,
                vec![ran.script.clone()],
                format!("{} {name}", What::Finished.tag()),
                // Aneural did not start it, so there is no exit code to report
                // and guessing one would be a lie about a run.
                format!("stopped after {}", elapsed(ran.secs)),
            ),
        );
    }

    // Work a live session started that the graph does not name -- the case the
    // `Script` pass above cannot reach, because an agent runs things out of a
    // scratchpad under `/private/tmp` and no indexer will ever walk that.
    //
    // The pids are not nodes and the command line is ephemeral, so the subject
    // is the *place* the work is happening: the repository the chain names,
    // else the session itself, else the workspace root, which always exists.
    // The session rides along as a second subject so its name is legible beside
    // the aperture even when a repository is what the aperture frames.
    //
    // The repository comes first on purpose. `raise` keeps one signal per
    // primary subject, and one session commonly has several jobs in flight; if
    // the session were primary they would collapse into one portal and only the
    // last would be seen.
    let subjects_for = |task: &Task| {
        let named = |id: NodeId| node(&id).is_some().then_some(id);
        let mut out: Vec<NodeId> = Vec::new();
        out.extend(task.repo.as_deref().map(NodeId::repo).and_then(named));
        out.extend(named(NodeId::session(&task.session)));
        if out.is_empty() {
            out.extend(named(NodeId::dir(".")));
        }
        out
    };
    // Claude Code writes a human name for every live session, so this works
    // whether or not `history.claude` is on to give the session a node.
    let who = |uuid: &str| {
        sample
            .sessions
            .iter()
            .find(|s| s.uuid == uuid)
            .map(|s| s.name.clone())
            .or_else(|| label(&NodeId::session(uuid)))
    };

    for task in &sample.tasks {
        let subjects = subjects_for(task);
        if subjects.is_empty() {
            continue;
        }
        let mut detail = format!("running {}", elapsed(task.secs));
        detail.push_str(&match task.pids.len() {
            1 => format!(", pid {}", task.pid),
            n => format!(", {}", plural(n as u64, "pid")),
        });
        if task.rss > 0 {
            detail.push_str(&format!(", {}", bytes_of(task.rss)));
        }
        if let Some(who) = who(&task.session) {
            detail.push_str(&format!(", started by {who}"));
        }
        if task.watched {
            // That the session is blocked on it is the more useful of the two
            // facts about a long job.
            detail.push_str(", session waiting");
        }
        signals.raise(
            now,
            Signal::new(
                What::Running,
                subjects,
                format!("{} {}", What::Running.tag(), task.name),
                detail,
            ),
        );
    }

    for task in &sample.done {
        let subjects = subjects_for(task);
        if subjects.is_empty() {
            continue;
        }
        signals.raise(
            now,
            Signal::new(
                What::Finished,
                subjects,
                format!("{} {}", What::Finished.tag(), task.name),
                // Aneural did not start it, so there is no exit status to
                // report and inventing one would be a lie about a run.
                format!("stopped after {}", elapsed(task.secs)),
            ),
        );
    }

    for (repo, op) in &sample.busy {
        let id = NodeId::repo(repo);
        let Some(name) = label(&id) else { continue };
        signals.raise(
            now,
            Signal::new(
                What::Merging,
                vec![id],
                format!("{} {name}", What::Merging.tag()),
                format!("{op} in progress"),
            ),
        );
    }
}

fn stop_on_exit(mut exit: MessageReader<AppExit>, tx: Option<Res<ProbeTx>>) {
    if exit.read().next().is_some()
        && let Some(tx) = tx
    {
        let _ = tx.0.send(Ask::Stop);
    }
}

// ------------------------------------------------------------------ the thread

fn watch(asks: Receiver<Ask>, out: Sender<Sample>, pinned: Option<Duration>) {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let mut look = Attention::default();
    let mut was: Vec<Ran> = Vec::new();
    let mut were: Vec<Task> = Vec::new();
    let mut heads: Vec<(String, String)> = Vec::new();
    let mut last_full: Option<std::time::Instant> = None;
    let mut seen: Vec<u32> = Vec::new();
    let mut busy = 0.0f32;
    let mut quiet: u32 = 0;
    loop {
        loop {
            match asks.try_recv() {
                Ok(Ask::Look(next)) => look = *next,
                Ok(Ask::Stop) => return,
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => return,
            }
        }

        let sample = if look.root.as_os_str().is_empty() {
            Sample::default()
        } else {
            take(
                &look,
                &mut was,
                &mut were,
                &mut heads,
                &mut last_full,
                &mut seen,
                &mut busy,
            )
        };
        let live =
            !sample.running.is_empty() || !sample.sessions.is_empty() || !sample.tasks.is_empty();
        let empty =
            !live && sample.ended.is_empty() && sample.done.is_empty() && sample.busy.is_empty();
        quiet = if empty { quiet.saturating_add(1) } else { 0 };
        if out.send(sample).is_err() {
            return;
        }
        let nap = pinned.unwrap_or_else(|| pace(busy, cores, look.focused, live, quiet));
        match asks.recv_timeout(nap) {
            Ok(Ask::Look(next)) => look = *next,
            Ok(Ask::Stop) => return,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn take(
    look: &Attention,
    was: &mut Vec<Ran>,
    were: &mut Vec<Task>,
    heads: &mut Vec<(String, String)>,
    last_full: &mut Option<std::time::Instant>,
    seen: &mut Vec<u32>,
    busy: &mut f32,
) -> Sample {
    let mut sample = Sample::default();

    // 1. Who claims to be alive. Read first, because a process is attributed to
    //    a session only if that session turns out to be one of these.
    let claimed = look
        .claude
        .as_deref()
        .map(|dir| sessions_in(dir, &look.root))
        .unwrap_or_default();

    // 2. The process table. A full sweep is the only way to notice something
    //    that has just started, and the only way to walk a parent chain; in
    //    between, re-reading the pids already in hand is less than half the
    //    cost and answers the only question left, which is "still?".
    let full = last_full.is_none_or(|t| t.elapsed() >= DISCOVER);
    let since = last_full.map(|t| t.elapsed().as_secs()).unwrap_or(0);
    if !full {
        // No fork on a fast tick. What was running goes on running until a
        // sweep says otherwise, with its clock advanced from ours.
        sample.sessions = claimed
            .into_iter()
            .filter(|(pid, _)| seen.contains(pid))
            .map(|(_, s)| s)
            .collect();
        sample.running = was
            .iter()
            .map(|r| Ran {
                secs: r.secs + since,
                ..r.clone()
            })
            .collect();
        sample.tasks = were
            .iter()
            .map(|t| Task {
                secs: t.secs + since,
                ..t.clone()
            })
            .collect();
        git_state(look, heads, &mut sample);
        return sample;
    }
    *last_full = Some(std::time::Instant::now());
    let forked = std::time::Instant::now();
    let procs = ps_all();
    let fork_ms = forked.elapsed().as_millis();
    *seen = procs.iter().map(|p| p.pid).collect();
    {
        // Runnable processes, smoothed: the instantaneous form of a load
        // average, and free from the sweep already done.
        let runnable = procs.iter().filter(|p| p.running).count() as f32;
        *busy = *busy * SMOOTH + runnable * (1.0 - SMOOTH);
    }
    let alive = |pid: u32| procs.iter().any(|p| p.pid == pid);

    sample.sessions = claimed
        .into_iter()
        .filter(|(pid, _)| alive(*pid))
        .map(|(_, seen)| seen)
        .collect();
    let live_uuids: Vec<String> = sample.sessions.iter().map(|s| s.uuid.clone()).collect();

    let project = encoded_root(&look.root);
    let mut found = Vec::new();
    for proc in &procs {
        let Some((id, _)) = look
            .scripts
            .iter()
            .find(|(_, rel)| !rel.is_empty() && proc.cmd.contains(rel.as_str()))
        else {
            continue;
        };
        found.push(Ran {
            script: id.clone(),
            pid: proc.pid,
            secs: proc.secs,
            rss: proc.rss,
            session: started_by(&procs, proc.pid, &live_uuids, &project),
        });
    }
    found.sort_by_key(|r| r.pid);
    sample.running = found;

    sample.ended = was
        .iter()
        .filter(|old| !sample.running.iter().any(|now| now.pid == old.pid))
        .cloned()
        .collect();
    *was = sample.running.clone();

    // Work a live session owns, whether or not the graph names what it runs.
    // A chain the `Script` pass above already reported is left to it, because
    // a declared script has a node of its own and more to say about itself.
    let tasks: Vec<Task> = tasks_from(&procs, &live_uuids, look, my_name())
        .into_iter()
        .filter(|t| !sample.running.iter().any(|r| t.pids.contains(&r.pid)))
        .collect();
    sample.done = were
        .iter()
        .filter(|old| !tasks.iter().any(|now| now.pid == old.pid))
        .cloned()
        .collect();
    *were = tasks.clone();
    sample.tasks = tasks;

    git_state(look, heads, &mut sample);
    tracing::debug!(
        "live probe: {} sessions, {} of {} scripts running, {} tasks, {} repos busy, \
         {:.1} runnable of {} cores, swept {} processes in {}ms",
        sample.sessions.len(),
        sample.running.len(),
        look.scripts.len(),
        sample.tasks.len(),
        sample.busy.len(),
        busy,
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        procs.len(),
        fork_ms
    );
    sample
}

/// Whether a repository is mid-operation, and whether its branch just changed.
/// Four `stat`s and one small read per repository, so it runs on every tick.
fn git_state(look: &Attention, heads: &mut Vec<(String, String)>, sample: &mut Sample) {
    for repo in &look.repos {
        let git = look.root.join(repo).join(".git");
        let op = if git.join("MERGE_HEAD").exists() {
            Some("merge")
        } else if git.join("REBASE_HEAD").exists() || git.join("rebase-merge").exists() {
            Some("rebase")
        } else if git.join("index.lock").exists() {
            Some("commit")
        } else {
            None
        };
        if let Some(op) = op {
            sample.busy.push((repo.clone(), op.to_string()));
        }
        // A branch switch is a changed `HEAD`, and the first reading of it is
        // not a switch — otherwise every launch would report one.
        if let Ok(head) = std::fs::read_to_string(git.join("HEAD")) {
            let head = head.trim().to_string();
            match heads.iter_mut().find(|(r, _)| r == repo) {
                Some((_, prev)) if *prev != head => *prev = head,
                Some(_) => {}
                None => heads.push((repo.clone(), head)),
            }
        }
    }
}

/// One row of `ps`.
#[derive(Clone, Debug, PartialEq)]
struct Proc {
    pid: u32,
    ppid: u32,
    /// The kernel had it on a run queue when `ps` looked.
    running: bool,
    secs: u64,
    /// Resident set, in bytes. `ps` reports kilobytes.
    rss: u64,
    cmd: String,
}

fn ps_all() -> Vec<Proc> {
    run_ps(&["-Ao", FIELDS])
}

/// Trailing `=` on each field suppresses the header, which is what makes the
/// output a table of values with nothing to skip.
const FIELDS: &str = "pid=,ppid=,stat=,etime=,rss=,command=";

fn run_ps(args: &[&str]) -> Vec<Proc> {
    // A process that has exited between being listed and being asked about is
    // an error from `ps` and an empty list here, which is the right answer.
    let Ok(out) = std::process::Command::new("/bin/ps").args(args).output() else {
        return Vec::new();
    };
    parse_ps(&String::from_utf8_lossy(&out.stdout))
}

/// Five fixed columns and then the command line, which may contain anything at
/// all including spaces — so it is taken as the rest of the line rather than
/// split.
fn parse_ps(text: &str) -> Vec<Proc> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(stat), Some(etime), Some(rss)) =
            (it.next(), it.next(), it.next(), it.next(), it.next())
        else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        // Everything after the fifth column, found by position so that a
        // command containing runs of spaces survives intact.
        let cmd = match line.find(rss).and_then(|i| line.get(i + rss.len()..)) {
            Some(rest) => rest.trim_start().to_string(),
            None => String::new(),
        };
        out.push(Proc {
            pid,
            ppid,
            running: stat.starts_with('R'),
            secs: etime_secs(etime),
            rss: rss.parse::<u64>().unwrap_or(0) * 1024,
            cmd,
        });
    }
    out
}

/// `ps` writes elapsed time as `[[DD-]HH:]MM:SS`.
fn etime_secs(s: &str) -> u64 {
    let (days, rest) = match s.split_once('-') {
        Some((d, rest)) => (d.parse::<u64>().unwrap_or(0), rest),
        None => (0, s),
    };
    let mut parts: Vec<u64> = rest.split(':').map(|p| p.parse().unwrap_or(0)).collect();
    while parts.len() < 3 {
        parts.insert(0, 0);
    }
    days * 86_400 + parts[0] * 3600 + parts[1] * 60 + parts[2]
}

/// The session id a command line *proves* it belongs to, and the project it
/// names while doing so.
///
/// Two shapes carry one, and both put it where only the system could have put
/// it rather than merely somewhere in the text:
///
/// - `/private/tmp/claude-501/-Users-someone-Code-Acme/<uuid>/scratchpad/...`
///   -- the directory an agent runs things out of, which names the project too.
/// - `claude --resume <uuid>` -- the session process itself, so a shell opened
///   for a tool call finds its owner by walking up to it.
///
/// **Matching the id anywhere in the command line was a real bug, not a
/// theoretical one.** The first version did, and credited an unrelated `ps` to
/// a session because the script it was running happened to *print* that id.
/// Mentioning a session is not belonging to one.
fn owner_in(cmd: &str) -> Option<(&str, Option<&str>)> {
    if let Some((uuid, project)) = scratchpad_owner(cmd) {
        return Some((uuid, Some(project)));
    }
    resumed(cmd).map(|uuid| (uuid, None))
}

/// `(session, project)` out of a scratchpad path, or nothing.
fn scratchpad_owner(cmd: &str) -> Option<(&str, &str)> {
    const AT: &str = "/private/tmp/claude-";
    let rest = &cmd[cmd.find(AT)? + AT.len()..];
    // The uid, then the encoded project, then the session, then anything.
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    let rest = rest.strip_prefix('/')?;
    let (project, rest) = rest.split_once('/')?;
    if !project.starts_with('-') {
        return None;
    }
    let (uuid, _) = rest.split_once('/')?;
    uuid_shaped(uuid).then_some((uuid, project))
}

/// The session a `claude --resume <uuid>` process is continuing.
fn resumed(cmd: &str) -> Option<&str> {
    let first = cmd.split_whitespace().next()?;
    if basename(first) != "claude" {
        return None;
    }
    let mut words = cmd.split_whitespace();
    while let Some(w) = words.next() {
        if w == "--resume" {
            return words.next().filter(|u| uuid_shaped(u));
        }
    }
    None
}

/// Cheaper than a regex and says exactly what is required: 8-4-4-4-12 hex.
fn uuid_shaped(s: &str) -> bool {
    s.len() == 36
        && s.as_bytes().iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The program a command line runs, with its directories dropped.
fn prog(cmd: &str) -> &str {
    basename(cmd.split_whitespace().next().unwrap_or(""))
}

/// This executable's own name, read once.
///
/// Aneural is started from an agent's shell as readily as anything else, so
/// without this the command centre opens a portal onto itself and reports its
/// own memory back to the person watching it.
fn my_name() -> Option<&'static str> {
    static ME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    ME.get_or_init(|| {
        std::env::current_exe()
            .ok()?
            .file_name()?
            .to_str()
            .map(str::to_string)
    })
    .as_deref()
}

/// Claude Code's own encoding of a project directory: every separator becomes a
/// dash.
///
/// Compared by encoding this workspace's root the same way, and never by
/// decoding theirs -- a repository called `my-project` decodes to `my/project`
/// and the ambiguity is not recoverable.
fn encoded_root(root: &Path) -> String {
    root.to_string_lossy().replace('/', "-")
}

/// Whether a process is the plumbing around the work rather than the work.
///
/// An agent's every tool call is a shell, and long-lived poll loops sit in
/// `sleep`, so without this the machine's own bookkeeping is most of what gets
/// reported.
fn inert(cmd: &str) -> bool {
    if cmd.contains(TOOL_SHELL) {
        return true;
    }
    let prog = prog(cmd);
    prog == "claude" || INERT.contains(&prog)
}

/// What to call a chain: the script at the top of it if there is one, else the
/// program holding the most memory. `uv run python refresh.py` reads as
/// `refresh.py`; a chain with no script in it reads as `python`.
fn name_of(top: &str, heavy: &str) -> String {
    if let Some(script) = script_in(top) {
        return script.to_string();
    }
    basename(heavy.split_whitespace().next().unwrap_or("")).to_string()
}

fn script_in(cmd: &str) -> Option<&str> {
    const EXT: [&str; 6] = [".sh", ".py", ".mjs", ".js", ".ts", ".rb"];
    cmd.split_whitespace()
        .find(|w| EXT.iter().any(|e| w.ends_with(e)))
        .map(basename)
}

/// Collapse every process a live session owns into one task per chain.
///
/// Ownership is resolved once per process rather than once per question: the
/// parent walk is up to [`DEPTH`] deep and there are a thousand processes, so
/// the claim each command line makes is read once and indexed.
fn tasks_from(procs: &[Proc], live: &[String], look: &Attention, me: Option<&str>) -> Vec<Task> {
    const DEPTH: usize = 12;
    let project = encoded_root(&look.root);
    let at: HashMap<u32, usize> = procs.iter().enumerate().map(|(i, p)| (p.pid, i)).collect();
    let claim: Vec<Option<(String, Option<String>)>> = procs
        .iter()
        .map(|p| owner_in(&p.cmd).map(|(u, e)| (u.to_string(), e.map(str::to_string))))
        .collect();

    // Walk up until something claims this process. A scratchpad path that names
    // another project ends the walk rather than continuing it: the chain is
    // owned, just not by work belonging here, and its `claude --resume` parent
    // further up would otherwise adopt it.
    let owner = |start: usize| -> Option<&str> {
        let mut i = start;
        for _ in 0..DEPTH {
            if let Some((uuid, encoded)) = &claim[i] {
                if encoded.as_deref().is_some_and(|e| e != project) {
                    return None;
                }
                if live.iter().any(|u| u == uuid) {
                    return Some(uuid);
                }
            }
            let ppid = procs[i].ppid;
            if ppid == 0 || ppid == procs[i].pid {
                return None;
            }
            i = *at.get(&ppid)?;
        }
        None
    };

    let mine: Vec<Option<&str>> = procs
        .iter()
        .enumerate()
        .map(|(i, p)| match inert(&p.cmd) || me == Some(prog(&p.cmd)) {
            true => None,
            false => owner(i),
        })
        .collect();

    let mut tasks = Vec::new();
    for (i, proc) in procs.iter().enumerate() {
        let Some(session) = mine[i] else { continue };
        // Only the top of a chain starts a task, and only once it has run long
        // enough to be worth a word. The age test belongs here rather than on
        // every member: the top of the chain is the age of the whole job, and a
        // child spawned a moment ago is still part of work an hour old.
        if proc.secs < YOUNG || ancestor_owned(procs, &at, &mine, i, DEPTH) {
            continue;
        }
        let mut fam = vec![i];
        loop {
            let before = fam.len();
            for (j, p) in procs.iter().enumerate() {
                if mine[j].is_some()
                    && !fam.contains(&j)
                    && fam.iter().any(|k| procs[*k].pid == p.ppid)
                {
                    fam.push(j);
                }
            }
            if fam.len() == before {
                break;
            }
        }
        let heavy = *fam.iter().max_by_key(|j| procs[**j].rss).unwrap_or(&i);
        let name = name_of(&proc.cmd, &procs[heavy].cmd);
        // The longest repository path the chain names, which is the most
        // specific place in the graph to put the portal.
        //
        // Matched as an *absolute* path rather than as the relative fragment
        // the graph stores. The root repository's path is `.`, and every
        // command line ever written contains a dot -- which matched every
        // chain to the workspace root and framed the portal on a node whose
        // every edge is `Contains`. The root is the fallback subject anyway,
        // so only a repository below it is worth finding here.
        let repo = look
            .repos
            .iter()
            .filter(|r| !r.is_empty() && r.as_str() != ".")
            .filter(|r| {
                let abs = look.root.join(r);
                let abs = abs.to_string_lossy().into_owned();
                fam.iter().any(|j| procs[*j].cmd.contains(&abs))
            })
            .max_by_key(|r| r.len())
            .cloned();
        tasks.push(Task {
            session: session.to_string(),
            pid: proc.pid,
            pids: fam.iter().map(|j| procs[*j].pid).collect(),
            secs: proc.secs,
            rss: fam.iter().map(|j| procs[*j].rss).sum(),
            watched: waited_on(procs, &name, session),
            name,
            repo,
        });
    }
    tasks.sort_by_key(|t| t.pid);
    tasks
}

/// Whether any owned ancestor already accounts for this process.
fn ancestor_owned(
    procs: &[Proc],
    at: &HashMap<u32, usize>,
    mine: &[Option<&str>],
    start: usize,
    depth: usize,
) -> bool {
    let mut ppid = procs[start].ppid;
    for _ in 0..depth {
        let Some(&i) = at.get(&ppid) else {
            return false;
        };
        if mine[i].is_some() {
            return true;
        }
        if procs[i].ppid == 0 || procs[i].ppid == procs[i].pid {
            return false;
        }
        ppid = procs[i].ppid;
    }
    false
}

/// Whether the owning session is sitting in a poll loop on this task.
///
/// An agent that starts something long and then waits for it writes exactly
/// that: a tool-call shell looping on `sleep` with the thing it is waiting for
/// named in the same command line. Those shells are chaff as *work*, which is
/// why they are matched here instead and reported as a state of the task.
fn waited_on(procs: &[Proc], name: &str, session: &str) -> bool {
    procs.iter().any(|p| {
        p.cmd.contains(TOOL_SHELL)
            && p.cmd.contains("sleep")
            && p.cmd.contains(name)
            && owner_in(&p.cmd).is_some_and(|(u, _)| u == session)
    })
}

/// Which live session started this process, for a script the graph names.
fn started_by(procs: &[Proc], pid: u32, live: &[String], project: &str) -> Option<String> {
    let mut at = pid;
    for _ in 0..12 {
        let proc = procs.iter().find(|p| p.pid == at)?;
        if let Some((uuid, encoded)) = owner_in(&proc.cmd) {
            if encoded.is_some_and(|e| e != project) {
                return None;
            }
            if live.iter().any(|u| u == uuid) {
                return Some(uuid.to_string());
            }
        }
        if proc.ppid == 0 || proc.ppid == at {
            return None;
        }
        at = proc.ppid;
    }
    None
}

/// Read `~/.claude/sessions/*.json`, keeping the ones working here.
///
/// The files are not deleted when a session ends — one on this machine was five
/// days stale with its pid still alive — so this returns the pid alongside and
/// the caller checks it against the process table. `statusUpdatedAt` is
/// deliberately not used as a heartbeat for the same reason.
fn sessions_in(dir: &Path, root: &Path) -> Vec<(u32, Seen)> {
    let Ok(entries) = std::fs::read_dir(dir.join("sessions")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(seen) = read_session(&v, root) else {
            continue;
        };
        let Some(pid) = v.get("pid").and_then(|p| p.as_u64()) else {
            continue;
        };
        out.push((pid as u32, seen));
    }
    out.sort_by_key(|(pid, _)| *pid);
    out
}

/// The fields worth having out of one session file, or nothing if it is not
/// about this workspace. Split out so it can be tested without a filesystem.
fn read_session(v: &serde_json::Value, root: &Path) -> Option<Seen> {
    let cwd = v.get("cwd")?.as_str()?;
    if !Path::new(cwd).starts_with(root) {
        return None;
    }
    let uuid = v.get("sessionId")?.as_str()?.to_string();
    if uuid.is_empty() {
        return None;
    }
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .filter(|n| !n.is_empty())
        .unwrap_or("session")
        .to_string();
    Some(Seen {
        uuid,
        name,
        status: v
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown")
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nobody_looking_and_nothing_live_is_the_slow_lane() {
        assert_eq!(pace(0.0, 10, false, false, 0), DROWSY);
    }

    #[test]
    fn something_running_in_front_of_you_is_sampled_eagerly() {
        assert_eq!(pace(0.2, 10, true, true, 0), EAGER);
    }

    #[test]
    fn a_loaded_machine_is_left_alone() {
        // One core busy on a ten-core box barely registers...
        assert_eq!(pace(1.0, 10, true, true, 0), EAGER);
        // ...every core busy backs all the way off, which is the whole point:
        // the work this feature exists to watch must not be slowed by watching.
        assert_eq!(pace(10.0, 10, true, true, 0), DROWSY);
        // and in between it is somewhere in between.
        let middle = pace(8.0, 10, true, true, 0);
        assert!(middle > EAGER && middle < DROWSY, "{middle:?}");
    }

    #[test]
    fn watching_nothing_for_a_while_slows_down_even_in_front_of_you() {
        assert!(pace(0.0, 10, true, false, QUIET) > pace(0.0, 10, true, false, 0));
    }

    #[test]
    fn the_interval_never_leaves_its_bounds() {
        for load in [0.0f32, 0.5, 4.0, 40.0, 400.0] {
            for cores in [1usize, 4, 16] {
                for focused in [true, false] {
                    for live in [true, false] {
                        let d = pace(load, cores, focused, live, 0);
                        assert!(d >= EAGER && d <= DROWSY, "{load} {cores} -> {d:?}");
                    }
                }
            }
        }
    }

    /// Real rows from a development machine; the user and session are stand-ins.
    const PS: &str = concat!(
        "48518     1 SN         03:51   2080 bash /private/tmp/claude-501/-Users-someone-Code-Aneural/1f2e3d4c-5b6a-4978-8a9b-0c1d2e3f4a5b/scratchpad/launch.sh\n",
        "48519     1 SN         03:51   1232 sleep 4000\n",
        "48521 48518 RN         03:51  14864 /Library/Frameworks/Python.framework/Versions/3.14/Resources/Python.app/Contents/MacOS/Python scripts/refresh_corpus.py\n",
        "   29     1 Ss   62-13:25:24  15248 /System/Library/Frameworks/CoreServices.framework/Versions/A/Support/fseventsd\n",
    );

    #[test]
    fn a_ps_table_reads_back_as_processes() {
        let procs = parse_ps(PS);
        assert_eq!(procs.len(), 4);
        assert_eq!(procs[2].pid, 48521);
        assert_eq!(procs[2].ppid, 48518);
        assert_eq!(procs[2].rss, 14864 * 1024, "ps reports kilobytes");
        assert!(procs[2].running, "state R is on a run queue");
        assert!(!procs[0].running);
        assert_eq!(
            procs[2].cmd,
            "/Library/Frameworks/Python.framework/Versions/3.14/Resources/Python.app/Contents/MacOS/Python scripts/refresh_corpus.py",
            "the command line is the rest of the line, spaces and all"
        );
    }

    #[test]
    fn elapsed_time_reads_in_every_shape_ps_writes_it() {
        assert_eq!(etime_secs("03:51"), 231);
        assert_eq!(etime_secs("1:02:03"), 3723);
        assert_eq!(
            etime_secs("62-13:25:24"),
            62 * 86400 + 13 * 3600 + 25 * 60 + 24
        );
        assert_eq!(
            etime_secs("nonsense"),
            0,
            "never a panic over a format change"
        );
    }

    #[test]
    fn a_script_is_matched_on_its_path_in_the_command_line() {
        let procs = parse_ps(PS);
        let rel = "scripts/refresh_corpus.py";
        let hit: Vec<u32> = procs
            .iter()
            .filter(|p| p.cmd.contains(rel))
            .map(|p| p.pid)
            .collect();
        assert_eq!(hit, vec![48521], "the interpreter is never guessed at");
    }

    #[test]
    fn a_script_is_attributed_to_the_session_whose_scratchpad_launched_it() {
        let procs = parse_ps(PS);
        let uuid = "1f2e3d4c-5b6a-4978-8a9b-0c1d2e3f4a5b".to_string();
        // python -> bash, whose argv carries the session's own scratchpad path
        let here = "-Users-someone-Code-Aneural";
        assert_eq!(
            started_by(&procs, 48521, std::slice::from_ref(&uuid), here),
            Some(uuid.clone())
        );
        // A session nobody has said is live is never inferred...
        assert_eq!(started_by(&procs, 48521, &[], here), None);
        // ...and a process with no such ancestor belongs to nobody.
        assert_eq!(
            started_by(&procs, 48519, std::slice::from_ref(&uuid), here),
            None
        );
        // ...and neither does work a session here is doing for somewhere else.
        assert_eq!(
            started_by(&procs, 48521, &[uuid], "-Users-someone-Code-Acme"),
            None
        );
    }

    #[test]
    fn a_chain_that_loops_or_runs_out_does_not_hang() {
        let loops = "10 11 S 00:01 100 a\n11 10 S 00:01 100 b\n";
        assert_eq!(started_by(&parse_ps(loops), 10, &["x".into()], "-p"), None);
        assert_eq!(started_by(&[], 10, &["x".into()], "-p"), None);
    }

    // ---------------------------------------------------- ownership and tasks
    //
    // Every command line below was captured from a development machine with
    // `ps -o pid=,ppid=,stat=,etime=,rss=,command=` while three real jobs an
    // agent had started were running. The user, project and session names
    // are stand-ins; the shapes are untouched. The watcher's line is the only one
    // shortened, and only in the middle, because it is a 900-character shell
    // one-liner; every marker the code looks at is kept verbatim.
    const OWNED: &str = concat!(
        "64788     1 SN   01:13:07   1056 /bin/bash /private/tmp/claude-501/-Users-someone-Code-Acme/0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d/scratchpad/nightly-corpus/rerun.sh\n",
        "67763 64788 SN      22:47  12200 uv run --no-sync --project /Users/someone/Code/Acme/tools/acme-eval acme eval --corpus devices-sql\n",
        "67764 67763 RN      22:47 301492 /Users/someone/Code/Acme/tools/acme-eval/.venv/bin/python /Users/someone/Code/Acme/tools/acme-eval/.venv/bin/acme eval --corpus devices-sql\n",
        "64789 55095 Ss   01:13:07   2112 /bin/zsh -c source /Users/someone/.claude/shell-snapshots/snapshot-zsh-1700000000000-abc123.sh 2>/dev/null || true && eval 'S=/private/tmp/claude-501/-Users-someone-Code-Acme/0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d/scratchpad/nightly-corpus; until grep -q \"chain done\" $S/rerun.log || ! pgrep -f \"nightly-corpus/rerun.sh\"; do sleep 60; done'\n",
        "55095 55056 S+   04:20:38 385638 claude --resume 0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d\n",
        "69304 64789 S+      00:03   1232 sleep 60\n",
    );

    const ACME: &str = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";

    fn looking_at_acme() -> Attention {
        Attention {
            focused: true,
            root: PathBuf::from("/Users/someone/Code/Acme"),
            claude: None,
            scripts: Vec::new(),
            repos: vec!["tools/acme-eval".into(), "acme-ingest".into()],
        }
    }

    #[test]
    fn a_session_id_is_only_believed_where_the_system_puts_it() {
        // The shape that proves it: the scratchpad an agent runs things out of,
        // which names the project as well as the session.
        let real = "/bin/bash /private/tmp/claude-501/-Users-someone-Code-Acme/0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d/scratchpad/nightly-corpus/rerun.sh";
        assert_eq!(
            owner_in(real),
            Some((ACME, Some("-Users-someone-Code-Acme")))
        );
        // The session process itself, so a tool call's shell can find its owner.
        let resumed = "claude --resume 0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";
        assert_eq!(owner_in(resumed), Some((ACME, None)));

        // And the regression this function exists for: a command line that
        // merely *mentions* a session owns nothing. This one is real -- it is a
        // script that printed the id, and the first version of this code
        // credited it to that session.
        let mention = "python3 -c live={\"0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d\":\"acme\"}";
        assert_eq!(owner_in(mention), None);
        // Nor does a scratchpad path with something that is not a session in it.
        let bogus =
            "sh /private/tmp/claude-501/-Users-someone-Code-Acme/not-a-uuid/scratchpad/x.sh";
        assert_eq!(owner_in(bogus), None);
        // Nor `--resume` on anything but Claude Code.
        assert_eq!(
            owner_in("systemctl --resume 0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d"),
            None
        );
    }

    #[test]
    fn a_project_path_is_compared_encoded_and_never_decoded() {
        assert_eq!(
            encoded_root(Path::new("/Users/someone/Code/Acme")),
            "-Users-someone-Code-Acme"
        );
        // The reason it is only ever compared in this direction: decoding is
        // ambiguous, and a repository with a dash in its name would come back
        // as two directories that do not exist.
        assert_eq!(
            encoded_root(Path::new("/Users/someone/Code/my-project")),
            "-Users-someone-Code-my-project"
        );
    }

    #[test]
    fn the_plumbing_around_the_work_is_not_the_work() {
        assert!(inert(
            "/bin/zsh -c source /Users/someone/.claude/shell-snapshots/snapshot-zsh-1.sh"
        ));
        assert!(inert(
            "claude --resume 0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d"
        ));
        assert!(inert("sleep 60"));
        assert!(inert("/bin/ps -Ao pid=,ppid="));
        assert!(!inert(
            "/bin/bash /private/tmp/claude-501/-p/u/scratchpad/rerun.sh"
        ));
        assert!(!inert("uv run python scripts/download_filings.py"));
    }

    #[test]
    fn a_chain_is_named_for_its_script_and_not_its_interpreter() {
        // Real: the interpreter is three levels down and says nothing useful.
        assert_eq!(
            name_of(
                "/bin/bash /private/tmp/claude-501/-p/u/scratchpad/nightly-corpus/rerun.sh",
                "/x/.venv/bin/python /x/.venv/bin/acme eval"
            ),
            "rerun.sh"
        );
        assert_eq!(
            name_of(
                "uv run python scripts/download_filings.py --documents-only",
                "python3"
            ),
            "download_filings.py"
        );
        // Nothing script-shaped anywhere: the program doing the work will do.
        assert_eq!(
            name_of("cargo build -j10", "/usr/bin/rustc --edition 2024"),
            "rustc"
        );
    }

    #[test]
    fn three_processes_an_agent_started_read_as_one_task() {
        let live = vec![ACME.to_string()];
        let tasks = tasks_from(&parse_ps(OWNED), &live, &looking_at_acme(), None);
        assert_eq!(
            tasks.len(),
            1,
            "one job, not three processes and two shells"
        );
        let task = &tasks[0];
        assert_eq!(
            task.pid, 64788,
            "the top of the chain, so it keeps its name"
        );
        assert_eq!(task.pids, vec![64788, 67763, 67764]);
        assert_eq!(task.name, "rerun.sh");
        assert_eq!(task.session, ACME);
        assert_eq!(
            task.secs, 4387,
            "the age of the job, not of its newest child"
        );
        assert_eq!(
            task.rss,
            (1056 + 12200 + 301492) * 1024,
            "summed, so a uv wrapper is not credited with its child's memory"
        );
        assert_eq!(
            task.repo.as_deref(),
            Some("tools/acme-eval"),
            "the most specific place in the graph the chain names"
        );
        assert!(
            task.watched,
            "the session is sitting in a sleep loop on rerun.log, so it is blocked"
        );
    }

    #[test]
    fn a_task_belongs_to_nobody_until_its_session_is_known_to_be_live() {
        let procs = parse_ps(OWNED);
        assert!(tasks_from(&procs, &[], &looking_at_acme(), None).is_empty());
        // A session working here but running something for another workspace is
        // not this workspace's business, and its `claude --resume` parent
        // further up the chain must not adopt it either.
        let mut elsewhere = looking_at_acme();
        elsewhere.root = PathBuf::from("/Users/someone/Code/Aneural");
        let live = vec![ACME.to_string()];
        assert!(tasks_from(&procs, &live, &elsewhere, None).is_empty());
    }

    #[test]
    fn aneural_never_reports_itself() {
        // It is launched from an agent's shell like anything else, and a
        // command centre that opens a portal onto its own memory use is a joke
        // at its own expense.
        let mine = "72619 55095 SN   02:10 368537 /Users/someone/Code/Aneural/target/debug/aneural-gui /Users/someone/Code/Acme\n";
        let procs = parse_ps(&format!("{OWNED}{mine}"));
        let live = vec![ACME.to_string()];
        let look = looking_at_acme();
        assert_eq!(
            tasks_from(&procs, &live, &look, Some("aneural-gui")).len(),
            1
        );
        assert_eq!(
            tasks_from(&procs, &live, &look, None).len(),
            2,
            "and it is excluded by name, not by being uninteresting"
        );
    }

    #[test]
    fn a_job_is_not_news_until_it_has_been_running_a_while() {
        // Everything an agent does is a process; without this every `ls` is a
        // portal. The child being young does not make the job young.
        let young = OWNED.replace("01:13:07", "00:04");
        let live = vec![ACME.to_string()];
        assert!(tasks_from(&parse_ps(&young), &live, &looking_at_acme(), None).is_empty());
    }

    #[test]
    fn a_job_nobody_is_waiting_on_says_so() {
        // Same chain with the watcher's poll loop removed.
        let unwatched: String = OWNED
            .lines()
            .filter(|l| !l.contains("shell-snapshots"))
            .map(|l| format!("{l}\n"))
            .collect();
        let live = vec![ACME.to_string()];
        let tasks = tasks_from(&parse_ps(&unwatched), &live, &looking_at_acme(), None);
        assert_eq!(tasks.len(), 1);
        assert!(!tasks[0].watched);
    }

    fn session(cwd: &str, uuid: &str) -> serde_json::Value {
        serde_json::json!({
            "pid": 88999, "sessionId": uuid, "cwd": cwd,
            "name": "plan-visualization", "status": "busy"
        })
    }

    #[test]
    fn a_session_working_elsewhere_is_not_this_workspaces_business() {
        let root = Path::new("/Users/someone/Code/Aneural");
        assert!(read_session(&session("/Users/someone/Code/Aneural", "a"), root).is_some());
        assert!(read_session(&session("/Users/someone/Code/Acme", "a"), root).is_none());
    }

    #[test]
    fn a_session_file_missing_what_matters_is_skipped_not_guessed_at() {
        let root = Path::new("/");
        assert!(read_session(&serde_json::json!({"cwd": "/"}), root).is_none());
        assert!(read_session(&session("/", ""), root).is_none());
        // No name is a session with no name, not a session that is not there.
        let mut v = session("/", "a");
        v.as_object_mut().unwrap().remove("name");
        assert_eq!(read_session(&v, root).unwrap().name, "session");
    }

    #[test]
    fn the_status_is_taken_as_written_and_a_missing_one_is_not_guessed() {
        let root = Path::new("/");
        assert_eq!(
            read_session(&session("/", "a"), root).unwrap().status,
            "busy"
        );
        let mut v = session("/", "a");
        v.as_object_mut().unwrap().remove("status");
        assert_eq!(read_session(&v, root).unwrap().status, "unknown");
    }
}
