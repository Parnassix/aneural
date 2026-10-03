//! Workspace resource: config, node type styles.

use crate::theme;
use aneural_core::config::{Config, NodeTypeDef};
use bevy::prelude::*;
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct KindStyle {
    pub color: Color,
    pub icon: &'static str,
    pub shape: String,
    pub label: String,
}

#[derive(Resource)]
pub struct WorkspaceRes {
    pub ws: aneural_core::Workspace,
    pub config: Config,
    pub node_types: Vec<NodeTypeDef>,
    pub styles: HashMap<String, KindStyle>,
}

impl WorkspaceRes {
    pub fn new(ws: aneural_core::Workspace, config: Config, node_types: Vec<NodeTypeDef>) -> Self {
        let mut styles = HashMap::new();
        for def in &node_types {
            let icon = aneural_icons::names()
                .find(|n| *n == def.icon)
                .unwrap_or(aneural_icons::FALLBACK_ICON);
            styles.insert(
                def.kind.clone(),
                KindStyle {
                    color: theme::hex_or(&def.color, theme::FALLBACK_NODE),
                    icon,
                    shape: def.shape.clone(),
                    label: if def.label.is_empty() {
                        def.kind.clone()
                    } else {
                        def.label.clone()
                    },
                },
            );
        }
        WorkspaceRes {
            ws,
            config,
            node_types,
            styles,
        }
    }

    pub fn style(&self, kind: &str) -> KindStyle {
        self.styles.get(kind).cloned().unwrap_or(KindStyle {
            color: theme::hex(theme::FALLBACK_NODE),
            icon: aneural_icons::FALLBACK_ICON,
            shape: "circle".into(),
            label: kind.to_string(),
        })
    }

    pub fn name(&self) -> String {
        if self.config.name.is_empty() {
            self.ws
                .root()
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "workspace".into())
        } else {
            self.config.name.clone()
        }
    }

    /// Kinds in a stable display order: builtins first, then spore/workspace kinds.
    pub fn kinds(&self) -> Vec<String> {
        self.node_types.iter().map(|d| d.kind.clone()).collect()
    }
}

pub fn node_radius(kind: &str) -> f32 {
    match kind {
        "Repo" => 16.0,
        "Directory" => 11.0,
        "Manifest" => 10.0,
        "File" | "Package" => 8.0,
        // A tag is a label many files share, and a vault has a hundred of
        // them; a note nobody wrote is smaller still than a note.
        "Tag" => 6.0,
        // A schedule and a run are satellites of the script they are about, and
        // a repository of scripts has one of each per script; at 7.0 they
        // out-shout the file tree they hang off.
        "Schedule" | "Run" => 6.0,
        "MissingNote" => 5.0,
        _ => 7.0,
    }
}
