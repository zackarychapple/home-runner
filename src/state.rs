//! Live state the supervisor publishes to `state.json` every couple of seconds: what each runner
//! slot is doing, the job it is running and how fleet reporting is going. `home-runner gui` reads
//! it; nothing else depends on it, so it never blocks or fails the supervisor.
//!
//! The GUI asks the supervisor to stop a job by creating `control/stop-<runner name>`.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::proc::{JobInfo, JobWatch};
use crate::timefmt;

pub const STATE_FILE: &str = "state.json";
pub const CONTROL_DIR: &str = "control";
/// The supervisor rewrites the file this often; a reader treats it as stale after a few misses.
pub const PUBLISH_SECS: u64 = 2;
/// Recent CPU samples kept per running job.
const CPU_HISTORY: usize = 60;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub host: String,
    pub version: String,
    pub pid: u32,
    pub started_at: i64,
    pub updated_at: i64,
    /// False once the supervisor has shut down cleanly.
    pub running: bool,
    pub scope: String,
    pub electricity_usd_per_kwh: f64,
    pub job_watts: f64,
    pub pools: Vec<PoolState>,
    pub reconcile: Option<ReconcileState>,
    pub fleet: Option<FleetStatus>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PoolState {
    pub name: String,
    pub backend: String,
    pub labels: Vec<String>,
    pub os: String,
    pub arch: String,
    pub cpus: u32,
    /// The GitHub-hosted runner jobs are priced against, and its per-minute rate.
    pub github_label: Option<String>,
    pub github_usd_per_minute: Option<f64>,
    pub slots: Vec<SlotState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Pulling images or downloading the runner before the first job.
    #[default]
    Preparing,
    Registering,
    /// Booting the VM or container.
    Launching,
    /// Registered and waiting for GitHub to assign a job.
    Idle,
    Busy,
    /// Collecting results and tearing down.
    Finishing,
    /// Waiting before retrying after a failure.
    Backoff,
    Stopped,
}

impl Phase {
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn label(self) -> &'static str {
        match self {
            Phase::Preparing => "preparing",
            Phase::Registering => "registering",
            Phase::Launching => "booting",
            Phase::Idle => "idle",
            Phase::Busy => "running",
            Phase::Finishing => "finishing",
            Phase::Backoff => "backoff",
            Phase::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SlotState {
    pub slot: usize,
    pub phase: Phase,
    /// When the slot entered this phase.
    pub since: i64,
    pub runner: Option<String>,
    pub error: Option<String>,
    pub retry_at: Option<i64>,
    pub job: Option<LiveJob>,
}

/// The job a busy slot is running, as far as the supervisor knows so far.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LiveJob {
    pub name: Option<String>,
    pub repo: Option<String>,
    pub workflow: Option<String>,
    pub actor: Option<String>,
    pub event: Option<String>,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    pub sha: Option<String>,
    pub url: Option<String>,
    pub started_at: i64,
    /// CPU utilization (0..1) samples, oldest first, a few seconds apart.
    pub cpu: Vec<f32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReconcileState {
    pub at: i64,
    pub removed: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FleetStatus {
    pub url: String,
    pub house: String,
    pub share: String,
    pub last_ok_at: Option<i64>,
    pub last_error: Option<String>,
    /// Ledger records written but not yet accepted by the fleet.
    pub pending_records: usize,
}

/// One runner slot's live status, shared between its loop and the publisher.
pub struct Slot {
    pub pool: usize,
    pub slot: usize,
    inner: Mutex<SlotInner>,
}

#[derive(Default)]
struct SlotInner {
    phase: Phase,
    since: i64,
    runner: Option<String>,
    error: Option<String>,
    retry_at: Option<i64>,
    watch: Option<JobWatch>,
    stop: Option<CancellationToken>,
    cpu: VecDeque<f32>,
    last_cpu: Option<(String, i64)>,
}

impl Slot {
    pub fn new(pool: usize, slot: usize) -> Arc<Self> {
        Arc::new(Self {
            pool,
            slot,
            inner: Mutex::new(SlotInner {
                since: timefmt::now(),
                ..Default::default()
            }),
        })
    }

    pub fn set(&self, phase: Phase) {
        let mut s = self.inner.lock().unwrap();
        if s.phase != phase {
            s.phase = phase;
            s.since = timefmt::now();
        }
        if phase != Phase::Backoff {
            s.retry_at = None;
        }
    }

    /// A new runner took over the slot: forget the previous one's job.
    pub fn start_runner(&self, name: &str, stop: CancellationToken) {
        let mut s = self.inner.lock().unwrap();
        s.runner = Some(name.to_string());
        s.stop = Some(stop);
        s.watch = None;
        s.cpu.clear();
        s.last_cpu = None;
    }

    pub fn watch(&self, watch: JobWatch) {
        self.inner.lock().unwrap().watch = Some(watch);
    }

    pub fn end_runner(&self) {
        let mut s = self.inner.lock().unwrap();
        s.runner = None;
        s.stop = None;
        s.watch = None;
    }

    pub fn ok(&self) {
        self.inner.lock().unwrap().error = None;
    }

    pub fn backoff(&self, error: String, retry_at: i64) {
        self.set(Phase::Backoff);
        let mut s = self.inner.lock().unwrap();
        s.error = Some(error);
        s.retry_at = Some(retry_at);
    }

    /// Cancels the running job if this slot owns `runner`.
    fn request_stop(&self, runner: &str) -> bool {
        let s = self.inner.lock().unwrap();
        match (&s.runner, &s.stop) {
            (Some(name), Some(stop)) if name == runner => {
                stop.cancel();
                true
            }
            _ => false,
        }
    }

    /// What the publisher needs: the slot's status, plus the watch to read the job from.
    fn snapshot(&self) -> (SlotState, Option<JobInfo>) {
        let s = self.inner.lock().unwrap();
        let info = s
            .watch
            .as_ref()
            .filter(|w| w.has_started())
            .map(JobWatch::snapshot);
        let phase = if info.is_some() && s.phase == Phase::Idle {
            Phase::Busy
        } else {
            s.phase
        };
        let state = SlotState {
            slot: self.slot,
            phase,
            since: s.since,
            runner: s.runner.clone(),
            error: s.error.clone(),
            retry_at: s.retry_at,
            job: None,
        };
        (state, info)
    }

    /// Adds a CPU sample from the job's `cpu_live` marker if it is new, and returns the history.
    fn sample_cpu(&self, meta: &HashMap<String, String>, cpus: u32) -> Vec<f32> {
        let mut s = self.inner.lock().unwrap();
        if let Some(live) = meta.get("cpu_live")
            && let Some((values, ts)) = live.rsplit_once(' ')
            && let Ok(ts) = ts.parse::<i64>()
        {
            if let Some((prev, prev_ts)) = &s.last_cpu
                && ts > *prev_ts
            {
                let probe: HashMap<String, String> = [
                    ("cpu_mode", meta.get("cpu_mode").map_or("", String::as_str)),
                    ("ncpu", meta.get("ncpu").map_or("", String::as_str)),
                    ("cpu_start", prev.as_str()),
                    ("cpu_end", values),
                    ("hook_start", &prev_ts.to_string()),
                    ("hook_end", &ts.to_string()),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
                if let Some((_, utilization)) = crate::hooks::cpu_usage(&probe, cpus) {
                    s.cpu.push_back(utilization as f32);
                    while s.cpu.len() > CPU_HISTORY {
                        s.cpu.pop_front();
                    }
                }
            }
            if s.last_cpu.as_ref().is_none_or(|(_, prev_ts)| ts > *prev_ts) {
                s.last_cpu = Some((values.to_string(), ts));
            }
        }
        s.cpu.iter().copied().collect()
    }
}

/// Fields of a job the supervisor knows about while it runs.
pub fn live_job(info: &JobInfo, cpu: Vec<f32>) -> LiveJob {
    let get = |k: &str| info.meta.get(k).filter(|v| !v.is_empty()).cloned();
    let url = match (get("repo"), get("run_id")) {
        (Some(repo), Some(run)) => Some(format!(
            "{}/{repo}/actions/runs/{run}",
            get("server_url").unwrap_or_else(|| "https://github.com".into())
        )),
        _ => None,
    };
    LiveJob {
        name: info.name.clone().or_else(|| get("job_key")),
        repo: get("repo"),
        workflow: get("workflow"),
        actor: get("triggering_actor").or_else(|| get("actor")),
        event: get("event"),
        git_ref: get("ref"),
        sha: get("sha"),
        url,
        started_at: info.started.map(timefmt::unix).unwrap_or_else(timefmt::now),
        cpu,
    }
}

/// Everything the publisher reads from, owned by the supervisor.
pub struct Board {
    pub path: PathBuf,
    pub control: PathBuf,
    pub base: Mutex<State>,
    pub slots: Vec<Arc<Slot>>,
}

impl Board {
    pub fn new(data_dir: &Path, base: State) -> Self {
        Self {
            path: data_dir.join(STATE_FILE),
            control: data_dir.join(CONTROL_DIR),
            base: Mutex::new(base),
            slots: Vec::new(),
        }
    }

    pub fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.base.lock().unwrap());
    }

    /// Builds the current state. `extra_meta` supplies job metadata a backend keeps on disk (the
    /// host backend) keyed by runner name; `host_cpu` is this machine's utilization for jobs
    /// that run directly on it.
    pub fn collect(
        &self,
        extra_meta: &HashMap<String, HashMap<String, String>>,
        host_cpu: Option<f32>,
    ) -> State {
        let mut state = self.base.lock().unwrap().clone();
        state.updated_at = timefmt::now();
        for pool in &mut state.pools {
            pool.slots.clear();
        }
        for slot in &self.slots {
            let (mut s, info) = slot.snapshot();
            let Some(pool) = state.pools.get_mut(slot.pool) else {
                continue;
            };
            if let Some(mut info) = info {
                if let Some(extra) = s.runner.as_ref().and_then(|r| extra_meta.get(r)) {
                    for (k, v) in extra {
                        info.meta.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
                let mut cpu = slot.sample_cpu(&info.meta, pool.cpus);
                if cpu.is_empty()
                    && let Some(host) = host_cpu
                {
                    cpu = vec![host];
                }
                s.job = Some(live_job(&info, cpu));
            }
            pool.slots.push(s);
        }
        state
    }

    /// Runner names of slots running a job right now.
    pub fn busy_runners(&self) -> Vec<String> {
        self.slots
            .iter()
            .filter_map(|s| {
                let (state, info) = s.snapshot();
                info.and(state.runner)
            })
            .collect()
    }

    /// Picks up `control/stop-<runner>` requests from the GUI.
    pub fn take_stop_requests(&self) {
        let Ok(entries) = std::fs::read_dir(&self.control) else {
            return;
        };
        for entry in entries.flatten() {
            let file = entry.file_name();
            let Some(runner) = file.to_str().and_then(|n| n.strip_prefix("stop-")) else {
                continue;
            };
            if self.slots.iter().any(|s| s.request_stop(runner)) {
                tracing::warn!(runner, "stopping job at the GUI's request");
            }
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Writes atomically so readers never see a partial file.
pub fn write(path: &Path, state: &State) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
    std::fs::rename(&tmp, path)
}

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn read(data_dir: &Path) -> Option<State> {
    let text = std::fs::read(data_dir.join(STATE_FILE)).ok()?;
    serde_json::from_slice(&text).ok()
}

/// Asks a running supervisor to stop one job.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn request_stop(data_dir: &Path, runner: &str) -> std::io::Result<()> {
    let dir = data_dir.join(CONTROL_DIR);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(format!("stop-{runner}")), b"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turns_cpu_markers_into_utilization() {
        let slot = Slot::new(0, 0);
        let mut meta: HashMap<String, String> = [("cpu_mode", "procstat"), ("ncpu", "4")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        meta.insert("cpu_live".into(), "100 1000 10".into());
        assert!(slot.sample_cpu(&meta, 4).is_empty());
        meta.insert("cpu_live".into(), "300 1400 15".into());
        let cpu = slot.sample_cpu(&meta, 4);
        assert_eq!(cpu.len(), 1);
        assert!((cpu[0] - 0.5).abs() < 1e-6);
        // The same sample twice adds nothing.
        assert_eq!(slot.sample_cpu(&meta, 4).len(), 1);
    }

    #[test]
    fn stop_requests_reach_the_owning_slot() {
        let dir = std::env::temp_dir().join(format!("hr-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut board = Board::new(&dir, State::default());
        let slot = Slot::new(0, 0);
        let token = CancellationToken::new();
        slot.start_runner("hr-a-linux-0-abc", token.clone());
        board.slots.push(slot);
        request_stop(&dir, "hr-a-linux-0-other").unwrap();
        board.take_stop_requests();
        assert!(!token.is_cancelled());
        request_stop(&dir, "hr-a-linux-0-abc").unwrap();
        board.take_stop_requests();
        assert!(token.is_cancelled());
        assert_eq!(std::fs::read_dir(dir.join(CONTROL_DIR)).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }
}
