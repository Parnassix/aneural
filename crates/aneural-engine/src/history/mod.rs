//! Where a change came from: each repository's commits, and the agent sessions
//! behind them.
//!
//! These are producers, not spores. They are not driven by the walker — `.git`
//! is excluded from it in three places, and a session transcript is not in the
//! workspace at all — so they follow the pattern the HTTP harvester already
//! established: a scheduled runner writing to a **synthetic origin** that no
//! file path can collide with. A full index prunes by walking the filesystem,
//! so it leaves these nodes alone; they are retracted by their own sweep and by
//! `delete_by_source` when the producer is turned off.

pub mod attribute;
pub mod claude;
pub mod git;
pub mod plans;
pub mod usage;

/// The origin every commit of one repository hangs off.
///
/// `repo_rel` is the repository root relative to the workspace, so a workspace
/// holding six repositories keeps six independently replaceable origins.
pub fn git_origin(repo_rel: &str) -> String {
    format!("git://{repo_rel}/log")
}

/// The prefix that finds every origin either producer wrote.
pub const GIT_PREFIX: &str = "git://";

/// The origin holding the joins between commits and the sessions that produced
/// them. One origin for all of them, because the answer for any commit can
/// change when a *different* session is read, so they are recomputed together.
pub const LINK_ORIGIN: &str = "claude://links";
