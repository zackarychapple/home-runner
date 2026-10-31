//! Reporting to a shared fleet: one Cloudflare Worker (see `fleet-worker/`) that home-runner
//! machines in different houses send a status snapshot to, and read the fleet-wide picture from.
//!
//! A machine joins with the fleet's enroll key and gets its own token. Every report is the
//! machine's whole current state: its pools with slot counts, and running totals worked out from
//! the local ledger. Nothing about individual jobs (repos, workflows, people, errors) is ever
//! sent, and because each report replaces the last one there is nothing to queue or de-duplicate
//! while the machine or the Worker is offline.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::{Config, FleetConfig};
use crate::daemon::Ctx;
use crate::health::{Phase, PoolState};
use crate::ledger::JobRecord;
use crate::timefmt;

/// Version of the report body the Worker is told about.
pub const SCHEMA: u32 = 2;
/// How many UTC days of per-day totals a report carries.
const DAYS_SENT: i64 = 45;
/// Days kept in memory; older ones only matter through the all-time total.
const DAYS_KEPT: i64 = 60;
const USER_AGENT: &str = concat!("home-runner/", env!("CARGO_PKG_VERSION"));

/// Counts a ledger accumulates into, and the numbers the fleet adds up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Totals {
    pub jobs: u64,
    pub runner_secs: f64,
    pub saved_vs_github_usd: f64,
    pub saved_vs_depot_usd: f64,
    pub own_usd: f64,
}

impl Totals {
    fn add(&mut self, r: &JobRecord) {
        self.jobs += 1;
        self.runner_secs += r.duration_secs;
        self.saved_vs_github_usd += r.saved_vs_github_usd.unwrap_or(0.0);
        self.saved_vs_depot_usd += r.saved_vs_depot_usd.unwrap_or(0.0);
        self.own_usd += r.electricity_usd + r.self_hosted_fee_usd;
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DayTotals {
    pub day: String,
    #[serde(flatten)]
    pub totals: Totals,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReportTotals {
    pub all_time: Totals,
    /// The last [`DAYS_SENT`] UTC days that had jobs, oldest first.
    pub days: Vec<DayTotals>,
}

/// Runner slots in each phase.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SlotCounts {
    pub idle: u32,
    pub busy: u32,
    pub starting: u32,
    pub backoff: u32,
    pub paused: u32,
    pub offline: u32,
}

impl SlotCounts {
    /// Slots whose runner is up and waiting for a job.
    pub fn open(&self) -> u32 {
        self.idle
    }

    pub fn total(&self) -> u32 {
        self.idle + self.busy + self.starting + self.backoff + self.paused + self.offline
    }

    pub fn add(&mut self, other: &SlotCounts) {
        self.idle += other.idle;
        self.busy += other.busy;
        self.starting += other.starting;
        self.backoff += other.backoff;
        self.paused += other.paused;
        self.offline += other.offline;
    }
}

/// A pool as the fleet sees it: its shape and how many slots are in each phase.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FleetPool {
    pub name: String,
    pub backend: String,
    pub os: String,
    pub arch: String,
    pub cpus: u32,
    pub labels: Vec<String>,
    pub github_label: Option<String>,
    /// The pool is drained.
    pub paused: bool,
    pub slots: SlotCounts,
}

impl FleetPool {
    /// Only these fields leave the machine: no running job, error text or runner name can slip in
    /// through a field added to `PoolState` later.
    fn from_state(p: &PoolState, online: bool) -> Self {
        let mut slots = SlotCounts::default();
        for slot in &p.slots {
            let counter = match slot.phase {
                _ if !online => &mut slots.offline,
                Phase::Idle => &mut slots.idle,
                Phase::Busy => &mut slots.busy,
                Phase::Starting => &mut slots.starting,
                Phase::Backoff => &mut slots.backoff,
                Phase::Paused => &mut slots.paused,
                Phase::Offline => &mut slots.offline,
            };
            *counter += 1;
        }
        Self {
            name: p.name.clone(),
            backend: p.backend.clone(),
            os: p.os.clone(),
            arch: p.arch.clone(),
            cpus: p.cpus,
            labels: p.labels.clone(),
            github_label: p.github_label.clone(),
            paused: p.paused.is_some(),
            slots,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub schema: u32,
    pub host: String,
    pub house: String,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub cpus: u32,
    /// False in the last report a supervisor sends as it shuts down.
    pub online: bool,
    pub pools: Vec<FleetPool>,
    pub totals: ReportTotals,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Joined {
    pub token: String,
    pub fleet: String,
}

/// The fleet-wide picture from `GET /v1/fleet`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FleetView {
    pub fleet: String,
    pub now: i64,
    pub month: Totals,
    pub all_time: Totals,
    pub houses: Vec<House>,
    pub daily: Vec<Day>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct House {
    pub house: String,
    pub saved_month_usd: f64,
    pub saved_total_usd: f64,
    pub jobs_month: u64,
    pub hosts: Vec<FleetHost>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FleetHost {
    pub host: String,
    pub online: bool,
    pub last_seen: i64,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub cpus: u32,
    pub pools: Vec<FleetPool>,
    pub saved_month_usd: f64,
    pub saved_total_usd: f64,
    pub jobs_month: u64,
    pub jobs_total: u64,
    pub days: Vec<Day>,
}

impl FleetHost {
    /// Slots across every pool of this machine.
    pub fn slots(&self) -> SlotCounts {
        let mut all = SlotCounts::default();
        for p in &self.pools {
            all.add(&p.slots);
        }
        all
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Day {
    pub day: String,
    pub jobs: u64,
    pub saved_vs_github_usd: f64,
}

pub struct Client {
    http: reqwest::Client,
    url: String,
    token: String,
}

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()?)
}

async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
        .unwrap_or_else(|| body.chars().take(200).collect());
    bail!("fleet returned {status}: {message}")
}

impl Client {
    pub fn new(url: &str, token: String) -> Result<Self> {
        Ok(Self {
            http: http()?,
            url: url.trim_end_matches('/').to_string(),
            token,
        })
    }

    /// The fleet this machine has joined, or `None` without a `[fleet]` section.
    pub fn from_config(config: &Config) -> Result<Option<Self>> {
        let Some(fleet) = &config.fleet else {
            return Ok(None);
        };
        let path = config.fleet_token_path();
        let token = std::fs::read_to_string(&path)
            .with_context(|| {
                format!(
                    "reading fleet token {} (run `home-runner fleet join`)",
                    path.display()
                )
            })?
            .trim()
            .to_string();
        Ok(Some(Self::new(&fleet.url, token)?))
    }

    pub async fn report(&self, report: &Report) -> Result<()> {
        let resp = self
            .http
            .post(format!("{}/v1/report", self.url))
            .bearer_auth(&self.token)
            .json(report)
            .send()
            .await?;
        check(resp).await?;
        Ok(())
    }

    pub async fn view(&self) -> Result<FleetView> {
        let resp = self
            .http
            .get(format!("{}/v1/fleet", self.url))
            .bearer_auth(&self.token)
            .send()
            .await?;
        Ok(check(resp).await?.json().await?)
    }
}

/// Trades the fleet's enroll key for this machine's token.
pub async fn join(url: &str, key: &str, host: &str, house: &str) -> Result<Joined> {
    let resp = http()?
        .post(format!("{}/v1/join", url.trim_end_matches('/')))
        .json(&serde_json::json!({ "key": key, "host": host, "house": house }))
        .send()
        .await?;
    Ok(check(resp).await?.json().await?)
}

/// `home-runner fleet join`: stores the token and adds a `[fleet]` section to the config.
pub async fn join_and_configure(
    config_path: &Path,
    url: &str,
    key: &str,
    house: &str,
) -> Result<String> {
    let config = Config::load(config_path)?;
    let url = url.trim_end_matches('/');
    let joined = join(url, key, &config.host_name(), house).await?;
    write_private(&config.fleet_token_path(), &joined.token)?;
    write_fleet_section(config_path, url, house)?;
    Ok(joined.fleet)
}

/// Sets `url` and `house` in `[fleet]`, keeping settings the user already chose (`token_file`,
/// `heartbeat_secs`) and dropping the retired `share` and `hash_private_repos`.
fn write_fleet_section(config_path: &Path, url: &str, house: &str) -> Result<()> {
    let text = std::fs::read_to_string(config_path)?;
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    let mut fleet = toml_edit::Table::new();
    fleet["url"] = toml_edit::value(url);
    fleet["house"] = toml_edit::value(house);
    if let Some(existing) = doc.get("fleet").and_then(|f| f.as_table()) {
        for (k, v) in existing.iter() {
            if !fleet.contains_key(k) && !matches!(k, "share" | "hash_private_repos") {
                fleet[k] = v.clone();
            }
        }
    }
    doc["fleet"] = toml_edit::Item::Table(fleet);
    std::fs::write(config_path, doc.to_string())?;
    Ok(())
}

/// Writes a secret readable only by this user (on Unix; Windows inherits the folder's ACL).
pub fn write_private(path: &Path, contents: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("writing {}", path.display()))?;
        f.write_all(contents.as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Calls `each` with every complete ledger record after byte `offset`, and returns the offset
/// after them. A half-written last line is left for next time. A missing ledger yields nothing.
pub fn read_from(path: &Path, offset: u64, mut each: impl FnMut(JobRecord)) -> Result<u64> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    let len = file.metadata()?.len();
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(offset.min(len)))?;
    let mut pos = offset.min(len);
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line)?;
        if n == 0 || line.last() != Some(&b'\n') {
            break;
        }
        pos += n as u64;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<JobRecord>(&line) {
            Ok(r) => each(r),
            Err(e) => debug!("skipping unreadable ledger line: {e}"),
        }
    }
    Ok(pos)
}

/// Running totals from the local ledger: read once at startup, then only what was appended.
#[derive(Default)]
struct Tally {
    all: Totals,
    days: BTreeMap<String, Totals>,
    /// How far into the ledger this has read.
    offset: u64,
}

impl Tally {
    fn add(&mut self, r: &JobRecord) {
        self.all.add(r);
        self.days
            .entry(timefmt::day(r.started_at))
            .or_default()
            .add(r);
    }

    /// Picks up records appended since the last call. A ledger shorter than what was already
    /// read has been replaced, so everything is counted again from the top.
    fn refresh(&mut self, path: &Path, now: i64) -> Result<()> {
        let len = std::fs::metadata(path).map_or(0, |m| m.len());
        if len < self.offset {
            *self = Self::default();
        }
        let mut records = Vec::new();
        self.offset = read_from(path, self.offset, |r| records.push(r))?;
        for r in &records {
            self.add(r);
        }
        let keep = timefmt::day(now - (DAYS_KEPT - 1) * 86400);
        self.days.retain(|day, _| *day >= keep);
        Ok(())
    }

    fn totals(&self, now: i64) -> ReportTotals {
        let from = timefmt::day(now - (DAYS_SENT - 1) * 86400);
        ReportTotals {
            all_time: self.all,
            days: self
                .days
                .range(from..)
                .map(|(day, totals)| DayTotals {
                    day: day.clone(),
                    totals: *totals,
                })
                .collect(),
        }
    }
}

pub struct Reporter {
    client: Client,
    cfg: FleetConfig,
    host: String,
    ledger: PathBuf,
    tally: Tally,
    ctx: Arc<Ctx>,
}

impl Reporter {
    pub fn new(config: &Config, ctx: Arc<Ctx>) -> Result<Option<Self>> {
        let Some(client) = Client::from_config(config)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            client,
            cfg: config.fleet.clone().expect("checked by from_config"),
            host: config.host_name(),
            ledger: config.ledger_path(),
            tally: Tally::default(),
            ctx,
        }))
    }

    /// Sends a report every `heartbeat_secs`, and soon after each finished job, until shutdown;
    /// then one last report so the fleet shows this machine offline straight away.
    pub async fn run(mut self, job_done: Arc<Notify>, shutdown: CancellationToken) {
        info!(url = %self.cfg.url, house = %self.cfg.house, "reporting to fleet");
        if self.cfg.share.is_some() || self.cfg.hash_private_repos.is_some() {
            warn!(
                "[fleet] share and hash_private_repos are ignored: reports no longer include job details"
            );
        }
        let heartbeat = Duration::from_secs(self.cfg.heartbeat_secs.max(15));
        // The first report should include the slots, which the monitor publishes on its first tick.
        for _ in 0..40 {
            if self.ctx.latest_state().is_some() || shutdown.is_cancelled() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        loop {
            // A failure is logged and shown in the status; the next report carries everything.
            let _ = self.send(true).await;
            tokio::select! {
                _ = tokio::time::sleep(heartbeat) => {}
                _ = job_done.notified() => tokio::time::sleep(Duration::from_secs(2)).await,
                _ = shutdown.cancelled() => break,
            }
        }
        let _ = tokio::time::timeout(Duration::from_secs(10), self.send(false)).await;
    }

    fn build(&mut self, online: bool) -> Report {
        let now = timefmt::now();
        if let Err(e) = self.tally.refresh(&self.ledger, now) {
            warn!("reading the ledger for fleet totals failed: {e:#}");
        }
        let pools = self
            .ctx
            .latest_state()
            .map(|s| s.pools())
            .unwrap_or_default()
            .iter()
            .map(|p| FleetPool::from_state(p, online))
            .collect();
        Report {
            schema: SCHEMA,
            host: self.host.clone(),
            house: self.cfg.house.clone(),
            version: env!("CARGO_PKG_VERSION").into(),
            os: crate::backend::os_label(std::env::consts::OS).into(),
            arch: crate::backend::arch_label(std::env::consts::ARCH).into(),
            cpus: crate::backend::host_cpus(),
            online,
            pools,
            totals: self.tally.totals(now),
        }
    }

    async fn send(&mut self, online: bool) -> Result<()> {
        let report = self.build(online);
        let result = self.client.report(&report).await;
        self.ctx.update_fleet_status(|f| {
            f.url.clone_from(&self.cfg.url);
            f.house.clone_from(&self.cfg.house);
            match &result {
                Ok(()) => {
                    f.last_ok_at = Some(timefmt::now());
                    f.last_error = None;
                }
                Err(e) => f.last_error = Some(format!("{e:#}")),
            }
        });
        if let Err(e) = &result {
            warn!("fleet report failed, will retry: {e:#}");
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::{Hold, SlotState};
    use crate::live::LiveJob;

    fn record(id: &str, started_at: i64) -> JobRecord {
        JobRecord {
            id: id.into(),
            repo: Some("acme/secret-app".into()),
            workflow: Some("Release".into()),
            job: Some("deploy".into()),
            actor: Some("mona".into()),
            sha: Some("abc123".into()),
            started_at,
            duration_secs: 60.0,
            electricity_usd: 0.01,
            self_hosted_fee_usd: 0.02,
            saved_vs_github_usd: Some(0.5),
            saved_vs_depot_usd: Some(0.4),
            ..Default::default()
        }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hr-fleet-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_ledger(path: &Path, records: &[JobRecord]) {
        let mut text = String::new();
        for r in records {
            text += &serde_json::to_string(r).unwrap();
            text += "\n";
        }
        std::fs::write(path, text).unwrap();
    }

    fn append_ledger(path: &Path, records: &[JobRecord]) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        for r in records {
            writeln!(f, "{}", serde_json::to_string(r).unwrap()).unwrap();
        }
    }

    // 2026-10-31 23:59:59 UTC and the second after it.
    const OCT_31_END: i64 = 1_793_491_199;
    const NOV_1: i64 = OCT_31_END + 1;

    #[test]
    fn totals_add_up_and_split_by_utc_day() {
        let mut none = record("none", NOV_1);
        none.saved_vs_github_usd = None;
        none.saved_vs_depot_usd = None;
        let mut t = Tally::default();
        t.add(&record("a", OCT_31_END));
        t.add(&record("b", NOV_1));
        t.add(&none);
        assert_eq!(t.all.jobs, 3);
        assert!((t.all.saved_vs_github_usd - 1.0).abs() < 1e-9);
        assert!((t.all.saved_vs_depot_usd - 0.8).abs() < 1e-9);
        assert!((t.all.own_usd - 0.09).abs() < 1e-9);
        assert!((t.all.runner_secs - 180.0).abs() < 1e-9);
        assert_eq!(t.days.len(), 2);
        assert_eq!(t.days["2026-10-31"].jobs, 1);
        assert_eq!(t.days["2026-11-01"].jobs, 2);
        assert!((t.days["2026-11-01"].saved_vs_github_usd - 0.5).abs() < 1e-9);
    }

    #[test]
    fn sends_only_the_last_45_days() {
        let mut t = Tally::default();
        for d in 0..60 {
            t.add(&record(&d.to_string(), NOV_1 - d * 86400));
        }
        let totals = t.totals(NOV_1);
        assert_eq!(totals.all_time.jobs, 60);
        assert_eq!(totals.days.len(), 45);
        assert_eq!(totals.days.last().unwrap().day, "2026-11-01");
        assert_eq!(totals.days.first().unwrap().day, "2026-09-18");
        assert!(totals.days.windows(2).all(|w| w[0].day < w[1].day));
    }

    #[test]
    fn follows_the_ledger_as_it_grows_and_when_it_is_replaced() {
        let dir = temp("tally");
        let path = dir.join("jobs.jsonl");
        // Nothing yet.
        let mut t = Tally::default();
        t.refresh(&path, NOV_1).unwrap();
        assert_eq!(t.all.jobs, 0);

        write_ledger(&path, &[record("a", NOV_1), record("b", NOV_1)]);
        t.refresh(&path, NOV_1).unwrap();
        assert_eq!(t.all.jobs, 2);
        // Reading again counts nothing twice.
        t.refresh(&path, NOV_1).unwrap();
        assert_eq!(t.all.jobs, 2);

        // A half-written last line waits until it is complete.
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            write!(f, "{{\"id\":\"partial").unwrap();
        }
        t.refresh(&path, NOV_1).unwrap();
        assert_eq!(t.all.jobs, 2);
        let offset = t.offset;
        assert!(offset < std::fs::metadata(&path).unwrap().len());

        // Complete that line and add another record after it.
        write_ledger(&path, &[record("a", NOV_1), record("b", NOV_1)]);
        append_ledger(&path, &[record("c", NOV_1)]);
        t.refresh(&path, NOV_1).unwrap();
        assert_eq!(t.all.jobs, 3);

        // A shorter, replaced ledger is counted from scratch.
        write_ledger(&path, &[record("z", NOV_1)]);
        t.refresh(&path, NOV_1).unwrap();
        assert_eq!(t.all.jobs, 1);
        assert_eq!(t.days["2026-11-01"].jobs, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn old_days_are_dropped_from_memory_but_not_from_the_total() {
        let dir = temp("prune");
        let path = dir.join("jobs.jsonl");
        write_ledger(
            &path,
            &[record("old", NOV_1 - 100 * 86400), record("new", NOV_1)],
        );
        let mut t = Tally::default();
        t.refresh(&path, NOV_1).unwrap();
        assert_eq!(t.all.jobs, 2);
        assert_eq!(t.days.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn pool_with_private_details() -> PoolState {
        let slot = |phase, job: Option<LiveJob>| SlotState {
            pool: "macos".into(),
            phase,
            runner: Some("hr-studio-macos-0-secret".into()),
            last_error: Some("clone of /Users/zack/private failed".into()),
            job,
            ..Default::default()
        };
        let job = LiveJob {
            name: Some("deploy".into()),
            repo: Some("acme/secret-app".into()),
            workflow: Some("Release".into()),
            actor: Some("mona".into()),
            sha: Some("abc123".into()),
            ..Default::default()
        };
        PoolState {
            name: "macos".into(),
            backend: "tart".into(),
            labels: vec!["home".into()],
            os: "macOS".into(),
            arch: "ARM64".into(),
            cpus: 4,
            github_label: Some("github-macos-arm64-3".into()),
            paused: None,
            slots: vec![
                slot(Phase::Idle, None),
                slot(Phase::Busy, Some(job)),
                slot(Phase::Backoff, None),
                slot(Phase::Starting, None),
            ],
            ..Default::default()
        }
    }

    fn keys(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(m) => {
                for (k, v) in m {
                    out.push(k.clone());
                    keys(v, out);
                }
            }
            serde_json::Value::Array(a) => a.iter().for_each(|v| keys(v, out)),
            _ => {}
        }
    }

    #[test]
    fn a_report_is_schema_2_and_carries_no_job_details() {
        let pool = pool_with_private_details();
        let dir = temp("report");
        let path = dir.join("jobs.jsonl");
        write_ledger(&path, &[record("a", NOV_1)]);
        let mut tally = Tally::default();
        tally.refresh(&path, NOV_1).unwrap();
        let report = Report {
            schema: SCHEMA,
            host: "studio".into(),
            house: "Zack's house".into(),
            version: "1.0.0".into(),
            os: "macOS".into(),
            arch: "ARM64".into(),
            cpus: 10,
            online: true,
            pools: vec![FleetPool::from_state(&pool, true)],
            totals: tally.totals(NOV_1),
        };
        let text = serde_json::to_string(&report).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["schema"], 2);
        assert_eq!(json["pools"][0]["slots"]["idle"], 1);
        assert_eq!(json["pools"][0]["slots"]["busy"], 1);
        assert_eq!(json["pools"][0]["slots"]["backoff"], 1);
        assert_eq!(json["pools"][0]["slots"]["starting"], 1);
        assert_eq!(json["pools"][0]["paused"], false);
        assert_eq!(json["totals"]["all_time"]["jobs"], 1);
        assert_eq!(json["totals"]["days"][0]["day"], "2026-11-01");
        assert_eq!(json["totals"]["days"][0]["jobs"], 1);

        let mut all = Vec::new();
        keys(&json, &mut all);
        for forbidden in [
            "repo",
            "workflow",
            "actor",
            "job",
            "last_error",
            "runner",
            "sha",
        ] {
            assert!(!all.iter().any(|k| k == forbidden), "{forbidden} was sent");
        }
        for secret in [
            "secret-app",
            "Release",
            "mona",
            "abc123",
            "hr-studio",
            "/Users/zack",
        ] {
            assert!(!text.contains(secret), "{secret} was sent");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_stopping_machine_reports_every_slot_offline() {
        let mut pool = pool_with_private_details();
        pool.paused = Some(Hold::default());
        let p = FleetPool::from_state(&pool, false);
        assert_eq!(p.slots.offline, 4);
        assert_eq!(p.slots.total(), 4);
        assert_eq!(p.slots.open(), 0);
        assert!(p.paused);
    }

    #[test]
    fn join_keeps_the_settings_the_user_chose() {
        let dir = temp("join");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "# mine\n[github]\norg = \"o\"\n\n[fleet]\nurl = \"https://old.example.workers.dev\"\n\
             house = \"Old\"\nshare = \"full\"\nhash_private_repos = false\n\
             token_file = \"/tmp/tok\"\nheartbeat_secs = 30\n",
        )
        .unwrap();
        write_fleet_section(&path, "https://fleet.example.workers.dev", "Zack's house").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# mine"));
        assert!(!text.contains("share") && !text.contains("hash_private_repos"));
        let doc: toml::Table = text.parse().unwrap();
        let fleet = doc["fleet"].as_table().unwrap();
        assert_eq!(
            fleet["url"].as_str(),
            Some("https://fleet.example.workers.dev")
        );
        assert_eq!(fleet["house"].as_str(), Some("Zack's house"));
        assert_eq!(fleet["token_file"].as_str(), Some("/tmp/tok"));
        assert_eq!(fleet["heartbeat_secs"].as_integer(), Some(30));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_view_parses_what_the_worker_sends() {
        let view: FleetView = serde_json::from_str(
            r#"{"fleet":"team","now":5,"month":{"jobs":3,"runner_secs":9.5,"saved_vs_github_usd":1.5,
            "saved_vs_depot_usd":1.0,"own_usd":0.1,"repos":0},"all_time":{"jobs":10},
            "houses":[{"house":"H","saved_month_usd":1.5,"jobs_month":3,"saved_total_usd":4.0,
            "hosts":[{"host":"a","version":"1","os":"macOS","arch":"ARM64","cpus":8,"online":true,
            "last_seen":4,"pools":[{"name":"p","backend":"tart","os":"macOS","arch":"ARM64","cpus":4,
            "labels":[],"github_label":null,"paused":false,"slots":{"idle":1,"busy":1}}],
            "saved_month_usd":1.5,"saved_total_usd":4.0,"jobs_month":3,"jobs_total":10,
            "days":[{"day":"2026-11-01","jobs":3,"saved_vs_github_usd":1.5}]}]}],
            "daily":[{"day":"2026-11-01","jobs":3,"saved_vs_github_usd":1.5}]}"#,
        )
        .unwrap();
        assert_eq!(view.all_time.jobs, 10);
        let host = &view.houses[0].hosts[0];
        assert_eq!(host.slots().open(), 1);
        assert_eq!(host.slots().total(), 2);
        assert_eq!(host.jobs_total, 10);
    }
}
