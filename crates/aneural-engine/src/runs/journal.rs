//! The run journal: one append-only JSONL file per machine per month, committed.
//!
//! The database that serves queries about runs is [`super::store`], and it is
//! derived from these files rather than the other way round. That split is the
//! whole design, and it comes from one requirement: run history has to survive a
//! `git pull` from another machine.
//!
//! A committed SQLite file loses on every axis. Every append rewrites pages, so
//! git stores a megabyte-scale binary blob per commit; two machines committing
//! conflict on *every* run; and a merge driver would have to open both databases
//! and replay rows — which is exactly the ingest below, plus a binary format git
//! cannot help with. So the durable form is text, and the fast form is a cache.
//!
//! Three rules make a merge work without a merge driver:
//!
//! 1. **One directory per machine.** Two machines never write the same file, so
//!    the common case has no conflict to resolve at all.
//! 2. **Append only, never edit.** A run writes two lines — one when it starts,
//!    one when it finishes — because mutating a line in place would break both
//!    `merge=union` and every reader's byte offset.
//! 3. **The id is derived from the content, never random.** `<machine>/<millis>-<hash>`
//!    is the same string whichever clone computes it, so the same run arriving
//!    from two directions is one row. A uuid would duplicate.
//!
//! Given those, `merge=union` (built into git, no driver to install) is not a
//! workaround — it is the correct resolution, because both sides' lines are
//! true. Whatever duplication it leaves is collapsed at ingest by last-`seq`-wins,
//! which is idempotent and order-independent. That matters: a merged file hands
//! you lines in no particular order.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// How much of a run's output is kept in the committed journal. The full output
/// lives in the gitignored log directory, or in whatever log the script writes
/// itself; nothing unbounded is ever committed.
pub const MAX_TAIL_BYTES: usize = 4096;

/// Read in this much at a time when hashing the part of a segment already read.
const HASH_CHUNK: usize = 64 * 1024;

/// How much of a log file is read to build a tail from.
///
/// Four times what is kept, so that collapsing repeated lines has something to
/// work with — and bounded, so that the size of the log does not matter. The
/// real log this was measured against is 21 MB and grows; reading all of it to
/// keep 4 KB would be 21 MB of I/O on every tick of a timer.
const TAIL_WINDOW: u64 = (MAX_TAIL_BYTES * 4) as u64;

/// What started a run.
pub mod trigger {
    /// A person asked for it.
    pub const MANUAL: &str = "manual";
    /// Aneural's own scheduler fired it.
    pub const SCHEDULER: &str = "scheduler";
    /// A launch agent fired it, and Aneural saw the evidence afterwards.
    pub const LAUNCHD: &str = "launchd";
    /// Something ran and Aneural cannot say what asked for it.
    pub const UNKNOWN: &str = "unknown";
}

/// Where a run got to.
pub mod status {
    pub const RUNNING: &str = "running";
    pub const OK: &str = "ok";
    pub const FAILED: &str = "failed";
    pub const TIMEOUT: &str = "timeout";
    pub const CANCELLED: &str = "cancelled";
    pub const SKIPPED_LOCKED: &str = "skipped-locked";
    pub const SKIPPED_PAUSED: &str = "skipped-paused";
    pub const SKIPPED_PRECONDITION: &str = "skipped-precondition";
    /// Something ran; how it ended cannot be told. The honest answer for an
    /// agent this machine has unloaded since it last fired.
    pub const UNKNOWN: &str = "unknown";
}

/// How much of a record is first-hand.
pub mod fidelity {
    /// Aneural started the process and watched it finish. Times and exit code
    /// are measured.
    pub const EXACT: &str = "exact";
    /// Aneural found the traces of a run it did not supervise. The instant is
    /// when output was last seen, not when the process began, and the duration
    /// is unknown rather than zero.
    pub const OBSERVED: &str = "observed";
}

/// One line of the journal.
///
/// Every optional field is genuinely unknown when absent, never zero: a run
/// with no `duration_ms` took an unknown amount of time, and writing `0` there
/// would turn "we do not know" into "it was instant".
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    /// `<machine>/<startedAtMillis>-<short hash of the script key>`. Derived,
    /// so the same run computed on two clones is the same row.
    pub run_id: String,
    pub machine: String,
    /// The `Script` node's id without its prefix — stable across machines,
    /// which an absolute path is not.
    pub script_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_id: Option<String>,
    pub trigger: String,
    /// The command as it was actually run, interpreter resolved. Recorded per
    /// run rather than stored in config, because the resolution is a fact about
    /// this machine at this moment and config is committed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cwd: String,
    /// RFC 3339. For an `observed` record this is when output was last seen.
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_bytes: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub err_bytes: Option<i64>,
    /// The last [`MAX_TAIL_BYTES`] of output, if any was captured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail: Option<String>,
    /// Where the full output is. It may well not exist on the machine reading
    /// this, which is why the tail is here too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
    pub fidelity: String,
    /// `0` for the line written when a run starts, `1` for the one written when
    /// it finishes. Higher wins at ingest, so the two lines collapse to the
    /// later truth whatever order they arrive in.
    pub seq: i64,
}

impl Record {
    /// The month this record belongs in, `YYYY-MM`, from its own timestamp.
    fn month(&self) -> String {
        match self.started_at.len() >= 7 && self.started_at.is_char_boundary(7) {
            true => self.started_at[..7].to_string(),
            // A record with no usable timestamp still has to land somewhere it
            // can be found again rather than be dropped on the floor.
            false => "unknown".to_string(),
        }
    }
}

/// The run id for a run of `script_key` that started at `millis` on `machine`.
///
/// Derived from all three, never random. `machine` gives global uniqueness with
/// no coordination between clones; the millisecond and the hash give local
/// uniqueness, which the per-script lock is what actually guarantees.
pub fn run_id(machine: &str, started_at_millis: i64, script_key: &str) -> String {
    format!(
        "{machine}/{started_at_millis}-{}",
        aneural_core::short_hash(script_key)
    )
}

/// What this machine calls itself.
///
/// The configured name wins, because it is committed and therefore stable: a
/// derived name changes the day someone renames their laptop, and a changed name
/// starts a second journal directory that nothing recognises as the same
/// machine. The hostname is only the convenience default.
pub fn machine_id(configured: &str) -> String {
    let configured = configured.trim();
    if !configured.is_empty() {
        return aneural_core::slug(configured);
    }
    static HOST: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HOST.get_or_init(|| {
        let name = std::process::Command::new("hostname")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            // Trailing `.local` is noise on every Mac on a network.
            .map(|s| s.trim_end_matches(".local").to_string())
            .unwrap_or_default();
        match aneural_core::slug(&name) {
            slug if slug.is_empty() => "unknown".to_string(),
            slug => slug,
        }
    })
    .clone()
}

/// The file a record belongs in: `<dir>/<machine>/<YYYY-MM>.jsonl`.
///
/// Per machine so concurrent writers are structurally impossible, and per month
/// so a file stays a size git is happy with and retention is a `git rm` of a
/// whole file rather than a rewrite of one.
pub fn segment_of(dir: &Path, record: &Record) -> PathBuf {
    dir.join(&record.machine)
        .join(format!("{}.jsonl", record.month()))
}

/// Append one record. Creates the machine's directory the first time.
pub fn append(dir: &Path, record: &Record) -> std::io::Result<PathBuf> {
    let path = segment_of(dir, record);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_string(record)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    // One `write_all` of one line: a line is the unit of the format, and a
    // partial one would be a record nobody can read.
    f.write_all(line.as_bytes())?;
    Ok(path)
}

/// Every segment under `dir`, sorted, so ingest is deterministic.
pub fn segments(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(machines) = std::fs::read_dir(dir) else {
        return out;
    };
    for machine in machines.flatten() {
        let Ok(files) = std::fs::read_dir(machine.path()) else {
            continue;
        };
        out.extend(
            files
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl")),
        );
    }
    out.sort();
    out
}

/// Hash of the **first `len` bytes** of a segment.
///
/// This is what decides whether a remembered read offset still means anything.
/// Hashing the region already read, rather than a fixed-size head, is the
/// difference between exact and nearly right: appending never touches those
/// bytes, so the hash is stable while a file only grows, and *any* rewrite that
/// reaches into the region — a merge, a rebase, a hand edit, even one that
/// leaves the file exactly the same length — changes it.
///
/// A fixed-size head would have got this wrong in both directions at once: for a
/// file shorter than the window, appending changes the hash and every tick would
/// re-read the whole journal; for a longer one, a rewrite past the window would
/// go unnoticed.
///
/// Costs one pass over the read region, which is why the caller checks size and
/// modification time first and only reaches for this when one of them moved.
pub fn prefix_hash(path: &Path, len: u64) -> String {
    if len == 0 {
        return String::new();
    }
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut hasher = blake3::Hasher::new();
    let mut left = len;
    let mut buf = vec![0u8; HASH_CHUNK];
    while left > 0 {
        let want = buf.len().min(left as usize);
        match f.read(&mut buf[..want]) {
            // Shorter than the length claimed: the file was truncated, and the
            // caller has already seen that in the size.
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
                left -= n as u64;
            }
            Err(_) => return String::new(),
        }
    }
    hasher.finalize().to_hex()[..12].to_string()
}

/// Read the records a segment has gained since `offset`.
///
/// Returns the new offset, which only ever advances past **complete** lines. A
/// process killed mid-append leaves a partial last line; stopping short of it
/// means the next pass reads it whole rather than discarding it.
///
/// A line that will not parse is skipped, not fatal. The alternative is that one
/// bad line — a stray conflict marker, a hand edit — costs a machine its entire
/// history.
pub fn read_from(path: &Path, offset: u64) -> std::io::Result<(Vec<Record>, u64)> {
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::new(f);
    let mut records = Vec::new();
    let mut at = offset;
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 || !line.ends_with('\n') {
            break;
        }
        at += read as u64;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Record>(trimmed) {
            Ok(r) if !r.run_id.is_empty() => records.push(r),
            Ok(_) => tracing::debug!("run journal {}: record with no id", path.display()),
            Err(e) => tracing::debug!("run journal {}: {e}", path.display()),
        }
    }
    Ok((records, at))
}

/// Delete whole segments older than `keep_days`, and report which went.
///
/// Whole files only. Pruning by rewriting a file would break the union merge
/// and invalidate every clone's read offset at once, to save bytes in a text
/// file that compresses well; dropping the oldest month is the same saving with
/// none of that.
pub fn prune(dir: &Path, keep_days: u32, now: time::Date) -> Vec<PathBuf> {
    if keep_days == 0 {
        return Vec::new();
    }
    let Some(cutoff) = now.checked_sub(time::Duration::days(keep_days as i64)) else {
        return Vec::new();
    };
    // A month is kept while any day in it is inside the window, so the boundary
    // never cuts a segment in half.
    let keep_from = format!("{:04}-{:02}", cutoff.year(), cutoff.month() as u8);
    let mut gone = Vec::new();
    for path in segments(dir) {
        let Some(month) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // `unknown` sorts after any real month and is never pruned: a record we
        // could not date is the last thing to throw away.
        if month.len() == 7 && month < keep_from.as_str() && std::fs::remove_file(&path).is_ok() {
            gone.push(path);
        }
    }
    gone
}

/// Cut a string of output down to what may be committed, keeping the **end**.
///
/// The end is where the error is. Three things the cut has to get right:
///
/// - it lands on a char boundary, or the committed line is not valid UTF-8;
/// - it starts at a line boundary, or at the cut itself when there is no line to
///   start at — see [`after_partial_line`].
pub fn tail_of(text: &str) -> String {
    if text.len() <= MAX_TAIL_BYTES {
        return text.to_string();
    }
    let mut from = text.len() - MAX_TAIL_BYTES;
    while from < text.len() && !text.is_char_boundary(from) {
        from += 1;
    }
    after_partial_line(&text[from..]).to_string()
}

/// Everything after the first newline — unless that would leave nothing.
///
/// Cutting into the middle of a file, whether by seeking or by capping, lands
/// mid-line, and a tail that begins mid-word reads as corruption rather than as
/// an excerpt. But a single enormous line with no newline in it is still the only
/// output there is, and returning empty because it could not be trimmed tidily
/// would lose all of it.
fn after_partial_line(text: &str) -> &str {
    match text.split_once('\n') {
        Some((_, rest)) if !rest.trim().is_empty() => rest,
        _ => text,
    }
}

/// The end of a log file, collapsed and capped, ready to be committed.
///
/// Reads only the last [`TAIL_WINDOW`] bytes: the log may be tens of megabytes
/// and this runs on a timer, so the cost has to depend on how much is kept
/// rather than on how much was written. Seeking lands mid-line, so the first
/// partial line is dropped — a tail that begins in the middle of a word is worse
/// than one that begins a line later.
pub fn tail_of_file(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let from = len.saturating_sub(TAIL_WINDOW);
    f.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let whole = match from > 0 {
        true => after_partial_line(&text),
        false => text.as_ref(),
    };
    match tail_of(&collapse_repeats(whole)) {
        t if t.trim().is_empty() => None,
        t => Some(t),
    }
}

/// Collapse runs of identical consecutive lines into one plus a count.
///
/// Measured on a real campaign's stderr: of the last 4 KB, about 4 KB was one
/// CoreGraphics warning repeated forty times, and the two lines anybody would
/// actually want — a timeout with its document id, and the progress counter —
/// were pushed off the top by it. A cap alone honours "nothing unbounded is
/// committed" while committing almost nothing *useful*, which is the same
/// failure in a nicer suit.
///
/// Lossless in meaning: nothing is dropped that the count does not account for,
/// which is the same bargain syslog's "last message repeated N times" makes.
///
/// The marker is ASCII. A `⋯` reads better and renders as a tofu box in both
/// the canvas font and the inspector's, which was found by looking at a real
/// node rather than by reasoning about it.
pub fn collapse_repeats(text: &str) -> String {
    fn flush(out: &mut String, line: &str, count: usize) {
        out.push_str(line);
        out.push('\n');
        if count > 1 {
            out.push_str(&format!("  ... x{count}\n"));
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut prev: Option<&str> = None;
    let mut count = 0usize;
    for line in text.lines() {
        match prev {
            Some(p) if p == line => count += 1,
            Some(p) => {
                flush(&mut out, p, count);
                prev = Some(line);
                count = 1;
            }
            None => {
                prev = Some(line);
                count = 1;
            }
        }
    }
    if let Some(p) = prev {
        flush(&mut out, p, count);
    }
    out
}

/// RFC 3339 for a unix-millisecond instant, in UTC.
///
/// UTC and not local time, because these strings are compared across machines
/// and sorted as text; a local offset would make the ordering a lie.
pub fn rfc3339(millis: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(millis as i128 * 1_000_000)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(machine: &str, started: &str, script: &str, seq: i64, status: &str) -> Record {
        let millis = 1_758_000_000_000;
        Record {
            run_id: run_id(machine, millis, script),
            machine: machine.into(),
            script_key: script.into(),
            trigger: trigger::LAUNCHD.into(),
            started_at: started.into(),
            status: status.into(),
            fidelity: fidelity::OBSERVED.into(),
            seq,
            ..Default::default()
        }
    }

    #[test]
    fn a_run_id_is_the_same_string_on_every_clone() {
        // The reason it is derived: the same run arriving from two directions
        // has to be one row, and a uuid would make it two.
        let a = run_id("devs-mbp", 1_758_000_000_000, "ingest/scripts/download.py");
        let b = run_id("devs-mbp", 1_758_000_000_000, "ingest/scripts/download.py");
        assert_eq!(a, b);
        assert!(a.starts_with("devs-mbp/1758000000000-"));
        // and a different script at the same instant is a different run
        assert_ne!(a, run_id("devs-mbp", 1_758_000_000_000, "ingest/other.py"));
        // as is the same script on another machine
        assert_ne!(
            a,
            run_id("build-box", 1_758_000_000_000, "ingest/scripts/download.py")
        );
    }

    #[test]
    fn each_machine_writes_its_own_file_so_two_can_never_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mine = append(
            dir,
            &record(
                "devs-mbp",
                "2026-09-27T04:00:00Z",
                "a.py",
                0,
                status::RUNNING,
            ),
        )
        .unwrap();
        let theirs = append(
            dir,
            &record(
                "build-box",
                "2026-09-27T04:00:00Z",
                "a.py",
                0,
                status::RUNNING,
            ),
        )
        .unwrap();
        assert_ne!(mine, theirs);
        assert!(mine.ends_with("devs-mbp/2026-09.jsonl"), "{mine:?}");
        assert!(theirs.ends_with("build-box/2026-09.jsonl"), "{theirs:?}");
        assert_eq!(segments(dir).len(), 2);
    }

    #[test]
    fn a_new_month_starts_a_new_segment() {
        let tmp = tempfile::tempdir().unwrap();
        append(
            tmp.path(),
            &record("mbp", "2026-09-30T23:59:00Z", "a.py", 0, status::OK),
        )
        .unwrap();
        append(
            tmp.path(),
            &record("mbp", "2026-10-01T00:01:00Z", "a.py", 0, status::OK),
        )
        .unwrap();
        let found: Vec<String> = segments(tmp.path())
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(found, ["2026-09.jsonl", "2026-10.jsonl"]);
    }

    #[test]
    fn reading_resumes_from_the_offset_and_returns_only_what_is_new() {
        let tmp = tempfile::tempdir().unwrap();
        let path = append(
            tmp.path(),
            &record("mbp", "2026-09-27T04:00:00Z", "a.py", 0, status::RUNNING),
        )
        .unwrap();
        let (first, at) = read_from(&path, 0).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].status, status::RUNNING);

        // nothing new yet
        let (none, still) = read_from(&path, at).unwrap();
        assert!(none.is_empty());
        assert_eq!(still, at);

        // the finish line, which is a second line and not an edit of the first
        append(
            tmp.path(),
            &record("mbp", "2026-09-27T04:00:00Z", "a.py", 1, status::OK),
        )
        .unwrap();
        let (next, after) = read_from(&path, at).unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].seq, 1);
        assert!(after > at);
    }

    #[test]
    fn a_half_written_last_line_is_left_for_next_time_not_thrown_away() {
        // What a process killed mid-append leaves behind. Advancing past it
        // would lose the record for good once the rest was written.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mbp").join("2026-09.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let whole = serde_json::to_string(&record(
            "mbp",
            "2026-09-27T04:00:00Z",
            "a.py",
            0,
            status::OK,
        ))
        .unwrap();
        std::fs::write(&path, format!("{whole}\n{}", &whole[..20])).unwrap();

        let (records, at) = read_from(&path, 0).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            at as usize,
            whole.len() + 1,
            "stopped before the partial line"
        );
    }

    #[test]
    fn one_unreadable_line_does_not_cost_a_machine_its_history() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mbp").join("2026-09.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let good = serde_json::to_string(&record(
            "mbp",
            "2026-09-27T04:00:00Z",
            "a.py",
            0,
            status::OK,
        ))
        .unwrap();
        std::fs::write(&path, format!("{good}\n<<<<<<< HEAD\n{{}}\n{good}\n")).unwrap();
        let (records, _) = read_from(&path, 0).unwrap();
        assert_eq!(records.len(), 2, "both readable lines survived");
    }

    #[test]
    fn a_rewrite_that_kept_the_length_still_shows_up_in_the_hash() {
        // The case size and modification time both miss, and the reason the
        // hash covers the region read rather than a fixed-size head.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("seg.jsonl");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let before = prefix_hash(&path, 8);
        std::fs::write(&path, "two\none\n").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 8, "same length");
        assert_ne!(before, prefix_hash(&path, 8));
    }

    #[test]
    fn appending_never_changes_the_hash_of_what_was_already_read() {
        // Including for a file far shorter than any fixed window — which is
        // every journal on its first day, and would otherwise be re-read in
        // full on every tick.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("seg.jsonl");
        std::fs::write(&path, "one\n").unwrap();
        let before = prefix_hash(&path, 4);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"two\n")
            .unwrap();
        assert_eq!(before, prefix_hash(&path, 4));
        // and hashing nothing is not an error, it is the state before any read
        assert_eq!(prefix_hash(&path, 0), "");
    }

    #[test]
    fn a_truncated_segment_cannot_hash_the_same_as_the_longer_one() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("seg.jsonl");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let before = prefix_hash(&path, 8);
        std::fs::write(&path, "one\n").unwrap();
        assert_ne!(before, prefix_hash(&path, 8));
    }

    #[test]
    fn retention_drops_whole_months_and_never_rewrites_one() {
        let tmp = tempfile::tempdir().unwrap();
        for month in ["2026-05", "2026-08", "2026-09"] {
            append(
                tmp.path(),
                &record(
                    "mbp",
                    &format!("{month}-15T04:00:00Z"),
                    "a.py",
                    0,
                    status::OK,
                ),
            )
            .unwrap();
        }
        let now = time::Date::from_calendar_date(2026, time::Month::September, 27).unwrap();
        let gone = prune(tmp.path(), 60, now);
        assert_eq!(gone.len(), 1, "{gone:?}");
        assert!(gone[0].ends_with("2026-05.jsonl"));
        // July is the cutoff month, so August is kept whole rather than trimmed
        let left: Vec<String> = segments(tmp.path())
            .iter()
            .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["2026-08", "2026-09"]);
    }

    #[test]
    fn a_record_nobody_could_date_is_the_last_thing_pruned() {
        let tmp = tempfile::tempdir().unwrap();
        append(tmp.path(), &record("mbp", "", "a.py", 0, status::UNKNOWN)).unwrap();
        let now = time::Date::from_calendar_date(2026, time::Month::September, 27).unwrap();
        assert!(prune(tmp.path(), 1, now).is_empty());
        assert_eq!(segments(tmp.path()).len(), 1);
    }

    #[test]
    fn the_noise_a_long_job_ends_in_does_not_crowd_out_what_matters() {
        // Measured on a real campaign's stderr. Without collapsing, the forty
        // CoreGraphics lines fill the whole 4 KB and push both interesting lines
        // off the top — a tail that is capped and says nothing.
        let noise = "CoreGraphics PDF has logged an error. \
                     Set environment variable \"CG_PDF_VERBOSE\" to learn more.";
        let mut log = String::new();
        log.push_str("01:42:09  ERROR    Doc 168664302 exceeded its 204s budget\n");
        for _ in 0..40 {
            log.push_str(noise);
            log.push('\n');
        }
        log.push_str("01:47:15  INFO     [17500/616267] extracted\n");

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("stderr.log");
        std::fs::write(&path, &log).unwrap();
        let tail = tail_of_file(&path).unwrap();

        assert!(tail.contains("exceeded its 204s budget"), "{tail}");
        assert!(tail.contains("[17500/616267]"), "{tail}");
        assert!(tail.contains("... x40"), "{tail}");
        assert_eq!(tail.matches(noise).count(), 1, "kept once, counted once");
        assert!(
            tail.len() < log.len() / 4,
            "{} of {}",
            tail.len(),
            log.len()
        );
    }

    #[test]
    fn a_tail_costs_the_same_whatever_the_log_grew_to() {
        // The real log is 21 MB and growing, and this runs on a timer.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("huge.log");
        let mut log = String::new();
        for i in 0..40_000 {
            log.push_str(&format!("line {i}\n"));
        }
        assert!(log.len() > 400_000);
        std::fs::write(&path, &log).unwrap();
        let tail = tail_of_file(&path).unwrap();
        assert!(tail.len() <= MAX_TAIL_BYTES);
        assert!(tail.ends_with("line 39999\n"), "the end, not the start");
        // and it begins on a line boundary rather than mid-word, because the
        // seek landed inside a line and that line was dropped
        assert!(tail.starts_with("line "), "{:?}", &tail[..20]);
    }

    #[test]
    fn a_log_nobody_wrote_to_has_no_tail_rather_than_an_empty_one() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("stdout.log");
        std::fs::write(&path, "").unwrap();
        assert_eq!(tail_of_file(&path), None);
        assert_eq!(tail_of_file(&tmp.path().join("nope.log")), None);
    }

    #[test]
    fn the_marker_collapsing_inserts_is_ascii() {
        // Whatever the log itself contains is the user's, but anything this
        // writes into it has to render in both fonts Aneural draws with.
        let marked = collapse_repeats("same\nsame\nsame\n");
        assert!(marked.is_ascii(), "{marked:?}");
        assert_eq!(marked, "same\n  ... x3\n");
    }

    #[test]
    fn collapsing_leaves_a_log_that_never_repeats_itself_alone() {
        let text = "one\ntwo\nthree\n";
        assert_eq!(collapse_repeats(text), text);
        // and it is consecutive repeats only: the same line again later is a
        // separate event, not part of the earlier run
        assert_eq!(collapse_repeats("a\nb\na\n"), "a\nb\na\n");
    }

    #[test]
    fn a_tail_keeps_the_end_and_stays_valid_utf8() {
        // The end is where the traceback is. And a cut has to land on a char
        // boundary or the committed line is not valid JSON.
        let long = "é".repeat(MAX_TAIL_BYTES);
        let cut = tail_of(&long);
        assert!(cut.len() <= MAX_TAIL_BYTES);
        assert!(long.ends_with(&cut));
        assert_eq!(tail_of("short"), "short");
    }

    #[test]
    fn a_configured_machine_name_wins_over_the_hostname() {
        assert_eq!(machine_id("Dev's MBP"), "dev-s-mbp");
        // and whatever the hostname is, it is a usable directory name
        let derived = machine_id("");
        assert!(!derived.is_empty());
        assert_eq!(derived, aneural_core::slug(&derived));
    }
}
