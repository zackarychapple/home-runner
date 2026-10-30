//! Append-only job ledger (`jobs.jsonl`, one JSON object per job) and the `jobs` / `savings` reports.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::pricing::Comparison;
use crate::timefmt;

pub const LEDGER_FILE: &str = "jobs.jsonl";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct JobRecord {
    /// Runner name; unique per job.
    pub id: String,
    pub host: String,
    pub pool: String,
    pub backend: String,
    pub os: String,
    pub arch: String,
    pub cpus: u32,

    pub repo: Option<String>,
    pub repo_private: Option<bool>,
    pub workflow: Option<String>,
    pub job: Option<String>,
    pub run_id: Option<u64>,
    pub run_attempt: Option<u32>,
    pub run_number: Option<u64>,
    pub url: Option<String>,
    pub actor: Option<String>,
    pub triggering_actor: Option<String>,
    pub event: Option<String>,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    pub sha: Option<String>,

    pub started: String,
    pub started_at: i64,
    pub finished_at: i64,
    pub duration_secs: f64,
    /// Time between GitHub queueing the job and this runner starting it.
    pub queued_secs: Option<f64>,
    /// Succeeded, Failed, Canceled, TimedOut, Interrupted, Unknown.
    pub result: String,

    pub cpu_seconds: Option<f64>,
    pub cpu_utilization: Option<f64>,

    pub github: Option<Comparison>,
    pub depot: Option<Comparison>,
    pub electricity_usd: f64,
    pub self_hosted_fee_usd: f64,
    pub saved_vs_github_usd: Option<f64>,
    pub saved_vs_depot_usd: Option<f64>,
}

pub fn append(path: &Path, record: &JobRecord) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut line = serde_json::to_string(record)?;
    line.push('\n');
    // One write per record keeps lines intact even if two processes append at once.
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.write_all(line.as_bytes())?;
    file.flush()?;
    Ok(())
}

/// Reads one or more ledgers (e.g. copied from several machines), oldest first.
pub fn load(paths: &[PathBuf]) -> Result<Vec<JobRecord>> {
    let mut records = Vec::new();
    for path in paths {
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && paths.len() == 1 => continue,
            Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
        };
        for (n, line) in BufReader::new(file).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<JobRecord>(&line) {
                Ok(r) => records.push(r),
                Err(e) => eprintln!("skipping {}:{}: {e}", path.display(), n + 1),
            }
        }
    }
    records.sort_by_key(|r| r.started_at);
    records.dedup_by(|a, b| a.id == b.id);
    Ok(records)
}

pub struct Filter {
    pub since: Option<i64>,
    pub repo: Option<String>,
    pub actor: Option<String>,
}

impl Filter {
    fn keep(&self, r: &JobRecord) -> bool {
        let contains = |field: &Option<String>, needle: &Option<String>| match needle {
            None => true,
            Some(n) => field
                .as_deref()
                .is_some_and(|f| f.to_lowercase().contains(&n.to_lowercase())),
        };
        self.since.is_none_or(|s| r.started_at >= s)
            && contains(&r.repo, &self.repo)
            && (contains(&r.actor, &self.actor) || contains(&r.triggering_actor, &self.actor))
    }

    pub fn apply(&self, records: Vec<JobRecord>) -> Vec<JobRecord> {
        records.into_iter().filter(|r| self.keep(r)).collect()
    }
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Format {
    Table,
    Json,
    Csv,
}

pub fn print_jobs(records: &[JobRecord], limit: usize, format: Format) -> Result<()> {
    let shown = &records[records.len().saturating_sub(limit)..];
    match format {
        Format::Json => {
            for r in shown {
                println!("{}", serde_json::to_string(r)?);
            }
        }
        Format::Csv => write_csv(&mut std::io::stdout().lock(), shown)?,
        Format::Table => {
            if shown.is_empty() {
                println!("no jobs recorded yet");
                return Ok(());
            }
            println!(
                "{:<16}  {:<26}  {:<30}  {:<14}  {:<14}  {:>6}  {:<10}  {:>4}  {:>7}",
                "STARTED (UTC)",
                "REPO",
                "WORKFLOW / JOB",
                "ACTOR",
                "POOL",
                "TIME",
                "RESULT",
                "CPU",
                "SAVED"
            );
            for r in shown {
                let what = match (&r.workflow, &r.job) {
                    (Some(w), Some(j)) => format!("{w} / {j}"),
                    (w, j) => w.clone().or_else(|| j.clone()).unwrap_or_default(),
                };
                println!(
                    "{:<16}  {:<26}  {:<30}  {:<14}  {:<14}  {:>6}  {:<10}  {:>4}  {:>7}",
                    timefmt::short(r.started_at),
                    fit(r.repo.as_deref().unwrap_or("?"), 26),
                    fit(&what, 30),
                    fit(
                        r.triggering_actor
                            .as_deref()
                            .or(r.actor.as_deref())
                            .unwrap_or("?"),
                        14
                    ),
                    fit(&r.pool, 14),
                    duration(r.duration_secs),
                    fit(&r.result, 10),
                    r.cpu_utilization
                        .map(|u| format!("{:.0}%", u * 100.0))
                        .unwrap_or_else(|| "-".into()),
                    r.saved_vs_github_usd.map(usd).unwrap_or_else(|| "-".into()),
                );
            }
            if records.len() > shown.len() {
                println!(
                    "({} older jobs not shown; use --limit)",
                    records.len() - shown.len()
                );
            }
        }
    }
    Ok(())
}

pub fn write_csv(out: &mut impl Write, records: &[JobRecord]) -> Result<()> {
    writeln!(
        out,
        "started,host,pool,repo,workflow,job,actor,triggering_actor,event,ref,result,duration_secs,queued_secs,cpu_seconds,cpu_utilization,github_sku,github_usd,depot_sku,depot_usd,electricity_usd,saved_vs_github_usd,saved_vs_depot_usd,url"
    )?;
    for r in records {
        let f = |v: Option<f64>| v.map(|v| format!("{v:.4}")).unwrap_or_default();
        let s = |v: &Option<String>| csv(v.as_deref().unwrap_or(""));
        writeln!(
            out,
            "{},{},{},{},{},{},{},{},{},{},{},{:.0},{},{},{},{},{},{},{},{:.4},{},{},{}",
            r.started,
            csv(&r.host),
            csv(&r.pool),
            s(&r.repo),
            s(&r.workflow),
            s(&r.job),
            s(&r.actor),
            s(&r.triggering_actor),
            s(&r.event),
            s(&r.git_ref),
            csv(&r.result),
            r.duration_secs,
            f(r.queued_secs),
            f(r.cpu_seconds),
            f(r.cpu_utilization),
            r.github.as_ref().map(|c| c.sku.as_str()).unwrap_or(""),
            f(r.github.as_ref().map(|c| c.usd)),
            r.depot.as_ref().map(|c| c.sku.as_str()).unwrap_or(""),
            f(r.depot.as_ref().map(|c| c.usd)),
            r.electricity_usd,
            f(r.saved_vs_github_usd),
            f(r.saved_vs_depot_usd),
            s(&r.url),
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum GroupBy {
    Repo,
    Actor,
    Pool,
    Host,
    Workflow,
    Day,
    Month,
}

impl GroupBy {
    pub fn key(self, r: &JobRecord) -> String {
        match self {
            GroupBy::Repo => r.repo.clone().unwrap_or_else(|| "(unknown)".into()),
            GroupBy::Actor => r
                .triggering_actor
                .clone()
                .or_else(|| r.actor.clone())
                .unwrap_or_else(|| "(unknown)".into()),
            GroupBy::Pool => format!("{}/{}", r.host, r.pool),
            GroupBy::Host => r.host.clone(),
            GroupBy::Workflow => format!(
                "{} · {}",
                r.repo.as_deref().unwrap_or("?"),
                r.workflow.as_deref().unwrap_or("?")
            ),
            GroupBy::Day => timefmt::day(r.started_at),
            GroupBy::Month => timefmt::month(r.started_at),
        }
    }

    #[cfg(feature = "ui")]
    pub fn label(self) -> &'static str {
        match self {
            GroupBy::Repo => "Repo",
            GroupBy::Actor => "Actor",
            GroupBy::Pool => "Host / pool",
            GroupBy::Host => "Host",
            GroupBy::Workflow => "Repo · workflow",
            GroupBy::Day => "Day",
            GroupBy::Month => "Month",
        }
    }
}

#[derive(Default)]
pub(crate) struct Totals {
    pub jobs: usize,
    pub secs: f64,
    pub cpu_secs: f64,
    pub capacity_secs: f64,
    pub github: f64,
    pub depot: f64,
    pub own: f64,
    pub saved_github: f64,
    pub saved_depot: f64,
    pub failed: usize,
    pub queued_secs: f64,
    pub queued_jobs: usize,
}

impl Totals {
    pub fn add(&mut self, r: &JobRecord) {
        self.jobs += 1;
        self.secs += r.duration_secs;
        if let Some(cpu) = r.cpu_seconds {
            self.cpu_secs += cpu;
            self.capacity_secs += r.duration_secs * f64::from(r.cpus.max(1));
        }
        self.github += r.github.as_ref().map_or(0.0, |c| c.usd);
        self.depot += r.depot.as_ref().map_or(0.0, |c| c.usd);
        self.own += r.electricity_usd + r.self_hosted_fee_usd;
        self.saved_github += r.saved_vs_github_usd.unwrap_or(0.0);
        self.saved_depot += r.saved_vs_depot_usd.unwrap_or(0.0);
        if r.result != "Succeeded" {
            self.failed += 1;
        }
        if let Some(q) = r.queued_secs {
            self.queued_secs += q;
            self.queued_jobs += 1;
        }
    }

    pub fn utilization(&self) -> String {
        if self.capacity_secs > 0.0 {
            format!("{:.0}%", 100.0 * self.cpu_secs / self.capacity_secs)
        } else {
            "-".into()
        }
    }
}

pub fn print_savings(records: &[JobRecord], by: GroupBy, period: &str) {
    if records.is_empty() {
        println!("no jobs recorded {period}");
        return;
    }
    let mut total = Totals::default();
    let mut groups: BTreeMap<String, Totals> = BTreeMap::new();
    for r in records {
        total.add(r);
        let key = by.key(r);
        groups.entry(key).or_default().add(r);
    }

    let first = records
        .first()
        .map(|r| timefmt::short(r.started_at))
        .unwrap_or_default();
    let last = records
        .last()
        .map(|r| timefmt::short(r.started_at))
        .unwrap_or_default();
    println!("home-runner savings {period} ({first} → {last} UTC)\n");
    println!(
        "  jobs                 {} ({} not successful)",
        total.jobs, total.failed
    );
    println!("  runner time          {}", hours(total.secs));
    println!(
        "  CPU used             {:.1} core-hours (avg utilization {})",
        total.cpu_secs / 3600.0,
        total.utilization()
    );
    if total.queued_jobs > 0 {
        println!(
            "  avg queue wait       {}",
            duration(total.queued_secs / total.queued_jobs as f64)
        );
    }
    println!();
    println!(
        "  GitHub-hosted would have cost   {:>10}",
        usd(total.github)
    );
    println!("  Depot would have cost           {:>10}", usd(total.depot));
    println!("  your cost (power + fees)        {:>10}", usd(total.own));
    println!(
        "  saved vs GitHub                 {:>10}",
        usd(total.saved_github)
    );
    println!(
        "  saved vs Depot                  {:>10}",
        usd(total.saved_depot)
    );
    println!();

    let mut rows: Vec<_> = groups.into_iter().collect();
    if !matches!(by, GroupBy::Day | GroupBy::Month) {
        rows.sort_by(|a, b| b.1.saved_github.total_cmp(&a.1.saved_github));
    }
    println!(
        "  {:<34}  {:>5}  {:>8}  {:>5}  {:>9}  {:>9}  {:>9}",
        match by {
            GroupBy::Repo => "REPO",
            GroupBy::Actor => "ACTOR",
            GroupBy::Pool => "HOST/POOL",
            GroupBy::Host => "HOST",
            GroupBy::Workflow => "REPO · WORKFLOW",
            GroupBy::Day => "DAY",
            GroupBy::Month => "MONTH",
        },
        "JOBS",
        "TIME",
        "CPU",
        "GITHUB $",
        "DEPOT $",
        "SAVED"
    );
    for (key, t) in rows {
        println!(
            "  {:<34}  {:>5}  {:>8}  {:>5}  {:>9}  {:>9}  {:>9}",
            fit(&key, 34),
            t.jobs,
            hours(t.secs),
            t.utilization(),
            usd(t.github),
            usd(t.depot),
            usd(t.saved_github)
        );
    }
}

/// One-line lifetime summary for `home-runner status`.
pub fn summary_line(records: &[JobRecord]) -> Option<String> {
    if records.is_empty() {
        return None;
    }
    let mut t = Totals::default();
    records.iter().for_each(|r| t.add(r));
    Some(format!(
        "{} jobs, {} of runner time, {} saved vs GitHub-hosted ({} vs Depot)",
        t.jobs,
        hours(t.secs),
        usd(t.saved_github),
        usd(t.saved_depot)
    ))
}

pub fn usd(v: f64) -> String {
    if v.abs() < 10.0 {
        format!("${v:.2}")
    } else {
        format!("${v:.0}")
    }
}

pub fn duration(secs: f64) -> String {
    let secs = secs.max(0.0).round() as u64;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, secs % 3600 / 60)
    }
}

pub fn hours(secs: f64) -> String {
    format!("{:.1}h", secs / 3600.0)
}

fn fit(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn csv(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}
