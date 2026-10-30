//! Loads what the GUI shows on a background thread: the config, the supervisor's `state.json`,
//! the job ledger and, if this machine has joined one, the fleet.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crate::config::Config;
use crate::fleet::{self, FleetJob, FleetView};
use crate::ledger::{self, JobRecord};
use crate::state::{self, State};
use crate::{paths, timefmt};

const LOCAL_EVERY: Duration = Duration::from_secs(1);
const FLEET_EVERY: Duration = Duration::from_secs(20);
/// Fleet history the History tab can show.
const FLEET_HISTORY_DAYS: i64 = 35;

#[derive(Clone, Default)]
pub struct Snapshot {
    pub config_path: PathBuf,
    pub config: Option<Config>,
    pub config_error: Option<String>,
    pub data_dir: PathBuf,
    pub state: Option<State>,
    pub ledger: Arc<Vec<JobRecord>>,
    pub fleet: Option<FleetView>,
    pub fleet_jobs: Arc<Vec<FleetJob>>,
    pub fleet_error: Option<String>,
    pub fleet_at: Option<i64>,
}

impl Snapshot {
    /// The supervisor is running if it has rewritten its state recently.
    pub fn daemon_running(&self) -> bool {
        self.state.as_ref().is_some_and(|s| {
            s.running && timefmt::now() - s.updated_at <= 4 * state::PUBLISH_SECS as i64
        })
    }

    pub fn host(&self) -> String {
        self.config
            .as_ref()
            .map(Config::host_name)
            .or_else(|| self.state.as_ref().map(|s| s.host.clone()))
            .unwrap_or_else(|| "this machine".into())
    }
}

pub struct Loader {
    pub snapshot: Mutex<Snapshot>,
    /// Set to fetch the fleet now instead of waiting for the next poll.
    pub refresh_fleet: AtomicBool,
}

pub fn spawn(config_path: PathBuf, ctx: eframe::egui::Context) -> Arc<Loader> {
    let loader = Arc::new(Loader {
        snapshot: Mutex::new(Snapshot {
            config_path: config_path.clone(),
            data_dir: paths::default_data_dir(),
            ..Default::default()
        }),
        refresh_fleet: AtomicBool::new(true),
    });
    let shared = loader.clone();
    std::thread::Builder::new()
        .name("home-runner-gui-data".into())
        .spawn(move || run(&config_path, &shared, &ctx))
        .expect("spawning the GUI data thread");
    loader
}

fn run(config_path: &Path, loader: &Loader, ctx: &eframe::egui::Context) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("building the GUI runtime");
    let mut config_seen = None;
    let mut ledger_seen = None;
    let mut last_fleet: Option<Instant> = None;
    loop {
        let config_changed = changed(config_path, &mut config_seen);
        if config_changed {
            let (config, error) = match Config::load(config_path) {
                Ok(c) => (Some(c), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            };
            let mut snap = loader.snapshot.lock().unwrap();
            snap.data_dir = config
                .as_ref()
                .map(Config::data_dir)
                .unwrap_or_else(paths::default_data_dir);
            snap.config = config;
            snap.config_error = error;
            ledger_seen = None;
            loader.refresh_fleet.store(true, Ordering::Relaxed);
        }
        let (data_dir, config) = {
            let snap = loader.snapshot.lock().unwrap();
            (snap.data_dir.clone(), snap.config.clone())
        };

        let state = state::read(&data_dir);
        let ledger_path = data_dir.join(ledger::LEDGER_FILE);
        let ledger = changed(&ledger_path, &mut ledger_seen)
            .then(|| Arc::new(ledger::load(&[ledger_path]).unwrap_or_default()));
        {
            let mut snap = loader.snapshot.lock().unwrap();
            snap.state = state;
            if let Some(ledger) = ledger {
                snap.ledger = ledger;
            }
        }

        let due = last_fleet.is_none_or(|t| t.elapsed() >= FLEET_EVERY)
            || loader.refresh_fleet.swap(false, Ordering::Relaxed);
        if due && let Some(config) = config.filter(|c| c.fleet.is_some()) {
            last_fleet = Some(Instant::now());
            let result = rt.block_on(async {
                let client = fleet::Client::from_config(&config)?.expect("fleet configured");
                let view = client.view().await?;
                let jobs = client
                    .jobs(timefmt::now() - FLEET_HISTORY_DAYS * 86400, 1000)
                    .await?;
                anyhow::Ok((view, jobs))
            });
            let mut snap = loader.snapshot.lock().unwrap();
            match result {
                Ok((view, jobs)) => {
                    snap.fleet = Some(view);
                    snap.fleet_jobs = Arc::new(jobs);
                    snap.fleet_error = None;
                    snap.fleet_at = Some(timefmt::now());
                }
                Err(e) => snap.fleet_error = Some(format!("{e:#}")),
            }
        }
        ctx.request_repaint();
        std::thread::sleep(LOCAL_EVERY);
    }
}

/// Whether a file's size or modification time changed since last time (or it appeared/vanished).
fn changed(path: &Path, seen: &mut Option<Option<(u64, SystemTime)>>) -> bool {
    let now = std::fs::metadata(path)
        .ok()
        .map(|m| (m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
    if seen.as_ref() == Some(&now) {
        return false;
    }
    *seen = Some(now);
    true
}
