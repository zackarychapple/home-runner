//! Reporting to a shared fleet: one Cloudflare Worker (see `fleet-worker/`) that home-runner
//! machines in different houses send heartbeats and job records to, and read fleet-wide totals
//! from.
//!
//! Each machine joins with an invite code and gets its own token. Job records are sent from the
//! local ledger in order, remembering how far it got (`fleet-cursor`), so nothing is lost while
//! the machine or the Worker is offline; the Worker ignores records it already has. What leaves
//! the machine is cut down to the `share` level first.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::{Config, FleetConfig, Share};
use crate::ledger::JobRecord;
use crate::state::{Board, FleetStatus, LiveJob, PoolState};
use crate::timefmt;

pub const CURSOR_FILE: &str = "fleet-cursor";
/// Job records per report; a machine catching up sends several reports back to back.
const BATCH: usize = 200;
const USER_AGENT: &str = concat!("home-runner/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub host: String,
    pub house: String,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub cpus: u32,
    pub share: Share,
    /// False in the last report a supervisor sends as it shuts down.
    pub online: bool,
    pub pools: Vec<PoolState>,
    pub jobs: Vec<JobRecord>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ReportResponse {
    pub accepted: usize,
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
    pub houses: Vec<House>,
    pub daily: Vec<Day>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Totals {
    pub jobs: u64,
    pub runner_secs: f64,
    pub saved_vs_github_usd: f64,
    pub saved_vs_depot_usd: f64,
    pub own_usd: f64,
    pub repos: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct House {
    pub house: String,
    pub saved_month_usd: f64,
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
    pub share: String,
    pub pools: Vec<PoolState>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Day {
    pub day: String,
    pub jobs: u64,
    pub saved_vs_github_usd: f64,
}

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
/// A job from any machine in the fleet.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FleetJob {
    pub house: String,
    #[serde(flatten)]
    pub record: JobRecord,
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

    pub async fn report(&self, report: &Report) -> Result<ReportResponse> {
        let resp = self
            .http
            .post(format!("{}/v1/report", self.url))
            .bearer_auth(&self.token)
            .json(report)
            .send()
            .await?;
        Ok(check(resp).await?.json().await?)
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

    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub async fn jobs(&self, since: i64, limit: usize) -> Result<Vec<FleetJob>> {
        #[derive(Deserialize)]
        struct Page {
            jobs: Vec<FleetJob>,
        }
        let resp = self
            .http
            .get(format!("{}/v1/jobs?since={since}&limit={limit}", self.url))
            .bearer_auth(&self.token)
            .send()
            .await?;
        Ok(check(resp).await?.json::<Page>().await?.jobs)
    }
}

/// Trades an invite code for this machine's token.
pub async fn join(url: &str, invite: &str, host: &str, house: &str) -> Result<Joined> {
    let resp = http()?
        .post(format!("{}/v1/join", url.trim_end_matches('/')))
        .json(&serde_json::json!({ "invite": invite, "host": host, "house": house }))
        .send()
        .await?;
    Ok(check(resp).await?.json().await?)
}

/// `home-runner fleet join`: stores the token and adds a `[fleet]` section to the config.
pub async fn join_and_configure(
    config_path: &Path,
    url: &str,
    invite: &str,
    house: &str,
    share: Share,
) -> Result<String> {
    let config = Config::load(config_path)?;
    let url = url.trim_end_matches('/');
    let joined = join(url, invite, &config.host_name(), house).await?;
    let token_path = config.fleet_token_path();
    write_private(&token_path, &joined.token)?;

    let text = std::fs::read_to_string(config_path)?;
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    let mut fleet = toml_edit::Table::new();
    fleet["url"] = toml_edit::value(url);
    fleet["house"] = toml_edit::value(house);
    fleet["share"] = toml_edit::value(share.as_str());
    if let Some(existing) = doc.get("fleet").and_then(|f| f.as_table()) {
        // Keep settings the user already chose, like token_file or heartbeat_secs.
        for (k, v) in existing.iter() {
            if !fleet.contains_key(k) {
                fleet[k] = v.clone();
            }
        }
    }
    doc["fleet"] = toml_edit::Item::Table(fleet);
    std::fs::write(config_path, doc.to_string())?;
    Ok(joined.fleet)
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

/// A stable stand-in for a private repo's name.
pub fn hash_repo(repo: &str) -> String {
    let digest = Sha256::digest(repo.to_lowercase().as_bytes());
    let hex: String = digest[..5].iter().map(|b| format!("{b:02x}")).collect();
    format!("private-{hex}")
}

/// Cuts a ledger record down to what `share` allows.
pub fn redact(mut r: JobRecord, share: Share, hash_private: bool) -> JobRecord {
    if share != Share::Full {
        r.actor = None;
        r.triggering_actor = None;
        r.event = None;
        r.git_ref = None;
        r.sha = None;
        r.url = None;
        r.run_id = None;
        r.run_attempt = None;
        r.run_number = None;
    }
    match share {
        Share::Totals => {
            r.repo = None;
            r.workflow = None;
            r.job = None;
        }
        // Unknown visibility counts as private.
        Share::Repos if hash_private && r.repo_private != Some(false) => {
            r.repo = r.repo.as_deref().map(hash_repo);
            r.workflow = None;
            r.job = None;
        }
        _ => {}
    }
    r
}

/// The same for a running job, whose repo visibility isn't known yet.
pub fn redact_live(mut j: LiveJob, share: Share, hash_private: bool) -> LiveJob {
    if share != Share::Full {
        j.actor = None;
        j.event = None;
        j.git_ref = None;
        j.sha = None;
        j.url = None;
    }
    match share {
        Share::Totals => {
            j.repo = None;
            j.workflow = None;
            j.name = None;
        }
        Share::Repos if hash_private => {
            j.repo = j.repo.as_deref().map(hash_repo);
            j.workflow = None;
            j.name = None;
        }
        _ => {}
    }
    j
}

/// Up to `max` complete records after byte `offset`, the offset after them, and how many
/// complete records remain beyond those. A shorter file than `offset` means the ledger was
/// replaced, so start over (the Worker ignores duplicates).
pub fn read_from(path: &Path, offset: u64, max: usize) -> Result<(Vec<JobRecord>, u64, usize)> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((vec![], 0, 0)),
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    let len = file.metadata()?.len();
    let offset = if offset > len { 0 } else { offset };
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let mut records = Vec::new();
    let mut consumed = 0usize;
    let mut remaining = 0usize;
    let mut pos = 0usize;
    while let Some(nl) = buf[pos..].iter().position(|&b| b == b'\n') {
        let line = &buf[pos..pos + nl];
        pos += nl + 1;
        if records.len() >= max {
            remaining += usize::from(!line.iter().all(u8::is_ascii_whitespace));
            continue;
        }
        consumed = pos;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<JobRecord>(line) {
            Ok(r) => records.push(r),
            Err(e) => debug!("skipping unreadable ledger line: {e}"),
        }
    }
    Ok((records, offset + consumed as u64, remaining))
}

fn read_cursor(path: &Path) -> u64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn write_cursor(path: &Path, offset: u64) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, offset.to_string())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub struct Reporter {
    client: Client,
    cfg: FleetConfig,
    host: String,
    ledger: PathBuf,
    cursor: PathBuf,
    board: Arc<Board>,
}

impl Reporter {
    pub fn new(config: &Config, board: Arc<Board>) -> Result<Option<Self>> {
        let Some(client) = Client::from_config(config)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            client,
            cfg: config.fleet.clone().expect("checked by from_config"),
            host: config.host_name(),
            ledger: config.ledger_path(),
            cursor: config.data_dir().join(CURSOR_FILE),
            board,
        }))
    }

    /// Sends a report every `heartbeat_secs`, and soon after each finished job, until shutdown;
    /// then one last report so the fleet shows this machine offline straight away.
    pub async fn run(self, job_done: Arc<Notify>, shutdown: CancellationToken) {
        info!(url = %self.cfg.url, house = %self.cfg.house, share = self.cfg.share.as_str(), "reporting to fleet");
        let heartbeat = Duration::from_secs(self.cfg.heartbeat_secs.max(15));
        loop {
            // Catch up in batches, then wait.
            while let Ok(true) = self.send(true).await {}
            tokio::select! {
                _ = tokio::time::sleep(heartbeat) => {}
                _ = job_done.notified() => tokio::time::sleep(Duration::from_secs(2)).await,
                _ = shutdown.cancelled() => break,
            }
        }
        let _ = tokio::time::timeout(Duration::from_secs(10), self.send(false)).await;
    }

    /// One report. Returns whether more ledger records are waiting.
    async fn send(&self, online: bool) -> Result<bool> {
        let offset = read_cursor(&self.cursor);
        let (records, next, remaining) = read_from(&self.ledger, offset, BATCH)?;
        let sent = records.len();
        let state = self.board.collect(&HashMap::new(), None);
        let (share, hash) = (self.cfg.share, self.cfg.hash_private_repos);
        let pools = state
            .pools
            .into_iter()
            .map(|mut p| {
                for slot in &mut p.slots {
                    slot.job = slot.job.take().map(|j| redact_live(j, share, hash));
                    if !online {
                        slot.phase = crate::state::Phase::Stopped;
                    }
                }
                p
            })
            .collect();
        let report = Report {
            host: self.host.clone(),
            house: self.cfg.house.clone(),
            version: env!("CARGO_PKG_VERSION").into(),
            os: crate::backend::os_label(std::env::consts::OS).into(),
            arch: crate::backend::arch_label(std::env::consts::ARCH).into(),
            cpus: crate::backend::host_cpus(),
            share,
            online,
            pools,
            jobs: records
                .into_iter()
                .map(|r| redact(r, share, hash))
                .collect(),
        };
        let status = |error: Option<String>, pending: usize| {
            self.board.update(|s| {
                let f = s.fleet.get_or_insert_with(|| FleetStatus {
                    url: self.cfg.url.clone(),
                    house: self.cfg.house.clone(),
                    share: share.as_str().into(),
                    ..Default::default()
                });
                if error.is_none() {
                    f.last_ok_at = Some(timefmt::now());
                }
                f.last_error = error;
                f.pending_records = pending;
            })
        };
        match self.client.report(&report).await {
            Ok(resp) => {
                if next != offset {
                    write_cursor(&self.cursor, next)?;
                }
                if sent > 0 {
                    debug!(sent, accepted = resp.accepted, "reported jobs to fleet");
                }
                status(None, remaining);
                Ok(remaining > 0)
            }
            Err(e) => {
                warn!("fleet report failed, will retry: {e:#}");
                status(Some(format!("{e:#}")), sent + remaining);
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, private: Option<bool>) -> JobRecord {
        JobRecord {
            id: id.into(),
            repo: Some("acme/app".into()),
            repo_private: private,
            workflow: Some("CI".into()),
            job: Some("build".into()),
            actor: Some("mona".into()),
            git_ref: Some("main".into()),
            sha: Some("abc".into()),
            url: Some("https://github.com/acme/app/actions/runs/1".into()),
            saved_vs_github_usd: Some(0.5),
            ..Default::default()
        }
    }

    #[test]
    fn redacts_to_the_share_level() {
        let full = redact(record("a", Some(true)), Share::Full, true);
        assert_eq!(full.actor.as_deref(), Some("mona"));
        assert_eq!(full.repo.as_deref(), Some("acme/app"));

        let public = redact(record("a", Some(false)), Share::Repos, true);
        assert_eq!(public.repo.as_deref(), Some("acme/app"));
        assert_eq!(public.workflow.as_deref(), Some("CI"));
        assert!(public.actor.is_none() && public.sha.is_none() && public.url.is_none());

        let private = redact(record("a", None), Share::Repos, true);
        assert_eq!(private.repo, Some(hash_repo("acme/app")));
        assert!(private.workflow.is_none());

        let named = redact(record("a", Some(true)), Share::Repos, false);
        assert_eq!(named.repo.as_deref(), Some("acme/app"));

        let totals = redact(record("a", Some(false)), Share::Totals, false);
        assert!(totals.repo.is_none() && totals.job.is_none());
        assert_eq!(totals.saved_vs_github_usd, Some(0.5));
        assert_eq!(hash_repo("Acme/App"), hash_repo("acme/app"));
    }

    #[test]
    fn reads_the_ledger_in_batches_from_a_cursor() {
        let dir = std::env::temp_dir().join(format!("hr-fleet-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("jobs.jsonl");
        let mut text = String::new();
        for i in 0..5 {
            text += &serde_json::to_string(&record(&format!("r{i}"), None)).unwrap();
            text += "\n";
        }
        // A half-written last line is left for next time.
        text += "{\"id\":\"partial";
        std::fs::write(&path, &text).unwrap();

        let (batch, next, remaining) = read_from(&path, 0, 2).unwrap();
        assert_eq!(
            batch.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["r0", "r1"]
        );
        assert_eq!(remaining, 3);
        let (batch, next, remaining) = read_from(&path, next, 10).unwrap();
        assert_eq!(batch.len(), 3);
        assert_eq!(remaining, 0);
        let (batch, again, _) = read_from(&path, next, 10).unwrap();
        assert!(batch.is_empty() && again == next);
        // A replaced, shorter ledger starts from the top.
        std::fs::write(&path, "").unwrap();
        assert_eq!(read_from(&path, next, 10).unwrap().1, 0);
        std::fs::remove_dir_all(&dir).ok();
    }
}
