use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{self, Color32, RichText, Ui};
use egui_extras::{Column, TableBuilder};
use egui_plot::{Bar, BarChart, HoverPosition, Legend, Line, Plot, PlotPoints};

use super::stats::{self, Stats};
use crate::ledger::{self, GroupBy, JobRecord};
use crate::timefmt;

const RELOAD_EVERY: Duration = Duration::from_secs(2);
/// Re-slice the time window now and then so "last 24h" keeps moving while the window is open.
const REFRESH_WINDOW_EVERY: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Range {
    Day,
    Week,
    Month,
    Quarter,
    All,
}

impl Range {
    const ALL: [Range; 5] = [
        Range::Day,
        Range::Week,
        Range::Month,
        Range::Quarter,
        Range::All,
    ];

    fn label(self) -> &'static str {
        match self {
            Range::Day => "24 hours",
            Range::Week => "7 days",
            Range::Month => "30 days",
            Range::Quarter => "90 days",
            Range::All => "All time",
        }
    }

    fn secs(self) -> Option<i64> {
        match self {
            Range::Day => Some(86_400),
            Range::Week => Some(7 * 86_400),
            Range::Month => Some(30 * 86_400),
            Range::Quarter => Some(90 * 86_400),
            Range::All => None,
        }
    }

    /// Maps a CLI `--since` value onto the nearest preset, defaulting to 30 days.
    fn from_since(since: &str) -> Self {
        match timefmt::parse_since(since) {
            Ok(None) => Range::All,
            Ok(Some(d)) => Range::ALL
                .into_iter()
                .filter_map(|r| r.secs().map(|s| (r, s)))
                .find(|&(_, s)| d.as_secs() <= s as u64)
                .map_or(Range::All, |(r, _)| r),
            Err(_) => Range::Month,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Breakdown,
    Jobs,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortBy {
    Started,
    Duration,
    Queue,
    Saved,
}

struct Colors {
    github: Color32,
    depot: Color32,
    ok: Color32,
    fail: Color32,
    other: Color32,
}

impl Colors {
    fn for_mode(dark: bool) -> Self {
        if dark {
            Self {
                github: Color32::from_rgb(0x39, 0x87, 0xe5),
                depot: Color32::from_rgb(0xd9, 0x59, 0x26),
                ok: Color32::from_rgb(0x3f, 0xb9, 0x6b),
                fail: Color32::from_rgb(0xe6, 0x67, 0x67),
                other: Color32::from_rgb(0x8a, 0x8a, 0x84),
            }
        } else {
            Self {
                github: Color32::from_rgb(0x2a, 0x78, 0xd6),
                depot: Color32::from_rgb(0xeb, 0x68, 0x34),
                ok: Color32::from_rgb(0x1b, 0x8a, 0x4b),
                fail: Color32::from_rgb(0xe3, 0x49, 0x48),
                other: Color32::from_rgb(0x8a, 0x8a, 0x84),
            }
        }
    }
}

pub struct App {
    ledgers: Vec<PathBuf>,
    mtimes: Vec<Option<SystemTime>>,
    records: Vec<JobRecord>,
    error: Option<String>,
    last_check: Instant,
    last_slice: Instant,
    loaded_at: Option<i64>,

    range: Range,
    repo: String,
    actor: String,
    host: String,
    tab: Tab,
    group_by: GroupBy,
    sort: SortBy,
    sort_desc: bool,

    /// Indices into `records` that pass the filters, oldest first.
    visible: Vec<usize>,
    stats: Stats,
    dirty: bool,
}

impl App {
    pub fn new(_cc: &eframe::CreationContext<'_>, ledgers: Vec<PathBuf>, since: &str) -> Self {
        let mut app = Self {
            mtimes: vec![None; ledgers.len()],
            ledgers,
            records: Vec::new(),
            error: None,
            last_check: Instant::now(),
            last_slice: Instant::now(),
            loaded_at: None,
            range: Range::from_since(since),
            repo: String::new(),
            actor: String::new(),
            host: String::new(),
            tab: Tab::Overview,
            group_by: GroupBy::Repo,
            sort: SortBy::Started,
            sort_desc: true,
            visible: Vec::new(),
            stats: Stats::compute(&[]),
            dirty: true,
        };
        app.reload();
        app
    }

    fn mtimes_now(&self) -> Vec<Option<SystemTime>> {
        self.ledgers
            .iter()
            .map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
            .collect()
    }

    fn reload(&mut self) {
        self.mtimes = self.mtimes_now();
        match ledger::load(&self.ledgers) {
            Ok(records) => {
                self.records = records;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
        self.loaded_at = Some(timefmt::now());
        self.dirty = true;
    }

    /// Picks up new jobs as the supervisor appends them.
    fn poll(&mut self) {
        if self.last_check.elapsed() < RELOAD_EVERY {
            return;
        }
        self.last_check = Instant::now();
        if self.mtimes_now() != self.mtimes {
            self.reload();
        }
        if self.last_slice.elapsed() >= REFRESH_WINDOW_EVERY {
            self.dirty = true;
        }
    }

    fn recompute(&mut self) {
        self.last_slice = Instant::now();
        let since = self.range.secs().map(|s| timefmt::now() - s);
        let has = |field: Option<&str>, needle: &str| {
            needle.is_empty()
                || field.is_some_and(|f| f.to_lowercase().contains(&needle.to_lowercase()))
        };
        self.visible = self
            .records
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                since.is_none_or(|s| r.started_at >= s)
                    && has(r.repo.as_deref(), &self.repo)
                    && (has(r.actor.as_deref(), &self.actor)
                        || has(r.triggering_actor.as_deref(), &self.actor))
                    && has(Some(&r.host), &self.host)
            })
            .map(|(i, _)| i)
            .collect();
        let rows = self.rows();
        self.stats = Stats::compute(&rows);
        self.dirty = false;
    }

    fn rows(&self) -> Vec<&JobRecord> {
        self.visible.iter().map(|&i| &self.records[i]).collect()
    }

    fn filters(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            let before = self.range;
            egui::ComboBox::from_id_salt("range")
                .selected_text(self.range.label())
                .show_ui(ui, |ui| {
                    for r in Range::ALL {
                        ui.selectable_value(&mut self.range, r, r.label());
                    }
                });
            self.dirty |= before != self.range;
            for (label, text) in [
                ("repo", &mut self.repo),
                ("actor", &mut self.actor),
                ("host", &mut self.host),
            ] {
                let edit = ui.add(
                    egui::TextEdit::singleline(text)
                        .hint_text(label)
                        .desired_width(110.0),
                );
                self.dirty |= edit.changed();
            }
            if ui.button("Reload").clicked() {
                self.reload();
            }
            if let (Some(first), Some(last)) = (self.stats.first, self.stats.last) {
                ui.weak(format!(
                    "{} jobs, {} to {} UTC",
                    self.stats.jobs(),
                    timefmt::short(first),
                    timefmt::short(last)
                ));
            }
            if let Some(at) = self.loaded_at {
                ui.weak(format!(
                    "{} ledger(s), updated {} UTC",
                    self.ledgers.len(),
                    { timefmt::short(at) }
                ));
            }
        });
    }
}

impl App {
    /// Draws the dashboard into the Stats tab.
    pub fn show(&mut self, ui: &mut Ui) {
        self.poll();
        ui.ctx().request_repaint_after(RELOAD_EVERY);
        let colors = Colors::for_mode(ui.visuals().dark_mode);

        egui::Panel::top("stats-top").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                for (tab, label) in [
                    (Tab::Overview, "Overview"),
                    (Tab::Breakdown, "Breakdown"),
                    (Tab::Jobs, "Jobs"),
                ] {
                    ui.selectable_value(&mut self.tab, tab, label);
                }
            });
            ui.add_space(4.0);
            self.filters(ui);
            ui.add_space(6.0);
        });

        if self.dirty {
            self.recompute();
        }

        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(e) = &self.error {
                ui.colored_label(colors.fail, format!("Could not read the ledger: {e}"));
            }
            if self.records.is_empty() && self.error.is_none() {
                empty_state(ui, &self.ledgers);
                return;
            }
            if self.stats.jobs() == 0 {
                ui.add_space(24.0);
                ui.label("No jobs match these filters.");
                return;
            }
            match self.tab {
                Tab::Overview => overview(ui, &self.stats, &colors),
                Tab::Breakdown => self.breakdown(ui),
                Tab::Jobs => self.jobs(ui, &colors),
            }
        });
    }
}

fn empty_state(ui: &mut Ui, ledgers: &[PathBuf]) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        ui.heading("No jobs recorded yet");
        ui.add_space(6.0);
        ui.label("Jobs appear here as soon as a runner finishes one.");
        for p in ledgers {
            ui.weak(p.display().to_string());
        }
    });
}

fn kpi(ui: &mut Ui, label: &str, value: String, sub: Option<String>, accent: Option<Color32>) {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_min_width(150.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(label).small().weak());
                let mut v = RichText::new(value).size(26.0).strong();
                if let Some(c) = accent {
                    v = v.color(c);
                }
                ui.label(v);
                if let Some(s) = sub {
                    ui.label(RichText::new(s).small().weak());
                }
            });
        });
}

fn overview(ui: &mut Ui, s: &Stats, c: &Colors) {
    let t = &s.totals;
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            kpi(
                ui,
                "Saved vs GitHub-hosted",
                ledger::usd(t.saved_github),
                Some(format!("would have cost {}", ledger::usd(t.github))),
                Some(c.github),
            );
            kpi(
                ui,
                "Saved vs Depot",
                ledger::usd(t.saved_depot),
                Some(format!("would have cost {}", ledger::usd(t.depot))),
                Some(c.depot),
            );
            kpi(
                ui,
                "Jobs",
                t.jobs.to_string(),
                Some(format!("{} not successful", t.jobs - s.succeeded)),
                None,
            );
            kpi(
                ui,
                "Success rate",
                s.success_rate()
                    .map_or("-".into(), |r| format!("{:.0}%", r * 100.0)),
                Some(format!("{} ok · {} failed", s.succeeded, s.failed)),
                None,
            );
            kpi(
                ui,
                "Runner time",
                ledger::hours(t.secs),
                s.duration
                    .map(|d| format!("avg {}", ledger::duration(d.avg))),
                None,
            );
            kpi(
                ui,
                "Your cost",
                ledger::usd(t.own),
                Some("power + fees".into()),
                None,
            );
        });
        ui.add_space(10.0);

        let half = (ui.available_width() - 12.0) / 2.0;
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(half);
                ui.strong("Cumulative savings");
                savings_chart(ui, s, c);
            });
            ui.vertical(|ui| {
                ui.set_width(half);
                ui.strong("Jobs per day (UTC)");
                jobs_chart(ui, s, c);
            });
        });
        ui.add_space(10.0);

        ui.strong("Job results");
        result_bar(ui, s, c);
        ui.add_space(10.0);

        ui.strong("Timing");
        egui::Grid::new("timing")
            .num_columns(5)
            .spacing([24.0, 4.0])
            .show(ui, |ui| {
                for h in ["", "average", "median", "p95", "longest"] {
                    ui.label(RichText::new(h).small().weak());
                }
                ui.end_row();
                for (name, p) in [("Job duration", s.duration), ("Queue wait", s.queue)] {
                    ui.label(name);
                    match p {
                        Some(p) => {
                            for v in [p.avg, p.median, p.p95, p.max] {
                                ui.label(ledger::duration(v));
                            }
                        }
                        None => {
                            ui.label("-");
                        }
                    }
                    ui.end_row();
                }
            });
        if s.queue.is_none() {
            ui.label(
                RichText::new("Queue wait needs the GitHub App's \"Actions: Read\" permission.")
                    .small()
                    .weak(),
            );
        }
        ui.add_space(6.0);
        ui.label(format!(
            "CPU: {:.1} core-hours used, average utilization {}",
            t.cpu_secs / 3600.0,
            s.avg_utilization()
                .map_or("-".into(), |u| format!("{:.0}%", u * 100.0)),
        ));
    });
}

fn day_axis(x: f64) -> String {
    let day = timefmt::day((x.round() as i64) * 86_400);
    day.get(5..).unwrap_or(&day).to_string()
}

fn savings_chart(ui: &mut Ui, s: &Stats, c: &Colors) {
    let series = |f: fn(&stats::DayPoint) -> f64| -> PlotPoints<'static> {
        s.daily.iter().map(|d| [d.day as f64, f(d)]).collect()
    };
    Plot::new("savings")
        .height(240.0)
        .legend(Legend::default())
        .include_y(0.0)
        .allow_scroll(false)
        .x_axis_formatter(|m, _| day_axis(m.value))
        .y_axis_formatter(|m, _| ledger::usd(m.value))
        .label_formatter(|pos| match pos {
            HoverPosition::NearDataPoint {
                plot_name,
                position,
                ..
            } => Some(format!(
                "{plot_name}\n{}\n{} saved",
                timefmt::day((position.x.round() as i64) * 86_400),
                ledger::usd(position.y)
            )),
            _ => None,
        })
        .show(ui, |p| {
            p.line(
                Line::new("vs GitHub-hosted", series(|d| d.cum_saved_github))
                    .color(c.github)
                    .width(2.0),
            );
            p.line(
                Line::new("vs Depot", series(|d| d.cum_saved_depot))
                    .color(c.depot)
                    .width(2.0),
            );
        });
}

fn jobs_chart(ui: &mut Ui, s: &Stats, c: &Colors) {
    let bars = |f: fn(&stats::DayPoint) -> usize, color: Color32, name: &str| {
        BarChart::new(
            name,
            s.daily
                .iter()
                .map(|d| {
                    Bar::new(d.day as f64, f(d) as f64)
                        .width(0.7)
                        .fill(color)
                        .stroke(egui::Stroke::NONE)
                })
                .collect(),
        )
        .color(color)
    };
    let ok = bars(|d| d.succeeded, c.ok, "Succeeded");
    let fail = bars(|d| d.failed, c.fail, "Failed").stack_on(&[&ok]);
    let other = bars(|d| d.other, c.other, "Canceled / other").stack_on(&[&ok, &fail]);
    Plot::new("jobs")
        .height(240.0)
        .legend(Legend::default())
        .include_y(0.0)
        .allow_scroll(false)
        .x_axis_formatter(|m, _| day_axis(m.value))
        .y_axis_formatter(|m, _| {
            if m.value.fract() == 0.0 {
                format!("{}", m.value as i64)
            } else {
                String::new()
            }
        })
        .show(ui, |p| {
            p.bar_chart(ok);
            p.bar_chart(fail);
            p.bar_chart(other);
        });
}

/// One segmented bar for the result split, with a labelled legend so color is never the only cue.
fn result_bar(ui: &mut Ui, s: &Stats, c: &Colors) {
    let total = s.jobs().max(1) as f32;
    let parts = [
        ("Succeeded", s.succeeded, c.ok),
        ("Failed / timed out", s.failed, c.fail),
        ("Canceled / other", s.other, c.other),
    ];
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 14.0), egui::Sense::hover());
    let mut x = rect.left();
    for (_, n, color) in parts {
        let w = rect.width() * n as f32 / total;
        if w > 0.0 {
            let seg =
                egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(w, rect.height()));
            // Two-pixel gaps keep neighbouring segments distinguishable.
            ui.painter()
                .rect_filled(seg.shrink2(egui::vec2(1.0, 0.0)), 3.0, color);
            x += w;
        }
    }
    ui.horizontal_wrapped(|ui| {
        for (label, n, color) in parts {
            let (dot, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
            ui.painter().circle_filled(dot.center(), 4.5, color);
            ui.label(format!("{label}: {n} ({:.0}%)", 100.0 * n as f32 / total));
            ui.add_space(10.0);
        }
    });
}

impl App {
    fn breakdown(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label("Group by");
            for by in [
                GroupBy::Repo,
                GroupBy::Actor,
                GroupBy::Workflow,
                GroupBy::Pool,
                GroupBy::Host,
                GroupBy::Day,
                GroupBy::Month,
            ] {
                ui.selectable_value(&mut self.group_by, by, by.label());
            }
        });
        ui.add_space(6.0);
        let rows = stats::group(&self.rows(), self.group_by);
        let num = |ui: &mut Ui, s: String| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(s);
            });
        };
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .column(Column::remainder().at_least(200.0).clip(true))
            .columns(Column::auto().at_least(70.0), 8)
            .header(22.0, |mut h| {
                h.col(|ui| {
                    ui.strong(self.group_by.label());
                });
                for name in [
                    "Jobs",
                    "Failed",
                    "Time",
                    "CPU",
                    "Avg queue",
                    "GitHub $",
                    "Depot $",
                    "Saved",
                ] {
                    h.col(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.strong(name);
                        });
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, rows.len(), |mut row| {
                    let g = &rows[row.index()];
                    let t = &g.totals;
                    row.col(|ui| {
                        ui.label(&g.key);
                    });
                    row.col(|ui| num(ui, t.jobs.to_string()));
                    row.col(|ui| num(ui, t.failed.to_string()));
                    row.col(|ui| num(ui, ledger::hours(t.secs)));
                    row.col(|ui| num(ui, t.utilization()));
                    row.col(|ui| {
                        num(
                            ui,
                            if t.queued_jobs > 0 {
                                ledger::duration(t.queued_secs / t.queued_jobs as f64)
                            } else {
                                "-".into()
                            },
                        )
                    });
                    row.col(|ui| num(ui, ledger::usd(t.github)));
                    row.col(|ui| num(ui, ledger::usd(t.depot)));
                    row.col(|ui| num(ui, ledger::usd(t.saved_github)));
                });
            });
    }

    fn jobs(&mut self, ui: &mut Ui, c: &Colors) {
        let (records, visible, sort_by) = (&self.records, &self.visible, self.sort);
        let mut rows: Vec<&JobRecord> = visible.iter().map(|&i| &records[i]).collect();
        let key = |r: &JobRecord| match sort_by {
            SortBy::Started => r.started_at as f64,
            SortBy::Duration => r.duration_secs,
            SortBy::Queue => r.queued_secs.unwrap_or(-1.0),
            SortBy::Saved => r.saved_vs_github_usd.unwrap_or(f64::MIN),
        };
        rows.sort_by(|a, b| key(a).total_cmp(&key(b)));
        if self.sort_desc {
            rows.reverse();
        }

        let sort_header = |ui: &mut Ui, sort: &mut SortBy, desc: &mut bool, which, label: &str| {
            let arrow = if *sort == which {
                if *desc { " v" } else { " ^" }
            } else {
                ""
            };
            if ui
                .button(RichText::new(format!("{label}{arrow}")).strong())
                .clicked()
            {
                if *sort == which {
                    *desc = !*desc;
                } else {
                    *sort = which;
                    *desc = true;
                }
            }
        };

        let (sort, desc) = (&mut self.sort, &mut self.sort_desc);
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .column(Column::auto().at_least(120.0))
            .column(Column::initial(170.0).at_least(80.0).clip(true))
            .column(Column::initial(220.0).at_least(80.0).clip(true))
            .column(Column::auto().at_least(80.0))
            .column(Column::auto().at_least(100.0))
            .column(Column::auto().at_least(60.0))
            .column(Column::auto().at_least(60.0))
            .column(Column::auto().at_least(80.0))
            .column(Column::auto().at_least(44.0))
            .column(Column::auto().at_least(60.0))
            .header(24.0, |mut h| {
                h.col(|ui| sort_header(ui, sort, desc, SortBy::Started, "Started (UTC)"));
                h.col(|ui| {
                    ui.strong("Repo");
                });
                h.col(|ui| {
                    ui.strong("Workflow / job");
                });
                h.col(|ui| {
                    ui.strong("Actor");
                });
                h.col(|ui| {
                    ui.strong("Host / pool");
                });
                h.col(|ui| sort_header(ui, sort, desc, SortBy::Duration, "Time"));
                h.col(|ui| sort_header(ui, sort, desc, SortBy::Queue, "Queue"));
                h.col(|ui| {
                    ui.strong("Result");
                });
                h.col(|ui| {
                    ui.strong("CPU");
                });
                h.col(|ui| sort_header(ui, sort, desc, SortBy::Saved, "Saved"));
            })
            .body(|body| {
                body.rows(20.0, rows.len(), |mut row| {
                    let r = rows[row.index()];
                    row.col(|ui| {
                        ui.label(timefmt::short(r.started_at));
                    });
                    row.col(|ui| {
                        ui.label(r.repo.as_deref().unwrap_or("?"));
                    });
                    row.col(|ui| {
                        let what = match (&r.workflow, &r.job) {
                            (Some(w), Some(j)) => format!("{w} / {j}"),
                            (w, j) => w.clone().or_else(|| j.clone()).unwrap_or_default(),
                        };
                        match &r.url {
                            Some(url) => ui.hyperlink_to(what, url),
                            None => ui.label(what),
                        };
                    });
                    row.col(|ui| {
                        ui.label(
                            r.triggering_actor
                                .as_deref()
                                .or(r.actor.as_deref())
                                .unwrap_or("?"),
                        );
                    });
                    row.col(|ui| {
                        ui.label(format!("{}/{}", r.host, r.pool));
                    });
                    row.col(|ui| {
                        ui.label(ledger::duration(r.duration_secs));
                    });
                    row.col(|ui| {
                        ui.label(r.queued_secs.map_or("-".into(), ledger::duration));
                    });
                    row.col(|ui| {
                        let color = match r.result.as_str() {
                            "Succeeded" => c.ok,
                            "Failed" | "TimedOut" => c.fail,
                            _ => c.other,
                        };
                        ui.label(RichText::new(&r.result).color(color));
                    });
                    row.col(|ui| {
                        ui.label(
                            r.cpu_utilization
                                .map_or("-".into(), |u| format!("{:.0}%", u * 100.0)),
                        );
                    });
                    row.col(|ui| {
                        ui.label(r.saved_vs_github_usd.map_or("-".into(), ledger::usd));
                    });
                });
            });
    }
}
