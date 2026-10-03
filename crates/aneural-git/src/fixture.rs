//! Building real repositories in tests.
//!
//! Shared rather than copied, because writing a tree by hand is easy to get
//! subtly wrong: a blob named `src/a.ts` looks like it works right up until
//! something asks git to build an index from the tree, and then every diff
//! comes back empty. Directories are nested trees, always.

use std::collections::BTreeMap;
use std::path::Path;

/// Write `files` as a properly nested tree and commit it on top of HEAD.
pub fn commit(repo: &gix::Repository, message: &str, files: &[(&str, &str)]) -> gix::ObjectId {
    let tree = write_tree(repo, files);
    // 2026-09-17T18:22:49Z. Fixed so shas are reproducible, and late enough to
    // sit after the session timestamps other fixtures use — a commit that
    // predates every session is attributed to none of them, correctly, which
    // makes an arbitrary epoch a confusing default.
    let who = gix::actor::SignatureRef {
        name: "Tester".into(),
        email: "tester@example.com".into(),
        time: "1789669369 +0000",
    };
    let parents: Vec<gix::ObjectId> = repo
        .head_id()
        .map(|id| vec![id.detach()])
        .unwrap_or_default();
    repo.commit_as(who, who, "HEAD", message, tree, parents)
        .expect("commit")
        .detach()
}

/// A repository with nothing in it yet, at `root`.
pub fn init(root: &Path) -> gix::Repository {
    gix::init(root).expect("init")
}

/// Turn `("src/a.ts", "…")` pairs into a tree, creating a subtree per directory.
fn write_tree(repo: &gix::Repository, files: &[(&str, &str)]) -> gix::ObjectId {
    // Group by first path segment: leaves become blobs, directories recurse.
    let mut blobs: Vec<(&str, &str)> = Vec::new();
    let mut dirs: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for (path, body) in files {
        match path.split_once('/') {
            Some((dir, rest)) => dirs.entry(dir).or_default().push((rest, body)),
            None => blobs.push((path, body)),
        }
    }

    let mut tree = gix::objs::Tree::empty();
    for (name, body) in blobs {
        let oid = repo.write_blob(body.as_bytes()).expect("blob").detach();
        tree.entries.push(gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Blob.into(),
            filename: name.into(),
            oid,
        });
    }
    for (name, inner) in dirs {
        let oid = write_tree(repo, &inner);
        tree.entries.push(gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Tree.into(),
            filename: name.into(),
            oid,
        });
    }
    tree.entries.sort();
    repo.write_object(&tree).expect("tree").detach()
}
