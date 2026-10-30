//! What each slot's runner is doing right now, for `state.json` (see `health`) and the desktop app:
//! the job it is running, recent CPU samples, and a way to stop it.
//!
//! The app asks the supervisor to stop a job by creating `control/stop-<runner name>` in the data
//! directory; the supervisor picks it up on its next tick.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::proc::{JobInfo, JobWatch};
use crate::timefmt;

pub const CONTROL_DIR: &str = "control";
/// Recent CPU samples kept per running job.
const CPU_HISTORY: usize = 60;

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

#[derive(Default)]
struct Runner {
    name: String,
    watch: Option<JobWatch>,
    stop: CancellationToken,
    cpu: VecDeque<f32>,
    last_cpu: Option<(String, i64)>,
}

/// The current runner of every slot, keyed by (pool, slot).
#[derive(Default)]
pub struct Live(Mutex<HashMap<(String, usize), Runner>>);

impl Live {
    /// A new runner took over the slot; `stop` is cancelled if someone asks to stop its job.
    pub fn start(&self, pool: &str, slot: usize, name: &str, stop: CancellationToken) {
        self.0.lock().unwrap().insert(
            (pool.to_string(), slot),
            Runner {
                name: name.to_string(),
                stop,
                ..Default::default()
            },
        );
    }

    pub fn watch(&self, pool: &str, slot: usize, watch: JobWatch) {
        if let Some(r) = self.0.lock().unwrap().get_mut(&(pool.to_string(), slot)) {
            r.watch = Some(watch);
        }
    }

    pub fn end(&self, pool: &str, slot: usize) {
        self.0.lock().unwrap().remove(&(pool.to_string(), slot));
    }

    /// The slot's runner name and, once it has started one, its job.
    pub fn job(&self, pool: &str, slot: usize) -> Option<(String, Option<JobInfo>)> {
        let runners = self.0.lock().unwrap();
        let r = runners.get(&(pool.to_string(), slot))?;
        let info = r
            .watch
            .as_ref()
            .filter(|w| w.has_started())
            .map(JobWatch::snapshot);
        Some((r.name.clone(), info))
    }

    /// Adds a CPU sample from the job's `cpu_live` marker if it is new (or `fallback`, this
    /// machine's utilization, for jobs that don't report their own), and returns the history.
    pub fn sample_cpu(
        &self,
        pool: &str,
        slot: usize,
        meta: &HashMap<String, String>,
        cpus: u32,
        fallback: Option<f32>,
    ) -> Vec<f32> {
        let mut runners = self.0.lock().unwrap();
        let Some(r) = runners.get_mut(&(pool.to_string(), slot)) else {
            return vec![];
        };
        let sample = match meta.get("cpu_live").and_then(|v| v.rsplit_once(' ')) {
            Some((values, ts)) => match ts.parse::<i64>() {
                Ok(ts) if r.last_cpu.as_ref().is_none_or(|(_, prev)| ts > *prev) => {
                    let utilization = r.last_cpu.as_ref().and_then(|(prev, prev_ts)| {
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
                        crate::hooks::cpu_usage(&probe, cpus).map(|(_, u)| u as f32)
                    });
                    r.last_cpu = Some((values.to_string(), ts));
                    utilization
                }
                _ => None,
            },
            None => fallback,
        };
        if let Some(u) = sample {
            r.cpu.push_back(u);
            while r.cpu.len() > CPU_HISTORY {
                r.cpu.pop_front();
            }
        }
        r.cpu.iter().copied().collect()
    }

    /// Acts on `control/stop-<runner>` requests.
    pub fn take_stop_requests(&self, data_dir: &Path) {
        let Ok(entries) = std::fs::read_dir(data_dir.join(CONTROL_DIR)) else {
            return;
        };
        for entry in entries.flatten() {
            let file = entry.file_name();
            let Some(runner) = file.to_str().and_then(|n| n.strip_prefix("stop-")) else {
                continue;
            };
            if let Some(r) = self.0.lock().unwrap().values().find(|r| r.name == runner) {
                tracing::warn!(runner, "stopping job on request");
                r.stop.cancel();
            }
            let _ = std::fs::remove_file(entry.path());
        }
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

/// Asks a running supervisor to stop one job.
#[cfg_attr(not(feature = "ui"), allow(dead_code))]
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
        let live = Live::default();
        live.start("linux", 0, "r", CancellationToken::new());
        let mut meta: HashMap<String, String> = [("cpu_mode", "procstat"), ("ncpu", "4")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        meta.insert("cpu_live".into(), "100 1000 10".into());
        assert!(live.sample_cpu("linux", 0, &meta, 4, None).is_empty());
        meta.insert("cpu_live".into(), "300 1400 15".into());
        let cpu = live.sample_cpu("linux", 0, &meta, 4, None);
        assert_eq!(cpu.len(), 1);
        assert!((cpu[0] - 0.5).abs() < 1e-6);
        // The same sample twice adds nothing.
        assert_eq!(live.sample_cpu("linux", 0, &meta, 4, None).len(), 1);
        // Jobs without their own samples use the machine's.
        live.start("host", 0, "h", CancellationToken::new());
        let cpu = live.sample_cpu("host", 0, &HashMap::new(), 4, Some(0.25));
        assert_eq!(cpu, [0.25]);
    }

    #[test]
    fn stop_requests_reach_the_owning_runner() {
        let dir = std::env::temp_dir().join(format!("hr-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let live = Live::default();
        let token = CancellationToken::new();
        live.start("linux", 0, "hr-a-linux-0-abc", token.clone());
        request_stop(&dir, "hr-a-linux-0-other").unwrap();
        live.take_stop_requests(&dir);
        assert!(!token.is_cancelled());
        request_stop(&dir, "hr-a-linux-0-abc").unwrap();
        live.take_stop_requests(&dir);
        assert!(token.is_cancelled());
        assert_eq!(std::fs::read_dir(dir.join(CONTROL_DIR)).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }
}
