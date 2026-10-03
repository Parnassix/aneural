//! Where the window opens.
//!
//! This is a preference about a desk, not about a workspace: which monitor the
//! app belongs on, and whether it fills it. So it lives in the user's config
//! directory beside the recent-workspaces list and never in a `.aneural/` — a
//! committed file has no business knowing what is plugged into one machine.
//!
//! `~/.config/aneural/window.json`:
//!
//! ```json
//! { "monitor": "2560x1440", "fullscreen": true }
//! ```
//!
//! `monitor` is matched, ignoring case, anywhere in `<name> <width>x<height>`.
//! The size is part of the label because the name alone cannot be relied on:
//! on macOS winit calls every display `Monitor #<model number>`, so the words
//! on the bezel never appear. Every label is logged at startup when a
//! preference is set, which is where to find the ones to write down.
//!
//! A monitor that is not plugged in is not an error. The laptop has left the
//! desk, so the window stays on whichever screen the system opened it on, and
//! `fullscreen` still applies there.

use bevy::prelude::*;
use bevy::window::{Monitor, MonitorSelection, PrimaryWindow, WindowMode, WindowPosition};

/// How many frames to wait for the system to report its monitors before
/// concluding it is not going to.
const PATIENCE: u32 = 120;

#[derive(Resource, serde::Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct Placement {
    /// Part of a monitor's label. `None` means whichever one the window is on.
    monitor: Option<String>,
    fullscreen: bool,
}

impl Placement {
    fn load() -> Self {
        let Some(path) = crate::switch::user_config_dir().map(|d| d.join("window.json")) else {
            return Placement::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Placement::default();
        };
        serde_json::from_str(&text).unwrap_or_else(|e| {
            warn!("{}: {e}", path.display());
            Placement::default()
        })
    }

    fn asks_for_anything(&self) -> bool {
        self.monitor.is_some() || self.fullscreen
    }
}

/// What a monitor is matched by: `Monitor #4003 2560x1440`.
fn label(monitor: &Monitor) -> String {
    format!(
        "{} {}x{}",
        monitor.name.as_deref().unwrap_or("unnamed"),
        monitor.physical_width,
        monitor.physical_height
    )
}

/// The first label containing `wanted`, whatever the case of either.
fn choose<'a>(wanted: &str, labels: impl IntoIterator<Item = &'a str>) -> Option<usize> {
    let wanted = wanted.trim().to_lowercase();
    if wanted.is_empty() {
        return None;
    }
    labels
        .into_iter()
        .position(|l| l.to_lowercase().contains(&wanted))
}

pub struct PlacementPlugin;

impl Plugin for PlacementPlugin {
    fn build(&self, app: &mut App) {
        let placement = Placement::load();
        if placement.asks_for_anything() {
            app.insert_resource(placement).add_systems(Update, place);
        }
    }
}

/// Runs until it has acted once. It cannot be a startup system: the monitors
/// are entities the windowing backend spawns, and they are not there yet.
fn place(
    placement: Res<Placement>,
    monitors: Query<(Entity, &Monitor)>,
    mut windows: Query<&mut Window, With<PrimaryWindow>>,
    mut waited: Local<u32>,
    mut done: Local<bool>,
) {
    if *done {
        return;
    }
    if monitors.is_empty() {
        *waited += 1;
        *done = *waited > PATIENCE;
        return;
    }
    let Ok(mut window) = windows.single_mut() else {
        return;
    };
    *done = true;

    let seen: Vec<(Entity, String)> = monitors.iter().map(|(e, m)| (e, label(m))).collect();
    let target = match &placement.monitor {
        None => MonitorSelection::Current,
        Some(wanted) => match choose(wanted, seen.iter().map(|(_, l)| l.as_str())) {
            Some(i) => MonitorSelection::Entity(seen[i].0),
            None => {
                let labels: Vec<&str> = seen.iter().map(|(_, l)| l.as_str()).collect();
                info!("window.json asks for a monitor matching {wanted:?}; connected: {labels:?}");
                MonitorSelection::Current
            }
        },
    };
    if placement.fullscreen {
        window.mode = WindowMode::BorderlessFullscreen(target);
    } else if target != MonitorSelection::Current {
        window.position = WindowPosition::Centered(target);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESK: [&str; 3] = [
        "Monitor #4001 5120x1440",
        "Monitor #4002 3456x2234",
        "Monitor #4003 2560x1440",
    ];

    #[test]
    fn a_monitor_is_found_by_its_size_or_its_name() {
        assert_eq!(choose("2560x1440", DESK), Some(2));
        assert_eq!(choose("monitor #4003", DESK), Some(2));
        // the wide one's size merely *ends* the same way, and is not it
        assert_eq!(choose("5120x1440", DESK), Some(0));
    }

    #[test]
    fn a_monitor_that_is_not_plugged_in_matches_nothing() {
        assert_eq!(choose("3840x2160", DESK), None);
        assert_eq!(choose("   ", DESK), None, "blank is not a wildcard");
        assert_eq!(choose("2560x1440", []), None);
    }

    #[test]
    fn a_file_that_says_nothing_asks_for_nothing() {
        let empty: Placement = serde_json::from_str("{}").unwrap();
        assert!(!empty.asks_for_anything());
        let full: Placement =
            serde_json::from_str(r#"{ "monitor": "2560x1440", "fullscreen": true }"#).unwrap();
        assert_eq!(full.monitor.as_deref(), Some("2560x1440"));
        assert!(full.fullscreen);
    }
}
