//! The fleet: totals for the month, every house and machine, which platforms are covered right
//! now, and savings per house. Without a fleet, a form to join one.

use std::collections::BTreeMap;

use eframe::egui::{self, Color32, RichText, Ui};

use super::App;
use super::data::Snapshot;
use super::widgets::{self, BAD, MUTED, OK, WARN};
use crate::config::Share;
use crate::state::Phase;

#[derive(Default)]
pub struct JoinUi {
    url: String,
    invite: String,
    house: String,
    share: Share,
}

pub fn show(ui: &mut Ui, app: &mut App, snap: &Snapshot) {
    let Some(config) = &snap.config else {
        ui.label(RichText::new("Fix the config to use the fleet.").color(MUTED));
        return;
    };
    if config.fleet.is_none() {
        join_form(ui, app, snap);
        return;
    }
    let Some(view) = &snap.fleet else {
        widgets::card(ui, |ui| match &snap.fleet_error {
            Some(e) => {
                ui.label(RichText::new("Can't reach the fleet").color(BAD).strong());
                ui.label(e);
            }
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Loading the fleet…");
                });
            }
        });
        return;
    };

    let hosts: Vec<_> = view.houses.iter().flat_map(|h| &h.hosts).collect();
    let online = hosts.iter().filter(|h| h.online).count();
    let slots: usize = hosts
        .iter()
        .filter(|h| h.online)
        .flat_map(|h| &h.pools)
        .map(|p| p.slots.len())
        .sum();
    let m = &view.month;
    widgets::kpis(
        ui,
        vec![
            (
                "Fleet saved this month",
                widgets::usd(m.saved_vs_github_usd),
                format!("vs Depot {}", widgets::usd(m.saved_vs_depot_usd)),
                Some(OK),
            ),
            (
                "Jobs this month",
                m.jobs.to_string(),
                format!("across {} repos", m.repos),
                None,
            ),
            (
                "Runner time",
                format!("{:.0}h", m.runner_secs / 3600.0),
                format!("power cost {}", widgets::usd(m.own_usd)),
                None,
            ),
            (
                "Online",
                format!("{online} / {}", hosts.len()),
                format!("{} houses · {slots} slots", view.houses.len()),
                None,
            ),
        ],
    );
    if let Some(e) = &snap.fleet_error {
        ui.label(RichText::new(format!("Last refresh failed: {e}")).color(WARN));
    }

    ui.add_space(4.0);
    ui.columns(2, |cols| {
        let ui = &mut cols[0];
        widgets::section(ui, "Houses", None);
        for house in &view.houses {
            widgets::card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&house.house).strong());
                    ui.label(
                        RichText::new(format!(
                            "{} machine{} · {} jobs",
                            house.hosts.len(),
                            if house.hosts.len() == 1 { "" } else { "s" },
                            house.jobs_month
                        ))
                        .size(12.0)
                        .color(MUTED),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(widgets::usd(house.saved_month_usd))
                                .strong()
                                .color(OK),
                        );
                    });
                });
                ui.separator();
                for host in &house.hosts {
                    let slots: Vec<_> = host.pools.iter().flat_map(|p| &p.slots).collect();
                    let busy = slots.iter().filter(|s| s.phase == Phase::Busy).count();
                    let backoff = slots.iter().any(|s| s.phase == Phase::Backoff);
                    ui.horizontal(|ui| {
                        widgets::dot(
                            ui,
                            match (host.online, backoff) {
                                (false, _) => MUTED,
                                (true, true) => WARN,
                                (true, false) => OK,
                            },
                        );
                        ui.label(RichText::new(&host.host).strong());
                        ui.label(
                            RichText::new(format!(
                                "{} {} · {} CPUs",
                                host.os, host.arch, host.cpus
                            ))
                            .size(12.0)
                            .color(MUTED),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                RichText::new(if host.online {
                                    format!("v{}", host.version)
                                } else {
                                    format!("seen {}", widgets::ago(host.last_seen))
                                })
                                .size(12.0)
                                .color(MUTED),
                            )
                            .on_hover_text(format!("shares: {}", host.share));
                            if host.online {
                                ui.label(format!("{busy}/{} busy", slots.len()));
                            }
                        });
                    });
                }
            });
            ui.add_space(6.0);
        }

        let ui = &mut cols[1];
        widgets::section(ui, "Coverage", Some("running · idle · offline".into()));
        coverage(ui, snap);

        widgets::section(ui, "Saved by house · this month", None);
        widgets::card(ui, |ui| {
            let max = view
                .houses
                .iter()
                .map(|h| h.saved_month_usd)
                .fold(0.01, f64::max);
            for house in &view.houses {
                ui.horizontal(|ui| {
                    ui.add_sized([110.0, 18.0], egui::Label::new(&house.house).truncate());
                    let w = (ui.available_width() - 70.0).max(40.0);
                    widgets::progress(ui, (house.saved_month_usd / max) as f32, widgets::CHART, w);
                    ui.label(RichText::new(widgets::usd(house.saved_month_usd)).color(OK));
                });
            }
            ui.label(
                RichText::new("Credited to the house whose machine ran the job.")
                    .size(11.5)
                    .color(MUTED),
            );
        });

        widgets::section(ui, "Reporting", None);
        widgets::card(ui, |ui| {
            let status = snap.state.as_ref().and_then(|s| s.fleet.as_ref());
            ui.horizontal(|ui| {
                let color = match status {
                    Some(s) if s.last_error.is_some() => WARN,
                    Some(_) => OK,
                    None => MUTED,
                };
                widgets::dot(ui, color);
                ui.label(
                    RichText::new(config.fleet.as_ref().map_or("", |f| f.url.as_str())).monospace(),
                );
            });
            let line = match status {
                Some(s) => format!(
                    "{} · {} job{} waiting to send · sharing {}",
                    s.last_ok_at
                        .map(|t| format!("last report {}", widgets::ago(t)))
                        .unwrap_or_else(|| "not reported yet".into()),
                    s.pending_records,
                    if s.pending_records == 1 { "" } else { "s" },
                    s.share
                ),
                None if snap.daemon_running() => {
                    "The supervisor hasn't picked up the fleet settings; restart it.".into()
                }
                None => "Reports start when the supervisor is running.".into(),
            };
            ui.label(RichText::new(line).size(12.0).color(MUTED));
            if let Some(e) = status.and_then(|s| s.last_error.as_ref()) {
                ui.label(RichText::new(e).size(12.0).color(WARN));
            }
            if let Some(t) = snap.fleet_at {
                ui.label(
                    RichText::new(format!("This view refreshed {}", widgets::ago(t)))
                        .size(11.5)
                        .color(MUTED),
                );
            }
        });
    });
}

/// One row per platform, one square per runner slot.
fn coverage(ui: &mut Ui, snap: &Snapshot) {
    let Some(view) = &snap.fleet else { return };
    let mut rows: BTreeMap<String, Vec<(Option<Color32>, String)>> = BTreeMap::new();
    for house in &view.houses {
        for host in &house.hosts {
            for pool in &host.pools {
                let key = format!("{} {}", pool.os, pool.arch);
                let cells = rows.entry(key).or_default();
                for slot in &pool.slots {
                    let fill = if !host.online {
                        None
                    } else {
                        Some(match slot.phase {
                            Phase::Busy => OK,
                            Phase::Idle => OK.gamma_multiply(0.35),
                            Phase::Backoff => WARN,
                            _ => widgets::INFO.gamma_multiply(0.6),
                        })
                    };
                    let state = if host.online {
                        slot.phase.label()
                    } else {
                        "offline"
                    };
                    cells.push((fill, format!("{} · {} · {state}", host.host, pool.name)));
                }
            }
        }
    }
    widgets::card(ui, |ui| {
        if rows.is_empty() {
            ui.label(RichText::new("No runners reported yet.").color(MUTED));
        }
        for (platform, cells) in rows {
            ui.horizontal(|ui| {
                ui.add_sized([110.0, 16.0], egui::Label::new(&platform));
                ui.spacing_mut().item_spacing.x = 3.0;
                for (fill, tip) in &cells {
                    widgets::cell(ui, *fill, tip);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let up = cells.iter().filter(|c| c.0.is_some()).count();
                    ui.label(
                        RichText::new(format!("{up} online"))
                            .size(12.0)
                            .color(MUTED),
                    );
                });
            });
        }
    });
}

fn join_form(ui: &mut Ui, app: &mut App, snap: &Snapshot) {
    widgets::section(ui, "Join a fleet", None);
    widgets::card(ui, |ui| {
        ui.label(
            "A fleet is a group of home-runner machines, possibly in different houses, that \
             report their savings and status to one Cloudflare Worker. Ask whoever runs it for \
             the URL and an invite code.",
        );
        ui.add_space(8.0);
        let j = &mut app.join;
        egui::Grid::new("join")
            .num_columns(2)
            .spacing([14.0, 8.0])
            .show(ui, |ui| {
                ui.label("Worker URL");
                ui.add(
                    egui::TextEdit::singleline(&mut j.url)
                        .hint_text("https://fleet.example.workers.dev")
                        .desired_width(340.0),
                );
                ui.end_row();
                ui.label("Invite code");
                ui.add(egui::TextEdit::singleline(&mut j.invite).hint_text("ABCD-EFGH"));
                ui.end_row();
                ui.label("House");
                ui.add(egui::TextEdit::singleline(&mut j.house).hint_text("e.g. Zack's house"));
                ui.end_row();
                ui.label("Share");
                ui.vertical(|ui| share_choice(ui, &mut j.share));
                ui.end_row();
            });
        ui.add_space(8.0);
        let ready =
            !j.url.trim().is_empty() && !j.invite.trim().is_empty() && !j.house.trim().is_empty();
        if ui
            .add_enabled(ready && app.task.is_none(), egui::Button::new("Join fleet"))
            .clicked()
        {
            let (url, invite, house, share) = (
                j.url.trim().to_string(),
                j.invite.trim().to_string(),
                j.house.trim().to_string(),
                j.share,
            );
            let config_path = snap.config_path.clone();
            app.spawn_task("Join", move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                let fleet = rt.block_on(crate::fleet::join_and_configure(
                    &config_path,
                    &url,
                    &invite,
                    &house,
                    share,
                ))?;
                Ok(format!(
                    "Joined {fleet:?}. Restart home-runner (Settings → Save & restart) to start reporting."
                ))
            });
        }
    });
}

pub fn share_choice(ui: &mut Ui, share: &mut Share) {
    for (value, title, detail) in [
        (
            Share::Totals,
            "Totals only",
            "Time, results and dollars. No repo names.",
        ),
        (
            Share::Repos,
            "Totals + repo and workflow names",
            "No people, branches or commits. Private repos can be hashed.",
        ),
        (
            Share::Full,
            "Full job metadata",
            "Adds who triggered it, branch, commit and run links.",
        ),
    ] {
        ui.radio_value(share, value, title);
        ui.label(RichText::new(detail).size(11.5).color(MUTED));
    }
}
