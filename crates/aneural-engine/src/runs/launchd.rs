//! The launch agents this machine actually has.
//!
//! Read, never written — writing one is a later, consented step. The directory
//! is injectable so no test touches the real `~/Library/LaunchAgents`, the same
//! arrangement as `history.claudeRoot`.
//!
//! The plist reader is deliberately small: it pulls out the handful of keys that
//! decide *when* and *what*, and ignores the rest. A full plist parser would be a
//! dependency, and a launch agent that uses something this does not understand
//! should read as "installed, interval unknown" rather than fail to appear.
//!
//! It also reads what launchd will say about a *past* run, which is very little.
//! launchd keeps no history at all: `launchctl list` gives the current pid and
//! the last exit status, and nothing else survives. So the only evidence that an
//! agent has ever fired is the log files its own plist names — their modification
//! time and their size. That is a real observation and it is recorded as one:
//! every record this file produces is [`journal::fidelity::OBSERVED`], and the
//! instant on it is when output was last *seen*, not when the process began.
//!
//! The one exception is an agent that is running *now*. A log's modification
//! time cannot name a run that is still writing to it — it moves on every tick,
//! and a run id minted from it is a new run each time, which is how one
//! `KeepAlive` agent once grew two hundred journal lines in a month. A live
//! process has a start time of its own, and that is what names it: `ps` gives
//! it, the id holds still for as long as the process lives, and when the pid
//! goes the same id gets its finish line.

use super::store::RunStore;
use super::{Installed, journal};
use crate::Result;
use aneural_core::NodeId;
use std::collections::BTreeMap;
use std::path::Path;

/// Labels Aneural itself owns. Nothing outside this prefix is ever written or
/// removed; a foreign agent is only ever an observation.
pub const OWNED_PREFIX: &str = "dev.aneural.";

/// Every agent in a directory, newest keys winning within a file.
pub fn installed(dir: &Path, ws_root: &Path, loaded: &BTreeMap<String, Loaded>) -> Vec<Installed> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("plist"))
        .collect();
    paths.sort();

    paths
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .filter_map(|text| agent(&text, ws_root, loaded))
        .filter(concerns_this_workspace)
        .collect()
}

/// Whether an agent has anything to do with the workspace that was opened.
///
/// A machine has launch agents for updaters, sync clients and whatever else;
/// putting all of them on a workspace's canvas is noise. An agent is relevant
/// when it runs something in this workspace, or when Aneural wrote it.
fn concerns_this_workspace(a: &Installed) -> bool {
    a.script.is_some() || a.label.starts_with(OWNED_PREFIX)
}

/// One agent out of one plist.
fn agent(text: &str, ws_root: &Path, loaded: &BTreeMap<String, Loaded>) -> Option<Installed> {
    let label = string_for("Label", text)?;
    let args = array_for("ProgramArguments", text);
    let working = string_for("WorkingDirectory", text).unwrap_or_default();
    let calendar = dict_for("StartCalendarInterval", text);
    let interval = integer_for("StartInterval", text);
    let keep_alive = bool_for("KeepAlive", text).unwrap_or(false);

    let state = loaded.get(&label).copied();
    Some(Installed {
        command: args.join(" "),
        script: script_of(&args, &working, ws_root),
        when: when(calendar.as_deref(), interval, keep_alive),
        every_days: every_days(calendar.as_deref(), interval, keep_alive),
        loaded: state.is_some(),
        log_out: string_for("StandardOutPath", text),
        log_err: string_for("StandardErrorPath", text),
        pid: state.and_then(|s| s.pid),
        last_exit: state.and_then(|s| s.last_exit),
        kind: "launchd".into(),
        label,
    })
}

/// How often it fires, in days, when that can be told.
///
/// The unit is days because that is what a declared cadence is in. A launch
/// agent saying "06:00 every day" and a table saying `daily` are the same claim
/// written two ways, and only a comparison in a shared unit sees that.
fn every_days(calendar: Option<&str>, interval: Option<i64>, keep_alive: bool) -> Option<u32> {
    if let Some(cal) = calendar {
        // Coarsest field present wins: a `Month` fires yearly however many
        // hours and minutes it also pins down.
        return Some(match cal {
            c if has_key("Month", c) => 365,
            c if has_key("Day", c) => 30,
            c if has_key("Weekday", c) => 7,
            _ => 1,
        });
    }
    if let Some(secs) = interval {
        // Integer days, and deliberately not floored at 1: an agent firing
        // hourly is not a daily one, and rounding it up would make it agree
        // with a `daily` declaration it does not match. Zero reads as "more
        // often than a cadence can express".
        return Some((secs / 86_400).max(0) as u32);
    }
    // Neither a calendar nor an interval: either a KeepAlive daemon, which is
    // always running rather than firing on a rhythm, or a run-once agent. There
    // is no interval to compare either against, and saying "every 1 day" would
    // be a lie, so the caller is told nothing rather than something wrong.
    let _ = keep_alive;
    None
}

fn when(calendar: Option<&str>, interval: Option<i64>, keep_alive: bool) -> String {
    if let Some(cal) = calendar {
        let at = |k: &str| integer_for(k, cal);
        let hhmm = match (at("Hour"), at("Minute")) {
            (Some(h), Some(m)) => format!("{h:02}:{m:02}"),
            (Some(h), None) => format!("{h:02}:00"),
            _ => String::new(),
        };
        let day = match (at("Weekday"), at("Day"), at("Month")) {
            (Some(w), _, _) => format!("weekday {w}"),
            (_, Some(d), None) => format!("day {d} of the month"),
            (_, Some(d), Some(m)) => format!("{d}/{m}"),
            _ => "daily".into(),
        };
        return format!("{day} at {hhmm}").trim().to_string();
    }
    if let Some(secs) = interval {
        return format!("every {secs}s");
    }
    match keep_alive {
        true => "kept alive".into(),
        false => "at load only".into(),
    }
}

/// Which script in this workspace an agent runs, if one can be recognised.
///
/// Matched by path rather than by name: the arguments hold an absolute
/// interpreter and whatever the author passed, and only the argument that
/// resolves to a file inside the workspace is the script.
fn script_of(args: &[String], working: &str, ws_root: &Path) -> Option<NodeId> {
    let base = match working.is_empty() {
        true => ws_root.to_path_buf(),
        false => Path::new(working).to_path_buf(),
    };
    for arg in args {
        if arg.starts_with('-') {
            continue;
        }
        let candidate = match arg.starts_with('/') {
            true => Path::new(arg).to_path_buf(),
            false => base.join(arg),
        };
        // An interpreter is an absolute path outside the workspace; the script
        // is the argument that lands inside it.
        if let Ok(rel) = candidate.strip_prefix(ws_root)
            && rel.extension().is_some()
        {
            return Some(NodeId::script(rel, None));
        }
    }
    None
}

/// What launchd currently says about one loaded job.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Loaded {
    /// The process, if it is running right now.
    pub pid: Option<u32>,
    /// How it exited last time. Negative is a signal number: `-9` is a kill.
    /// `None` when launchd reports it has never exited.
    pub last_exit: Option<i64>,
}

/// What launchd has loaded for this user, and how each one last ended.
///
/// One `launchctl list` rather than a `launchctl print` per agent: one process
/// instead of one per job, and the three columns it prints — pid, last exit
/// status, label — are the whole of what launchd remembers about a past run.
/// `print` is richer about *configuration*, which the plist already told us.
#[cfg(target_os = "macos")]
pub fn loaded() -> BTreeMap<String, Loaded> {
    let Ok(out) = std::process::Command::new("launchctl").arg("list").output() else {
        return BTreeMap::new();
    };
    parse_list(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(not(target_os = "macos"))]
pub fn loaded() -> BTreeMap<String, Loaded> {
    BTreeMap::new()
}

/// `PID\tStatus\tLabel`, with `-` for either number when there is none.
#[cfg(any(target_os = "macos", test))]
fn parse_list(text: &str) -> BTreeMap<String, Loaded> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let mut cols = line.split('\t');
            let pid = cols.next()?.trim();
            let status = cols.next()?.trim();
            let label = cols.next()?.trim();
            if label.is_empty() {
                return None;
            }
            Some((
                label.to_string(),
                Loaded {
                    pid: pid.parse().ok(),
                    last_exit: status.parse().ok(),
                },
            ))
        })
        .collect()
}

/// When each of these processes began, in unix milliseconds, for the ones that
/// are alive.
///
/// One `ps` for all of them, and none at all for an empty list, so a machine
/// with nothing running — and every test — never forks for this. `lstart` and
/// not `etime`: elapsed time is read against a clock that has moved on by the
/// next tick, so "now minus elapsed" lands on a different millisecond each time
/// it is computed, and the whole point of the start time is that it holds still.
#[cfg(target_os = "macos")]
pub fn process_starts(pids: &[u32]) -> BTreeMap<u32, i64> {
    if pids.is_empty() {
        return BTreeMap::new();
    }
    let list: Vec<String> = pids.iter().map(|p| p.to_string()).collect();
    let Ok(out) = std::process::Command::new("ps")
        .args(["-o", "pid=,lstart=", "-p", &list.join(",")])
        .output()
    else {
        return BTreeMap::new();
    };
    parse_starts(&String::from_utf8_lossy(&out.stdout), local_offset)
}

#[cfg(not(target_os = "macos"))]
pub fn process_starts(_pids: &[u32]) -> BTreeMap<u32, i64> {
    BTreeMap::new()
}

/// The offset this machine's clock shows at a given wall-clock instant, so a
/// start time `ps` prints in local time can be placed on the UTC line the
/// journal is written in.
#[cfg(target_os = "macos")]
fn local_offset(local: time::PrimitiveDateTime) -> time::UtcOffset {
    // The offset depends on the instant, and the instant is what is being
    // looked for: take the offset in force if the local reading were UTC, then
    // read it again at the instant that implies. Two steps cover a start on
    // the far side of a daylight-saving change.
    let guess =
        time::UtcOffset::local_offset_at(local.assume_utc()).unwrap_or(time::UtcOffset::UTC);
    time::UtcOffset::local_offset_at(local.assume_offset(guess)).unwrap_or(guess)
}

/// `PID  Www Mmm dd HH:MM:SS YYYY`, one per line, as `ps -o pid=,lstart=`
/// prints it. The offset is passed in so the parser can be tested against a
/// known zone rather than whatever the test machine is set to.
#[cfg(any(target_os = "macos", test))]
fn parse_starts(
    text: &str,
    offset: impl Fn(time::PrimitiveDateTime) -> time::UtcOffset,
) -> BTreeMap<u32, i64> {
    text.lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let pid: u32 = cols.next()?.parse().ok()?;
            let _weekday = cols.next()?;
            let month = month_of(cols.next()?)?;
            let day: u8 = cols.next()?.parse().ok()?;
            let mut hms = cols.next()?.split(':');
            let hour: u8 = hms.next()?.parse().ok()?;
            let minute: u8 = hms.next()?.parse().ok()?;
            let second: u8 = hms.next()?.parse().ok()?;
            let year: i32 = cols.next()?.parse().ok()?;
            let date = time::Date::from_calendar_date(year, month, day).ok()?;
            let time = time::Time::from_hms(hour, minute, second).ok()?;
            let local = time::PrimitiveDateTime::new(date, time);
            let at = local.assume_offset(offset(local));
            Some((pid, (at.unix_timestamp_nanos() / 1_000_000) as i64))
        })
        .collect()
}

#[cfg(any(target_os = "macos", test))]
fn month_of(name: &str) -> Option<time::Month> {
    use time::Month::*;
    Some(match name {
        "Jan" => January,
        "Feb" => February,
        "Mar" => March,
        "Apr" => April,
        "May" => May,
        "Jun" => June,
        "Jul" => July,
        "Aug" => August,
        "Sep" => September,
        "Oct" => October,
        "Nov" => November,
        "Dec" => December,
        _ => return None,
    })
}

/// The journal records for runs that have left evidence behind.
///
/// At most two per agent — a finish for a run opened earlier and a start for
/// the one running now — and only when there is something to date a run by: an
/// undated run is not a run record, and standing `now` in for the missing
/// instant would mint a brand-new fake run on every tick of the timer.
///
/// The id is derived from what was observed, so **observing the same state
/// twice produces the same record**. That is what makes it safe to call this on
/// a timer and append whatever it returns: the second observation is the same
/// row, not a second run. A running agent is dated by its process start, from
/// `starts`, and not by its log, which it is still writing to; the store is
/// asked which run that agent already has open, so a pid that has gone since
/// the last look closes that run instead of opening another.
pub fn observe(
    agents: &[Installed],
    machine: &str,
    starts: &BTreeMap<u32, i64>,
    store: &RunStore,
) -> Result<Vec<journal::Record>> {
    let mut out = Vec::new();
    for agent in agents {
        out.extend(observed(agent, machine, starts, store)?);
    }
    Ok(out)
}

fn observed(
    agent: &Installed,
    machine: &str,
    starts: &BTreeMap<u32, i64>,
    store: &RunStore,
) -> Result<Vec<journal::Record>> {
    // Without a script there is nothing in the graph to hang the run off, and
    // the point of a Run node is to sit beside the thing that ran.
    let Some(script) = agent.script.as_ref() else {
        return Ok(Vec::new());
    };
    let script_key = super::script_key(script);
    let out = agent.log_out.as_deref().and_then(log_state);
    let err = agent.log_err.as_deref().and_then(log_state);

    // The newest of the two, because either stream may be the one in use. A
    // zero-byte stdout beside a busy stderr is the normal shape for anything
    // using Python's logging, and reading "no output" as "never ran" would
    // report every such job as having never fired.
    let log_millis = [out, err].iter().flatten().map(|(mtime, _)| *mtime).max();

    let schedule_id = NodeId::installed_schedule(&agent.label)
        .as_str()
        .to_string();
    let latest = store.latest_for_schedule(&schedule_id)?;
    let open = latest
        .clone()
        .filter(|r| r.status == journal::status::RUNNING);

    // Everything but the fields that say *which* run and *where it got to*.
    let seen = |millis: i64| journal::Record {
        run_id: journal::run_id(machine, millis, &script_key),
        machine: machine.to_string(),
        script_key: script_key.clone(),
        schedule_id: Some(schedule_id.clone()),
        trigger: journal::trigger::LAUNCHD.to_string(),
        argv: agent.command.split_whitespace().map(String::from).collect(),
        cwd: String::new(),
        started_at: journal::rfc3339(millis),
        // Not `started_at` plus something: launchd has not told us how long it
        // ran, and a duration invented from the log's timestamps would be the
        // gap between writes rather than the life of the process.
        ended_at: None,
        status: String::new(),
        exit_code: None,
        duration_ms: None,
        out_bytes: out.map(|(_, size)| size),
        err_bytes: err.map(|(_, size)| size),
        tail: tail_of_newest(out, err, agent),
        log_path: newest_path(out, err, agent),
        fidelity: journal::fidelity::OBSERVED.to_string(),
        // A run seen after the fact is a finished observation, never half of a
        // pair, so it takes the finished sequence number. A run seen *running*
        // takes it too, so that a journal written before runs were keyed on
        // the process reads the same as one written after; its finish line
        // then has to outrank it, which is what `CLOSED` is for.
        seq: 1,
    };
    // The line that closes a run opened on an earlier tick: the same id, so
    // the two collapse at ingest, and one `seq` higher, so the finish wins.
    let closed = |open: &journal::Record, status: &str, exit_code: Option<i64>| journal::Record {
        run_id: open.run_id.clone(),
        started_at: open.started_at.clone(),
        status: status.to_string(),
        exit_code,
        seq: CLOSED,
        ..seen(0)
    };

    let mut records = Vec::new();
    match (agent.pid, agent.pid.and_then(|p| starts.get(&p).copied())) {
        (Some(_), Some(start)) => {
            let running = journal::Record {
                status: journal::status::RUNNING.to_string(),
                ..seen(start)
            };
            // A run left open under another id is not this process. Either it
            // was a process launchd has since restarted, or a line from before
            // runs were keyed on the process; in neither case is how it ended
            // known, and `last_exit` may belong to it or to something older.
            if let Some(prev) = open.as_ref().filter(|r| r.run_id != running.run_id) {
                records.push(closed(prev, journal::status::UNKNOWN, None));
            }
            records.push(running);
        }
        (Some(_), None) => {
            // Alive, and `ps` could not date it. The run it already has open
            // is still its run; failing that, the log is the only date there
            // is, and the open row this mints is what the next tick finds.
            match (open, log_millis) {
                (Some(prev), _) => records.push(journal::Record {
                    run_id: prev.run_id.clone(),
                    started_at: prev.started_at.clone(),
                    status: journal::status::RUNNING.to_string(),
                    ..seen(0)
                }),
                (None, Some(millis)) => records.push(journal::Record {
                    status: journal::status::RUNNING.to_string(),
                    ..seen(millis)
                }),
                (None, None) => {}
            }
        }
        (None, _) => {
            let status = ended(agent.last_exit);
            if let Some(prev) = open {
                let mut done = closed(&prev, status, agent.last_exit);
                // When output was last seen, which is the nearest thing to an
                // end an observer has — and only if that is after the start,
                // or the run would be recorded as ending before it began.
                done.ended_at = log_millis
                    .filter(|m| journal::millis_of(&prev.started_at).is_some_and(|s| *m >= s))
                    .map(journal::rfc3339);
                records.push(done);
            } else if let Some(millis) = log_millis {
                // A run already closed on this very evidence: the finish line
                // carries the log instant it was closed on, and the log has not
                // moved since. Minting from it would make one run two.
                let already = latest
                    .as_ref()
                    .is_some_and(|r| r.ended_at.as_deref() == Some(&journal::rfc3339(millis)));
                if !already {
                    records.push(journal::Record {
                        status: status.to_string(),
                        exit_code: agent.last_exit,
                        ..seen(millis)
                    });
                }
            }
        }
    }
    Ok(records)
}

/// The `seq` of the line that closes an observed run. Above the `1` every
/// observed line carries, so it wins at ingest whichever order the two arrive.
pub const CLOSED: i64 = 2;

/// How a run that is no longer running ended, as far as launchd will say.
fn ended(last_exit: Option<i64>) -> &'static str {
    match last_exit {
        Some(0) => journal::status::OK,
        Some(_) => journal::status::FAILED,
        // Installed, has run at some point, and launchd has since forgotten —
        // which is what an unloaded agent looks like. Guessing `ok` here would
        // turn a job that died into a job that succeeded.
        None => journal::status::UNKNOWN,
    }
}

/// `(mtime millis, size)` for a log file that exists.
fn log_state(path: &str) -> Option<(i64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    Some((mtime, meta.len() as i64))
}

/// Which log was written to most recently, out of the two.
fn newest_path(
    out: Option<(i64, i64)>,
    err: Option<(i64, i64)>,
    agent: &Installed,
) -> Option<String> {
    let candidates = [
        (out, agent.log_out.as_deref()),
        (err, agent.log_err.as_deref()),
    ];
    candidates
        .iter()
        .filter_map(|(state, path)| Some((state.as_ref()?.0, (*path)?)))
        .max_by_key(|(mtime, _)| *mtime)
        .map(|(_, path)| path.to_string())
}

/// The end of the newest log, collapsed and capped to what may be committed.
fn tail_of_newest(
    out: Option<(i64, i64)>,
    err: Option<(i64, i64)>,
    agent: &Installed,
) -> Option<String> {
    journal::tail_of_file(std::path::Path::new(&newest_path(out, err, agent)?))
}

// ---- the small plist reader -------------------------------------------------

/// The raw text of the value element following `<key>name</key>`.
fn value_after(name: &str, text: &str) -> Option<(usize, usize)> {
    let key = format!("<key>{name}</key>");
    let at = text.find(&key)? + key.len();
    let rest = &text[at..];
    let open = rest.find('<')?;
    Some((at + open, text.len()))
}

fn string_for(name: &str, text: &str) -> Option<String> {
    let (from, _) = value_after(name, text)?;
    let rest = &text[from..];
    let inner = rest.strip_prefix("<string>")?;
    let end = inner.find("</string>")?;
    Some(unescape(&inner[..end]))
}

fn integer_for(name: &str, text: &str) -> Option<i64> {
    let (from, _) = value_after(name, text)?;
    let rest = &text[from..];
    let inner = rest.strip_prefix("<integer>")?;
    let end = inner.find("</integer>")?;
    inner[..end].trim().parse().ok()
}

fn bool_for(name: &str, text: &str) -> Option<bool> {
    let (from, _) = value_after(name, text)?;
    let rest = text[from..].trim_start();
    if rest.starts_with("<true/>") {
        return Some(true);
    }
    if rest.starts_with("<false/>") {
        return Some(false);
    }
    None
}

fn array_for(name: &str, text: &str) -> Vec<String> {
    let Some((from, _)) = value_after(name, text) else {
        return Vec::new();
    };
    let rest = &text[from..];
    let Some(inner) = rest.strip_prefix("<array>") else {
        return Vec::new();
    };
    let Some(end) = inner.find("</array>") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut body = &inner[..end];
    while let Some(open) = body.find("<string>") {
        body = &body[open + "<string>".len()..];
        let Some(close) = body.find("</string>") else {
            break;
        };
        out.push(unescape(&body[..close]));
        body = &body[close..];
    }
    out
}

/// The inner text of a `<dict>` value, so nested keys can be read from it alone.
fn dict_for(name: &str, text: &str) -> Option<String> {
    let (from, _) = value_after(name, text)?;
    let rest = &text[from..];
    let inner = rest.strip_prefix("<dict>")?;
    let end = inner.find("</dict>")?;
    Some(inner[..end].to_string())
}

fn has_key(name: &str, text: &str) -> bool {
    text.contains(&format!("<key>{name}</key>"))
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Shaped on a real agent, comments and all: the ones in the wild wrap the
    /// command in `caffeinate`, call the interpreter by absolute path, and carry
    /// explanatory comments *inside* the `<array>`.
    const DAILY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.example.extract</string>

    <key>ProgramArguments</key>
    <array>
        <!-- -i = no idle sleep, -s = no system sleep on AC. -->
        <string>/usr/bin/caffeinate</string>
        <string>-i</string>
        <string>-s</string>
        <string>/Users/someone/.local/bin/uv</string>
        <string>run</string>
        <string>python</string>
        <string>scripts/extract_summaries.py</string>
    </array>

    <key>WorkingDirectory</key>
    <string>/ws/ingest</string>

    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>/opt/homebrew/bin:/usr/bin:/bin</string>
    </dict>

    <key>StartCalendarInterval</key>
    <dict>
        <key>Hour</key>
        <integer>6</integer>
        <key>Minute</key>
        <integer>0</integer>
    </dict>

    <!-- Explicitly NOT KeepAlive: it would relaunch after every clean exit. -->
    <key>RunAtLoad</key>
    <false/>
</dict>
</plist>
"#;

    fn ws() -> PathBuf {
        PathBuf::from("/ws")
    }

    /// Observe against an empty index with nothing running — every test but
    /// the ones about a live process.
    fn see(agents: &[Installed]) -> Vec<journal::Record> {
        let store = RunStore::open_in_memory().unwrap();
        observe(agents, "mbp", &BTreeMap::new(), &store).unwrap()
    }

    /// Observe as the engine does on a tick: against the index, and with what
    /// was observed appended to a journal and ingested, so the next look sees
    /// what this one wrote.
    fn tick(
        journal_dir: &Path,
        store: &mut RunStore,
        agents: &[Installed],
        starts: &BTreeMap<u32, i64>,
    ) -> Vec<journal::Record> {
        store.ingest(journal_dir).unwrap();
        let seen = observe(agents, "mbp", starts, store).unwrap();
        for r in &seen {
            if !store.seq_of(&r.run_id).unwrap().is_some_and(|s| s >= r.seq) {
                journal::append(journal_dir, r).unwrap();
            }
        }
        store.ingest(journal_dir).unwrap();
        seen
    }

    fn lines(journal_dir: &Path) -> Vec<journal::Record> {
        journal::segments(journal_dir)
            .iter()
            .flat_map(|p| journal::read_from(p, 0).unwrap().0)
            .collect()
    }

    /// Nothing loaded — the state of a directory launchd has never been told
    /// about, which is what every test but one wants.
    fn none() -> BTreeMap<String, Loaded> {
        BTreeMap::new()
    }

    #[test]
    fn a_real_shaped_agent_reads_its_label_command_and_hour() {
        let loaded = BTreeMap::from([(
            "com.example.extract".to_string(),
            Loaded {
                pid: None,
                last_exit: Some(0),
            },
        )]);
        let a = agent(DAILY, &ws(), &loaded).expect("parsed");
        assert_eq!(a.label, "com.example.extract");
        assert_eq!(a.kind, "launchd");
        assert!(a.loaded);
        // comments inside the array are not arguments
        assert_eq!(
            a.command,
            "/usr/bin/caffeinate -i -s /Users/someone/.local/bin/uv run python \
             scripts/extract_summaries.py"
        );
        assert_eq!(a.when, "daily at 06:00");
        assert_eq!(a.every_days, Some(1));
    }

    #[test]
    fn the_script_is_the_argument_that_lands_inside_the_workspace() {
        // Not the interpreter, which is also an absolute path, and not a flag.
        let a = agent(DAILY, &ws(), &none()).unwrap();
        assert_eq!(
            a.script,
            Some(NodeId::script("ingest/scripts/extract_summaries.py", None))
        );
    }

    #[test]
    fn an_agent_this_machine_has_not_loaded_says_so() {
        let a = agent(DAILY, &ws(), &none()).unwrap();
        assert!(!a.loaded, "installed is not the same as running");
    }

    #[test]
    fn the_coarsest_calendar_field_decides_how_often_it_fires() {
        // A weekly agent also pins an hour and a minute; the hour is not the
        // cadence. Getting this backwards reports a weekly job as daily and
        // then reports drift against a `weekly-mon` declaration forever.
        let weekly = DAILY.replace(
            "<key>Hour</key>\n        <integer>6</integer>",
            "<key>Weekday</key>\n        <integer>1</integer>\n        <key>Hour</key>\n        <integer>6</integer>",
        );
        let a = agent(&weekly, &ws(), &none()).unwrap();
        assert_eq!(a.every_days, Some(7));
        assert_eq!(a.when, "weekday 1 at 06:00");

        let monthly = DAILY.replace(
            "<key>Hour</key>",
            "<key>Day</key>\n        <integer>7</integer>\n        <key>Hour</key>",
        );
        assert_eq!(
            agent(&monthly, &ws(), &none()).unwrap().every_days,
            Some(30)
        );
    }

    #[test]
    fn a_kept_alive_daemon_has_no_interval_to_compare() {
        // It is always running rather than firing on a rhythm. Calling that
        // "every 1 day" would invent a claim and then find drift against it.
        let daemon = DAILY
            .replace(
                "    <key>StartCalendarInterval</key>\n    <dict>\n        <key>Hour</key>\n        <integer>6</integer>\n        <key>Minute</key>\n        <integer>0</integer>\n    </dict>\n",
                "",
            )
            .replace("<key>RunAtLoad</key>\n    <false/>", "<key>KeepAlive</key>\n    <true/>");
        let a = agent(&daemon, &ws(), &none()).unwrap();
        assert_eq!(a.every_days, None);
        assert_eq!(a.when, "kept alive");
    }

    #[test]
    fn a_start_interval_in_seconds_becomes_whole_days() {
        let every_two_days = DAILY.replace(
            "    <key>StartCalendarInterval</key>\n    <dict>\n        <key>Hour</key>\n        <integer>6</integer>\n        <key>Minute</key>\n        <integer>0</integer>\n    </dict>\n",
            "    <key>StartInterval</key>\n    <integer>172800</integer>\n",
        );
        let a = agent(&every_two_days, &ws(), &none()).unwrap();
        assert_eq!(a.every_days, Some(2));
        assert_eq!(a.when, "every 172800s");
    }

    #[test]
    fn an_agent_with_nothing_to_do_with_this_workspace_is_left_out() {
        // A real machine has updaters and sync clients in the same directory.
        let foreign = DAILY
            .replace("com.example.extract", "com.google.GoogleUpdater.wake")
            .replace("scripts/extract_summaries.py", "--nothing-here");
        let a = agent(&foreign, &ws(), &none()).unwrap();
        assert_eq!(a.script, None);
        assert!(!concerns_this_workspace(&a));
        // but one Aneural wrote is ours to show even before it resolves
        let ours = foreign.replace(
            "com.google.GoogleUpdater.wake",
            "dev.aneural.acme.download-pubmed",
        );
        assert!(concerns_this_workspace(
            &agent(&ours, &ws(), &none()).unwrap()
        ));
    }

    #[test]
    fn a_sub_daily_interval_is_not_rounded_up_into_a_daily_one() {
        // An updater firing hourly would otherwise read as `every_days: 1` and
        // agree with a `daily` declaration it does not match.
        let hourly = DAILY.replace(
            "    <key>StartCalendarInterval</key>\n    <dict>\n        <key>Hour</key>\n        <integer>6</integer>\n        <key>Minute</key>\n        <integer>0</integer>\n    </dict>\n",
            "    <key>StartInterval</key>\n    <integer>3600</integer>\n",
        );
        let a = agent(&hourly, &ws(), &none()).unwrap();
        assert_eq!(a.every_days, Some(0), "not Some(1)");
        assert_eq!(a.when, "every 3600s");
    }

    #[test]
    fn a_plist_with_no_label_is_not_an_agent() {
        assert!(agent("<plist><dict></dict></plist>", &ws(), &none()).is_none());
    }

    #[test]
    fn only_plists_in_the_directory_are_read_and_a_missing_one_is_quiet() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.plist"), DAILY).unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "not a plist").unwrap();
        let found = installed(tmp.path(), &ws(), &none());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].label, "com.example.extract");

        assert!(installed(&tmp.path().join("nope"), &ws(), &none()).is_empty());
    }

    #[test]
    fn launchctl_columns_give_the_pid_and_how_it_last_ended() {
        // The real shape, dashes and all. This table is the *whole* of what
        // launchd remembers about a past run.
        let listed = parse_list(
            "PID\tStatus\tLabel\n\
             -\t0\tcom.apple.SafariHistoryServiceAgent\n\
             -\t-9\tcom.apple.progressd\n\
             1645\t0\tcom.apple.cloudphotod\n\
             -\t-\tcom.example.neverexited\n",
        );
        assert_eq!(listed.len(), 4);
        assert_eq!(
            listed["com.apple.SafariHistoryServiceAgent"],
            Loaded {
                pid: None,
                last_exit: Some(0)
            }
        );
        // a signal, not an exit code
        assert_eq!(listed["com.apple.progressd"].last_exit, Some(-9));
        assert_eq!(listed["com.apple.cloudphotod"].pid, Some(1645));
        // loaded, and launchd says it has never exited
        assert_eq!(
            listed["com.example.neverexited"],
            Loaded {
                pid: None,
                last_exit: None
            }
        );
    }

    /// An agent whose logs live in `dir`, shaped on the installed one this was
    /// developed against: an absolute log path per stream, stderr carrying the
    /// output and stdout left at zero bytes.
    fn logged(dir: &Path, out: &str, err: &str) -> String {
        DAILY.replace(
            "    <key>RunAtLoad</key>\n    <false/>",
            &format!(
                "    <key>StandardOutPath</key>\n    <string>{}</string>\n\
                 \x20   <key>StandardErrorPath</key>\n    <string>{}</string>",
                dir.join(out).display(),
                dir.join(err).display()
            ),
        )
    }

    #[test]
    fn a_zero_byte_stdout_beside_a_busy_stderr_is_a_run_not_a_silence() {
        // The normal shape for anything that logs through Python's logging, and
        // the installed agent this was written against is exactly it. Reading
        // "no output" as "never ran" would report every such job as never fired.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("out.log"), "").unwrap();
        std::fs::write(tmp.path().join("err.log"), "progress: 412 pages\n").unwrap();
        let text = logged(tmp.path(), "out.log", "err.log");
        let agent = agent(&text, &ws(), &none()).unwrap();

        let seen = see(std::slice::from_ref(&agent));
        assert_eq!(seen.len(), 1);
        let r = &seen[0];
        assert_eq!(r.script_key, "ingest/scripts/extract_summaries.py");
        assert_eq!(r.out_bytes, Some(0));
        assert_eq!(r.err_bytes, Some(20));
        assert_eq!(r.tail.as_deref(), Some("progress: 412 pages\n"));
        assert!(r.log_path.as_deref().unwrap().ends_with("err.log"));
        assert_eq!(r.fidelity, journal::fidelity::OBSERVED);
        assert_eq!(r.trigger, journal::trigger::LAUNCHD);
        // Nothing was measured, so nothing is claimed.
        assert_eq!(r.duration_ms, None);
        assert_eq!(r.ended_at, None);
        // and it points back at the agent that fired it
        assert_eq!(
            r.schedule_id.as_deref(),
            Some(NodeId::installed_schedule("com.example.extract").as_str())
        );
    }

    #[test]
    fn observing_the_same_unchanged_agent_twice_is_the_same_run() {
        // What makes it safe to call this on a timer: the id comes from the
        // instant observed, so a second look at an unchanged log is the same
        // record rather than a second run appended to a committed file.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("err.log"), "done\n").unwrap();
        let text = logged(tmp.path(), "out.log", "err.log");
        let agent = agent(&text, &ws(), &none()).unwrap();
        let first = see(std::slice::from_ref(&agent));
        let again = see(std::slice::from_ref(&agent));
        assert_eq!(first[0].run_id, again[0].run_id);
        assert_eq!(first, again);
    }

    #[test]
    fn an_agent_that_has_left_no_trace_is_not_a_run() {
        // Installed, never fired, or firing somewhere Aneural cannot see. An
        // undated run is not a run, and standing `now` in for the missing
        // instant would mint a brand-new fake run on every tick of the timer.
        let a = agent(DAILY, &ws(), &none()).unwrap();
        assert_eq!(a.log_out, None);
        assert!(see(&[a]).is_empty());

        // named log paths that do not exist yet are the same answer
        let tmp = tempfile::tempdir().unwrap();
        let text = logged(tmp.path(), "out.log", "err.log");
        assert!(see(&[agent(&text, &ws(), &none()).unwrap()]).is_empty());
    }

    #[test]
    fn how_a_run_ended_is_only_claimed_when_launchd_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("err.log"), "boom\n").unwrap();
        let text = logged(tmp.path(), "out.log", "err.log");
        let label = "com.example.extract".to_string();

        let with = |state: Loaded| {
            let loaded = BTreeMap::from([(label.clone(), state)]);
            see(&[agent(&text, &ws(), &loaded).unwrap()]).pop().unwrap()
        };

        assert_eq!(
            with(Loaded {
                pid: Some(1645),
                last_exit: Some(0)
            })
            .status,
            journal::status::RUNNING,
            "a live pid outranks whatever it exited with last time"
        );
        assert_eq!(
            with(Loaded {
                pid: None,
                last_exit: Some(0)
            })
            .status,
            journal::status::OK
        );
        assert_eq!(
            with(Loaded {
                pid: None,
                last_exit: Some(-9)
            })
            .status,
            journal::status::FAILED
        );
        // Unloaded: it ran, and launchd has since forgotten how it went.
        // Guessing `ok` would turn a job that died into one that succeeded.
        let unloaded = see(&[agent(&text, &ws(), &none()).unwrap()]).pop().unwrap();
        assert_eq!(unloaded.status, journal::status::UNKNOWN);
        assert_eq!(unloaded.exit_code, None);
    }

    #[test]
    fn ps_dates_a_process_in_the_zone_the_machine_is_set_to() {
        // The real shape: pid, then `lstart` as a `%c` date, columns of
        // whitespace between them and a space-padded day.
        let fixed = |_: time::PrimitiveDateTime| time::UtcOffset::from_hms(-7, 0, 0).unwrap();
        let starts = parse_starts(
            "    1 Wed Sep 30 11:56:59 2026    \n\
             90038 Sun Oct  4 15:30:20 2026    \n\
             garbage line\n",
            fixed,
        );
        assert_eq!(starts.len(), 2);
        // 2026-10-04T15:30:20-07:00 is 22:30:20Z
        assert_eq!(journal::rfc3339(starts[&90038]), "2026-10-04T22:30:20Z");
        assert_eq!(journal::rfc3339(starts[&1]), "2026-09-30T18:56:59Z");
        // and nothing is asked when there is nothing to ask about
        assert!(process_starts(&[]).is_empty());
    }

    #[test]
    fn a_running_agent_is_named_by_its_process_and_not_by_its_log() {
        // The log of a running agent is being written to, so its mtime moves
        // on every tick. Keyed on that, one KeepAlive agent grew two hundred
        // journal lines in a month; keyed on the process, it is one run for
        // as long as the process lives.
        let tmp = tempfile::tempdir().unwrap();
        let journal_dir = tmp.path().join("runs");
        let mut store = RunStore::open_in_memory().unwrap();
        let text = logged(tmp.path(), "out.log", "err.log");
        let loaded = BTreeMap::from([(
            "com.example.extract".to_string(),
            Loaded {
                pid: Some(4242),
                last_exit: Some(0),
            },
        )]);
        let a = agent(&text, &ws(), &loaded).unwrap();
        let started = 1_789_430_400_000;
        let starts = BTreeMap::from([(4242, started)]);

        for i in 0..3 {
            std::fs::write(tmp.path().join("err.log"), format!("line {i}\n")).unwrap();
            let seen = tick(&journal_dir, &mut store, std::slice::from_ref(&a), &starts);
            assert_eq!(seen.len(), 1);
            assert_eq!(
                seen[0].run_id,
                journal::run_id("mbp", started, &seen[0].script_key)
            );
            assert_eq!(seen[0].started_at, journal::rfc3339(started));
            assert_eq!(seen[0].status, journal::status::RUNNING);
            // a running process has not exited; `last_exit` is the one before
            assert_eq!(seen[0].exit_code, None);
        }
        assert_eq!(lines(&journal_dir).len(), 1, "one run, three looks");
        assert_eq!(store.count().unwrap(), 1);
    }

    #[test]
    fn when_the_pid_goes_the_open_run_is_closed_rather_than_a_new_one_minted() {
        let tmp = tempfile::tempdir().unwrap();
        let journal_dir = tmp.path().join("runs");
        let mut store = RunStore::open_in_memory().unwrap();
        let text = logged(tmp.path(), "out.log", "err.log");
        std::fs::write(tmp.path().join("err.log"), "working\n").unwrap();
        let label = "com.example.extract".to_string();
        let started = 1_789_430_400_000;
        let starts = BTreeMap::from([(4242, started)]);

        let alive = BTreeMap::from([(
            label.clone(),
            Loaded {
                pid: Some(4242),
                last_exit: None,
            },
        )]);
        let running = agent(&text, &ws(), &alive).unwrap();
        tick(&journal_dir, &mut store, &[running], &starts);
        let open = store
            .latest_running_for_schedule(NodeId::installed_schedule(&label).as_str())
            .unwrap();
        assert!(open.is_some(), "the run is open");

        // Then it exits, and launchd says how.
        std::fs::write(tmp.path().join("err.log"), "working\ndone\n").unwrap();
        let gone = BTreeMap::from([(
            label.clone(),
            Loaded {
                pid: None,
                last_exit: Some(3),
            },
        )]);
        let exited = agent(&text, &ws(), &gone).unwrap();
        let seen = tick(
            &journal_dir,
            &mut store,
            std::slice::from_ref(&exited),
            &starts,
        );
        assert_eq!(seen.len(), 1);
        let done = &seen[0];
        assert_eq!(
            done.run_id,
            journal::run_id("mbp", started, &done.script_key)
        );
        assert_eq!(done.started_at, journal::rfc3339(started));
        assert_eq!(done.status, journal::status::FAILED);
        assert_eq!(done.exit_code, Some(3));
        assert_eq!(done.seq, CLOSED);
        assert_eq!(done.err_bytes, Some(13), "sizes are as of the finish");
        assert_eq!(done.tail.as_deref(), Some("working\ndone\n"));
        assert!(done.ended_at.is_some(), "when output was last seen");

        let all = lines(&journal_dir);
        assert_eq!(all.len(), 2, "a start line and a finish line");
        assert_eq!(all[0].run_id, all[1].run_id, "of the same run");
        assert_eq!(store.count().unwrap(), 1);
        assert_eq!(
            store.latest_per_script().unwrap()[0].status,
            journal::status::FAILED
        );

        // Looking again, with the log untouched since, adds nothing: the run
        // was closed on this evidence and is not a new one.
        tick(&journal_dir, &mut store, &[exited], &starts);
        assert_eq!(lines(&journal_dir).len(), 2);
        assert_eq!(store.count().unwrap(), 1);
    }

    #[test]
    fn a_run_left_open_under_another_id_is_closed_when_a_dated_process_appears() {
        // Two ways to get here: launchd restarted the agent between looks, or
        // the open line is from before runs were keyed on the process. Either
        // way the old row cannot be this process, and how it ended is unknown.
        let tmp = tempfile::tempdir().unwrap();
        let journal_dir = tmp.path().join("runs");
        let mut store = RunStore::open_in_memory().unwrap();
        let text = logged(tmp.path(), "out.log", "err.log");
        std::fs::write(tmp.path().join("err.log"), "x\n").unwrap();
        let label = "com.example.extract".to_string();
        let alive = BTreeMap::from([(
            label.clone(),
            Loaded {
                pid: Some(4242),
                last_exit: Some(0),
            },
        )]);
        let a = agent(&text, &ws(), &alive).unwrap();
        let undated = BTreeMap::new();

        // First look: alive, but `ps` had nothing to say. The log dates it.
        let first = tick(&journal_dir, &mut store, std::slice::from_ref(&a), &undated);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].status, journal::status::RUNNING);
        let from_log = first[0].run_id.clone();

        // Second look, still undated: the open run is reused, nothing appended.
        tick(&journal_dir, &mut store, std::slice::from_ref(&a), &undated);
        assert_eq!(lines(&journal_dir).len(), 1);

        // Third look: `ps` dates it, and the date names a different run.
        let starts = BTreeMap::from([(4242, 1_789_430_400_000)]);
        let seen = tick(&journal_dir, &mut store, std::slice::from_ref(&a), &starts);
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].run_id, from_log);
        assert_eq!(seen[0].status, journal::status::UNKNOWN);
        assert_eq!(seen[0].seq, CLOSED);
        assert_eq!(seen[1].status, journal::status::RUNNING);
        assert_ne!(seen[1].run_id, from_log);
        assert_eq!(store.count().unwrap(), 2);
        // and only the newest row is ever open, so the old one stays closed
        tick(&journal_dir, &mut store, std::slice::from_ref(&a), &starts);
        assert_eq!(lines(&journal_dir).len(), 3);
    }

    #[test]
    fn an_agent_with_no_script_in_this_workspace_is_never_a_run() {
        // There would be nothing in the graph for the run to sit beside, and the
        // point of a Run node is that it grows off the thing that ran.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("err.log"), "x\n").unwrap();
        let foreign = logged(tmp.path(), "out.log", "err.log")
            .replace("com.example.extract", "dev.aneural.ws.nothing")
            .replace("scripts/extract_summaries.py", "--nothing-here");
        let a = agent(&foreign, &ws(), &none()).unwrap();
        assert!(concerns_this_workspace(&a), "ours, so it is on the canvas");
        assert!(see(&[a]).is_empty(), "but it is not a run");
    }

    #[test]
    fn a_committed_record_never_carries_an_unbounded_log() {
        // The stderr of one real campaign was 21 MB. Nothing that size may be
        // written into a file git tracks.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("err.log"),
            "x".repeat(journal::MAX_TAIL_BYTES * 8),
        )
        .unwrap();
        let text = logged(tmp.path(), "out.log", "err.log");
        let r = see(&[agent(&text, &ws(), &none()).unwrap()]).pop().unwrap();
        assert_eq!(r.tail.as_deref().unwrap().len(), journal::MAX_TAIL_BYTES);
        // and the size of the whole thing is still reported, so the tail never
        // reads as the entire output
        assert_eq!(r.err_bytes, Some((journal::MAX_TAIL_BYTES * 8) as i64));
    }

    #[test]
    fn aneural_owns_only_its_own_prefix() {
        // Anything else is an observation. Nothing outside this may be written
        // or removed, and the test exists so that stays true by construction.
        assert!("dev.aneural.acme.download-pubmed".starts_with(OWNED_PREFIX));
        assert!(!"com.acme.nightly_summaries".starts_with(OWNED_PREFIX));
    }
}
