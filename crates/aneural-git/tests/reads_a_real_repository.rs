//! Builds a repository commit by commit and reads it back, so the walk, the
//! tree diff and the trailer are all exercised against real objects rather than
//! against this checkout, which a tarball build would not have.

use aneural_git::fixture::{commit, init};
use std::path::Path;

#[test]
fn a_repository_reads_back_as_commits_changed_files_and_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init(tmp.path());

    let first = commit(
        &repo,
        "Add the entry point\n\nThe body.",
        &[("a.ts", "one\n")],
    );
    let second = commit(
        &repo,
        "Change one file and add another\n\nClaude-Session: https://claude.ai/code/session_01HU8B",
        &[("a.ts", "two\n"), ("b.ts", "new\n")],
    );

    let log = aneural_git::history(tmp.path(), 10).unwrap();
    assert_eq!(log.len(), 2, "newest first");
    assert_eq!(log[0].sha, second.to_hex().to_string());
    assert_eq!(log[1].sha, first.to_hex().to_string());

    let head = &log[0];
    assert_eq!(head.subject, "Change one file and add another");
    assert_eq!(head.session.as_deref(), Some("01HU8B"));
    assert_eq!(head.author, "Tester");
    assert_eq!(head.parents, vec![first.to_hex().to_string()]);

    let mut changed: Vec<_> = head
        .changed
        .iter()
        .map(|c| (c.path.as_str(), c.status.as_str()))
        .collect();
    changed.sort();
    assert_eq!(
        changed,
        vec![("a.ts", "modified"), ("b.ts", "added")],
        "only what this commit touched, against its parent"
    );

    let root = &log[1];
    assert!(root.parents.is_empty());
    assert_eq!(root.session, None);
    assert_eq!(
        root.changed
            .iter()
            .map(|c| (c.path.as_str(), c.status.as_str()))
            .collect::<Vec<_>>(),
        vec![("a.ts", "added")],
        "a root commit adds its whole tree"
    );
}

/// `dirty` compares the worktree against the *index*, not against HEAD, so a
/// repository whose index was never written has nothing to be dirty against —
/// writing a commit object does not stage anything. Worth pinning: the
/// alternative reading, that an empty index means "everything changed", would
/// light up every file in a freshly cloned-but-unchecked-out repository.
#[test]
fn an_unpopulated_index_makes_nothing_dirty() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init(tmp.path());
    commit(&repo, "Seed", &[("a.ts", "one\n")]);
    std::fs::write(tmp.path().join("a.ts"), "edited\n").unwrap();

    assert!(aneural_git::dirty(tmp.path()).unwrap().is_empty());
}

/// Against this checkout, where the index is real and files have genuinely been
/// edited, every path reported must be a tracked, workspace-relative one.
#[test]
fn dirty_paths_are_relative_and_tracked() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    if !root.join(".git").exists() {
        return;
    }
    for path in aneural_git::dirty(&root).unwrap() {
        assert!(!path.starts_with('/'), "{path} must be relative");
        assert!(!path.contains(".git/"), "{path} must be a tracked file");
    }
}

#[test]
fn a_deep_path_keeps_its_directories() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init(tmp.path());
    commit(&repo, "Deep", &[("src/nested/deep.ts", "deep\n")]);

    let log = aneural_git::history(tmp.path(), 10).unwrap();
    assert_eq!(
        log[0]
            .changed
            .iter()
            .map(|c| c.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/nested/deep.ts"],
        "the file, with its whole path \u{2014} not the directories it sits in"
    );
}
