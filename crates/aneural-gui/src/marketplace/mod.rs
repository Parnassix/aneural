//! The Open Spores Marketplace, as a modal over the graph.
//!
//! Not a docked panel: `ee00a1c` removed the bottom spores panel precisely
//! because a permanent 170px strip obstructed the mycelium. A modal costs
//! nothing at all when closed, which is almost always.

pub mod worker;

use crate::theme;
use crate::workspace::WorkspaceRes;
use aneural_core::spore::SporeInfo;
use aneural_engine::EngineCommand;
use bevy::prelude::*;
use bevy_egui::{EguiContexts, EguiPrimaryContextPass, egui};
use worker::{ConsentPlan, Listing, RegistryCommand, RegistryEvent, RegistryRx, RegistryTx};

/// What the engine says is installed. Pushed over the event channel, because the
/// GUI holds no `Engine` of its own — the engine thread owns the only one.
#[derive(Resource, Default)]
pub struct SporesRes {
    pub spores: Vec<SporeInfo>,
    pub errors: Vec<String>,
}

impl SporesRes {
    pub fn installed(&self, id: &str) -> Option<&SporeInfo> {
        self.spores.iter().find(|s| s.id == id)
    }
}

/// The marketplace's states really are mutually exclusive, unlike indexing's
/// (which can be busy *and* watching), so this is an enum where `IndexStatus`
/// is a set of flags. Not an inconsistency to tidy up.
#[derive(Default, PartialEq)]
pub enum MarketState {
    #[default]
    Idle,
    Loading,
    Loaded,
    Error(String),
}

#[derive(Resource, Default)]
pub struct Marketplace {
    pub open: bool,
    pub query: String,
    pub state: MarketState,
    pub listings: Vec<Listing>,
    pub statuses: Vec<aneural_registry::RegistryStatus>,
    pub selected: Option<String>,
    pub consent: Option<ConsentPlan>,
    pub busy: Option<String>,
    pub notice: Option<(String, bool)>,
    /// Written by the UI, sent by an `Update` system next frame — the deferred
    /// house style from `switch.rs`, so no render pass ever does IO.
    pub pending: Vec<RegistryCommand>,
    /// In-progress edits of a spore's settings, keyed `"<id>/<key>"`. Held here
    /// rather than written per keystroke: each save rewrites the config file.
    pub setting_drafts: std::collections::HashMap<String, String>,
}

pub struct MarketplacePlugin;

impl Plugin for MarketplacePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Marketplace>()
            .init_resource::<SporesRes>()
            .add_systems(Startup, spawn_registry)
            .add_systems(PreUpdate, drain_registry_events)
            .add_systems(Update, send_pending)
            .add_systems(EguiPrimaryContextPass, modal)
            .add_systems(Last, stop_on_exit);
    }
}

fn spawn_registry(mut commands: Commands, ws: Res<WorkspaceRes>) {
    let (rx, tx) = worker::start_registry(ws.ws.root());
    commands.insert_resource(rx);
    commands.insert_resource(tx);
}

fn drain_registry_events(
    rx: Res<RegistryRx>,
    mut market: ResMut<Marketplace>,
    engine_tx: Res<crate::engine::EngineTx>,
) {
    while let Ok(ev) = rx.0.try_recv() {
        match ev {
            RegistryEvent::Statuses(statuses) => {
                // One dead private index must not hide a working official one.
                if statuses.iter().all(|s| !s.ok) && !statuses.is_empty() {
                    let msg = statuses
                        .iter()
                        .filter_map(|s| s.error.clone())
                        .next()
                        .unwrap_or_else(|| "no registry could be reached".into());
                    market.state = MarketState::Error(msg);
                }
                market.statuses = statuses;
            }
            RegistryEvent::Listings(listings) => {
                if market.state != MarketState::Loaded {
                    market.state = MarketState::Loaded;
                }
                if market.selected.is_none() {
                    market.selected = listings.first().map(|l| l.id.clone());
                }
                market.listings = listings;
            }
            RegistryEvent::Planned(plan) => {
                market.busy = None;
                market.consent = Some(*plan);
            }
            RegistryEvent::Installed { id, version } => {
                market.busy = None;
                market.consent = None;
                market.notice = Some((format!("installed {id} {version}"), false));
                let _ = engine_tx.0.send(EngineCommand::ReloadSpores);
            }
            RegistryEvent::SettingSaved { id, key } => {
                market.busy = None;
                market.notice = Some((format!("saved {key}"), false));
                // Reloading re-reads the config, which clears the refresh
                // schedule, so the spore fetches with the new value at once.
                let _ = engine_tx.0.send(EngineCommand::ReloadSpores);
                let _ = engine_tx.0.send(EngineCommand::RefreshHttp {
                    id: Some(id),
                    force: true,
                });
            }
            RegistryEvent::EnabledSaved { id, on } => {
                market.busy = None;
                market.notice = Some((
                    match on {
                        true => format!("enabled {id}"),
                        false => format!("disabled {id}"),
                    },
                    false,
                ));
                // Now that it is written, converge the graph to match.
                let _ = engine_tx.0.send(EngineCommand::SetSporeEnabled { id, on });
            }
            RegistryEvent::Uninstalled(id) => {
                market.busy = None;
                market.notice = Some((format!("removed {id}"), false));
                let _ = engine_tx.0.send(EngineCommand::ReloadSpores);
            }
            RegistryEvent::Failed { op, message } => {
                market.busy = None;
                market.consent = None;
                market.notice = Some((format!("{op} failed: {message}"), true));
            }
        }
    }
}

fn stop_on_exit(mut exits: MessageReader<AppExit>, tx: Option<Res<RegistryTx>>) {
    if exits.read().next().is_some()
        && let Some(tx) = tx
    {
        let _ = tx.0.send(RegistryCommand::Stop);
    }
}

fn send_pending(mut market: ResMut<Marketplace>, tx: Res<RegistryTx>) {
    for cmd in std::mem::take(&mut market.pending) {
        let _ = tx.0.send(cmd);
    }
}

/// Display-only mirror of `EnvSecrets::var_name`, so the sheet can name the
/// exact variable the engine will look for.
fn env_name(secret: &str) -> String {
    aneural_core::net::EnvSecrets::var_name(secret)
        .strip_prefix("ANEURAL_SECRET_")
        .unwrap_or_default()
        .to_string()
}

fn tier_color(tier: &str, p: &theme::Palette) -> egui::Color32 {
    // Anything past `declarative` runs with capabilities the user must grant.
    match tier {
        "declarative" => theme::egui_color(p.accent),
        "http" => theme::egui_color(p.selection),
        _ => theme::egui_color(p.warning),
    }
}

/// A small outlined badge. Shared with the plan drawer, which labels a
/// plan's state the same way the marketplace labels a tier.
pub fn chip(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.20))
        .stroke(egui::Stroke::new(1.0, color))
        .corner_radius(3.0)
        .inner_margin(egui::Margin::symmetric(5, 1))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).small().color(color));
        });
}

#[allow(clippy::too_many_arguments)]
fn modal(
    mut contexts: EguiContexts,
    mut market: ResMut<Marketplace>,
    spores: Res<SporesRes>,
    ws: Res<WorkspaceRes>,
    vibe: Res<crate::circadian::Vibe>,
    engine_tx: Res<crate::engine::EngineTx>,
) {
    if !market.open {
        return;
    }
    let Ok(ctx) = contexts.ctx_mut() else { return };
    let palette = vibe.palette;

    let modal = egui::Modal::new(egui::Id::new("marketplace")).show(ctx, |ui| {
        ui.set_width(880.0);
        ui.set_height(540.0);

        ui.horizontal(|ui| {
            ui.heading("Spores");
            ui.label(
                egui::RichText::new("grow the graph — nothing to maintain")
                    .weak()
                    .small(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("×").on_hover_text("Close").clicked() {
                    market.open = false;
                }
                if ui.small_button("⟳").on_hover_text("Refresh").clicked() {
                    market.state = MarketState::Loading;
                    market
                        .pending
                        .push(RegistryCommand::Refresh { force: true });
                }
            });
        });

        if let Some((text, is_error)) = market.notice.clone() {
            let color = if is_error {
                theme::egui_color(palette.warning)
            } else {
                theme::egui_color(palette.accent)
            };
            ui.label(egui::RichText::new(text).small().color(color));
        }

        // A registry that failed while others worked is reported inline, never
        // by blanking the list.
        for status in market.statuses.clone().iter().filter(|s| !s.ok) {
            ui.label(
                egui::RichText::new(format!(
                    "⚠ {} unreachable — {}",
                    status.name,
                    status.error.clone().unwrap_or_default()
                ))
                .small()
                .color(theme::egui_color(palette.warning)),
            );
        }

        ui.separator();

        match &market.state {
            MarketState::Loading if market.listings.is_empty() => {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::Spinner::new()
                            .size(12.0)
                            .color(theme::egui_color(palette.accent)),
                    );
                    ui.label(egui::RichText::new("reaching the registries").weak());
                });
                return;
            }
            MarketState::Error(err) if market.listings.is_empty() => {
                ui.label(
                    egui::RichText::new(format!("⚠ {err}"))
                        .color(theme::egui_color(palette.warning)),
                );
                ui.label(
                    egui::RichText::new("check `spores.registries` in .aneural/config.json")
                        .weak()
                        .small(),
                );
                return;
            }
            _ => {}
        }

        ui.horizontal_top(|ui| {
            results_list(ui, &mut market, &spores, &palette);
            ui.separator();
            detail(ui, &mut market, &spores, &ws, &palette, &engine_tx);
        });
    });

    if modal.should_close() {
        market.open = false;
    }

    if let Some(plan) = market.consent.clone() {
        consent_sheet(ctx, &mut market, &plan, &ws, &palette);
    }
}

fn results_list(
    ui: &mut egui::Ui,
    market: &mut Marketplace,
    spores: &SporesRes,
    palette: &theme::Palette,
) {
    ui.vertical(|ui| {
        ui.set_width(300.0);
        ui.add(
            egui::TextEdit::singleline(&mut market.query)
                .hint_text("search spores")
                .desired_width(f32::INFINITY),
        );
        ui.add_space(4.0);

        let query = market.query.to_lowercase();
        let matching: Vec<Listing> = market
            .listings
            .iter()
            .filter(|l| {
                query.is_empty()
                    || l.id.to_lowercase().contains(&query)
                    || l.display_name.to_lowercase().contains(&query)
                    || l.description.to_lowercase().contains(&query)
            })
            .cloned()
            .collect();

        if matching.is_empty() {
            ui.label(egui::RichText::new("nothing found").weak().small());
            return;
        }

        egui::ScrollArea::vertical()
            .id_salt("marketplace-results")
            .show(ui, |ui| {
                for listing in matching {
                    let selected = market.selected.as_deref() == Some(listing.id.as_str());
                    let installed = spores.installed(&listing.id).is_some();
                    let response = egui::Frame::new()
                        .fill(if selected {
                            theme::egui_color(palette.hover)
                        } else {
                            egui::Color32::TRANSPARENT
                        })
                        .corner_radius(4.0)
                        .inner_margin(6.0)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(&listing.display_name).strong());
                                if installed {
                                    chip(ui, "installed", theme::egui_color(palette.accent));
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{} · v{}",
                                        listing.id, listing.version
                                    ))
                                    .weak()
                                    .small(),
                                );
                                chip(ui, &listing.publisher, theme::egui_color(palette.dim));
                                chip(ui, &listing.tier, tier_color(&listing.tier, palette));
                            });
                        })
                        .response;
                    if response.interact(egui::Sense::click()).clicked() {
                        market.selected = Some(listing.id.clone());
                    }
                }
            });
    });
}

fn detail(
    ui: &mut egui::Ui,
    market: &mut Marketplace,
    spores: &SporesRes,
    ws: &WorkspaceRes,
    palette: &theme::Palette,
    engine_tx: &crate::engine::EngineTx,
) {
    ui.vertical(|ui| {
        let Some(id) = market.selected.clone() else {
            ui.label(egui::RichText::new("Pick a spore to see what it would add.").weak());
            return;
        };
        let Some(listing) = market.listings.iter().find(|l| l.id == id).cloned() else {
            return;
        };
        let installed = spores.installed(&id).cloned();

        egui::ScrollArea::vertical()
            .id_salt("marketplace-detail")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(&listing.display_name)
                            .strong()
                            .size(16.0),
                    );
                    chip(ui, &listing.tier, tier_color(&listing.tier, palette));
                });
                ui.label(
                    egui::RichText::new(format!(
                        "{} · v{} · from {}",
                        listing.id, listing.version, listing.registry
                    ))
                    .weak()
                    .small(),
                );
                if !listing.also_in.is_empty() {
                    ui.label(
                        egui::RichText::new(format!(
                            "also listed in {}",
                            listing.also_in.join(", ")
                        ))
                        .weak()
                        .small(),
                    );
                }
                if !listing.first_party {
                    ui.label(
                        egui::RichText::new("unverified third-party code")
                            .small()
                            .color(theme::egui_color(palette.warning)),
                    );
                }
                ui.add_space(6.0);
                ui.label(&listing.description);
                ui.add_space(8.0);

                if !listing.node_kinds.is_empty() {
                    ui.label(egui::RichText::new("Adds").strong().small());
                    ui.horizontal_wrapped(|ui| {
                        for kind in &listing.node_kinds {
                            // Colour the chip from the real kind style when the
                            // spore is installed, so it matches the canvas.
                            let color = theme::egui_color(ws.style(kind).color);
                            chip(ui, kind, color);
                        }
                    });
                    ui.add_space(8.0);
                }

                // A spore that needs a value cannot do anything until it has
                // one, so the form lives right here rather than in the CLI.
                if let Some(info) = &installed
                    && !info.settings.is_empty()
                {
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("Settings").strong().small());
                    for setting in &info.settings {
                        let slot = format!("{id}/{}", setting.key);
                        let draft = market
                            .setting_drafts
                            .entry(slot.clone())
                            .or_insert_with(|| setting.value.clone().unwrap_or_default());
                        let mut save = false;
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(&setting.label)
                                    .small()
                                    .color(theme::egui_color(palette.dim)),
                            );
                            let hint = setting.example.clone().unwrap_or_default();
                            let edit = ui.add(
                                egui::TextEdit::singleline(draft)
                                    .hint_text(hint)
                                    .desired_width(180.0),
                            );
                            save =
                                edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                            if ui.button("Save").clicked() {
                                save = true;
                            }
                            if setting.required && setting.value.is_none() {
                                chip(ui, "needed", theme::egui_color(palette.warning));
                            }
                        });
                        if !setting.description.is_empty() {
                            ui.label(egui::RichText::new(&setting.description).weak().small());
                        }
                        if save {
                            let value = draft.clone();
                            market.busy = Some(format!("saving {}", setting.key));
                            market.pending.push(RegistryCommand::SetSetting {
                                id: id.clone(),
                                key: setting.key.clone(),
                                value,
                            });
                        }
                    }
                }

                ui.horizontal(|ui| {
                    match &installed {
                        None => {
                            if ui.button("Install").clicked() {
                                market.busy = Some(format!("planning {id}"));
                                market.pending.push(RegistryCommand::Plan(id.clone()));
                            }
                        }
                        Some(info) => {
                            let mut on = info.enabled;
                            if ui.checkbox(&mut on, "Enabled").changed() {
                                // Through the worker, not straight at the
                                // engine: the engine converges the graph but
                                // does not write the config, so a direct send
                                // reverted on restart. The graph follows once
                                // the write lands, in `EnabledSaved`.
                                market.busy = Some(match on {
                                    true => format!("enabling {id}"),
                                    false => format!("disabling {id}"),
                                });
                                market
                                    .pending
                                    .push(RegistryCommand::SetEnabled { id: id.clone(), on });
                            }
                            // Only a spore that reads an API has anything to
                            // re-fetch; the rest converge through the watcher.
                            if info.tier != "declarative" && ui.button("Refresh now").clicked() {
                                let _ = engine_tx.0.send(EngineCommand::RefreshHttp {
                                    id: Some(id.clone()),
                                    force: true,
                                });
                            }
                            // A builtin has nothing on disk to remove; it is
                            // only ever enabled or disabled.
                            if info.location == "workspace" && ui.button("Remove").clicked() {
                                market.busy = Some(format!("removing {id}"));
                                market.pending.push(RegistryCommand::Uninstall(id.clone()));
                            }
                        }
                    }
                    if let Some(busy) = market.busy.clone() {
                        ui.add(
                            egui::Spinner::new()
                                .size(10.0)
                                .color(theme::egui_color(palette.accent)),
                        );
                        ui.label(egui::RichText::new(busy).weak().small());
                    }
                });
            });
    });
}

fn consent_sheet(
    ctx: &egui::Context,
    market: &mut Marketplace,
    plan: &ConsentPlan,
    ws: &WorkspaceRes,
    palette: &theme::Palette,
) {
    let mut accepted = false;
    let mut cancelled = false;

    let sheet = egui::Modal::new(egui::Id::new("marketplace-consent")).show(ctx, |ui| {
        ui.set_width(520.0);
        ui.horizontal(|ui| {
            ui.heading(&plan.display_name);
            chip(ui, &plan.tier, tier_color(&plan.tier, palette));
        });
        if !plan.description.is_empty() {
            ui.label(&plan.description);
        }
        ui.label(
            egui::RichText::new(match &plan.previous_version {
                Some(prev) => format!("{} · {prev} → v{}", plan.id, plan.version),
                None => format!("{} · v{}", plan.id, plan.version),
            })
            .weak()
            .small(),
        );

        if !plan.first_party {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("This is unverified third-party code.")
                    .strong()
                    .color(theme::egui_color(palette.warning)),
            );
        }

        ui.add_space(8.0);
        ui.label(egui::RichText::new("It will be allowed to:").strong());
        for line in &plan.consent_lines {
            ui.label(format!("  · {line}"));
        }

        // A spore can be perfectly safe and still do nothing until it is told
        // *whose* data to read, so the sheet says that here rather than leaving
        // the user to discover an empty graph.
        if !plan.settings.is_empty() {
            ui.add_space(8.0);
            ui.label(egui::RichText::new("You will need to set:").strong());
            for (key, example, answered) in &plan.settings {
                ui.horizontal(|ui| {
                    ui.label(format!("  · {key}"));
                    if *answered {
                        chip(ui, "set", theme::egui_color(palette.accent));
                    } else if !example.is_empty() {
                        ui.label(
                            egui::RichText::new(format!("e.g. {example}"))
                                .weak()
                                .small(),
                        );
                    }
                });
            }
        }

        if !plan.missing_secrets.is_empty() {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Credentials it needs, which are not set:")
                    .strong()
                    .color(theme::egui_color(palette.warning)),
            );
            for name in &plan.missing_secrets {
                ui.label(
                    egui::RichText::new(format!("  · ANEURAL_SECRET_{}", env_name(name)))
                        .weak()
                        .small(),
                );
            }
        }

        if !plan.node_kinds.is_empty() {
            ui.add_space(8.0);
            ui.label(egui::RichText::new("It will add:").strong());
            ui.horizontal_wrapped(|ui| {
                for kind in &plan.node_kinds {
                    chip(ui, kind, theme::egui_color(ws.style(kind).color));
                }
            });
        }

        ui.add_space(8.0);
        ui.label(egui::RichText::new("Where it came from:").strong());
        ui.label(
            egui::RichText::new(format!("  {}", plan.registry))
                .weak()
                .small(),
        );
        ui.label(
            egui::RichText::new(format!("  {}", plan.source))
                .weak()
                .small(),
        );

        if let Some(readme) = &plan.readme {
            ui.add_space(8.0);
            ui.collapsing("README", |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("consent-readme")
                    .max_height(160.0)
                    .show(ui, |ui| {
                        crate::markdown::body(ui, readme, palette, &|_| false);
                    });
            });
        }

        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(aneural_registry::DISCLAIMER)
                .weak()
                .small(),
        );
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            if ui.button("Install and enable").clicked() {
                accepted = true;
            }
            if ui.button("Cancel").clicked() {
                cancelled = true;
            }
        });
    });

    if accepted {
        market.busy = Some(format!("installing {}", plan.id));
        market
            .pending
            .push(RegistryCommand::Install(plan.id.clone()));
        market.consent = None;
    } else if cancelled || sheet.should_close() {
        market.consent = None;
    }
}
