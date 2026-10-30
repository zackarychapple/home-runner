//! What is running right now: job cards with elapsed time against the job's usual duration,
//! live CPU and money saved so far, then every runner slot, then the last few finished jobs.

use std::collections::BTreeMap;

use eframe::egui::{self, RichText, Ui, Vec2};

use super::data::Snapshot;
use super::widgets::{self, BAD, MUTED, OK, WARN};
use super::{App, Scope};
use crate::health::{Phase, PoolState, SlotState};
use crate::ledger::JobRecord;
use crate::live::LiveJob;
use crate::timefmt;

/// A running job wherever it is.
struct Running<'a> {
    host: &'a str,
    house: Option<&'a str>,
    pool: &'a PoolState,
    slot: &'a SlotState,
    job: &'a LiveJob,
    /// Only this machine's jobs can be stopped from here.
    local: bool,
}

pub fn show(ui: &mut Ui, app: &mut App, snap: &Snapshot) {
    if !snap.daemon_running() && app.scope == Scope::Host {
        widgets::card(ui, |ui| {
            ui.horizontal(|ui| {
                widgets::dot(ui, BAD);
                ui.label(RichText::new("home-runner isn't running on this machine").strong());
            });
            ui.label(
                RichText::new(
                    "Start it with `home-runner install` (background service) or `home-runner run`. \
                     History and Settings still work.",
                )
                .color(MUTED),
            );
        });
    }

    if app.scope == Scope::Host
        && let Some(state) = snap.state.as_ref().filter(|_| snap.daemon_running())
    {
        supervisor_notices(ui, app, snap, state);
    }

    let mut running = Vec::new();
    let local_pools: Vec<PoolState> = snap
        .state
        .as_ref()
        .filter(|_| snap.daemon_running())
        .map(|s| s.pools())
        .unwrap_or_default();
    match app.scope {
        Scope::Host => {
            let host = snap.state.as_ref().map_or("", |s| s.host.as_str());
            for pool in &local_pools {
                for slot in &pool.slots {
                    if let Some(job) = &slot.job {
                        running.push(Running {
                            host,
                            house: None,
                            pool,
                            slot,
                            job,
                            local: true,
                        });
                    }
                }
            }
        }
        Scope::Fleet => {
            if let Some(view) = &snap.fleet {
                coverage_gaps(ui, snap);
                for house in &view.houses {
                    for host in house.hosts.iter().filter(|h| h.online) {
                        for pool in &host.pools {
                            for slot in &pool.slots {
                                if let Some(job) = &slot.job {
                                    running.push(Running {
                                        host: &host.host,
                                        house: Some(&house.house),
                                        pool,
                                        slot,
                                        job,
                                        local: host.host == snap.host(),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    running.sort_by_key(|r| r.job.started_at);

    widgets::section(
        ui,
        &format!("Running now · {}", running.len()),
        (!running.is_empty()).then(|| "updates every second".into()),
    );
    if running.is_empty() {
        ui.label(RichText::new("No jobs running.").color(MUTED));
    }
    let typical = typical_durations(&snap.ledger);
    let (price, watts) = snap
        .state
        .as_ref()
        .map_or((0.17, 30.0), |s| (s.electricity_usd_per_kwh, s.job_watts));
    for r in &running {
        job_card(ui, app, snap, r, &typical, price, watts);
        ui.add_space(6.0);
    }

    match app.scope {
        Scope::Host => {
            let right = snap
                .state
                .as_ref()
                .filter(|_| snap.daemon_running())
                .map(|s| format!("GitHub answered {}", widgets::ago(s.github_ok_at)));
            widgets::section(ui, "Runner slots", right);
            if !local_pools.is_empty() {
                pools(ui, &local_pools);
            } else {
                ui.label(RichText::new("No live slot information.").color(MUTED));
            }
        }
        Scope::Fleet => {
            if let Some(view) = &snap.fleet {
                for house in &view.houses {
                    for host in &house.hosts {
                        widgets::section(
                            ui,
                            &format!("{} · {}", host.host, house.house),
                            Some(if host.online {
                                "online".into()
                            } else {
                                format!("last seen {}", widgets::ago(host.last_seen))
                            }),
                        );
                        pools(ui, &host.pools);
                    }
                }
            }
        }
    }

    if app.scope == Scope::Host && !snap.ledger.is_empty() {
        widgets::section(ui, "Just finished", None);
        widgets::card(ui, |ui| {
            for r in snap.ledger.iter().rev().take(3) {
                ui.horizontal(|ui| {
                    widgets::dot(ui, widgets::result_color(&r.result));
                    ui.label(&r.result);
                    ui.label(
                        RichText::new(format!(
                            "{} · {}",
                            r.repo.as_deref().unwrap_or("?"),
                            what(r.workflow.as_deref(), r.job.as_deref())
                        ))
                        .strong(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(saved) = r.saved_vs_github_usd {
                            ui.label(RichText::new(widgets::usd(saved)).color(OK));
                        }
                        ui.label(widgets::dur(r.duration_secs));
                        ui.label(RichText::new(widgets::ago(r.finished_at)).color(MUTED));
                    });
                });
            }
        });
    }
}

fn job_card(
    ui: &mut Ui,
    app: &mut App,
    snap: &Snapshot,
    r: &Running,
    typical: &BTreeMap<String, f64>,
    electricity: f64,
    watts: f64,
) {
    let j = r.job;
    let elapsed = (timefmt::now() - j.started_at).max(0) as f64;
    widgets::keep_ticking(ui.ctx());
    widgets::card(ui, |ui| {
        ui.horizontal(|ui| {
            widgets::avatar(ui, j.actor.as_deref().unwrap_or("?"));
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(j.repo.as_deref().unwrap_or("(job starting)")).strong());
                    ui.label(
                        RichText::new(format!(
                            "· {}",
                            what(j.workflow.as_deref(), j.name.as_deref())
                        ))
                        .color(MUTED),
                    );
                });
                let mut meta = Vec::new();
                if let Some(a) = &j.actor {
                    meta.push(a.clone());
                }
                if let Some(e) = &j.event {
                    meta.push(e.clone());
                }
                if let Some(rf) = &j.git_ref {
                    let sha: String = j.sha.as_deref().unwrap_or("").chars().take(7).collect();
                    meta.push(if sha.is_empty() {
                        rf.clone()
                    } else {
                        format!("{rf}@{sha}")
                    });
                }
                if !meta.is_empty() {
                    ui.label(RichText::new(meta.join(" · ")).size(12.0).color(MUTED));
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                let place = match r.house {
                    Some(house) => format!("{} · {house} · {}", r.host, r.pool.name),
                    None => r.pool.name.clone(),
                };
                widgets::tag(ui, place);
            });
        });
        ui.add_space(6.0);

        ui.columns(3, |cols| {
            // Elapsed against this job's usual duration.
            let key = typical_key(j.repo.as_deref(), j.workflow.as_deref(), j.name.as_deref());
            let usual = typical.get(&key).copied();
            let ui = &mut cols[0];
            ui.horizontal(|ui| {
                ui.label(RichText::new("Elapsed").size(11.5).color(MUTED));
                ui.label(RichText::new(widgets::dur(elapsed)).size(16.0).strong());
            });
            match usual {
                Some(usual) => {
                    let over = elapsed > usual;
                    widgets::progress(
                        ui,
                        (elapsed / usual) as f32,
                        if over { WARN } else { OK },
                        ui.available_width(),
                    );
                    ui.label(
                        RichText::new(if over {
                            format!(
                                "over its usual {} by {}",
                                widgets::dur(usual),
                                widgets::dur(elapsed - usual)
                            )
                        } else {
                            format!(
                                "usually {} · about {} left",
                                widgets::dur(usual),
                                widgets::dur(usual - elapsed)
                            )
                        })
                        .size(11.5)
                        .color(MUTED),
                    );
                }
                None => {
                    ui.label(
                        RichText::new("no earlier runs to compare with")
                            .size(11.5)
                            .color(MUTED),
                    );
                }
            }

            let ui = &mut cols[1];
            let now = j.cpu.last().copied();
            ui.label(
                RichText::new(format!(
                    "CPU · {} cores{}",
                    r.pool.cpus,
                    now.map(|v| format!(" · {:.0}%", v * 100.0))
                        .unwrap_or_default()
                ))
                .size(11.5)
                .color(MUTED),
            );
            widgets::sparkline(ui, &j.cpu, Vec2::new(ui.available_width().min(180.0), 26.0))
                .on_hover_text(if r.pool.backend == "host" || r.pool.backend == "tart" {
                    "Machine-wide CPU (includes anything else running)"
                } else {
                    "CPU inside the job's VM or container"
                });

            let ui = &mut cols[2];
            ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
                ui.label(RichText::new("Saved so far").size(11.5).color(MUTED));
                match r.pool.github_usd_per_minute {
                    Some(rate) => {
                        let billed = (elapsed / 60.0).ceil().max(1.0);
                        let own = watts * elapsed / 3600.0 / 1000.0 * electricity;
                        ui.label(
                            RichText::new(widgets::usd(billed * rate - own))
                                .size(16.0)
                                .strong()
                                .color(OK),
                        );
                        if let Some(label) = &r.pool.github_label {
                            ui.label(
                                RichText::new(format!(
                                    "vs {}",
                                    label.trim_start_matches("GitHub ")
                                ))
                                .size(11.5)
                                .color(MUTED),
                            );
                        }
                    }
                    None => {
                        ui.label(RichText::new("-").color(MUTED));
                    }
                }
            });
        });

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if let Some(url) = &j.url
                && ui.button("Open run").clicked()
            {
                ui.ctx().open_url(egui::OpenUrl::new_tab(url));
            }
            if r.local
                && ui
                    .button("Logs")
                    .on_hover_text("Open the log folder")
                    .clicked()
            {
                widgets::reveal(&snap.data_dir.join("logs"));
            }
            if r.local
                && let Some(runner) = &r.slot.runner
            {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(RichText::new("Stop job").color(BAD)).clicked() {
                        let what = format!(
                            "{} · {}",
                            j.repo.as_deref().unwrap_or("This job"),
                            what(j.workflow.as_deref(), j.name.as_deref())
                        );
                        app.confirm_stop = Some((runner.clone(), what));
                    }
                });
            }
        });
    });
}

fn pools(ui: &mut Ui, pools: &[PoolState]) {
    widgets::card(ui, |ui| {
        for (i, pool) in pools.iter().enumerate() {
            if i > 0 {
                ui.separator();
            }
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(150.0);
                    ui.label(RichText::new(&pool.name).strong());
                    ui.label(RichText::new(&pool.backend).size(11.5).color(MUTED));
                    if let Some(hold) = &pool.paused {
                        ui.label(RichText::new("paused").size(11.5).color(WARN))
                            .on_hover_text(hold.describe());
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    ui.set_max_width(ui.available_width() - 260.0);
                    for slot in &pool.slots {
                        slot_pill(ui, slot);
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("[{}]", pool.labels.join(", ")))
                            .monospace()
                            .size(11.5)
                            .color(MUTED),
                    )
                    .on_hover_text("runs-on labels for this pool");
                });
            });
        }
    });
}

fn slot_pill(ui: &mut Ui, slot: &SlotState) {
    let label = match slot.phase {
        Phase::Backoff => match slot.retry_at {
            Some(t) => format!("retry in {}", widgets::dur((t - timefmt::now()) as f64)),
            None => "backoff".into(),
        },
        Phase::Busy => "running".into(),
        p => p.label().into(),
    };
    let color = widgets::phase_color(slot.phase);
    let resp = widgets::pill(ui, label, color, slot.phase != Phase::Idle);
    let mut tip = format!(
        "slot {} · {} for {}",
        slot.slot,
        slot.phase.label(),
        widgets::dur((timefmt::now() - slot.since) as f64)
    );
    if let Some(runner) = &slot.runner {
        tip += &format!("\n{runner}");
    }
    if let Some(error) = &slot.last_error {
        tip += &format!("\n\nLast error: {error}");
    }
    resp.on_hover_text(tip);
}

/// Firing alerts and drain mode, from the supervisor's own health checks.
fn supervisor_notices(ui: &mut Ui, app: &mut App, snap: &Snapshot, state: &crate::health::State) {
    for alert in &state.alerts {
        callout(ui, BAD, |ui| {
            ui.label(RichText::new(alert).strong());
        });
    }
    if !state.drain.is_empty() {
        callout(ui, WARN, |ui| {
            let whole = state.drain.all.as_ref();
            ui.label(
                RichText::new(if whole.is_some() {
                    "Draining.".to_string()
                } else {
                    format!(
                        "Draining {}.",
                        state
                            .drain
                            .pools
                            .keys()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })
                .strong(),
            );
            ui.label("No new jobs start there; running jobs finish.");
            for (scope, hold) in whole
                .map(|h| ("all pools", h))
                .into_iter()
                .chain(state.drain.pools.iter().map(|(p, h)| (p.as_str(), h)))
            {
                ui.label(RichText::new(format!("{scope}: {}", hold.describe())).color(MUTED));
            }
            if ui.button("Resume").clicked() {
                match crate::health::write_drain(&snap.data_dir, &crate::health::Drain::default()) {
                    Ok(()) => app.notify("Resumed: runners will register again", true),
                    Err(e) => app.notify(format!("Couldn't resume: {e}"), false),
                }
            }
        });
    }
}

fn callout(ui: &mut Ui, color: egui::Color32, add: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.12))
        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.5)))
        .corner_radius(6)
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                widgets::dot(ui, color);
                add(ui);
            });
        });
    ui.add_space(6.0);
}

/// Callouts for platforms only offline machines can run, so jobs for them will queue.
fn coverage_gaps(ui: &mut Ui, snap: &Snapshot) {
    let Some(view) = &snap.fleet else { return };
    let mut online = std::collections::HashSet::new();
    let mut offline: BTreeMap<(String, String), (String, String, i64)> = BTreeMap::new();
    for house in &view.houses {
        for host in &house.hosts {
            for pool in &host.pools {
                let key = (pool.os.clone(), pool.arch.clone());
                if host.online {
                    online.insert(key);
                } else {
                    offline.entry(key).or_insert((
                        host.host.clone(),
                        house.house.clone(),
                        host.last_seen,
                    ));
                }
            }
        }
    }
    let gaps: Vec<_> = offline
        .into_iter()
        .filter(|(key, _)| !online.contains(key))
        .collect();
    let (title, detail) = match gaps.as_slice() {
        [] => return,
        [((os, arch), (host, house, seen))] => (
            format!("No {os} {arch} runner is online."),
            format!(
                "Jobs for [self-hosted, {os}, {arch}] will wait in GitHub's queue. {host} ({house}) was last seen {}.",
                widgets::ago(*seen)
            ),
        ),
        many => {
            let platforms: Vec<String> = many
                .iter()
                .map(|((os, arch), _)| format!("{os} {arch}"))
                .collect();
            let mut hosts: Vec<&str> = many.iter().map(|(_, (host, _, _))| host.as_str()).collect();
            hosts.sort_unstable();
            hosts.dedup();
            (
                format!("No runner is online for {}.", platforms.join(", ")),
                format!(
                    "Jobs for these platforms will wait in GitHub's queue until {} come back.",
                    hosts.join(", ")
                ),
            )
        }
    };
    callout(ui, WARN, |ui| {
        ui.label(RichText::new(title).strong());
        ui.label(detail);
    });
}

pub fn what(workflow: Option<&str>, job: Option<&str>) -> String {
    match (workflow, job) {
        (Some(w), Some(j)) => format!("{w} / {j}"),
        (w, j) => w.or(j).unwrap_or("").to_string(),
    }
}

fn typical_key(repo: Option<&str>, workflow: Option<&str>, job: Option<&str>) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}",
        repo.unwrap_or(""),
        workflow.unwrap_or(""),
        job.unwrap_or("")
    )
}

/// Median duration of successful runs per repo/workflow/job, from at least two runs.
pub fn typical_durations(ledger: &[JobRecord]) -> BTreeMap<String, f64> {
    let mut runs: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for r in ledger.iter().filter(|r| r.result == "Succeeded") {
        runs.entry(typical_key(
            r.repo.as_deref(),
            r.workflow.as_deref(),
            r.job.as_deref(),
        ))
        .or_default()
        .push(r.duration_secs);
    }
    runs.into_iter()
        .filter(|(_, v)| v.len() >= 2)
        .map(|(k, mut v)| {
            v.sort_by(f64::total_cmp);
            (k, v[v.len() / 2])
        })
        .collect()
}

/// How a finished job compares with its usual successful duration, e.g. 2.5.
pub fn vs_usual(typical: &BTreeMap<String, f64>, r: &JobRecord) -> Option<f64> {
    let key = typical_key(r.repo.as_deref(), r.workflow.as_deref(), r.job.as_deref());
    typical
        .get(&key)
        .filter(|u| **u > 0.0)
        .map(|u| r.duration_secs / u)
}
