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

use super::{Installed, journal};
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

/// The journal records for runs that have left evidence behind.
///
/// One per agent at most, and only when there is something to date it by: an
/// undated run is not a run record, and standing in `now` for the missing
/// instant would mint a brand-new fake run on every tick of the timer.
///
/// The id is derived from that instant, so **observing the same unchanged state
/// twice produces the same record**. That is what makes it safe to call this on
/// a timer and append whatever it returns: the second observation is the same
/// row, not a second run.
pub fn observe(agents: &[Installed], machine: &str) -> Vec<journal::Record> {
    agents.iter().filter_map(|a| observed(a, machine)).collect()
}

fn observed(agent: &Installed, machine: &str) -> Option<journal::Record> {
    // Without a script there is nothing in the graph to hang the run off, and
    // the point of a Run node is to sit beside the thing that ran.
    let script_key = super::script_key(agent.script.as_ref()?);
    let out = agent.log_out.as_deref().and_then(log_state);
    let err = agent.log_err.as_deref().and_then(log_state);

    // The newest of the two, because either stream may be the one in use. A
    // zero-byte stdout beside a busy stderr is the normal shape for anything
    // using Python's logging, and reading "no output" as "never ran" would
    // report every such job as having never fired.
    let millis = [out, err].iter().flatten().map(|(mtime, _)| *mtime).max()?;

    let status = match (agent.pid, agent.last_exit) {
        (Some(_), _) => journal::status::RUNNING,
        (None, Some(0)) => journal::status::OK,
        (None, Some(_)) => journal::status::FAILED,
        // Installed, has run at some point, and launchd has since forgotten —
        // which is what an unloaded agent looks like. Guessing `ok` here would
        // turn a job that died into a job that succeeded.
        (None, None) => journal::status::UNKNOWN,
    };

    Some(journal::Record {
        run_id: journal::run_id(machine, millis, &script_key),
        machine: machine.to_string(),
        script_key,
        schedule_id: Some(
            NodeId::installed_schedule(&agent.label)
                .as_str()
                .to_string(),
        ),
        trigger: journal::trigger::LAUNCHD.to_string(),
        argv: agent.command.split_whitespace().map(String::from).collect(),
        cwd: String::new(),
        started_at: journal::rfc3339(millis),
        // Not `started_at` plus something: launchd has not told us how long it
        // ran, and a duration invented from the log's timestamps would be the
        // gap between writes rather than the life of the process.
        ended_at: None,
        status: status.to_string(),
        exit_code: agent.last_exit,
        duration_ms: None,
        out_bytes: out.map(|(_, size)| size),
        err_bytes: err.map(|(_, size)| size),
        tail: tail_of_newest(out, err, agent),
        log_path: newest_path(out, err, agent),
        fidelity: journal::fidelity::OBSERVED.to_string(),
        // A run seen after the fact is a finished observation, never half of a
        // pair, so it takes the finished sequence number.
        seq: 1,
    })
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

        let seen = observe(std::slice::from_ref(&agent), "mbp");
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
        let first = observe(std::slice::from_ref(&agent), "mbp");
        let again = observe(std::slice::from_ref(&agent), "mbp");
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
        assert!(observe(&[a], "mbp").is_empty());

        // named log paths that do not exist yet are the same answer
        let tmp = tempfile::tempdir().unwrap();
        let text = logged(tmp.path(), "out.log", "err.log");
        assert!(observe(&[agent(&text, &ws(), &none()).unwrap()], "mbp").is_empty());
    }

    #[test]
    fn how_a_run_ended_is_only_claimed_when_launchd_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("err.log"), "boom\n").unwrap();
        let text = logged(tmp.path(), "out.log", "err.log");
        let label = "com.example.extract".to_string();

        let with = |state: Loaded| {
            let loaded = BTreeMap::from([(label.clone(), state)]);
            observe(&[agent(&text, &ws(), &loaded).unwrap()], "mbp")
                .pop()
                .unwrap()
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
        let unloaded = observe(&[agent(&text, &ws(), &none()).unwrap()], "mbp")
            .pop()
            .unwrap();
        assert_eq!(unloaded.status, journal::status::UNKNOWN);
        assert_eq!(unloaded.exit_code, None);
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
        assert!(observe(&[a], "mbp").is_empty(), "but it is not a run");
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
        let r = observe(&[agent(&text, &ws(), &none()).unwrap()], "mbp")
            .pop()
            .unwrap();
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
