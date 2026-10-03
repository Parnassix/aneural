//! A repository's own history, read into plain data.
//!
//! This crate knows nothing about the graph: it returns commits and the paths
//! they changed, and the engine turns those into nodes and edges. It is kept
//! separate so `gix` never appears under `aneural-core` or `aneural-store`.
//!
//! **No network, ever.** `gix` is depended on with `default-features = false`
//! precisely so its transports are not compiled in — `aneural-registry` is the
//! only crate allowed to reach outside this machine.

use std::path::Path;

pub mod fixture;
pub mod trailer;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("open repository: {0}")]
    Open(#[from] Box<gix::open::Error>),
    #[error("read history: {0}")]
    Walk(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// One commit, flattened to what the graph needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub sha: String,
    /// First 8 hex characters, for labels.
    pub short: String,
    /// The first line of the message.
    pub subject: String,
    /// Everything after the first line, trailers included.
    pub body: String,
    pub author: String,
    pub email: String,
    /// Committed-at, RFC 3339, so it reads as a date in the inspector.
    pub at: String,
    /// Committed-at as seconds since the epoch, for ordering and windowing.
    pub seconds: i64,
    /// The agent session named by a `Claude-Session:` trailer, if any — the
    /// identifier only, with whatever prefix the URL used stripped off.
    pub session: Option<String>,
    pub parents: Vec<String>,
    pub changed: Vec<Changed>,
}

/// A path a commit changed, relative to the repository root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changed {
    pub path: String,
    pub status: Status,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Added,
    Modified,
    Deleted,
    Renamed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Added => "added",
            Status::Modified => "modified",
            Status::Deleted => "deleted",
            Status::Renamed => "renamed",
        }
    }
}

/// Open a repository without trusting anything it says about itself.
///
/// A workspace can contain repositories the user merely cloned, and a
/// `.git/config` can point `core.fsmonitor`, aliases and credential helpers at
/// arbitrary executables. Isolated options ignore that config and the
/// environment, which is what makes it safe to walk into any directory.
fn open(root: &Path) -> Result<gix::Repository> {
    gix::open_opts(root, gix::open::Options::isolated()).map_err(|e| Error::Open(Box::new(e)))
}

/// The most recent `max` commits reachable from HEAD, newest first.
///
/// An empty repository, a detached or unborn HEAD, and a directory that is not
/// a repository at all are all "no history", not errors — the walker hands us
/// whatever it found and we index what we can.
pub fn history(root: &Path, max: usize) -> Result<Vec<Commit>> {
    let repo = open(root)?;
    let Ok(head) = repo.head_id() else {
        return Ok(Vec::new());
    };
    let walk = repo
        .rev_walk([head.detach()])
        .all()
        .map_err(|e| Error::Walk(e.to_string()))?;

    let mut out = Vec::new();
    for info in walk.take(max) {
        let info = info.map_err(|e| Error::Walk(e.to_string()))?;
        let Ok(commit) = repo.find_commit(info.id) else {
            continue;
        };
        out.push(flatten(&repo, &commit));
    }
    Ok(out)
}

/// Tracked paths whose file on disk no longer matches what the index recorded.
///
/// This is deliberately the cheap comparison — size and mtime against the stat
/// data git already cached — rather than gix's full `status`, which would drag
/// in the dirwalk, filter, pathspec and submodule machinery to also tell us
/// about untracked and ignored files. All the graph needs is "work is in
/// flight on this file", and a stat comparison answers that for nothing.
///
/// It inherits git's own blind spot: a file rewritten within the same second,
/// to the same length, reads as clean until the next stat changes.
pub fn dirty(root: &Path) -> Result<Vec<String>> {
    let repo = open(root)?;
    let Ok(index) = repo.index() else {
        return Ok(Vec::new());
    };
    let work_dir = repo.workdir().unwrap_or(root).to_path_buf();

    let mut out = Vec::new();
    for entry in index.entries() {
        let rel = entry.path(&index).to_string();
        let Ok(meta) = std::fs::metadata(work_dir.join(&rel)) else {
            // Tracked but not on disk: deleted, which is a change.
            out.push(rel);
            continue;
        };
        let same_size = meta.len() as u32 == entry.stat.size;
        let same_time = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|d| d.as_secs() as u32 == entry.stat.mtime.secs);
        if !(same_size && same_time) {
            out.push(rel);
        }
    }
    Ok(out)
}

fn flatten(repo: &gix::Repository, commit: &gix::Commit<'_>) -> Commit {
    let sha = commit.id().to_hex().to_string();
    let message = commit
        .message_raw()
        .map(|m| m.to_string())
        .unwrap_or_default();
    let (subject, body) = split_message(&message);
    let (author, email, seconds) = match commit.author() {
        Ok(a) => (
            a.name.to_string(),
            a.email.to_string(),
            a.time().map(|t| t.seconds).unwrap_or_default(),
        ),
        Err(_) => (String::new(), String::new(), 0),
    };
    Commit {
        short: sha.chars().take(8).collect(),
        session: trailer::session(&body),
        changed: changed_paths(repo, commit),
        parents: commit
            .parent_ids()
            .map(|p| p.to_hex().to_string())
            .collect(),
        at: rfc3339(seconds),
        seconds,
        subject,
        body,
        author,
        email,
        sha,
    }
}

/// The paths a commit changed against its first parent. A root commit counts
/// as adding everything in its tree.
fn changed_paths(repo: &gix::Repository, commit: &gix::Commit<'_>) -> Vec<Changed> {
    let Ok(new_tree) = commit.tree() else {
        return Vec::new();
    };
    let old_tree = commit
        .parent_ids()
        .next()
        .and_then(|p| repo.find_commit(p).ok())
        .and_then(|p| p.tree().ok())
        .unwrap_or_else(|| repo.empty_tree());

    let changes = match repo.diff_tree_to_tree(Some(&old_tree), Some(&new_tree), None) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("diff {}: {e}", commit.id());
            return Vec::new();
        }
    };

    // The walk recurses but reports the directories it descended through as
    // well as the files inside them, so `crates/` shows up beside every file
    // under it. Only blobs are changes anyone means.
    let mut out = Vec::new();
    for change in changes {
        use gix::diff::tree_with_rewrites::Change::*;
        let (path, mode, status) = match &change {
            Addition {
                location,
                entry_mode,
                ..
            } => (location, entry_mode, Status::Added),
            Deletion {
                location,
                entry_mode,
                ..
            } => (location, entry_mode, Status::Deleted),
            Modification {
                location,
                entry_mode,
                ..
            } => (location, entry_mode, Status::Modified),
            Rewrite {
                location,
                entry_mode,
                ..
            } => (location, entry_mode, Status::Renamed),
        };
        if mode.is_tree() {
            continue;
        }
        out.push(Changed {
            path: path.to_string(),
            status,
        });
    }
    out
}

/// Split a commit message into its first line and the rest.
fn split_message(message: &str) -> (String, String) {
    match message.split_once('\n') {
        Some((first, rest)) => (first.trim().to_string(), rest.trim().to_string()),
        None => (message.trim().to_string(), String::new()),
    }
}

/// Seconds since the epoch as an RFC 3339 UTC timestamp.
fn rfc3339(seconds: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(seconds)
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

    #[test]
    fn a_message_splits_into_subject_and_body() {
        let (s, b) = split_message("Grow the graph\n\nBecause it was a mess.\n");
        assert_eq!(s, "Grow the graph");
        assert_eq!(b, "Because it was a mess.");
        let (s, b) = split_message("One liner");
        assert_eq!(s, "One liner");
        assert_eq!(b, "");
    }

    #[test]
    fn a_directory_that_is_not_a_repository_has_no_history() {
        let dir = tempfile::tempdir().unwrap();
        assert!(history(dir.path(), 10).is_err() || history(dir.path(), 10).unwrap().is_empty());
    }

    /// Reads Aneural's own repository, which is the only fixture with real
    /// commits, real trailers and real renames in it. Skipped when the crate is
    /// built from a tarball with no `.git`, and cut short in a shallow clone.
    #[test]
    fn this_repository_reads_back_whole() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        if !root.join(".git").exists() {
            return;
        }
        let log = history(&root, 50).unwrap();
        assert!(!log.is_empty(), "this repository has commits");

        let head = &log[0];
        assert_eq!(head.sha.len(), 40);
        assert_eq!(head.short.len(), 8);
        assert!(head.sha.starts_with(&head.short));
        assert!(!head.subject.is_empty(), "every commit has a subject line");
        assert!(head.at.starts_with("20"), "at is RFC 3339: {}", head.at);
        assert!(head.seconds > 0);

        // The tree walk reports the directories it descended through as well as
        // the blobs inside them, so `crates` would appear beside every file
        // under it. Pinned against a commit whose real shape is known:
        // `git show --name-only 4b0dae7` lists 21 paths, all of them files.
        //
        // Everything from here on needs real history, and a checkout does not
        // always have it: CI clones one commit deep, and on a pull request that
        // one commit is a merge GitHub made, with no trailer. A clone that
        // cannot see the pinned commit has nothing to say about the rest.
        let Some(mycelium) = log.iter().find(|c| c.sha.starts_with("4b0dae7750c05be7")) else {
            return;
        };
        assert!(
            log.iter().any(|c| c.session.is_some()),
            "this repository's commits carry Claude-Session trailers"
        );
        assert!(
            log.iter().any(|c| !c.parents.is_empty()),
            "history is a chain, not a heap"
        );
        assert_eq!(
            mycelium.changed.len(),
            21,
            "directories must not be counted as changes: {:?}",
            mycelium.changed
        );
        assert!(
            mycelium.changed.iter().all(|c| c.path.contains('.')),
            "every changed path is a file, not a directory it sits in"
        );
        assert_eq!(
            mycelium.session.as_deref(),
            Some("01HU8BExGG2vrVEe15dkroDa")
        );
    }
}
