//! The registry lives on its own thread.
//!
//! Exactly the shape `engine.rs` already uses: a `std::thread` plus a pair of
//! crossbeam channels, drained without blocking in `PreUpdate`. `ureq` is
//! synchronous, so there is no runtime here and no reason for one.

use aneural_core::config::SporesConfig;
use aneural_registry::{Federation, Plan, RegistryStatus, search};
use bevy::prelude::*;
use crossbeam_channel::{Receiver, Sender};
use std::path::Path;

/// A listing, flattened for the UI so the render pass never touches the crate's
/// borrowed types.
#[derive(Clone, Debug)]
pub struct Listing {
    pub id: String,
    pub publisher: String,
    pub display_name: String,
    pub description: String,
    pub version: String,
    pub registry: String,
    pub also_in: Vec<String>,
    pub tier: String,
    pub node_kinds: Vec<String>,
    pub first_party: bool,
}

/// What the consent sheet renders. Owned and flat for the same reason.
#[derive(Clone, Debug)]
pub struct ConsentPlan {
    pub id: String,
    pub display_name: String,
    pub description: String,
    pub version: String,
    pub previous_version: Option<String>,
    pub registry: String,
    /// Where the files actually came from, resolved — `repo` alone is `.` for a
    /// registry sitting beside its own spores.
    pub source: String,
    pub tier: String,
    pub consent_lines: Vec<String>,
    /// `(key, example, already answered)` for each value the spore needs before
    /// it can do anything. A capability sheet that did not mention these would
    /// leave the user with a spore that installs and then does nothing.
    pub settings: Vec<(String, String, bool)>,
    /// Declared secrets that are not resolvable in this environment.
    pub missing_secrets: Vec<String>,
    pub node_kinds: Vec<String>,
    pub readme: Option<String>,
    pub first_party: bool,
}

impl ConsentPlan {
    fn from(plan: &Plan, root: &Path) -> Self {
        // What this workspace has already answered, so re-installing does not
        // ask again for something it has.
        let recorded = aneural_core::Workspace::at(root)
            .load_config()
            .map(|c| c.spores.settings_for(&plan.id, plan.manifest.name.as_str()))
            .unwrap_or_default();
        ConsentPlan {
            id: plan.id.clone(),
            display_name: if plan.manifest.display_name.is_empty() {
                plan.id.clone()
            } else {
                plan.manifest.display_name.clone()
            },
            description: plan.manifest.description.clone(),
            version: plan.entry.version.clone(),
            previous_version: plan.previous.clone(),
            registry: plan.registry.clone(),
            source: plan.entry.base_url(),
            tier: plan.tier.label().to_string(),
            consent_lines: plan.consent_lines(),
            settings: plan
                .manifest
                .settings
                .iter()
                .map(|d| {
                    (
                        d.key.clone(),
                        d.example.clone().unwrap_or_default(),
                        recorded.get(&d.key).is_some_and(|v| !v.trim().is_empty()),
                    )
                })
                .collect(),
            missing_secrets: {
                use aneural_core::net::{EnvSecrets, SecretStore};
                let declared: Vec<String> = plan
                    .manifest
                    .capabilities
                    .iter()
                    .flat_map(|c| match c {
                        aneural_core::spore::Capability::Secret { names, .. } => names.clone(),
                        _ => Vec::new(),
                    })
                    .collect();
                EnvSecrets.missing(&declared)
            },
            node_kinds: plan
                .manifest
                .node_types
                .iter()
                .map(|n| n.kind.clone())
                .collect(),
            readme: plan
                .files
                .get(aneural_registry::README_FILE)
                .map(|b| String::from_utf8_lossy(b).to_string()),
            first_party: plan.manifest.is_first_party(),
        }
    }
}

pub enum RegistryCommand {
    Refresh {
        force: bool,
    },
    /// Resolve and verify without writing: this is what the sheet consents to.
    Plan(String),
    /// Commit a plan the user accepted.
    Install(String),
    Uninstall(String),
    /// Record a value a spore declared it needs. Writes the workspace config,
    /// so it goes through the worker like every other write.
    SetSetting {
        id: String,
        key: String,
        value: String,
    },
    /// Turn a spore on or off *durably*. The engine converges the graph by
    /// itself, but only this writes `config.spores.enabled`, so a toggle that
    /// skipped the worker looked like it worked and was lost on restart.
    SetEnabled {
        id: String,
        on: bool,
    },
    Stop,
}

pub enum RegistryEvent {
    Statuses(Vec<RegistryStatus>),
    Listings(Vec<Listing>),
    Planned(Box<ConsentPlan>),
    Installed { id: String, version: String },
    Uninstalled(String),
    SettingSaved { id: String, key: String },
    EnabledSaved { id: String, on: bool },
    Failed { op: &'static str, message: String },
}

#[derive(Resource)]
pub struct RegistryTx(pub Sender<RegistryCommand>);

#[derive(Resource)]
pub struct RegistryRx(pub Receiver<RegistryEvent>);

pub fn start_registry(root: &Path) -> (RegistryRx, RegistryTx) {
    let (ev_tx, ev_rx) = crossbeam_channel::unbounded::<RegistryEvent>();
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<RegistryCommand>();
    let root = root.to_path_buf();
    std::thread::Builder::new()
        .name("aneural-registry".into())
        .spawn(move || serve(&root, ev_tx, cmd_rx))
        .expect("spawn registry thread");
    (RegistryRx(ev_rx), RegistryTx(cmd_tx))
}

/// Rebuild the federation from config on every command. Config can change under
/// us — the marketplace itself writes it — and an index fetch is cached, so
/// there is nothing to gain from holding one open.
fn federation_for(root: &Path) -> aneural_registry::Result<(Federation, SporesConfig)> {
    let config = aneural_core::Workspace::at(root).load_config()?;
    let fed = aneural_registry::federation(&config.spores)?;
    Ok((fed, config.spores))
}

fn serve(root: &Path, tx: Sender<RegistryEvent>, commands: Receiver<RegistryCommand>) {
    while let Ok(cmd) = commands.recv() {
        match cmd {
            RegistryCommand::Stop => return,
            RegistryCommand::Refresh { force } => {
                let Ok((fed, _)) = federation_for(root) else {
                    let _ = tx.send(RegistryEvent::Failed {
                        op: "refresh",
                        message: "could not read the workspace config".into(),
                    });
                    continue;
                };
                let _ = tx.send(RegistryEvent::Statuses(fed.refresh(force)));
                let _ = tx.send(RegistryEvent::Listings(listings(&fed)));
            }
            RegistryCommand::Plan(id) => match plan_for(root, &id) {
                Ok(plan) => {
                    let _ = tx.send(RegistryEvent::Planned(Box::new(plan)));
                }
                Err(e) => {
                    let _ = tx.send(RegistryEvent::Failed {
                        op: "plan",
                        message: e.to_string(),
                    });
                }
            },
            RegistryCommand::Install(id) => match install(root, &id) {
                Ok(version) => {
                    let _ = tx.send(RegistryEvent::Installed { id, version });
                }
                Err(e) => {
                    let _ = tx.send(RegistryEvent::Failed {
                        op: "install",
                        message: e.to_string(),
                    });
                }
            },
            RegistryCommand::SetSetting { id, key, value } => {
                let trimmed = value.trim();
                let write = (!trimmed.is_empty()).then_some(trimmed);
                match aneural_registry::set_setting(root, &id, &key, write) {
                    Ok(_) => {
                        let _ = tx.send(RegistryEvent::SettingSaved { id, key });
                    }
                    Err(e) => {
                        let _ = tx.send(RegistryEvent::Failed {
                            op: "setting",
                            message: e.to_string(),
                        });
                    }
                }
            }
            RegistryCommand::SetEnabled { id, on } => {
                match aneural_registry::set_enabled(root, &id, on) {
                    Ok(_) => {
                        let _ = tx.send(RegistryEvent::EnabledSaved { id, on });
                    }
                    Err(e) => {
                        let _ = tx.send(RegistryEvent::Failed {
                            op: "enable",
                            message: e.to_string(),
                        });
                    }
                }
            }
            RegistryCommand::Uninstall(id) => match aneural_registry::uninstall(root, &id) {
                Ok(()) => {
                    let _ = tx.send(RegistryEvent::Uninstalled(id));
                }
                Err(e) => {
                    let _ = tx.send(RegistryEvent::Failed {
                        op: "uninstall",
                        message: e.to_string(),
                    });
                }
            },
        }
    }
}

fn listings(fed: &Federation) -> Vec<Listing> {
    let indexes = fed.indexes();
    let sources: Vec<search::Source<'_>> = indexes
        .iter()
        .map(|(name, index)| search::Source {
            registry: name,
            entries: &index.spores,
        })
        .collect();
    // An empty query lists everything; filtering happens live in the UI, so the
    // worker is not round-tripped on every keystroke.
    search::search(&sources, "")
        .into_iter()
        .map(|h| Listing {
            id: h.entry.id.clone(),
            publisher: h.entry.publisher().to_string(),
            display_name: if h.entry.display_name.is_empty() {
                h.entry.id.clone()
            } else {
                h.entry.display_name.clone()
            },
            description: h.entry.description.clone(),
            version: h.entry.version.clone(),
            registry: h.registry,
            also_in: h.also_in,
            tier: h.entry.tier().label().to_string(),
            node_kinds: h.entry.node_kinds.clone(),
            first_party: h.entry.first_party,
        })
        .collect()
}

fn plan_for(root: &Path, id: &str) -> aneural_registry::Result<ConsentPlan> {
    let (fed, _) = federation_for(root)?;
    fed.refresh(false);
    Ok(ConsentPlan::from(
        &aneural_registry::plan(root, &fed, id)?,
        root,
    ))
}

fn install(root: &Path, id: &str) -> aneural_registry::Result<String> {
    let (fed, _) = federation_for(root)?;
    fed.refresh(false);
    // Re-resolve and re-verify rather than trusting the plan we showed: a stale
    // plan must never be what actually lands on disk.
    let plan = aneural_registry::plan(root, &fed, id)?;
    let grants = aneural_registry::Grants {
        capabilities: plan.manifest.capabilities.clone(),
    };
    let out = aneural_registry::commit(root, &plan, &grants, true)?;
    Ok(out.version)
}
