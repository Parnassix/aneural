//! Every builtin node type must render.
//!
//! `aneural-core` names icons as strings and `aneural-icons` holds the table;
//! neither depends on the other, so nothing inside either crate can check that
//! a name resolves. This crate depends on both, so the check lives here — and
//! it is derived from the registry rather than hand-listed, because the
//! hand-maintained list in `aneural-icons` had already drifted behind
//! `VsSourceControl` and `LuGauge` by the time anyone looked.

use aneural_core::config::builtin_node_types;

#[test]
fn every_builtin_node_type_has_an_icon_that_exists() {
    for def in builtin_node_types() {
        assert!(
            aneural_icons::is_valid(&def.icon),
            "node kind {} names icon {}, which is not in the curated table",
            def.kind,
            def.icon
        );
    }
}

#[test]
fn every_builtin_node_type_has_a_shape_that_draws() {
    // `shape_mesh` falls back to a circle for anything it does not know, so a
    // typo would silently round every node off instead of failing.
    for def in builtin_node_types() {
        assert!(
            matches!(
                def.shape.as_str(),
                "circle" | "hexagon" | "pill" | "square" | "diamond"
            ),
            "node kind {} names shape {}, which would silently render as a circle",
            def.kind,
            def.shape
        );
    }
}

/// A spore that emits a builtin kind declares it too, so its own style wins
/// while it is enabled — that is what `merge_node_type` replacing wholesale is
/// for, and `plans` has done it since it was written. The cost is two copies of
/// one style, and nothing stops them drifting apart, which would make a node
/// change colour depending on whether a spore happens to be on.
#[test]
fn a_spore_declaring_a_builtin_kind_declares_it_identically() {
    let builtin = builtin_node_types();
    for (name, json) in aneural_engine::spores::BUILTIN_SPORES {
        let manifest: aneural_core::spore::SporeManifest =
            serde_json::from_str(json).unwrap_or_else(|e| panic!("{name}: {e}"));
        for declared in &manifest.node_types {
            let Some(def) = builtin.iter().find(|d| d.kind == declared.kind) else {
                continue; // a kind of the spore's own invention
            };
            for (field, ours, theirs) in [
                ("label", &def.label, &declared.label),
                ("icon", &def.icon, &declared.icon),
                ("color", &def.color, &declared.color),
                ("shape", &def.shape, &declared.shape),
                ("description", &def.description, &declared.description),
            ] {
                assert_eq!(
                    ours, theirs,
                    "spore {name} declares {} with a different {field} than the builtin, \
                     so the kind would change appearance when the spore is toggled",
                    declared.kind
                );
            }
        }
    }
}
