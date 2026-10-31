//! The fleet: totals for the month, every house and machine, which platforms are covered right
//! now, and savings per house. Without a fleet, a form to join one.

use std::collections::BTreeMap;

use eframe::egui::{self, Color32, RichText, Ui};

use super::App;
use super::data::Snapshot;
use super::widgets::{self, BAD, MUTED, OK, WARN};
use crate::fleet::{FleetPool, SlotCounts};

#[derive(Default)]
pub struct JoinUi {
    url: String,
    key: String,
    house: String,
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
    let mut open = SlotCounts::default();
    for h in hosts.iter().filter(|h| h.online) {
        open.add(&h.slots());
    }
    let m = &view.month;
    let all = &view.all_time;
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
                "Saved all time",
                widgets::usd(all.saved_vs_github_usd),
                format!("{} jobs", all.jobs),
                Some(OK),
            ),
            (
                "Jobs this month",
                m.jobs.to_string(),
                format!(
                    "{:.0}h runner time · power {}",
                    m.runner_secs / 3600.0,
                    widgets::usd(m.own_usd)
                ),
                None,
            ),
            (
                "Open slots",
                format!("{} / {}", open.open(), open.total()),
                format!(
                    "{online} of {} machines online · {} houses",
                    hosts.len(),
                    view.houses.len()
                ),
                None,
            ),
        ],
    );
    if let Some(e) = &snap.fleet_error {
        ui.label(RichText::new(format!("Last refresh failed: {e}")).color(WARN));
    }
    super::active::coverage_gaps(ui, snap);

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
                            "{} machine{} · {} jobs this month",
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
                        )
                        .on_hover_text(format!(
                            "{} saved all time",
                            widgets::usd(house.saved_total_usd)
                        ));
                    });
                });
                ui.separator();
                for host in &house.hosts {
                    let slots = host.slots();
                    ui.horizontal(|ui| {
                        widgets::dot(
                            ui,
                            match (host.online, slots.backoff > 0) {
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
                            ui.label(RichText::new(widgets::usd(host.saved_month_usd)).color(OK))
                                .on_hover_text(format!(
                                    "this month · {} saved all time, {} jobs",
                                    widgets::usd(host.saved_total_usd),
                                    host.jobs_total
                                ));
                            ui.label(
                                RichText::new(if host.online {
                                    format!("v{}", host.version)
                                } else {
                                    format!("seen {}", widgets::ago(host.last_seen))
                                })
                                .size(12.0)
                                .color(MUTED),
                            );
                            if host.online {
                                ui.label(format!(
                                    "{} open · {} busy of {}",
                                    slots.open(),
                                    slots.busy,
                                    slots.total()
                                ));
                            }
                        });
                    });
                    for pool in &host.pools {
                        ui.label(
                            RichText::new(format!("    {}", pool_line(pool, host.online)))
                                .size(11.5)
                                .color(MUTED),
                        );
                    }
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
                    "{} · sharing pool types, slot counts and totals only",
                    s.last_ok_at
                        .map(|t| format!("last report {}", widgets::ago(t)))
                        .unwrap_or_else(|| "not reported yet".into()),
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

/// "macos · tart · macOS ARM64 · 4 CPUs: 1 open, 1 busy, 1 other".
fn pool_line(pool: &FleetPool, online: bool) -> String {
    let s = &pool.slots;
    let counts = if online {
        let other = s.starting + s.backoff + s.paused + s.offline;
        format!("{} open, {} busy, {other} other", s.open(), s.busy)
    } else {
        format!("{} offline", s.total())
    };
    format!(
        "{} · {} · {} {} · {} CPUs: {counts}{}",
        pool.name,
        pool.backend,
        pool.os,
        pool.arch,
        pool.cpus,
        if pool.paused { " (paused)" } else { "" }
    )
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
                let s = &pool.slots;
                // (count, label, colour when the machine is online)
                let phases = [
                    (s.busy, "running", OK),
                    (s.idle, "idle", OK.gamma_multiply(0.35)),
                    (s.backoff, "backoff", WARN),
                    (s.starting, "starting", widgets::INFO.gamma_multiply(0.6)),
                    (s.paused, "paused", widgets::INFO.gamma_multiply(0.6)),
                    (s.offline, "offline", MUTED),
                ];
                for (count, label, color) in phases {
                    for _ in 0..count {
                        let down = !host.online || label == "offline";
                        let state = if host.online { label } else { "offline" };
                        let mut tip = format!("{} · {} · {state}", host.host, pool.name);
                        if pool.paused && host.online {
                            tip += "\npool is paused";
                        }
                        cells.push(((!down).then_some(color), tip));
                    }
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
             report their status and savings to one Cloudflare Worker. Ask whoever runs it for \
             the URL and the enroll key.",
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
                ui.label("Enroll key");
                ui.add(
                    egui::TextEdit::singleline(&mut j.key)
                        .password(true)
                        .desired_width(340.0),
                );
                ui.end_row();
                ui.label("House");
                ui.add(egui::TextEdit::singleline(&mut j.house).hint_text("e.g. Zack's house"));
                ui.end_row();
            });
        ui.add_space(8.0);
        let ready =
            !j.url.trim().is_empty() && !j.key.trim().is_empty() && !j.house.trim().is_empty();
        if ui
            .add_enabled(ready && app.task.is_none(), egui::Button::new("Join fleet"))
            .clicked()
        {
            let (url, key, house) = (
                j.url.trim().to_string(),
                j.key.trim().to_string(),
                j.house.trim().to_string(),
            );
            let config_path = snap.config_path.clone();
            app.spawn_task("Join", move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                let fleet = rt.block_on(crate::fleet::join_and_configure(
                    &config_path,
                    &url,
                    &key,
                    &house,
                ))?;
                Ok(format!(
                    "Joined {fleet:?}. Restart home-runner (Settings → Save & restart) to start reporting."
                ))
            });
        }
    });
}
