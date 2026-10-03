//! The index built from the run journal: `.aneural/cache/runs.db`.
//!
//! Its own file, not a table in the graph store. That store's `USER_VERSION`
//! means "drop the cache and rebuild it", which is right for a graph that can be
//! re-derived from the workspace in seconds — and wrong for run history, which
//! can only be re-derived from the journal. Keeping them separate means adding a
//! node kind never costs anyone their run history, and it is also why this file
//! carries a version of its own.
//!
//! Ingest is **idempotent and order-independent**, because that is what a merged
//! journal demands. Two rules do it:
//!
//! - a run id is derived from its content, so the same run arriving from two
//!   clones is the same primary key;
//! - a later `seq` overwrites an earlier one and an earlier one is ignored, so
//!   the start line and the finish line collapse to the finish whichever order
//!   they are read in.
//!
//! Re-running ingest over an unchanged journal therefore changes nothing, which
//! matters because it runs on a timer.

use super::journal::{self, Record};
use crate::Result;
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::path::{Path, PathBuf};

/// Bump when the shape of what is stored changes. A mismatch drops the tables
/// and re-ingests from the journal — which is lossless, because the journal is
/// the durable copy and this file never held anything the journal does not.
pub const USER_VERSION: i32 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS runs(
  run_id      TEXT PRIMARY KEY,
  machine     TEXT NOT NULL,
  script_key  TEXT NOT NULL,
  schedule_id TEXT,
  trigger     TEXT NOT NULL,
  argv        TEXT NOT NULL,
  cwd         TEXT NOT NULL,
  started_at  TEXT NOT NULL,
  ended_at    TEXT,
  status      TEXT NOT NULL,
  exit_code   INTEGER,
  duration_ms INTEGER,
  out_bytes   INTEGER,
  err_bytes   INTEGER,
  tail        TEXT,
  log_path    TEXT,
  fidelity    TEXT NOT NULL,
  seq         INTEGER NOT NULL,
  segment     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS runs_by_script ON runs(script_key, started_at DESC);
CREATE TABLE IF NOT EXISTS segments(
  path      TEXT PRIMARY KEY,
  size      INTEGER NOT NULL,
  mtime     INTEGER NOT NULL,
  offset    INTEGER NOT NULL,
  -- Hash of the first `offset` bytes as they were when that offset was
  -- recorded. If they still hash the same, the offset still means something.
  read_hash TEXT NOT NULL
);
"#;

/// What one pass of [`RunStore::ingest`] did. Reported rather than logged so a
/// test can assert the interesting half — that a rewritten segment was noticed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ingested {
    /// Records read and applied. Not the number of rows that changed: a record
    /// already stored at a higher `seq` is applied and ignored.
    pub records: usize,
    /// Segments that had to be read again from the start because they were
    /// rewritten rather than appended to.
    pub reingested: Vec<PathBuf>,
}

pub struct RunStore {
    conn: Connection,
}

impl RunStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        let version: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version != 0 && version != USER_VERSION {
            // Safe to drop: every row here was read out of the journal, and the
            // cursors that go with them have to go at the same time or the
            // re-ingest would start from the wrong offset.
            conn.execute_batch("DROP TABLE IF EXISTS runs; DROP TABLE IF EXISTS segments;")?;
        }
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "user_version", USER_VERSION)?;
        Ok(RunStore { conn })
    }

    /// Read whatever the journal has gained since last time.
    ///
    /// A segment is read on from its remembered offset only while that offset
    /// still means something — that is, while the bytes before it are the same
    /// bytes. A merge, a rebase or a hand edit rewrites them, and then the
    /// segment is read again from zero with its old rows deleted first, so a line
    /// the rewrite *removed* goes away rather than lingering as a row nothing
    /// backs any more.
    ///
    /// Size and modification time come first because they are a `stat`, and a
    /// journal that has not been touched since the last pass is the common case;
    /// the hash is only reached for when one of them moved.
    pub fn ingest(&mut self, dir: &Path) -> Result<Ingested> {
        let mut summary = Ingested::default();
        for path in journal::segments(dir) {
            let key = path.to_string_lossy().to_string();
            let meta = std::fs::metadata(&path)?;
            let size = meta.len() as i64;
            let mtime = mtime_millis(&meta);

            let seen: Option<(i64, i64, i64, String)> = self
                .conn
                .query_row(
                    "SELECT size, mtime, offset, read_hash FROM segments WHERE path = ?1",
                    [&key],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;

            let from = match &seen {
                // Not written to at all since the last pass.
                Some((prev_size, prev_mtime, offset, _))
                    if size == *prev_size && mtime == *prev_mtime && *offset >= size =>
                {
                    continue;
                }
                Some((_, _, offset, prev_hash))
                    if journal::prefix_hash(&path, *offset as u64) == *prev_hash =>
                {
                    *offset
                }
                Some(_) => {
                    summary.reingested.push(path.clone());
                    self.conn
                        .execute("DELETE FROM runs WHERE segment = ?1", [&key])?;
                    0
                }
                None => 0,
            };

            let (records, at) = journal::read_from(&path, from as u64)?;
            summary.records += records.len();
            // The hash goes in with the offset it belongs to, computed after the
            // read: storing one without the other would leave a cursor nothing
            // can check.
            let hash = journal::prefix_hash(&path, at);
            let tx = self.conn.transaction()?;
            for record in &records {
                upsert(&tx, record, &key)?;
            }
            tx.execute(
                "INSERT INTO segments(path, size, mtime, offset, read_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(path) DO UPDATE SET
                   size = excluded.size, mtime = excluded.mtime,
                   offset = excluded.offset, read_hash = excluded.read_hash",
                params![&key, size, mtime, at as i64, &hash],
            )?;
            tx.commit()?;
        }
        Ok(summary)
    }

    /// Forget segments whose files are gone, so retention does not leave the
    /// cursor table growing forever.
    pub fn forget_missing_segments(&mut self, dir: &Path) -> Result<usize> {
        let live: Vec<String> = journal::segments(dir)
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        let known: Vec<String> = {
            let mut st = self.conn.prepare("SELECT path FROM segments")?;
            let rows = st.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut gone = 0;
        for path in known.iter().filter(|p| !live.contains(p)) {
            self.conn
                .execute("DELETE FROM runs WHERE segment = ?1", [path])?;
            self.conn
                .execute("DELETE FROM segments WHERE path = ?1", [path])?;
            gone += 1;
        }
        Ok(gone)
    }

    /// Whether this run is already known. What stops an observation being
    /// appended to the journal a second time on the next tick.
    pub fn has(&self, run_id: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT 1 FROM runs WHERE run_id = ?1", [run_id], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// The most recent run of each script.
    ///
    /// One row per script, which is what the graph shows: a node per run would
    /// churn an entity every time a schedule fired, and the durable history is
    /// the journal.
    pub fn latest_per_script(&self) -> Result<Vec<Record>> {
        let mut st = self.conn.prepare(
            "SELECT * FROM runs WHERE run_id = (
               SELECT x.run_id FROM runs AS x WHERE x.script_key = runs.script_key
               ORDER BY x.started_at DESC, x.run_id DESC LIMIT 1
             ) ORDER BY script_key",
        )?;
        let rows = st.query_map([], record_of)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One script's runs, newest first.
    pub fn history(&self, script_key: &str, limit: u32) -> Result<Vec<Record>> {
        let mut st = self.conn.prepare(
            "SELECT * FROM runs WHERE script_key = ?1
             ORDER BY started_at DESC, run_id DESC LIMIT ?2",
        )?;
        let rows = st.query_map(params![script_key, limit], record_of)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn count(&self) -> Result<u64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM runs", [], |r| r.get::<_, i64>(0))? as u64)
    }

    /// Keep at most `keep_per_script` runs of each script, dropping the oldest.
    ///
    /// The committed journal is untouched: this bounds the derived index only,
    /// so a machine that has kept a longer journal still has its history and
    /// pruning here is never a loss of data.
    pub fn prune(&mut self, keep_per_script: u32) -> Result<usize> {
        if keep_per_script == 0 {
            return Ok(0);
        }
        Ok(self.conn.execute(
            "DELETE FROM runs WHERE run_id IN (
               SELECT run_id FROM (
                 SELECT run_id, ROW_NUMBER() OVER (
                   PARTITION BY script_key ORDER BY started_at DESC, run_id DESC
                 ) AS rn FROM runs
               ) WHERE rn > ?1
             )",
            [keep_per_script],
        )?)
    }
}

fn upsert(tx: &Connection, r: &Record, segment: &str) -> rusqlite::Result<()> {
    let argv = serde_json::to_string(&r.argv).unwrap_or_else(|_| "[]".into());
    tx.execute(
        "INSERT INTO runs(run_id, machine, script_key, schedule_id, trigger, argv, cwd,
                          started_at, ended_at, status, exit_code, duration_ms,
                          out_bytes, err_bytes, tail, log_path, fidelity, seq, segment)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)
         ON CONFLICT(run_id) DO UPDATE SET
           machine = excluded.machine, script_key = excluded.script_key,
           schedule_id = excluded.schedule_id, trigger = excluded.trigger,
           argv = excluded.argv, cwd = excluded.cwd,
           started_at = excluded.started_at, ended_at = excluded.ended_at,
           status = excluded.status, exit_code = excluded.exit_code,
           duration_ms = excluded.duration_ms, out_bytes = excluded.out_bytes,
           err_bytes = excluded.err_bytes, tail = excluded.tail,
           log_path = excluded.log_path, fidelity = excluded.fidelity,
           seq = excluded.seq, segment = excluded.segment
         WHERE excluded.seq > runs.seq",
        params![
            &r.run_id,
            &r.machine,
            &r.script_key,
            &r.schedule_id,
            &r.trigger,
            &argv,
            &r.cwd,
            &r.started_at,
            &r.ended_at,
            &r.status,
            &r.exit_code,
            &r.duration_ms,
            &r.out_bytes,
            &r.err_bytes,
            &r.tail,
            &r.log_path,
            &r.fidelity,
            r.seq,
            segment,
        ],
    )?;
    Ok(())
}

fn record_of(row: &Row) -> rusqlite::Result<Record> {
    let argv: String = row.get("argv")?;
    Ok(Record {
        run_id: row.get("run_id")?,
        machine: row.get("machine")?,
        script_key: row.get("script_key")?,
        schedule_id: row.get("schedule_id")?,
        trigger: row.get("trigger")?,
        argv: serde_json::from_str(&argv).unwrap_or_default(),
        cwd: row.get("cwd")?,
        started_at: row.get("started_at")?,
        ended_at: row.get("ended_at")?,
        status: row.get("status")?,
        exit_code: row.get("exit_code")?,
        duration_ms: row.get("duration_ms")?,
        out_bytes: row.get("out_bytes")?,
        err_bytes: row.get("err_bytes")?,
        tail: row.get("tail")?,
        log_path: row.get("log_path")?,
        fidelity: row.get("fidelity")?,
        seq: row.get("seq")?,
    })
}

fn mtime_millis(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::journal::{fidelity, status, trigger};

    /// 2026-09-15T00:00:00Z — a real instant, because the segment a record
    /// lands in is derived from it and a test that hardcodes the filename
    /// separately can silently write to a different file.
    const MILLIS: i64 = 1_789_430_400_000;

    /// The one segment the journal has. Asked for rather than spelled out, for
    /// the reason above.
    fn only_segment(dir: &Path) -> PathBuf {
        let found = journal::segments(dir);
        assert_eq!(found.len(), 1, "{found:?}");
        found.into_iter().next().unwrap()
    }

    fn record(machine: &str, script: &str, millis: i64, seq: i64, status: &str) -> Record {
        Record {
            run_id: journal::run_id(machine, millis, script),
            machine: machine.into(),
            script_key: script.into(),
            trigger: trigger::LAUNCHD.into(),
            started_at: journal::rfc3339(millis),
            status: status.into(),
            fidelity: fidelity::OBSERVED.into(),
            seq,
            ..Default::default()
        }
    }

    fn ingested(dir: &Path) -> (RunStore, Ingested) {
        let mut store = RunStore::open_in_memory().unwrap();
        let summary = store.ingest(dir).unwrap();
        (store, summary)
    }

    #[test]
    fn the_finish_line_wins_over_the_start_line_whichever_order_they_arrive() {
        // The two lines of one run. Reading them backwards is not hypothetical:
        // a merged file has no useful order.
        for reversed in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let mut lines = vec![
                record("mbp", "a.py", MILLIS, 0, status::RUNNING),
                Record {
                    exit_code: Some(0),
                    duration_ms: Some(1_200),
                    ..record("mbp", "a.py", MILLIS, 1, status::OK)
                },
            ];
            if reversed {
                lines.reverse();
            }
            for line in &lines {
                journal::append(tmp.path(), line).unwrap();
            }
            let (store, _) = ingested(tmp.path());
            assert_eq!(store.count().unwrap(), 1, "one run, not two lines");
            let latest = store.latest_per_script().unwrap();
            assert_eq!(latest[0].status, status::OK, "reversed: {reversed}");
            assert_eq!(latest[0].duration_ms, Some(1_200));
        }
    }

    #[test]
    fn the_same_run_arriving_from_two_machines_stays_one_row() {
        // What a `git pull` produces when both clones already had the record:
        // the union merge keeps both lines, and the derived id collapses them.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mbp").join("2026-09.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let line =
            serde_json::to_string(&record("mbp", "a.py", MILLIS, 1, status::OK)).unwrap() + "\n";
        std::fs::write(&path, format!("{line}{line}")).unwrap();
        let (store, summary) = ingested(tmp.path());
        assert_eq!(summary.records, 2, "both lines were read");
        assert_eq!(store.count().unwrap(), 1, "and are one run");
    }

    #[test]
    fn ingesting_twice_over_an_unchanged_journal_does_nothing() {
        // It runs on a timer, so this is the common case rather than an edge one.
        let tmp = tempfile::tempdir().unwrap();
        journal::append(tmp.path(), &record("mbp", "a.py", MILLIS, 1, status::OK)).unwrap();
        let (mut store, first) = ingested(tmp.path());
        assert_eq!(first.records, 1);
        let again = store.ingest(tmp.path()).unwrap();
        assert_eq!(again.records, 0, "nothing re-read");
        assert_eq!(store.count().unwrap(), 1);
    }

    #[test]
    fn a_rewritten_segment_is_read_again_from_zero() {
        let tmp = tempfile::tempdir().unwrap();
        journal::append(tmp.path(), &record("mbp", "a.py", MILLIS, 1, status::OK)).unwrap();
        let (mut store, _) = ingested(tmp.path());

        // A rebase reorders the file and drops a line: same length is possible,
        // so size alone would miss it.
        let path = only_segment(tmp.path());
        let replacement =
            serde_json::to_string(&record("mbp", "b.py", MILLIS, 1, status::FAILED)).unwrap();
        std::fs::write(&path, format!("{replacement}\n")).unwrap();

        let again = store.ingest(tmp.path()).unwrap();
        assert_eq!(again.reingested.len(), 1, "{again:?}");
        // and the run the rewrite removed is gone rather than lingering
        assert_eq!(store.count().unwrap(), 1);
        assert_eq!(store.latest_per_script().unwrap()[0].script_key, "b.py");
    }

    #[test]
    fn a_shrunken_segment_is_read_again_from_zero() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..3 {
            journal::append(
                tmp.path(),
                &record("mbp", "a.py", MILLIS + i, 1, status::OK),
            )
            .unwrap();
        }
        let (mut store, _) = ingested(tmp.path());
        assert_eq!(store.count().unwrap(), 3);

        let path = only_segment(tmp.path());
        let kept = serde_json::to_string(&record("mbp", "a.py", MILLIS, 1, status::OK)).unwrap();
        std::fs::write(&path, format!("{kept}\n")).unwrap();
        let again = store.ingest(tmp.path()).unwrap();
        assert_eq!(again.reingested.len(), 1);
        assert_eq!(store.count().unwrap(), 1);
    }

    #[test]
    fn an_older_line_never_undoes_a_newer_one() {
        // A clone that only ever saw the start line hands it over long after
        // the finish line was recorded. Applying it would resurrect a finished
        // run as running.
        let tmp = tempfile::tempdir().unwrap();
        journal::append(tmp.path(), &record("mbp", "a.py", MILLIS, 1, status::OK)).unwrap();
        let (mut store, _) = ingested(tmp.path());
        journal::append(
            tmp.path(),
            &record("mbp", "a.py", MILLIS, 0, status::RUNNING),
        )
        .unwrap();
        store.ingest(tmp.path()).unwrap();
        assert_eq!(store.latest_per_script().unwrap()[0].status, status::OK);
    }

    #[test]
    fn only_the_newest_run_of_each_script_is_the_latest_one() {
        let tmp = tempfile::tempdir().unwrap();
        for (script, millis) in [
            ("a.py", MILLIS),
            ("a.py", MILLIS + 86_400_000),
            ("b.py", MILLIS),
        ] {
            journal::append(tmp.path(), &record("mbp", script, millis, 1, status::OK)).unwrap();
        }
        let (store, _) = ingested(tmp.path());
        let latest = store.latest_per_script().unwrap();
        assert_eq!(latest.len(), 2);
        assert_eq!(latest[0].script_key, "a.py");
        assert_eq!(latest[0].started_at, journal::rfc3339(MILLIS + 86_400_000));
        assert_eq!(store.history("a.py", 10).unwrap().len(), 2);
    }

    #[test]
    fn retention_bounds_the_index_per_script_and_keeps_the_newest() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..5 {
            journal::append(
                tmp.path(),
                &record("mbp", "a.py", MILLIS + i * 1_000, 1, status::OK),
            )
            .unwrap();
            journal::append(
                tmp.path(),
                &record("mbp", "b.py", MILLIS + i * 1_000, 1, status::OK),
            )
            .unwrap();
        }
        let (mut store, _) = ingested(tmp.path());
        assert_eq!(store.prune(2).unwrap(), 6, "three dropped from each script");
        assert_eq!(store.history("a.py", 10).unwrap().len(), 2);
        assert_eq!(
            store.history("a.py", 10).unwrap()[0].started_at,
            journal::rfc3339(MILLIS + 4_000),
            "the newest survived"
        );
        // and zero means no limit rather than delete everything
        assert_eq!(store.prune(0).unwrap(), 0);
        assert_eq!(store.count().unwrap(), 4);
    }

    #[test]
    fn a_pruned_segment_takes_its_rows_and_its_cursor_with_it() {
        let tmp = tempfile::tempdir().unwrap();
        journal::append(tmp.path(), &record("mbp", "a.py", MILLIS, 1, status::OK)).unwrap();
        let (mut store, _) = ingested(tmp.path());
        std::fs::remove_file(only_segment(tmp.path())).unwrap();
        assert_eq!(store.forget_missing_segments(tmp.path()).unwrap(), 1);
        assert_eq!(store.count().unwrap(), 0);
        // and ingesting the file back re-reads it from the start
        journal::append(tmp.path(), &record("mbp", "a.py", MILLIS, 1, status::OK)).unwrap();
        assert_eq!(store.ingest(tmp.path()).unwrap().records, 1);
    }

    #[test]
    fn a_run_survives_a_graph_cache_rebuild_because_it_was_never_in_it() {
        // The point of a separate file with a version of its own: the graph
        // store's version bump means "drop and rebuild", and this must not.
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("cache").join("runs.db");
        journal::append(
            &tmp.path().join("runs"),
            &record("mbp", "a.py", MILLIS, 1, status::OK),
        )
        .unwrap();
        {
            let mut store = RunStore::open(&db).unwrap();
            store.ingest(&tmp.path().join("runs")).unwrap();
            assert_eq!(store.count().unwrap(), 1);
        }
        // reopened, as a restart would
        let store = RunStore::open(&db).unwrap();
        assert_eq!(store.count().unwrap(), 1);
        assert!(store.has(&journal::run_id("mbp", MILLIS, "a.py")).unwrap());
    }

    #[test]
    fn a_version_change_rebuilds_from_the_journal_rather_than_losing_history() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("runs");
        let db = tmp.path().join("runs.db");
        journal::append(&dir, &record("mbp", "a.py", MILLIS, 1, status::OK)).unwrap();
        {
            let mut store = RunStore::open(&db).unwrap();
            store.ingest(&dir).unwrap();
        }
        // a future version of this file
        {
            let conn = Connection::open(&db).unwrap();
            conn.pragma_update(None, "user_version", USER_VERSION + 1)
                .unwrap();
        }
        let mut store = RunStore::open(&db).unwrap();
        assert_eq!(store.count().unwrap(), 0, "the cache was dropped");
        // and the cursor went with it, so the journal is read again in full
        assert_eq!(store.ingest(&dir).unwrap().records, 1);
        assert_eq!(store.count().unwrap(), 1);
    }
}
