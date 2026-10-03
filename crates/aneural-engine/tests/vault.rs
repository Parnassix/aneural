//! End-to-end: an Obsidian vault indexed as a workspace.
//!
//! The shapes here are the ones measured in a real 280-note vault: links that
//! carry a stale relative path, links written to an alias, links to notes
//! nobody has written, and the same title used in two folders.

use aneural_core::kinds::EdgeKind;
use aneural_core::{NodeId, Workspace};
use aneural_engine::{Engine, EngineEvent};
use aneural_store::{EdgeQuery, NodeQuery};
use std::path::Path;

fn write(root: &Path, rel: &str, body: &str) {
    let abs = root.join(rel);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    std::fs::write(abs, body).unwrap();
}

fn vault() -> (tempfile::TempDir, Workspace) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir_all(&root).unwrap();

    // `.obsidian/` as Obsidian actually leaves it: UI state, and one
    // preference that has an Aneural equivalent.
    write(
        &root,
        ".obsidian/graph.json",
        r#"{"hideUnresolved": false, "scale": 0.246, "showTags": true}"#,
    );
    write(&root, ".obsidian/app.json", "{}");

    // A note that answers to two names besides its filename.
    write(
        &root,
        "Data Sources/Refresh Cadences.md",
        "---\naliases:\n  - Cadences\n  - \"Refresh Schedule\"\ntags: [data-source, observability]\n---\n         # Refresh Cadences\n\nSee [[Identifier Catalog]].\n",
    );
    write(
        &root,
        "Data Sources/Identifier Catalog.md",
        "---\ntags: [data-source, FDA]\n---\n# Identifier Catalog\n\nNothing yet.\n",
    );

    // Obsidian's own relative-path spelling, an alias, and two links to notes
    // that do not exist — one of them asked for twice, from two files.
    write(
        &root,
        "Engineering/Ingest Scripts/Index.md",
        "# Index\n\nSee [[../../Data Sources/Refresh Cadences|Refresh Cadences]] and [[Cadences]].\n\
         Also [[Download Script Trust Audit]] and [[Watch All]].\n",
    );
    // A table row, where the alias separator must be escaped or the row breaks.
    // This is how most cross-folder links in a real vault are written.
    write(
        &root,
        "Engineering/Derivations/Index.md",
        "# Index\n\nBack to [[Index]], and [[Watch All]] again.\n\n         | doc | note |\n|---|---|\n         | a | [[../../Data Sources/Identifier Catalog\\|Identifier Catalog]] |\n",
    );

    let root = root.canonicalize().unwrap();
    let ws = Workspace::at(&root);
    ws.init(None, true).unwrap();
    (tmp, ws)
}

fn index(ws: &Workspace) -> Engine {
    let mut engine = Engine::open_in_memory(ws.clone()).unwrap();
    engine.index_full(false, &mut |_: EngineEvent| {}).unwrap();
    engine
}

/// Every wiki link out of `from`, as (target id, via).
fn links(engine: &Engine, from: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = engine
        .store()
        .get_edges(&EdgeQuery {
            src: Some(NodeId::file(from)),
            kinds: vec![EdgeKind::RELATES_TO.into()],
            ..Default::default()
        })
        .unwrap()
        .iter()
        .map(|e| {
            (
                e.dst.to_string(),
                e.props["via"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    out.sort();
    out
}

#[test]
fn a_vault_is_marked_and_keeps_its_own_unresolved_link_preference() {
    let (_tmp, ws) = vault();
    let engine = index(&ws);

    let root = engine.store().get_node(&NodeId::dir(".")).unwrap().unwrap();
    assert_eq!(root.props["vault"], true);
    assert_eq!(
        root.props["hideUnresolvedLinks"], false,
        "read from the vault's own graph.json, not guessed"
    );
}

#[test]
fn links_resolve_by_name_through_a_stale_path_and_through_an_alias() {
    let (_tmp, ws) = vault();
    let engine = index(&ws);

    let cadences = "file:Data Sources/Refresh Cadences.md";
    let got = links(&engine, "Engineering/Ingest Scripts/Index.md");

    // `[[../../Data Sources/Refresh Cadences|…]]` and `[[Cadences]]` are the
    // same note reached two ways, so they are one edge.
    assert!(
        got.iter()
            .any(|(dst, via)| dst == cadences && via == "wikilink"),
        "the path-spelled link and the alias both land on the note: {got:?}"
    );
    assert_eq!(
        got.iter().filter(|(dst, _)| dst == cadences).count(),
        1,
        "one edge per pair, however many ways it was written"
    );
}

#[test]
fn a_link_in_a_table_resolves_rather_than_becoming_a_missing_note() {
    let (_tmp, ws) = vault();
    let engine = index(&ws);

    let got = links(&engine, "Engineering/Derivations/Index.md");
    assert!(
        got.iter().any(
            |(dst, via)| dst == "file:Data Sources/Identifier Catalog.md" && via == "wikilink"
        ),
        "the escaped pipe is markdown, not part of the name: {got:?}"
    );
    assert!(
        !got.iter()
            .any(|(dst, _)| dst.starts_with("missing:identifier")),
        "and it must not be invented as a missing note: {got:?}"
    );
}

#[test]
fn a_note_nobody_wrote_is_kept_once_and_shared_by_everyone_asking() {
    let (_tmp, ws) = vault();
    let engine = index(&ws);

    let missing = engine
        .store()
        .query_nodes(&NodeQuery {
            kinds: vec!["MissingNote".into()],
            ..Default::default()
        })
        .unwrap();
    let mut labels: Vec<&str> = missing.iter().map(|n| n.label.as_str()).collect();
    labels.sort();
    assert_eq!(labels, ["Download Script Trust Audit", "Watch All"]);

    let watch = missing.iter().find(|n| n.label == "Watch All").unwrap();
    assert_eq!(watch.id, NodeId::new("missing:watch-all"));
    assert_eq!(watch.props["unresolved"], true);
    assert!(
        watch.origin.is_none(),
        "asked for by two files, owned by neither"
    );

    // Both files that asked for it point at the one node.
    for from in [
        "Engineering/Ingest Scripts/Index.md",
        "Engineering/Derivations/Index.md",
    ] {
        assert!(
            links(&engine, from)
                .iter()
                .any(|(dst, via)| dst == "missing:watch-all" && via == "unresolved"),
            "{from} asked for Watch All"
        );
    }
}

#[test]
fn a_tag_is_one_node_joining_every_note_that_carries_it() {
    let (_tmp, ws) = vault();
    let engine = index(&ws);

    let tags = engine
        .store()
        .query_nodes(&NodeQuery {
            kinds: vec!["Tag".into()],
            ..Default::default()
        })
        .unwrap();
    let mut ids: Vec<&str> = tags.iter().map(|n| n.id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, ["tag:data-source", "tag:fda", "tag:observability"]);

    let shared = tags
        .iter()
        .find(|n| n.id.as_str() == "tag:data-source")
        .unwrap();
    assert!(
        shared.origin.is_none(),
        "carried by two notes, owned by none"
    );

    // The point of a tag node: it is the one place both notes meet.
    let carriers = engine
        .store()
        .get_edges(&EdgeQuery {
            dst: Some(shared.id.clone()),
            ..Default::default()
        })
        .unwrap();
    let mut paths: Vec<&str> = carriers.iter().map(|e| e.src.path_part()).collect();
    paths.sort();
    assert_eq!(
        paths,
        [
            "Data Sources/Identifier Catalog.md",
            "Data Sources/Refresh Cadences.md"
        ]
    );
    assert!(carriers.iter().all(|e| e.props["via"] == "tag"));
}

#[test]
fn a_tag_nothing_carries_any_more_is_collected() {
    let (_tmp, ws) = vault();
    let mut engine = index(&ws);
    let root = ws.root().to_path_buf();
    let only = NodeId::new("tag:observability");
    let shared = NodeId::new("tag:data-source");

    // Drop both tags from the one note that carried `observability`.
    write(
        &root,
        "Data Sources/Refresh Cadences.md",
        "---\ntags: [data-source]\n---\n# Refresh Cadences\n",
    );
    engine.index_full(false, &mut |_: EngineEvent| {}).unwrap();

    assert!(
        engine.store().get_node(&only).unwrap().is_none(),
        "nothing carries it any more"
    );
    assert!(
        engine.store().get_node(&shared).unwrap().is_some(),
        "but a tag the other note still carries stays"
    );
}

#[test]
fn a_placeholder_survives_one_file_dropping_it_and_dies_with_the_last() {
    let (_tmp, ws) = vault();
    let mut engine = index(&ws);
    let root = ws.root().to_path_buf();
    let id = NodeId::new("missing:watch-all");

    // One of the two files stops asking. The other still does.
    write(
        &root,
        "Engineering/Derivations/Index.md",
        "# Index\n\nBack to [[Index]].\n",
    );
    engine.index_full(false, &mut |_: EngineEvent| {}).unwrap();
    assert!(
        engine.store().get_node(&id).unwrap().is_some(),
        "one file still links to it"
    );

    // Now the last one does too.
    write(
        &root,
        "Engineering/Ingest Scripts/Index.md",
        "# Index\n\nSee [[Cadences]].\n",
    );
    engine.index_full(false, &mut |_: EngineEvent| {}).unwrap();
    assert!(
        engine.store().get_node(&id).unwrap().is_none(),
        "nothing asks for it any more, so it is collected"
    );
    // The note that still resolves is untouched by the sweep.
    assert!(
        engine
            .store()
            .get_node(&NodeId::file("Data Sources/Refresh Cadences.md"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn the_same_title_in_two_folders_resolves_to_the_one_next_door() {
    let (_tmp, ws) = vault();
    let engine = index(&ws);

    // Two notes are called `Index`. `Derivations/Index.md` links to [[Index]],
    // and the only sane answer is itself — which is a self-link, so it is
    // dropped rather than pointing at the other folder's Index.
    let got = links(&engine, "Engineering/Derivations/Index.md");
    assert!(
        !got.iter()
            .any(|(dst, _)| dst == "file:Engineering/Ingest Scripts/Index.md"),
        "a bare [[Index]] must not jump folders: {got:?}"
    );
}
