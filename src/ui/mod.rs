//! `home-runner ui`: a desktop window over this machine's supervisor and, if it has joined one,
//! the fleet. It only reads `state.json`, the ledger and the fleet Worker, and asks the supervisor
//! to do things through files in the data dir, so it can be opened and closed at any time.
//! The Stats tab is the ledger dashboard (`app`), which works without a config.

mod active;
mod app;
mod data;
mod fleet_tab;
mod history;
mod settings;
mod stats;
mod widgets;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui::{self, Color32, Margin, RichText, Ui};

use data::{Loader, Snapshot};
use widgets::{BAD, BORDER, MUTED, OK, WARN};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Active,
    History,
    Stats,
    Fleet,
    Settings,
}

/// Whether Active and History show this machine or every machine in the fleet.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Host,
    Fleet,
}

/// Something running off the UI thread (restart, doctor, join) and what it reports back.
struct Task {
    label: String,
    rx: Receiver<Result<String, String>>,
}

pub struct App {
    loader: Arc<Loader>,
    stats: app::App,
    tab: Tab,
    scope: Scope,
    history: history::HistoryUi,
    settings: settings::SettingsUi,
    join: fleet_tab::JoinUi,
    /// Runner whose job the user asked to stop, awaiting confirmation.
    confirm_stop: Option<(String, String)>,
    task: Option<Task>,
    /// Output to show in a window (doctor), with its title.
    output: Option<(String, String)>,
    toast: Option<(String, bool, Instant)>,
}

/// `ledgers` and `since` set up the Stats tab (by default, this machine's ledger).
pub fn run(config_path: PathBuf, ledgers: Vec<PathBuf>, since: String) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("home-runner")
            .with_inner_size([1120.0, 780.0])
            .with_min_inner_size([820.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "home-runner",
        options,
        Box::new(move |cc| {
            style(&cc.egui_ctx);
            let loader = data::spawn(config_path, cc.egui_ctx.clone());
            Ok(Box::new(App {
                stats: app::App::new(cc, ledgers, &since),
                loader,
                tab: Tab::Active,
                scope: Scope::Host,
                history: Default::default(),
                settings: Default::default(),
                join: Default::default(),
                confirm_stop: None,
                task: None,
                output: None,
                toast: None,
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("starting the GUI: {e}"))
}

fn style(ctx: &egui::Context) {
    ctx.set_visuals(egui::Visuals::dark());
    ctx.global_style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(9.0, 4.0);
        style.visuals.panel_fill = Color32::from_rgb(13, 17, 23);
        style.visuals.window_fill = Color32::from_rgb(22, 27, 34);
        style.visuals.widgets.noninteractive.bg_stroke.color = BORDER;
        style.visuals.selection.bg_fill = Color32::from_rgb(31, 111, 235);
    });
}

impl App {
    fn spawn_task(&mut self, label: &str, f: impl FnOnce() -> Result<String> + Send + 'static) {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(f().map_err(|e| format!("{e:#}")));
        });
        self.task = Some(Task {
            label: label.into(),
            rx,
        });
    }

    fn notify(&mut self, message: impl Into<String>, ok: bool) {
        self.toast = Some((message.into(), ok, Instant::now()));
    }

    fn poll_task(&mut self) {
        let Some(task) = &self.task else { return };
        if let Ok(result) = task.rx.try_recv() {
            let label = task.label.clone();
            self.task = None;
            match result {
                Ok(text) if label == "Doctor" => self.output = Some((label, text)),
                Ok(text) => self.notify(text, true),
                Err(e) => self.notify(format!("{label} failed: {e}"), false),
            }
        }
    }

    pub fn restart_service(&mut self) {
        self.spawn_task("Restart", || {
            crate::service::restart()?;
            Ok("Restarted home-runner".into())
        });
    }

    pub fn run_doctor(&mut self, config_path: PathBuf) {
        self.spawn_task("Doctor", move || {
            let out = std::process::Command::new(std::env::current_exe()?)
                .arg("doctor")
                .arg("--config")
                .arg(&config_path)
                .output()?;
            let mut text = String::from_utf8_lossy(&out.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            Ok(text)
        });
    }

    fn top_bar(&mut self, ui: &mut Ui, snap: &Snapshot) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("home-runner").strong().size(15.0));
            ui.add_space(10.0);
            let joined = snap.config.as_ref().is_some_and(|c| c.fleet.is_some());
            let host = snap.host();
            ui.selectable_value(&mut self.scope, Scope::Host, host);
            let houses = snap.fleet.as_ref().map_or(0, |f| f.houses.len());
            ui.add_enabled_ui(joined, |ui| {
                ui.selectable_value(
                    &mut self.scope,
                    Scope::Fleet,
                    if houses > 0 {
                        format!(
                            "Fleet · {houses} house{}",
                            if houses == 1 { "" } else { "s" }
                        )
                    } else {
                        "Fleet".into()
                    },
                )
                .on_disabled_hover_text("Join a fleet in the Fleet tab");
            });
            if !joined {
                self.scope = Scope::Host;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (color, text) = status_line(snap);
                ui.label(RichText::new(text).size(12.0).color(MUTED));
                widgets::dot(ui, color);
                if let Some(task) = &self.task {
                    ui.spinner();
                    ui.label(RichText::new(format!("{}…", task.label)).color(MUTED));
                }
            });
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let running = snap
                .state
                .as_ref()
                .filter(|_| snap.daemon_running())
                .map_or(0, |s| s.slots.iter().filter(|s| s.job.is_some()).count());
            for (tab, label) in [
                (Tab::Active, format!("Active  {running}")),
                (Tab::History, "History".into()),
                (Tab::Stats, "Stats".into()),
                (Tab::Fleet, "Fleet".into()),
                (Tab::Settings, "Settings".into()),
            ] {
                ui.selectable_value(&mut self.tab, tab, RichText::new(label).size(13.5));
            }
        });
    }

    fn stop_dialog(&mut self, ctx: &egui::Context, snap: &Snapshot) {
        let Some((runner, what)) = self.confirm_stop.clone() else {
            return;
        };
        let modal = egui::Modal::new(egui::Id::unique("confirm-stop")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.heading("Stop this job?");
            ui.label(format!(
                "{what} will be killed on this machine and recorded as Canceled. GitHub marks the job failed once the runner disappears."
            ));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Keep running").clicked() {
                    self.confirm_stop = None;
                }
                if ui
                    .button(RichText::new("Stop job").color(BAD))
                    .clicked()
                {
                    match crate::live::request_stop(&snap.data_dir, &runner) {
                        Ok(()) => self.notify("Asked home-runner to stop the job", true),
                        Err(e) => self.notify(format!("Couldn't request stop: {e}"), false),
                    }
                    self.confirm_stop = None;
                }
            });
        });
        if modal.should_close() {
            self.confirm_stop = None;
        }
    }
}

fn status_line(snap: &Snapshot) -> (Color32, String) {
    if !snap.daemon_running() {
        return (BAD, "Supervisor not running".into());
    }
    match snap.state.as_ref().and_then(|s| s.fleet.as_ref()) {
        Some(f) if f.last_error.is_some() => (WARN, "Fleet report failing".into()),
        Some(f) => match f.last_ok_at {
            Some(t) => (OK, format!("Reported to fleet {}", widgets::ago(t))),
            None => (WARN, "Not reported to fleet yet".into()),
        },
        None => (OK, "Supervisor running".into()),
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.poll_task();
        let snap = self.loader.snapshot.lock().unwrap().clone();
        let ctx = ui.ctx().clone();

        egui::Panel::top("top")
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(22, 27, 34))
                    .inner_margin(Margin::symmetric(16, 10)),
            )
            .show(ui, |ui| self.top_bar(ui, &snap));

        egui::CentralPanel::default()
            .frame(egui::Frame::new().inner_margin(Margin::symmetric(18, 12)))
            .show(ui, |ui| {
                if let Some(error) = &snap.config_error {
                    widgets::card(ui, |ui| {
                        ui.label(RichText::new("Config problem").color(BAD).strong());
                        ui.label(error);
                        ui.label(
                            RichText::new(format!("Config file: {}", snap.config_path.display()))
                                .color(MUTED),
                        );
                    });
                    ui.add_space(8.0);
                }
                if self.tab == Tab::Stats {
                    // The dashboard lays out its own panels and scrolling.
                    self.stats.show(ui);
                    return;
                }
                egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| match self.tab {
                        Tab::Active => active::show(ui, self, &snap),
                        Tab::History => history::show(ui, self, &snap),
                        Tab::Fleet => fleet_tab::show(ui, self, &snap),
                        Tab::Settings => settings::show(ui, self, &snap),
                        Tab::Stats => {}
                    });
            });

        self.stop_dialog(&ctx, &snap);

        if let Some((title, text)) = self.output.clone() {
            let mut open = true;
            egui::Window::new(title)
                .open(&mut open)
                .default_size([620.0, 420.0])
                .show(&ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.label(RichText::new(text).monospace().size(12.0));
                    });
                });
            if !open {
                self.output = None;
            }
        }

        if let Some((message, ok, at)) = self.toast.clone() {
            if at.elapsed() > Duration::from_secs(6) {
                self.toast = None;
            } else {
                egui::Area::new(egui::Id::unique("toast"))
                    .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -24.0])
                    .show(&ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                widgets::dot(ui, if ok { OK } else { BAD });
                                ui.label(message);
                            });
                        });
                    });
            }
        }

        // Running jobs ask for a redraw every second (`widgets::keep_ticking`); otherwise the data
        // thread wakes the window when something changed, and this only refreshes the "ago" labels.
        if !widgets::minimized(&ctx) {
            ctx.request_repaint_after(widgets::IDLE_REDRAW);
        }
    }
}
