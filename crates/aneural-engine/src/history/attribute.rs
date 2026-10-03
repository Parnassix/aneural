//! Which session produced which commit.
//!
//! A commit's `Claude-Session` trailer names a *bridge* session, and a bridge
//! spans every local session that continued the same conversation — on the
//! machine this was written for, one bridge covered three sessions across eight
//! days and two unrelated plans. So the trailer narrows the field; it does not
//! answer the question.
//!
//! Two further tests settle it, in order:
//!
//! 1. **Time.** A session that had not started when the commit was made cannot
//!    have produced it. Among those that had, the one still running closest to
//!    the commit is the best candidate.
//! 2. **Overlap.** A session that touched none of the files in the commit is
//!    demoted below one that touched some, however good its timing — this is
//!    what keeps two sessions on the same bridge, working on different things,
//!    from being confused for each other.
//!
//! When nothing passes, the commit is attributed to no session. That is the
//! ordinary case, not a failure: most work is not done in plan mode, and plenty
//! of commits predate the trailer entirely.

use std::collections::BTreeSet;

/// What the matcher needs to know about a session.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Candidate {
    pub uuid: String,
    /// The identifier a trailer would name, reduced to bare form.
    pub bridge: Option<String>,
    pub started: String,
    pub ended: String,
    /// Workspace-relative paths the session was seen to touch.
    pub touched: BTreeSet<String>,
}

/// What the matcher needs to know about a commit.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Subject {
    /// The identifier from the commit's trailer, if it had one.
    pub session: Option<String>,
    /// Committed-at, RFC 3339, comparable as a string.
    pub at: String,
    /// Workspace-relative paths the commit changed.
    pub changed: BTreeSet<String>,
}

/// The session that best explains this commit, if any does.
pub fn best<'a>(commit: &Subject, sessions: &'a [Candidate]) -> Option<&'a Candidate> {
    let wanted = commit.session.as_ref()?;
    sessions
        .iter()
        .filter(|s| s.bridge.as_ref() == Some(wanted))
        // A session that began after the commit cannot have written it. An
        // unknown start time is not evidence against, so it passes.
        .filter(|s| s.started.is_empty() || s.started <= commit.at)
        .max_by(|a, b| {
            overlap(a, commit)
                .cmp(&overlap(b, commit))
                .then_with(|| closeness(a, commit).cmp(&closeness(b, commit)))
                .then_with(|| a.uuid.cmp(&b.uuid))
        })
}

/// How many of the commit's files this session was seen to touch.
fn overlap(session: &Candidate, commit: &Subject) -> usize {
    session.touched.intersection(&commit.changed).count()
}

/// How recently the session was still active when the commit landed. A session
/// still running at that moment is the strongest signal there is, so it ranks
/// above every finished one.
fn closeness(session: &Candidate, commit: &Subject) -> (u8, String) {
    if session.ended.is_empty() {
        return (0, String::new());
    }
    if session.ended >= commit.at {
        (2, session.started.clone())
    } else {
        (1, session.ended.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(uuid: &str, started: &str, ended: &str, touched: &[&str]) -> Candidate {
        Candidate {
            uuid: uuid.into(),
            bridge: Some("BRIDGE".into()),
            started: started.into(),
            ended: ended.into(),
            touched: touched.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn commit(at: &str, changed: &[&str]) -> Subject {
        Subject {
            session: Some("BRIDGE".into()),
            at: at.into(),
            changed: changed.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn a_commit_without_a_trailer_is_attributed_to_nobody() {
        let sessions = [session("a", "2026-09-17T09:00Z", "2026-09-17T12:00Z", &[])];
        let subject = Subject {
            session: None,
            at: "2026-09-17T10:00Z".into(),
            ..Default::default()
        };
        assert!(best(&subject, &sessions).is_none());
    }

    #[test]
    fn a_trailer_naming_a_bridge_we_do_not_have_matches_nothing() {
        let sessions = [Candidate {
            bridge: Some("OTHER".into()),
            ..session("a", "2026-09-17T09:00Z", "2026-09-17T12:00Z", &[])
        }];
        assert!(best(&commit("2026-09-17T10:00Z", &[]), &sessions).is_none());
    }

    #[test]
    fn a_session_that_had_not_started_yet_is_never_the_answer() {
        let sessions = [session(
            "later",
            "2026-09-18T09:00Z",
            "2026-09-18T12:00Z",
            &[],
        )];
        assert!(best(&commit("2026-09-17T10:00Z", &[]), &sessions).is_none());
    }

    /// The case that made this module necessary: one bridge, three sessions,
    /// and the right answer is the one that touched the files.
    #[test]
    fn overlap_beats_timing_when_a_bridge_spans_several_sessions() {
        let sessions = [
            session(
                "marketplace",
                "2026-09-17T05:00Z",
                "2026-09-17T17:00Z",
                &["docs/marketplace.md"],
            ),
            session(
                "mycelium",
                "2026-09-17T06:00Z",
                "2026-09-17T16:00Z",
                &["crates/aneural-gui/src/layout.rs"],
            ),
        ];
        let subject = commit(
            "2026-09-17T18:22Z",
            &["crates/aneural-gui/src/layout.rs", "README.md"],
        );
        assert_eq!(best(&subject, &sessions).unwrap().uuid, "mycelium");
    }

    #[test]
    fn with_no_overlap_to_go_on_the_session_still_running_wins() {
        let sessions = [
            session("finished", "2026-09-17T05:00Z", "2026-09-17T06:00Z", &[]),
            session("running", "2026-09-17T09:00Z", "2026-09-17T23:00Z", &[]),
        ];
        assert_eq!(
            best(&commit("2026-09-17T18:22Z", &["a.ts"]), &sessions)
                .unwrap()
                .uuid,
            "running"
        );
    }

    #[test]
    fn among_finished_sessions_the_most_recent_one_wins() {
        let sessions = [
            session("older", "2026-09-15T05:00Z", "2026-09-15T06:00Z", &[]),
            session("newer", "2026-09-17T05:00Z", "2026-09-17T06:00Z", &[]),
        ];
        assert_eq!(
            best(&commit("2026-09-17T18:22Z", &["a.ts"]), &sessions)
                .unwrap()
                .uuid,
            "newer"
        );
    }

    #[test]
    fn a_tie_resolves_the_same_way_every_time() {
        let sessions = [
            session("bbb", "2026-09-17T05:00Z", "2026-09-17T06:00Z", &["a.ts"]),
            session("aaa", "2026-09-17T05:00Z", "2026-09-17T06:00Z", &["a.ts"]),
        ];
        let subject = commit("2026-09-17T18:22Z", &["a.ts"]);
        assert_eq!(best(&subject, &sessions).unwrap().uuid, "bbb");
        let flipped = [sessions[1].clone(), sessions[0].clone()];
        assert_eq!(best(&subject, &flipped).unwrap().uuid, "bbb");
    }
}
