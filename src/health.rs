//! What the supervisor tells the outside world about itself: a state file other commands read,
//! drain requests, and alerts when a pool can't keep a runner online.
//!
//! The supervisor and the CLI never talk over a socket. `home-runner drain` writes a flag file in
//! the data directory (which pools, since when, why, until when) and the supervisor notices it
//! within a couple of seconds; the supervisor rewrites `state.json` every few seconds so `status`
//! and `drain --wait` can see what it is doing.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::{AlertFormat, AlertsConfig, Config};
use crate::timefmt;

pub const STATE_FILE: &str = "state.json";
pub const DRAIN_FILE: &str = "drain";
/// A state file older than this means no supervisor is running.
const STATE_STALE_SECS: i64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Preparing the backend, registering with GitHub or launching the runner.
    #[default]
    Starting,
    /// A runner is registered and waiting for a job.
    Idle,
    /// A job is running.
    Busy,
    /// The last attempt failed; waiting before the next one.
    Backoff,
    /// Drained: not running anything.
    Paused,
    /// The machine's supervisor has stopped. Only seen in fleet reports.
    Offline,
}

impl Phase {
    #[cfg_attr(not(feature = "ui"), allow(dead_code))]
    pub fn label(self) -> &'static str {
        match self {
            Phase::Starting => "starting",
            Phase::Idle => "idle",
            Phase::Busy => "running",
            Phase::Backoff => "backoff",
            Phase::Paused => "paused",
            Phase::Offline => "offline",
        }
    }
}

impl SlotState {
    /// Running a job, deliberately stopped, or waiting for one on a runner that has proven it
    /// stays up. A runner that starts and dies, over and over, passes through `Idle` each time
    /// without ever being healthy.
    pub fn is_healthy(&self) -> bool {
        match self.phase {
            Phase::Busy | Phase::Paused => true,
            Phase::Idle => self.proven,
            Phase::Starting | Phase::Backoff | Phase::Offline => false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SlotState {
    pub pool: String,
    pub slot: usize,
    pub phase: Phase,
    /// Unix time the slot entered this phase.
    pub since: i64,
    /// A runner that is up and waiting has stayed up past the quick-exit window, so it isn't
    /// just starting and dying over and over.
    pub proven: bool,
    /// Unix time the slot last stopped being healthy (or started, if it never was).
    pub last_healthy: i64,
    /// Failed attempts since the last successful job or runner start.
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
    pub jobs_done: u64,
    /// The runner this slot currently owns.
    pub runner: Option<String>,
    /// While in backoff: when the next attempt starts.
    pub retry_at: Option<i64>,
    /// The job the runner is running, as far as the supervisor knows so far.
    pub job: Option<crate::live::LiveJob>,
}

/// Every slot's current state, written by the slot loops and read by the health task.
#[derive(Default)]
pub struct Slots(Mutex<Vec<SlotState>>);

impl Slots {
    pub fn register(&self, pool: &str, slot: usize) {
        let now = timefmt::now();
        self.0.lock().unwrap().push(SlotState {
            pool: pool.to_string(),
            slot,
            phase: Phase::Starting,
            since: now,
            proven: false,
            last_healthy: now,
            consecutive_failures: 0,
            last_error: None,
            jobs_done: 0,
            ..Default::default()
        });
    }

    /// Applies `f`, and notes when the slot stops being healthy.
    fn with(&self, pool: &str, slot: usize, f: impl FnOnce(&mut SlotState)) {
        let mut slots = self.0.lock().unwrap();
        if let Some(s) = slots.iter_mut().find(|s| s.pool == pool && s.slot == slot) {
            let was_healthy = s.is_healthy();
            f(s);
            if was_healthy && !s.is_healthy() {
                s.last_healthy = timefmt::now();
            }
        }
    }

    pub fn phase(&self, pool: &str, slot: usize, phase: Phase) {
        let now = timefmt::now();
        self.with(pool, slot, |s| {
            if s.phase != phase {
                s.phase = phase;
                s.since = now;
                s.proven = false;
                s.retry_at = None;
            }
            // Taking a job proves the runner works.
            if phase == Phase::Busy {
                s.consecutive_failures = 0;
            }
        });
    }

    /// The current runner has stayed up long enough that it isn't crash-looping.
    pub fn proven(&self, pool: &str, slot: usize) {
        self.with(pool, slot, |s| {
            s.proven = true;
            s.consecutive_failures = 0;
        });
    }

    pub fn failed(&self, pool: &str, slot: usize, error: String) {
        let now = timefmt::now();
        self.with(pool, slot, |s| {
            s.consecutive_failures += 1;
            s.last_error = Some(error);
            s.proven = false;
            if s.phase != Phase::Backoff {
                s.phase = Phase::Backoff;
                s.since = now;
            }
        });
    }

    /// When a slot in backoff will try again.
    pub fn retry_at(&self, pool: &str, slot: usize, at: i64) {
        self.with(pool, slot, |s| s.retry_at = Some(at));
    }

    pub fn job_done(&self, pool: &str, slot: usize) {
        self.with(pool, slot, |s| {
            s.jobs_done += 1;
            s.consecutive_failures = 0;
        });
    }

    pub fn snapshot(&self) -> Vec<SlotState> {
        self.0.lock().unwrap().clone()
    }
}

/// What `state.json` holds.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub version: String,
    pub host: String,
    pub pid: u32,
    pub started_at: i64,
    pub updated_at: i64,
    /// Every pool is drained (the plain `drain`).
    pub draining: bool,
    /// What is drained, since when, why and until when.
    pub drain: Drain,
    /// Last time GitHub answered an API call.
    pub github_ok_at: i64,
    pub slots: Vec<SlotState>,
    /// Alerts currently firing.
    pub alerts: Vec<String>,
    /// Where runners are registered, e.g. "org acme".
    pub scope: String,
    pub electricity_usd_per_kwh: f64,
    pub job_watts: f64,
    /// Each pool's settings. Its slots are in `slots`; [`State::pools`] puts them together.
    pub pools: Vec<PoolState>,
    pub fleet: Option<FleetStatus>,
}

/// A pool and, when built by [`State::pools`] or received from the fleet, its slots.
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
    /// Set while the pool is drained, by its own request or the whole machine's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused: Option<Hold>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub slots: Vec<SlotState>,
}

/// How fleet reporting is going (see `fleet`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FleetStatus {
    pub url: String,
    pub house: String,
    pub last_ok_at: Option<i64>,
    pub last_error: Option<String>,
}

impl State {
    pub fn is_fresh(&self, now: i64) -> bool {
        now - self.updated_at <= STATE_STALE_SECS
    }

    /// Pools with their slots.
    pub fn pools(&self) -> Vec<PoolState> {
        self.pools
            .iter()
            .map(|p| PoolState {
                slots: self
                    .slots
                    .iter()
                    .filter(|s| s.pool == p.name)
                    .cloned()
                    .collect(),
                paused: self.drain.hold_for(&p.name).cloned(),
                ..p.clone()
            })
            .collect()
    }

    /// Every slot of these pools (every pool, when `pools` is empty) has stopped taking work.
    pub fn drained(&self, pools: &[String]) -> bool {
        self.in_pools(pools).all(|s| s.phase == Phase::Paused)
    }

    /// Jobs running in these pools (every pool, when `pools` is empty).
    pub fn busy_in(&self, pools: &[String]) -> usize {
        self.in_pools(pools)
            .filter(|s| s.phase == Phase::Busy)
            .count()
    }

    fn in_pools<'a>(&'a self, pools: &'a [String]) -> impl Iterator<Item = &'a SlotState> {
        self.slots
            .iter()
            .filter(move |s| pools.is_empty() || pools.contains(&s.pool))
    }
}

pub fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join(STATE_FILE)
}

/// Why, since when and until when something has stopped taking jobs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hold {
    /// Unix time the drain was requested.
    pub since: i64,
    /// Unix time it ends by itself; `None` lasts until `resume`.
    pub until: Option<i64>,
    pub reason: Option<String>,
}

impl Hold {
    fn expired(&self, now: i64) -> bool {
        self.until.is_some_and(|until| until <= now)
    }

    /// `since 2026-10-29 10:05 (until 2026-10-29 12:05): rebuilding the image`
    pub fn describe(&self) -> String {
        let mut text = format!("since {}", timefmt::short(self.since));
        if let Some(until) = self.until {
            text.push_str(&format!(", until {}", timefmt::short(until)));
        }
        if let Some(reason) = &self.reason {
            text.push_str(&format!(": {reason}"));
        }
        text
    }
}

/// The drain flag file: the whole machine, and/or named pools.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Drain {
    /// Every pool.
    pub all: Option<Hold>,
    pub pools: BTreeMap<String, Hold>,
}

impl Drain {
    pub fn is_empty(&self) -> bool {
        self.all.is_none() && self.pools.is_empty()
    }

    /// The hold that keeps this pool from taking jobs, if any. The pool's own comes first.
    pub fn hold_for(&self, pool: &str) -> Option<&Hold> {
        self.pools.get(pool).or(self.all.as_ref())
    }

    /// Without the holds that have run out by `now`.
    pub fn live(&self, now: i64) -> Drain {
        Drain {
            all: self.all.clone().filter(|h| !h.expired(now)),
            pools: self
                .pools
                .iter()
                .filter(|(_, h)| !h.expired(now))
                .map(|(p, h)| (p.clone(), h.clone()))
                .collect(),
        }
    }
}

pub fn drain_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DRAIN_FILE)
}

/// The flag file as written, expired holds included. A file from before pools could be drained
/// separately holds a timestamp, or nothing, and drains the whole machine.
pub fn load_drain(data_dir: &Path) -> Drain {
    let path = drain_path(data_dir);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Drain::default();
    };
    serde_json::from_str(&text).unwrap_or_else(|_| Drain {
        all: Some(Hold {
            since: timefmt::parse_rfc3339(text.trim()).unwrap_or_default(),
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// What is drained right now.
pub fn read_drain(data_dir: &Path, now: i64) -> Drain {
    load_drain(data_dir).live(now)
}

/// Writes the flag file, or removes it when nothing is drained.
pub fn write_drain(data_dir: &Path, drain: &Drain) -> Result<()> {
    let path = drain_path(data_dir);
    if drain.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("removing {}", path.display()))
            }
            _ => Ok(()),
        };
    }
    std::fs::create_dir_all(data_dir)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(drain)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Drops holds that have run out from the flag file, so a later reader doesn't revive them.
pub fn expire_drain(data_dir: &Path, now: i64) -> Result<()> {
    let stored = load_drain(data_dir);
    let live = stored.live(now);
    if live != stored {
        write_drain(data_dir, &live)?;
    }
    Ok(())
}

/// Atomic, so a reader never sees half a file.
pub fn write_state(data_dir: &Path, state: &State) -> Result<()> {
    let path = state_path(data_dir);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

pub fn read_state(data_dir: &Path) -> Option<State> {
    serde_json::from_slice(&std::fs::read(state_path(data_dir)).ok()?).ok()
}

pub fn remove_state(data_dir: &Path) {
    let _ = std::fs::remove_file(state_path(data_dir));
}

// ---------------------------------------------------------------------------------------------
// Alerts

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    /// Stable id of the condition; a `resolved` alert has the same key as the `firing` one.
    pub key: String,
    pub firing: bool,
    pub message: String,
}

/// Decides when to fire and resolve alerts. Pure: callers pass the clock and the observations.
pub struct Health {
    host: String,
    stalled_secs: i64,
    firing: BTreeSet<String>,
}

impl Health {
    pub fn new(host: &str, cfg: &AlertsConfig) -> Self {
        Self {
            host: host.to_string(),
            stalled_secs: cfg.stalled_minutes.max(1) as i64 * 60,
            firing: BTreeSet::new(),
        }
    }

    pub fn firing(&self) -> Vec<String> {
        self.firing.iter().cloned().collect()
    }

    /// Returns only the changes: a condition that keeps holding doesn't alert again.
    pub fn evaluate(&mut self, now: i64, slots: &[SlotState], github_ok_at: i64) -> Vec<Alert> {
        let mut conditions: BTreeMap<String, String> = BTreeMap::new();

        let mut by_pool: BTreeMap<&str, Vec<&SlotState>> = BTreeMap::new();
        for s in slots {
            if !s.is_healthy() && now - s.last_healthy >= self.stalled_secs {
                by_pool.entry(&s.pool).or_default().push(s);
            }
        }
        for (pool, stalled) in by_pool {
            let worst = stalled.iter().min_by_key(|s| s.last_healthy).unwrap();
            let minutes = (now - worst.last_healthy) / 60;
            let mut message = format!(
                "[{}] pool {pool}: {} slot(s) haven't been able to run a job for {minutes} min",
                self.host,
                stalled.len()
            );
            if let Some(error) = worst.last_error.as_deref() {
                message.push_str(&format!(" (last error: {})", one_line(error)));
            }
            conditions.insert(format!("pool:{pool}"), message);
        }

        if now - github_ok_at >= self.stalled_secs {
            conditions.insert(
                "github".into(),
                format!(
                    "[{}] GitHub hasn't answered for {} min",
                    self.host,
                    (now - github_ok_at) / 60
                ),
            );
        }

        let mut alerts = Vec::new();
        for (key, message) in &conditions {
            if self.firing.insert(key.clone()) {
                alerts.push(Alert {
                    key: key.clone(),
                    firing: true,
                    message: message.clone(),
                });
            }
        }
        for key in self.firing.clone() {
            if !conditions.contains_key(&key) {
                self.firing.remove(&key);
                let what = key.strip_prefix("pool:").map_or_else(
                    || "GitHub is reachable again".to_string(),
                    |pool| format!("pool {pool} is running again"),
                );
                alerts.push(Alert {
                    key,
                    firing: false,
                    message: format!("[{}] {what}", self.host),
                });
            }
        }
        alerts
    }
}

fn one_line(text: &str) -> String {
    let line: String = text
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(300)
        .collect();
    line.trim().to_string()
}

/// The request to make for an alert.
pub struct Delivery {
    pub body: String,
    pub content_type: &'static str,
    pub title: Option<String>,
}

pub fn delivery(format: AlertFormat, host: &str, alert: &Alert, now: i64) -> Delivery {
    let status = if alert.firing { "ALERT" } else { "OK" };
    let text = format!("{status}: {}", alert.message);
    match format {
        AlertFormat::Json => Delivery {
            body: json!({
                "event": if alert.firing { "firing" } else { "resolved" },
                "host": host,
                "key": alert.key,
                "message": alert.message,
                "time": timefmt::rfc3339(now),
            })
            .to_string(),
            content_type: "application/json",
            title: None,
        },
        AlertFormat::Slack => Delivery {
            body: json!({ "text": text }).to_string(),
            content_type: "application/json",
            title: None,
        },
        AlertFormat::Discord => Delivery {
            body: json!({ "content": text }).to_string(),
            content_type: "application/json",
            title: None,
        },
        AlertFormat::Ntfy => Delivery {
            body: alert.message.clone(),
            content_type: "text/plain",
            title: Some(format!("home-runner {status}: {host}")),
        },
    }
}

pub struct Notifier {
    http: reqwest::Client,
    url: String,
    format: AlertFormat,
    host: String,
}

impl Notifier {
    pub fn new(host: &str, cfg: &AlertsConfig) -> Result<Option<Self>> {
        let Some(url) = cfg.webhook_url.clone() else {
            return Ok(None);
        };
        Ok(Some(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
            url,
            format: cfg.format,
            host: host.to_string(),
        }))
    }

    pub async fn send(&self, alert: &Alert) -> Result<()> {
        let d = delivery(self.format, &self.host, alert, timefmt::now());
        let mut req = self
            .http
            .post(&self.url)
            .header("Content-Type", d.content_type)
            .body(d.body);
        if let Some(title) = d.title {
            req = req.header("Title", title);
        }
        req.send()
            .await
            .context("sending alert")?
            .error_for_status()
            .context("alert webhook rejected the request")?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// `drain` and `resume`

/// What `home-runner drain` was asked to do.
#[derive(Debug, Default)]
pub struct DrainRequest {
    /// Pools to drain; empty means the whole machine.
    pub pools: Vec<String>,
    /// Resume by itself after this long.
    pub duration: Option<Duration>,
    pub reason: Option<String>,
    /// Wait until no job is running, for at most this long.
    pub wait: Option<Duration>,
}

/// Asks the running supervisor to stop taking jobs: idle runners are deregistered (and their VMs
/// or containers removed), running jobs finish. With `wait`, returns once nothing is running.
pub async fn drain(config: &Config, request: DrainRequest) -> Result<()> {
    let dir = config.data_dir();
    let now = timefmt::now();
    let hold = Hold {
        since: now,
        until: request.duration.map(|d| now + d.as_secs() as i64),
        reason: request.reason.filter(|r| !r.trim().is_empty()),
    };
    let mut drain = load_drain(&dir).live(now);
    if request.pools.is_empty() {
        drain.all = Some(hold);
    } else {
        for pool in &request.pools {
            drain.pools.insert(pool.clone(), hold.clone());
        }
    }
    write_drain(&dir, &drain)?;
    let scope = if request.pools.is_empty() {
        "all pools".to_string()
    } else {
        format!("pool(s) {}", request.pools.join(", "))
    };
    println!(
        "drain requested for {scope}: idle runners are being removed and running jobs will finish"
    );
    if let Some(until) = drain_until(&request.pools, &drain) {
        println!("resumes by itself at {}", timefmt::short(until));
    }
    let Some(timeout) = request.wait else {
        println!("`home-runner resume` puts this machine back in service");
        return Ok(());
    };
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_report = None;
    loop {
        let on_file = read_drain(&dir, timefmt::now());
        let still_drained = if request.pools.is_empty() {
            on_file.all.is_some()
        } else {
            request.pools.iter().all(|p| on_file.hold_for(p).is_some())
        };
        if !still_drained {
            bail!("the drain ended before the jobs finished (resumed, or its --for ran out)");
        }
        match read_state(&dir).filter(|s| s.is_fresh(timefmt::now())) {
            None => {
                println!("no supervisor is running here; nothing to wait for");
                return Ok(());
            }
            Some(state)
                if request
                    .pools
                    .iter()
                    .all(|p| state.drain.hold_for(p).is_some())
                    && (!request.pools.is_empty() || state.draining)
                    && state.drained(&request.pools) =>
            {
                println!("drained: no runners are registered and no jobs are running");
                return Ok(());
            }
            Some(state) => {
                let busy = state.busy_in(&request.pools);
                if last_report != Some(busy) {
                    println!("waiting for {busy} running job(s) to finish…");
                    last_report = Some(busy);
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("still jobs running after the timeout; run `home-runner status` to see them");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// When the drain of these pools (the whole machine when empty) ends by itself, if it does.
fn drain_until(pools: &[String], drain: &Drain) -> Option<i64> {
    if pools.is_empty() {
        drain.all.as_ref()?.until
    } else {
        pools
            .iter()
            .map(|p| drain.hold_for(p).and_then(|h| h.until))
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .max()
    }
}

/// Undoes `drain`: the named pools, or everything when `pools` is empty.
pub fn resume(config: &Config, pools: &[String]) -> Result<()> {
    let dir = config.data_dir();
    let mut drain = load_drain(&dir).live(timefmt::now());
    if drain.is_empty() {
        println!("not draining");
        return Ok(());
    }
    if pools.is_empty() {
        drain = Drain::default();
    } else {
        for pool in pools {
            drain.pools.remove(pool);
        }
    }
    write_drain(&dir, &drain)?;
    println!("resumed: runners will register again");
    if !pools.is_empty() && drain.all.is_some() {
        println!(
            "the whole machine is still drained; `home-runner resume` without --pool ends that"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(minutes: u64) -> AlertsConfig {
        AlertsConfig {
            stalled_minutes: minutes,
            ..Default::default()
        }
    }

    fn slot(pool: &str, n: usize, phase: Phase, last_healthy: i64) -> SlotState {
        SlotState {
            pool: pool.into(),
            slot: n,
            phase,
            since: last_healthy,
            proven: true,
            last_healthy,
            consecutive_failures: 4,
            last_error: Some("registering runner with GitHub\nHTTP 500".into()),
            jobs_done: 0,
            ..Default::default()
        }
    }

    #[test]
    fn alerts_once_when_a_pool_stalls_and_resolves_when_it_recovers() {
        let mut health = Health::new("mini", &cfg(10));
        let t0 = 1_000_000;
        // Backing off for 9 minutes: not yet.
        let slots = [slot("linux", 0, Phase::Backoff, t0)];
        assert!(health.evaluate(t0 + 9 * 60, &slots, t0 + 9 * 60).is_empty());

        // 10 minutes: fires, naming the pool, the duration and the first line of the error.
        let alerts = health.evaluate(t0 + 10 * 60, &slots, t0 + 10 * 60);
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].firing);
        assert_eq!(alerts[0].key, "pool:linux");
        assert_eq!(
            alerts[0].message,
            "[mini] pool linux: 1 slot(s) haven't been able to run a job for 10 min \
             (last error: registering runner with GitHub)"
        );
        assert_eq!(health.firing(), ["pool:linux"]);

        // Still stalled: no repeat.
        assert!(
            health
                .evaluate(t0 + 20 * 60, &slots, t0 + 20 * 60)
                .is_empty()
        );

        // One slot comes back: resolved.
        let slots = [slot("linux", 0, Phase::Idle, t0 + 21 * 60)];
        let alerts = health.evaluate(t0 + 21 * 60, &slots, t0 + 21 * 60);
        assert_eq!(alerts.len(), 1);
        assert!(!alerts[0].firing);
        assert_eq!(alerts[0].message, "[mini] pool linux is running again");
        assert!(health.firing().is_empty());
    }

    #[test]
    fn one_alert_per_pool_however_many_slots_are_down() {
        let mut health = Health::new("mini", &cfg(5));
        let t0 = 5_000;
        let slots = [
            slot("linux", 0, Phase::Backoff, t0),
            slot("linux", 1, Phase::Starting, t0),
            slot("macos", 0, Phase::Idle, t0 + 600),
        ];
        let alerts = health.evaluate(t0 + 600, &slots, t0 + 600);
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].message.contains("2 slot(s)"));
    }

    #[test]
    fn a_runner_that_keeps_dying_is_stalled_even_though_it_reaches_idle() {
        let mut health = Health::new("mini", &cfg(5));
        let mut crash_looping = slot("linux", 0, Phase::Idle, 0);
        crash_looping.proven = false;
        let alerts = health.evaluate(600, &[crash_looping], 600);
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].firing);
    }

    #[test]
    fn busy_idle_and_paused_slots_are_healthy() {
        let mut health = Health::new("mini", &cfg(1));
        let slots = [
            slot("a", 0, Phase::Busy, 0),
            slot("a", 1, Phase::Idle, 0),
            slot("a", 2, Phase::Paused, 0),
        ];
        // `last_healthy` is ancient, but the phases are healthy, so nothing is stalled.
        assert!(health.evaluate(10_000, &slots, 10_000).is_empty());
    }

    #[test]
    fn alerts_when_github_stops_answering() {
        let mut health = Health::new("mini", &cfg(10));
        let slots = [slot("a", 0, Phase::Busy, 0)];
        let alerts = health.evaluate(1_200, &slots, 0);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].key, "github");
        assert_eq!(
            alerts[0].message,
            "[mini] GitHub hasn't answered for 20 min"
        );
        let alerts = health.evaluate(1_300, &slots, 1_290);
        assert_eq!(alerts[0].message, "[mini] GitHub is reachable again");
    }

    #[test]
    fn slot_registry_tracks_failures_and_recovery() {
        let slots = Slots::default();
        slots.register("p", 0);
        slots.failed("p", 0, "boom".into());
        slots.failed("p", 0, "boom again".into());
        let s = &slots.snapshot()[0];
        assert_eq!((s.phase, s.consecutive_failures), (Phase::Backoff, 2));
        assert_eq!(s.last_error.as_deref(), Some("boom again"));

        // Reaching idle isn't recovery yet: the runner may die a moment later.
        slots.phase("p", 0, Phase::Idle);
        let s = &slots.snapshot()[0];
        assert_eq!(
            (s.phase, s.consecutive_failures, s.is_healthy()),
            (Phase::Idle, 2, false)
        );
        slots.proven("p", 0);
        let s = &slots.snapshot()[0];
        assert_eq!((s.consecutive_failures, s.is_healthy()), (0, true));
        // The error stays for `status` to show until the next failure.
        assert_eq!(s.last_error.as_deref(), Some("boom again"));
        slots.job_done("p", 0);
        assert_eq!(slots.snapshot()[0].jobs_done, 1);

        // Failing again starts the clock from the moment it stopped being healthy.
        slots.failed("p", 0, "x".into());
        let s = &slots.snapshot()[0];
        assert!(!s.is_healthy() && s.last_healthy >= s.since);
    }

    #[test]
    fn payloads_match_each_service() {
        let firing = Alert {
            key: "pool:linux".into(),
            firing: true,
            message: "[mini] pool linux is down".into(),
        };
        let json_body: serde_json::Value =
            serde_json::from_str(&delivery(AlertFormat::Json, "mini", &firing, 0).body).unwrap();
        assert_eq!(json_body["event"], "firing");
        assert_eq!(json_body["host"], "mini");
        assert_eq!(json_body["key"], "pool:linux");
        assert_eq!(json_body["time"], "1970-01-01T00:00:00Z");

        let slack: serde_json::Value =
            serde_json::from_str(&delivery(AlertFormat::Slack, "mini", &firing, 0).body).unwrap();
        assert_eq!(slack["text"], "ALERT: [mini] pool linux is down");
        let discord: serde_json::Value =
            serde_json::from_str(&delivery(AlertFormat::Discord, "mini", &firing, 0).body).unwrap();
        assert_eq!(discord["content"], "ALERT: [mini] pool linux is down");

        let resolved = Alert {
            firing: false,
            ..firing
        };
        let ntfy = delivery(AlertFormat::Ntfy, "mini", &resolved, 0);
        assert_eq!(ntfy.content_type, "text/plain");
        assert_eq!(ntfy.title.as_deref(), Some("home-runner OK: mini"));
        assert_eq!(ntfy.body, "[mini] pool linux is down");
    }

    #[test]
    fn state_file_round_trips_and_goes_stale() {
        let dir = std::env::temp_dir().join(format!("hr-health-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = State {
            version: "1".into(),
            host: "mini".into(),
            pid: 7,
            started_at: 100,
            updated_at: 200,
            draining: true,
            github_ok_at: 200,
            slots: vec![
                slot("p", 0, Phase::Paused, 100),
                slot("p", 1, Phase::Busy, 100),
            ],
            alerts: vec![],
            pools: vec![PoolState {
                name: "p".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        write_state(&dir, &state).unwrap();
        let read = read_state(&dir).unwrap();
        assert_eq!(read.pid, 7);
        assert!(!read.drained(&[]));
        assert!(
            read.drained(&["x".to_string()]),
            "no slots of a pool that has none"
        );
        assert_eq!(read.busy_in(&["p".to_string()]), 1);
        let pools = read.pools();
        assert_eq!(pools[0].slots.len(), 2);
        assert!(read.is_fresh(200 + STATE_STALE_SECS));
        assert!(!read.is_fresh(200 + STATE_STALE_SECS + 1));

        remove_state(&dir);
        assert!(read_state(&dir).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn hold(since: i64, until: Option<i64>, reason: &str) -> Hold {
        Hold {
            since,
            until,
            reason: Some(reason.into()),
        }
    }

    #[test]
    fn a_pool_hold_applies_to_that_pool_only() {
        let drain = Drain {
            pools: BTreeMap::from([("macos".into(), hold(1, None, "image"))]),
            ..Default::default()
        };
        assert!(drain.hold_for("macos").is_some());
        assert!(drain.hold_for("linux").is_none());
        let all = Drain {
            all: Some(hold(1, None, "upgrade")),
            ..drain
        };
        assert_eq!(
            all.hold_for("macos").unwrap().reason.as_deref(),
            Some("image"),
            "a pool's own hold wins"
        );
        assert_eq!(
            all.hold_for("linux").unwrap().reason.as_deref(),
            Some("upgrade")
        );
    }

    #[test]
    fn holds_expire_by_themselves() {
        let drain = Drain {
            all: Some(hold(0, Some(100), "a")),
            pools: BTreeMap::from([
                ("macos".into(), hold(0, Some(50), "b")),
                ("linux".into(), hold(0, None, "c")),
            ]),
        };
        assert_eq!(drain.live(49), drain);
        let live = drain.live(50);
        assert!(live.pools.contains_key("linux") && !live.pools.contains_key("macos"));
        assert!(live.all.is_some());
        let live = drain.live(100);
        assert!(live.all.is_none());
        assert_eq!(live.pools.len(), 1, "a hold with no end never expires");
    }

    #[test]
    fn the_flag_file_round_trips_expires_and_reads_the_old_format() {
        let dir = std::env::temp_dir().join(format!("hr-drain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(read_drain(&dir, 0).is_empty());

        // The file before pools could be drained separately: a timestamp, or nothing.
        std::fs::write(drain_path(&dir), "2026-10-29T10:05:00Z\n").unwrap();
        let old = read_drain(&dir, 0);
        assert_eq!(old.all.as_ref().unwrap().since, 1_793_268_300);
        assert!(old.pools.is_empty());
        std::fs::write(drain_path(&dir), "").unwrap();
        assert!(read_drain(&dir, 0).all.is_some());

        let drain = Drain {
            pools: BTreeMap::from([("macos".into(), hold(10, Some(20), "image"))]),
            ..Default::default()
        };
        write_drain(&dir, &drain).unwrap();
        assert_eq!(read_drain(&dir, 19), drain);
        assert!(read_drain(&dir, 20).is_empty());
        assert!(drain_path(&dir).exists(), "still on disk until pruned");
        expire_drain(&dir, 20).unwrap();
        assert!(!drain_path(&dir).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_state_file_from_an_older_version_still_parses() {
        let state: State = serde_json::from_str(
            r#"{"host":"studio","pid":1,"fleet":{"url":"https://fleet.example.workers.dev",
            "house":"H","share":"repos","last_ok_at":5,"last_error":null,"pending_records":3}}"#,
        )
        .unwrap();
        let fleet = state.fleet.unwrap();
        assert_eq!(fleet.house, "H");
        assert_eq!(fleet.last_ok_at, Some(5));
        assert!(fleet.last_error.is_none());
    }
}
