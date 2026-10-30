//! Edits the common settings in `config.toml` in place (comments and other settings are kept),
//! checks the result parses, then saves and optionally restarts the supervisor.

use eframe::egui::{self, RichText, Ui};

use super::App;
use super::data::Snapshot;
use super::fleet_tab::share_choice;
use super::widgets::{self, MUTED, OK};
use crate::config::{Config, Share};

#[derive(Default)]
pub struct SettingsUi {
    form: Option<Form>,
    error: Option<String>,
}

#[derive(Clone, PartialEq)]
struct Form {
    host_name: String,
    prevent_sleep: bool,
    electricity: f64,
    job_watts: f64,
    annotate: bool,
    pools: Vec<(String, String, usize)>,
    fleet: Option<FleetForm>,
}

#[derive(Clone, PartialEq)]
struct FleetForm {
    house: String,
    share: Share,
    hash_private: bool,
}

impl Form {
    fn from(config: &Config) -> Self {
        Self {
            host_name: config.host_name(),
            prevent_sleep: config.host.prevent_sleep,
            electricity: config.pricing.electricity_usd_per_kwh,
            job_watts: config.pricing.job_watts,
            annotate: config.pricing.annotate_jobs,
            pools: config
                .pools
                .iter()
                .map(|p| (p.name.clone(), p.backend.kind().to_string(), p.count))
                .collect(),
            fleet: config.fleet.as_ref().map(|f| FleetForm {
                house: f.house.clone(),
                share: f.share,
                hash_private: f.hash_private_repos,
            }),
        }
    }

    /// Writes the fields that differ from `saved` into the config text, keeping comments and
    /// everything the form doesn't cover, and checks the result is a valid config.
    fn apply(&self, saved: &Form, text: &str) -> anyhow::Result<String> {
        use toml_edit::{DocumentMut, value};
        let mut doc: DocumentMut = text.parse()?;
        if self.host_name.trim() != saved.host_name {
            table(&mut doc, "host")["name"] = value(self.host_name.trim());
        }
        if self.prevent_sleep != saved.prevent_sleep {
            table(&mut doc, "host")["prevent_sleep"] = value(self.prevent_sleep);
        }
        if self.electricity != saved.electricity {
            table(&mut doc, "pricing")["electricity_usd_per_kwh"] = value(self.electricity);
        }
        if self.job_watts != saved.job_watts {
            table(&mut doc, "pricing")["job_watts"] = value(self.job_watts);
        }
        if self.annotate != saved.annotate {
            table(&mut doc, "pricing")["annotate_jobs"] = value(self.annotate);
        }
        if self.pools != saved.pools
            && let Some(pools) = doc.get_mut("pool").and_then(|p| p.as_array_of_tables_mut())
        {
            for t in pools.iter_mut() {
                let name = t.get("name").and_then(|n| n.as_str()).map(String::from);
                if let Some((_, _, count)) = self.pools.iter().find(|p| Some(&p.0) == name.as_ref())
                {
                    t["count"] = value(*count as i64);
                }
            }
        }
        if let (Some(f), Some(old)) = (&self.fleet, &saved.fleet)
            && f != old
        {
            let fleet = table(&mut doc, "fleet");
            fleet["house"] = value(f.house.trim());
            fleet["share"] = value(f.share.as_str());
            fleet["hash_private_repos"] = value(f.hash_private);
        }
        let out = doc.to_string();
        Config::parse(&out)?;
        Ok(out)
    }
}

/// A top-level table, added after everything else if it doesn't exist yet (toml_edit would
/// otherwise put it first, above the file's opening comments).
fn table<'a>(doc: &'a mut toml_edit::DocumentMut, name: &str) -> &'a mut toml_edit::Table {
    if !doc.contains_key(name) {
        let last = doc
            .iter()
            .flat_map(|(_, item)| match item {
                toml_edit::Item::Table(t) => vec![t.position()],
                toml_edit::Item::ArrayOfTables(a) => a.iter().map(|t| t.position()).collect(),
                _ => vec![],
            })
            .flatten()
            .max()
            .unwrap_or(0);
        let mut t = toml_edit::Table::new();
        t.set_position(Some(last + 1));
        doc.insert(name, toml_edit::Item::Table(t));
    }
    doc[name]
        .as_table_mut()
        .expect("config sections are tables")
}

pub fn show(ui: &mut Ui, app: &mut App, snap: &Snapshot) {
    let Some(config) = &snap.config else {
        ui.label(
            RichText::new(format!(
                "Fix {} first; the error is shown above.",
                snap.config_path.display()
            ))
            .color(MUTED),
        );
        return;
    };
    let saved = Form::from(config);
    let s = &mut app.settings;
    let form = s.form.get_or_insert_with(|| saved.clone());
    let dirty = *form != saved;

    let mut action = None;
    ui.columns(2, |cols| {
        let ui = &mut cols[0];
        widgets::section(ui, "This machine", None);
        widgets::card(ui, |ui| {
            egui::Grid::new("machine")
                .num_columns(2)
                .spacing([14.0, 10.0])
                .show(ui, |ui| {
                    ui.label(RichText::new("Host name").color(MUTED));
                    ui.add(egui::TextEdit::singleline(&mut form.host_name).desired_width(200.0))
                        .on_hover_text("Unique per machine. Prefixes every runner, VM and container it creates.");
                    ui.end_row();
                    ui.label(RichText::new("GitHub").color(MUTED));
                    ui.horizontal(|ui| {
                        let ok = snap.daemon_running();
                        widgets::dot(ui, if ok { OK } else { MUTED });
                        ui.label(snap.state.as_ref().map_or_else(
                            || config.github.scope().map(|_| "configured".into()).unwrap_or_default(),
                            |st| st.scope.clone(),
                        ));
                    });
                    ui.end_row();
                    ui.label(RichText::new("Keep awake").color(MUTED));
                    ui.checkbox(&mut form.prevent_sleep, "");
                    ui.end_row();
                    ui.label(RichText::new("Electricity").color(MUTED));
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::DragValue::new(&mut form.electricity)
                                .range(0.0..=2.0)
                                .speed(0.005)
                                .prefix("$")
                                .suffix(" / kWh")
                                .max_decimals(3),
                        );
                        ui.add(
                            egui::DragValue::new(&mut form.job_watts)
                                .range(0.0..=1000.0)
                                .speed(1.0)
                                .suffix(" W per job"),
                        );
                    });
                    ui.end_row();
                    ui.label(RichText::new("Post savings on runs").color(MUTED));
                    ui.checkbox(&mut form.annotate, "")
                        .on_hover_text("Adds a \"saved ~$X\" notice to each workflow run");
                    ui.end_row();
                });
        });

        widgets::section(ui, "Pools", Some("runners kept online per pool".into()));
        widgets::card(ui, |ui| {
            egui::Grid::new("pools")
                .num_columns(2)
                .spacing([14.0, 10.0])
                .show(ui, |ui| {
                    for (name, backend, count) in &mut form.pools {
                        ui.vertical(|ui| {
                            ui.set_min_width(130.0);
                            ui.add(egui::Label::new(RichText::new(name.as_str()).strong()).extend());
                            ui.label(RichText::new(backend.as_str()).size(11.5).color(MUTED));
                        });
                        ui.horizontal(|ui| {
                            ui.add(egui::DragValue::new(count).range(1..=16));
                            if backend == "tart" {
                                ui.label(RichText::new("at most 2 macOS VMs per Mac").size(11.5).color(MUTED));
                            }
                        });
                        ui.end_row();
                    }
                });
        });

        let ui = &mut cols[1];
        widgets::section(ui, "Fleet reporting", None);
        widgets::card(ui, |ui| match (&mut form.fleet, &config.fleet) {
            (Some(f), Some(cfg)) => {
                egui::Grid::new("fleet")
                    .num_columns(2)
                    .spacing([14.0, 10.0])
                    .show(ui, |ui| {
                        ui.label(RichText::new("Reports to").color(MUTED));
                        ui.label(RichText::new(&cfg.url).monospace());
                        ui.end_row();
                        ui.label(RichText::new("Fleet").color(MUTED));
                        ui.label(
                            snap.fleet
                                .as_ref()
                                .map_or_else(|| "-".to_string(), |v| v.fleet.clone()),
                        );
                        ui.end_row();
                        ui.label(RichText::new("House").color(MUTED));
                        ui.add(egui::TextEdit::singleline(&mut f.house).desired_width(200.0));
                        ui.end_row();
                        ui.label(RichText::new("Machine token").color(MUTED));
                        ui.label(
                            RichText::new(config.fleet_token_path().display().to_string())
                                .size(11.5)
                                .color(MUTED),
                        )
                        .on_hover_text("Readable only by you. Join again with a new invite to replace it.");
                        ui.end_row();
                        ui.label(RichText::new("Heartbeat").color(MUTED));
                        ui.label(format!("every {}s, plus after each job", cfg.heartbeat_secs.max(15)));
                        ui.end_row();
                    });
                widgets::section(ui, "What this machine shares", None);
                share_choice(ui, &mut f.share);
                ui.add_space(4.0);
                ui.checkbox(&mut f.hash_private, "Send private repo names as a short hash");
            }
            _ => {
                ui.label(RichText::new("Not part of a fleet. Join one from the Fleet tab.").color(MUTED));
            }
        });

        widgets::section(ui, "Files", None);
        widgets::card(ui, |ui| {
            ui.label(RichText::new(format!("Config  {}", snap.config_path.display())).size(12.0));
            ui.label(RichText::new(format!("Data    {}", snap.data_dir.display())).size(12.0));
            ui.horizontal(|ui| {
                if ui.button("Open data folder").clicked() {
                    widgets::reveal(&snap.data_dir);
                }
                if ui.button("Open logs").clicked() {
                    widgets::reveal(&snap.data_dir.join("logs"));
                }
            });
        });
    });

    ui.add_space(12.0);
    ui.separator();
    ui.horizontal(|ui| {
        if ui.button("Run doctor").clicked() {
            action = Some("doctor");
        }
        if ui
            .add_enabled(dirty, egui::Button::new("Discard changes"))
            .clicked()
        {
            action = Some("discard");
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add_enabled(
                    app_task_free(&app.task),
                    egui::Button::new("Save & restart"),
                )
                .on_hover_text("Saves, then restarts the background service so changes take effect")
                .clicked()
            {
                action = Some("save-restart");
            }
            if ui.add_enabled(dirty, egui::Button::new("Save")).clicked() {
                action = Some("save");
            }
            if dirty {
                ui.label(RichText::new("Unsaved changes").color(widgets::WARN));
            }
        });
    });
    if let Some(e) = &app.settings.error {
        ui.label(RichText::new(e).color(widgets::BAD));
    }

    match action {
        Some("doctor") => app.run_doctor(snap.config_path.clone()),
        Some("discard") => {
            app.settings.form = None;
            app.settings.error = None;
        }
        Some(a @ ("save" | "save-restart")) => {
            let form = app.settings.form.clone().expect("form exists");
            let result = std::fs::read_to_string(&snap.config_path)
                .map_err(anyhow::Error::from)
                .and_then(|text| form.apply(&saved, &text))
                .and_then(|text| Ok(std::fs::write(&snap.config_path, text)?));
            match result {
                Ok(()) => {
                    app.settings.error = None;
                    // Reload from the saved file on the next frame.
                    app.settings.form = None;
                    if a == "save-restart" {
                        app.restart_service();
                    } else {
                        app.notify(
                            "Saved. Restart home-runner for changes to take effect.",
                            true,
                        );
                    }
                }
                Err(e) => app.settings.error = Some(format!("Not saved: {e:#}")),
            }
        }
        _ => {}
    }
}

fn app_task_free(task: &Option<super::Task>) -> bool {
    task.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"# my comment
[github]
org = "acme"
[github.auth]
type = "token"
file = "/dev/null" # kept

[[pool]]
name = "linux"
backend = "docker"
count = 2

[[pool]]
name = "jobs"
backend = "host"

[fleet]
url = "https://fleet.example.workers.dev"
house = "Old"
heartbeat_secs = 30
"#;

    #[test]
    fn edits_keep_comments_and_unrelated_settings() {
        let config = Config::parse(CONFIG).unwrap();
        let mut form = Form::from(&config);
        form.pools[0].2 = 3;
        form.electricity = 0.31;
        form.fleet.as_mut().unwrap().house = "Zack's house".into();
        form.fleet.as_mut().unwrap().share = Share::Totals;
        let out = form.apply(&Form::from(&config), CONFIG).unwrap();
        assert!(out.starts_with("# my comment"));
        assert!(out.contains("# kept"));
        let back = Config::parse(&out).unwrap();
        assert_eq!(back.pools[0].count, 3);
        assert_eq!(back.pools[1].count, 1);
        assert_eq!(back.pricing.electricity_usd_per_kwh, 0.31);
        let fleet = back.fleet.unwrap();
        assert_eq!(fleet.house, "Zack's house");
        assert_eq!(fleet.share, Share::Totals);
        assert_eq!(fleet.heartbeat_secs, 30);
        // New sections go after the existing ones; untouched settings aren't written.
        assert!(out.find("[pricing]").unwrap() > out.find("[fleet]").unwrap());
        assert!(!out.contains("prevent_sleep") && !out.contains("[host]"));
    }

    #[test]
    fn refuses_to_save_an_invalid_config() {
        let saved = Form::from(&Config::parse(CONFIG).unwrap());
        let mut form = saved.clone();
        form.pools[0].2 = 0;
        assert!(form.apply(&saved, CONFIG).is_err());
    }
}
