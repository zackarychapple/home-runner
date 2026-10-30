//! Finished jobs: totals for the period, saved per day, and a filterable table whose rows open to
//! show what the job would have cost elsewhere and how it compared with its usual run.

use std::collections::BTreeMap;

use eframe::egui::{self, Align, Layout, RichText, Sense, Ui, Vec2};

use super::active::{typical_durations, vs_usual, what};
use super::data::Snapshot;
use super::widgets::{self, BAD, MUTED, OK, SUBTLE};
use super::{App, Scope};
use crate::ledger::JobRecord;
use crate::timefmt;

/// Rows drawn at most; the rest are reachable by filtering.
const MAX_ROWS: usize = 300;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Period {
    Day,
    #[default]
    Week,
    Month,
    All,
}

impl Period {
    fn secs(self) -> Option<i64> {
        match self {
            Period::Day => Some(86400),
            Period::Week => Some(7 * 86400),
            Period::Month => Some(30 * 86400),
            Period::All => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Period::Day => "24h",
            Period::Week => "7d",
            Period::Month => "30d",
            Period::All => "All",
        }
    }
}

#[derive(Default)]
pub struct HistoryUi {
    period: Period,
    search: String,
    result: Option<&'static str>,
    /// Pool (this machine) or house (fleet).
    place: Option<String>,
    open: Option<String>,
}

struct Row<'a> {
    r: &'a JobRecord,
    house: Option<&'a str>,
}

impl Row<'_> {
    fn key(&self) -> String {
        format!("{}/{}", self.r.host, self.r.id)
    }

    fn place(&self, scope: Scope) -> String {
        match scope {
            Scope::Host => self.r.pool.clone(),
            Scope::Fleet => self.house.unwrap_or("").to_string(),
        }
    }

    fn actor(&self) -> &str {
        self.r
            .triggering_actor
            .as_deref()
            .or(self.r.actor.as_deref())
            .unwrap_or("")
    }

    fn matches(&self, needle: &str) -> bool {
        needle.is_empty()
            || [
                self.r.repo.as_deref(),
                self.r.workflow.as_deref(),
                self.r.job.as_deref(),
                Some(self.actor()),
                self.r.git_ref.as_deref(),
                Some(self.r.host.as_str()),
            ]
            .into_iter()
            .flatten()
            .any(|f| f.to_lowercase().contains(needle))
    }
}

const RESULTS: [(&str, &str); 4] = [
    ("Succeeded", "Succeeded"),
    ("Failed", "Failed or timed out"),
    ("Canceled", "Canceled"),
    ("Interrupted", "Interrupted"),
];

fn result_matches(filter: Option<&str>, result: &str) -> bool {
    match filter {
        None => true,
        Some("Failed") => matches!(result, "Failed" | "TimedOut" | "VmCrashed"),
        Some(f) => f == result,
    }
}

pub fn show(ui: &mut Ui, app: &mut App, snap: &Snapshot) {
    let scope = app.scope;
    let h = &mut app.history;
    let all: Vec<Row> = match scope {
        Scope::Host => snap.ledger.iter().map(|r| Row { r, house: None }).collect(),
        Scope::Fleet => {
            let mut rows: Vec<Row> = snap
                .fleet_jobs
                .iter()
                .map(|j| Row {
                    r: &j.record,
                    house: Some(&j.house),
                })
                .collect();
            rows.sort_by_key(|r| r.r.started_at);
            rows
        }
    };
    let needle = h.search.trim().to_lowercase();
    // Everything but the period applies to the chart, which always shows 14 days.
    let filtered: Vec<&Row> = all
        .iter()
        .filter(|r| r.matches(&needle))
        .filter(|r| result_matches(h.result, &r.r.result))
        .filter(|r| h.place.as_ref().is_none_or(|p| *p == r.place(scope)))
        .collect();
    let since = h.period.secs().map(|s| timefmt::now() - s);
    let rows: Vec<&Row> = filtered
        .iter()
        .copied()
        .filter(|r| since.is_none_or(|s| r.r.started_at >= s))
        .collect();

    kpis(ui, &rows, h.period);

    widgets::section(
        ui,
        "Saved per day · 14 days",
        Some("GitHub-hosted price minus your power cost".into()),
    );
    let (labels, values) = daily(&filtered);
    widgets::bar_chart(ui, &values, &labels, 80.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(&labels[0]).size(10.5).color(MUTED));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new("today").size(10.5).color(MUTED));
        });
    });

    widgets::section(
        ui,
        &format!("Jobs · {}", rows.len()),
        (scope == Scope::Fleet).then(|| "as shared by each machine".into()),
    );
    let mut exported = None;
    ui.horizontal_wrapped(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut h.search)
                .hint_text("Search repo, workflow, actor, branch, host…")
                .desired_width(280.0),
        );
        for p in [Period::Day, Period::Week, Period::Month, Period::All] {
            ui.selectable_value(&mut h.period, p, p.label());
        }
        egui::ComboBox::from_id_salt("result")
            .selected_text(
                h.result
                    .map(|r| RESULTS.iter().find(|x| x.0 == r).map_or(r, |x| x.1))
                    .unwrap_or("Any result"),
            )
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut h.result, None, "Any result");
                for (value, label) in RESULTS {
                    ui.selectable_value(&mut h.result, Some(value), label);
                }
            });
        let mut places: Vec<String> = all.iter().map(|r| r.place(scope)).collect();
        places.sort();
        places.dedup();
        let any = if scope == Scope::Host {
            "Any pool"
        } else {
            "Any house"
        };
        egui::ComboBox::from_id_salt("place")
            .selected_text(h.place.clone().unwrap_or_else(|| any.into()))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut h.place, None, any);
                for p in places {
                    ui.selectable_value(&mut h.place, Some(p.clone()), p);
                }
            });
        if ui.button("Export CSV").clicked() {
            let records: Vec<JobRecord> = rows.iter().map(|r| r.r.clone()).collect();
            exported = Some(export(&records));
        }
    });
    match exported {
        Some(Ok(path)) => {
            widgets::reveal(path.parent().unwrap_or(&path));
            app.notify(format!("Saved {}", path.display()), true);
        }
        Some(Err(e)) => app.notify(format!("Export failed: {e:#}"), false),
        None => {}
    }
    ui.add_space(6.0);
    table(ui, &mut app.history, snap, scope, &rows);
}

fn kpis(ui: &mut Ui, rows: &[&Row], period: Period) {
    let sum = |f: &dyn Fn(&JobRecord) -> f64| rows.iter().map(|r| f(r.r)).sum::<f64>();
    let saved = sum(&|r| r.saved_vs_github_usd.unwrap_or(0.0));
    let depot = sum(&|r| r.saved_vs_depot_usd.unwrap_or(0.0));
    let secs = sum(&|r| r.duration_secs);
    let own = sum(&|r| r.electricity_usd + r.self_hosted_fee_usd);
    let ok = rows.iter().filter(|r| r.r.result == "Succeeded").count();
    let queued: Vec<f64> = rows.iter().filter_map(|r| r.r.queued_secs).collect();
    let p = period.label();
    widgets::kpis(
        ui,
        vec![
            (
                "Saved vs GitHub",
                widgets::usd(saved),
                format!("vs Depot {} · {p}", widgets::usd(depot)),
                Some(OK),
            ),
            (
                "Jobs",
                rows.len().to_string(),
                if rows.is_empty() {
                    p.to_string()
                } else {
                    format!(
                        "{:.0}% succeeded · {p}",
                        100.0 * ok as f64 / rows.len() as f64
                    )
                },
                None,
            ),
            (
                "Runner time",
                format!("{:.1}h", secs / 3600.0),
                if queued.is_empty() {
                    "queue wait unknown".into()
                } else {
                    format!(
                        "avg queue wait {}",
                        widgets::dur(queued.iter().sum::<f64>() / queued.len() as f64)
                    )
                },
                None,
            ),
            (
                "Your cost",
                widgets::usd(own),
                "electricity (+ any GitHub fee)".into(),
                None,
            ),
        ],
    );
}

fn daily(rows: &[&Row]) -> (Vec<String>, Vec<f64>) {
    let now = timefmt::now();
    let days: Vec<String> = (0..14)
        .rev()
        .map(|i| timefmt::day(now - i * 86400))
        .collect();
    let mut sums: BTreeMap<&str, f64> = days.iter().map(|d| (d.as_str(), 0.0)).collect();
    for r in rows {
        let day = timefmt::day(r.r.started_at);
        if let Some(v) = sums.get_mut(day.as_str()) {
            *v += r.r.saved_vs_github_usd.unwrap_or(0.0);
        }
    }
    let values = days.iter().map(|d| sums[d.as_str()]).collect();
    (days, values)
}

const COLS: [(&str, f32); 8] = [
    ("Result", 100.0),
    ("Repo · workflow / job", 0.0),
    ("Actor", 90.0),
    ("Where", 120.0),
    ("Started", 80.0),
    ("Time", 70.0),
    ("CPU", 44.0),
    ("Saved", 64.0),
];

fn table(ui: &mut Ui, h: &mut HistoryUi, snap: &Snapshot, scope: Scope, rows: &[&Row]) {
    if rows.is_empty() {
        ui.label(RichText::new("No jobs match.").color(MUTED));
        return;
    }
    let width = ui.available_width();
    let fixed: f32 = COLS.iter().map(|c| c.1).sum::<f32>() + 8.0 * COLS.len() as f32;
    let flex = (width - fixed).max(160.0);
    let widths: Vec<f32> = COLS
        .iter()
        .map(|c| if c.1 == 0.0 { flex } else { c.1 })
        .collect();
    let right = |i: usize| i >= 5;

    ui.horizontal(|ui| {
        for (i, (name, _)) in COLS.iter().enumerate() {
            let name = if i == 3 {
                if scope == Scope::Host { "Pool" } else { "Host" }
            } else {
                name
            };
            cell(ui, widths[i], right(i), |ui| {
                ui.label(RichText::new(name).size(11.5).color(MUTED));
            });
        }
    });
    ui.separator();

    let typical = typical_durations(&snap.ledger);
    for row in rows.iter().rev().take(MAX_ROWS) {
        let r = row.r;
        let key = row.key();
        let is_open = h.open.as_deref() == Some(key.as_str());
        let resp = ui
            .horizontal(|ui| {
                cell(ui, widths[0], false, |ui| {
                    widgets::dot(ui, widgets::result_color(&r.result));
                    ui.label(&r.result);
                });
                cell(ui, widths[1], false, |ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(r.repo.as_deref().unwrap_or("(not shared)")).strong(),
                        );
                        let w = what(r.workflow.as_deref(), r.job.as_deref());
                        if !w.is_empty() {
                            ui.label(RichText::new(w).size(11.5).color(MUTED));
                        }
                    });
                });
                cell(ui, widths[2], false, |ui| {
                    ui.label(row.actor());
                });
                cell(ui, widths[3], false, |ui| {
                    ui.label(if scope == Scope::Host {
                        &r.pool
                    } else {
                        &r.host
                    });
                });
                cell(ui, widths[4], false, |ui| {
                    ui.label(RichText::new(widgets::ago(r.started_at)).color(MUTED))
                        .on_hover_text(format!("{} UTC", timefmt::short(r.started_at)));
                });
                cell(ui, widths[5], true, |ui| {
                    ui.label(widgets::dur(r.duration_secs));
                });
                cell(ui, widths[6], true, |ui| {
                    ui.label(
                        r.cpu_utilization
                            .map(|u| format!("{:.0}%", u * 100.0))
                            .unwrap_or_else(|| "-".into()),
                    );
                });
                cell(ui, widths[7], true, |ui| match r.saved_vs_github_usd {
                    Some(v) if r.repo_private == Some(false) && v <= 0.0 => {
                        ui.label(RichText::new("free").color(MUTED))
                            .on_hover_text("Public repo: standard GitHub runners are free");
                    }
                    Some(v) => {
                        ui.label(RichText::new(widgets::usd(v)).color(OK));
                    }
                    None => {
                        ui.label(RichText::new("-").color(MUTED));
                    }
                });
            })
            .response;
        let click = ui.interact(resp.rect, ui.scope_id().with(&key), Sense::click());
        if click.hovered() {
            ui.painter()
                .rect_filled(resp.rect.expand(2.0), 4, SUBTLE.gamma_multiply(0.35));
        }
        if click.clicked() {
            h.open = if is_open { None } else { Some(key.clone()) };
        }
        if is_open {
            details(ui, r, &typical);
        }
        ui.separator();
    }
    if rows.len() > MAX_ROWS {
        ui.label(
            RichText::new(format!(
                "{} older jobs not shown; narrow the filters or export CSV",
                rows.len() - MAX_ROWS
            ))
            .color(MUTED),
        );
    }
}

fn cell(ui: &mut Ui, width: f32, right: bool, add: impl FnOnce(&mut Ui)) {
    let layout = if right {
        Layout::right_to_left(Align::Center)
    } else {
        Layout::left_to_right(Align::Center)
    };
    ui.allocate_ui_with_layout(Vec2::new(width, 20.0), layout, |ui| {
        ui.set_width(width);
        add(ui);
    });
}

fn details(ui: &mut Ui, r: &JobRecord, typical: &BTreeMap<String, f64>) {
    widgets::card(ui, |ui| {
        egui::Grid::new(format!("details-{}", r.id))
            .num_columns(4)
            .spacing([28.0, 8.0])
            .show(ui, |ui| {
                let item = |ui: &mut Ui, label: &str, value: String| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new(label).size(11.0).color(MUTED));
                        ui.label(value);
                    });
                };
                let trigger = match (&r.event, &r.git_ref) {
                    (Some(e), Some(rf)) => format!("{e} · {rf}"),
                    (e, rf) => e
                        .clone()
                        .or_else(|| rf.clone())
                        .unwrap_or_else(|| "-".into()),
                };
                item(ui, "Trigger", trigger);
                item(
                    ui,
                    "Queue wait",
                    r.queued_secs
                        .map(widgets::dur)
                        .unwrap_or_else(|| "-".into()),
                );
                item(
                    ui,
                    "Ran on",
                    format!("{} · {} · {}", r.host, r.pool, r.backend),
                );
                item(
                    ui,
                    "Commit",
                    r.sha
                        .as_deref()
                        .map(|s| s.chars().take(12).collect())
                        .unwrap_or_else(|| "-".into()),
                );
                ui.end_row();
                item(
                    ui,
                    "GitHub would cost",
                    r.github.as_ref().map_or("-".into(), |c| {
                        format!(
                            "{} · {} billed min of {}",
                            widgets::usd(c.usd),
                            c.billed_minutes,
                            c.label
                        )
                    }),
                );
                item(
                    ui,
                    "Depot would cost",
                    r.depot.as_ref().map_or("-".into(), |c| {
                        format!("{} · per second, {}", widgets::usd(c.usd), c.label)
                    }),
                );
                item(
                    ui,
                    "Your cost",
                    widgets::usd(r.electricity_usd + r.self_hosted_fee_usd),
                );
                let usual = vs_usual(typical, r);
                ui.vertical(|ui| {
                    ui.label(RichText::new("vs its usual run").size(11.0).color(MUTED));
                    match usual {
                        Some(x) if x >= 1.5 => {
                            ui.label(RichText::new(format!("{x:.1}× slower")).color(BAD));
                        }
                        Some(x) if x <= 0.67 => {
                            ui.label(RichText::new(format!("{:.1}× faster", 1.0 / x)).color(OK));
                        }
                        Some(_) => {
                            ui.label("about usual");
                        }
                        None => {
                            ui.label(RichText::new("not enough runs").color(MUTED));
                        }
                    }
                });
                ui.end_row();
            });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if let Some(url) = &r.url
                && ui.button("Open run").clicked()
            {
                ui.ctx().open_url(egui::OpenUrl::new_tab(url));
            }
            if r.repo_private == Some(false) {
                ui.label(
                    RichText::new(
                        "Public repo: standard GitHub runners are free, so these count as $0 saved",
                    )
                    .size(11.5)
                    .color(MUTED),
                );
            }
        });
    });
}

fn export(records: &[JobRecord]) -> anyhow::Result<std::path::PathBuf> {
    let dir = crate::paths::home_dir().join("Downloads");
    let dir = if dir.is_dir() {
        dir
    } else {
        crate::paths::home_dir()
    };
    let path = dir.join(format!(
        "home-runner-jobs-{}.csv",
        timefmt::day(timefmt::now())
    ));
    let mut file = std::fs::File::create(&path)?;
    crate::ledger::write_csv(&mut file, records)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{Event, PointerButton, Pos2, RawInput, Rect, vec2};

    #[test]
    fn clicking_a_row_opens_its_details() {
        let ctx = egui::Context::default();
        let rec = JobRecord {
            id: "r1".into(),
            host: "studio".into(),
            pool: "linux".into(),
            repo: Some("acme/app".into()),
            result: "Succeeded".into(),
            started_at: timefmt::now(),
            ..Default::default()
        };
        let row = Row {
            r: &rec,
            house: None,
        };
        let snap = Snapshot::default();
        let mut h = HistoryUi::default();
        let at = Pos2::new(400.0, 42.0);
        let frame = |events: Vec<Event>, h: &mut HistoryUi| {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 800.0))),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                table(ui, h, &snap, Scope::Host, &[&row]);
            });
            out.textures_delta.clear();
        };
        let press = |pressed| Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        frame(vec![], &mut h);
        frame(vec![Event::PointerMoved(at)], &mut h);
        frame(vec![press(true)], &mut h);
        frame(vec![press(false)], &mut h);
        assert_eq!(h.open.as_deref(), Some("studio/r1"));
        frame(vec![press(true)], &mut h);
        frame(vec![press(false)], &mut h);
        assert_eq!(h.open, None);
    }
}
