//! The `Claude-Session:` trailer, which is what joins a commit to the session
//! that produced it.
//!
//! Claude Code writes the session as a URL:
//!
//! ```text
//! Claude-Session: https://claude.ai/code/session_01HU8BExGG2vrVEe15dkroDa
//! ```
//!
//! while a transcript records the same session as
//! `"bridgeSessionId": "cse_01HU8BExGG2vrVEe15dkroDa"`. The prefixes differ and
//! the identifier does not, so both sides are reduced to the identifier alone
//! and compared on that.

/// The session identifier named by a `Claude-Session` trailer, if the message
/// has one. Returns the last such trailer, since a rebase can stack them.
pub fn session(body: &str) -> Option<String> {
    body.lines()
        .rev()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case("claude-session")
                .then(|| identifier(value.trim()))
        })
        .find(|id| !id.is_empty())
}

/// Reduce either spelling to the bare identifier: take the last path segment
/// of a URL, then drop a `session_` or `cse_` prefix.
pub fn identifier(raw: &str) -> String {
    let last = raw.rsplit('/').next().unwrap_or(raw).trim();
    for prefix in ["session_", "cse_"] {
        if let Some(rest) = last.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    last.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = "Eight edge kinds had become four jobs.\n\n\
        Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>\n\
        Claude-Session: https://claude.ai/code/session_01HU8BExGG2vrVEe15dkroDa";

    #[test]
    fn the_two_spellings_reduce_to_the_same_identifier() {
        assert_eq!(
            session(REAL).as_deref(),
            Some("01HU8BExGG2vrVEe15dkroDa"),
            "the commit side"
        );
        assert_eq!(
            identifier("cse_01HU8BExGG2vrVEe15dkroDa"),
            "01HU8BExGG2vrVEe15dkroDa",
            "the transcript side"
        );
    }

    #[test]
    fn a_message_without_the_trailer_names_no_session() {
        assert_eq!(session("Just a commit.\n\nWith a body."), None);
        assert_eq!(session(""), None);
        assert_eq!(
            session("Co-Authored-By: someone <a@b.c>"),
            None,
            "another trailer is not this one"
        );
    }

    #[test]
    fn a_rebase_that_stacked_trailers_takes_the_last() {
        let body = "Body.\n\nClaude-Session: https://claude.ai/code/session_AAA\n\
                    Claude-Session: https://claude.ai/code/session_BBB";
        assert_eq!(session(body).as_deref(), Some("BBB"));
    }

    #[test]
    fn a_bare_identifier_survives_unchanged() {
        assert_eq!(
            identifier("01HU8BExGG2vrVEe15dkroDa"),
            "01HU8BExGG2vrVEe15dkroDa"
        );
    }
}
