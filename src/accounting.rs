//! Turns a finished job into a ledger record: who ran what, how long, how much CPU, and what it
//! would have cost on GitHub-hosted and Depot runners.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::config::{PoolConfig, PricingConfig};
use crate::github::GitHub;
use crate::hooks;
use crate::ledger::{self, JobRecord};
use crate::pricing::{self, Pricing, Provider, Sku};
use crate::proc::JobInfo;
use crate::timefmt;

/// The comparable hosted runners for one pool, chosen once at startup.
pub struct PoolPricing {
    pub os: String,
    pub arch: String,
    pub cpus: u32,
    pub github: Option<&'static Sku>,
    pub depot: Option<&'static Sku>,
}

impl PoolPricing {
    pub fn new(cfg: &PoolConfig, os: &str, arch: &str, cpus: u32) -> Self {
        let pick = |id: &Option<String>, provider| match id {
            Some(id) => pricing::find(id),
            None => pricing::closest(provider, os, arch, cpus),
        };
        Self {
            os: os.to_string(),
            arch: arch.to_string(),
            cpus,
            github: pick(&cfg.compare_github, Provider::GitHub),
            depot: pick(&cfg.compare_depot, Provider::Depot),
        }
    }
}

pub struct Accounting {
    gh: Arc<GitHub>,
    pricing: Pricing,
    annotate: bool,
    host: String,
    ledger: PathBuf,
    repo_private: Mutex<HashMap<String, Option<bool>>>,
}

impl Accounting {
    pub fn new(gh: Arc<GitHub>, cfg: PricingConfig, host: String, ledger: PathBuf) -> Self {
        Self {
            gh,
            annotate: cfg.annotate_jobs,
            pricing: Pricing::new(cfg),
            host,
            ledger,
            repo_private: Mutex::default(),
        }
    }

    /// Environment for the job hooks: identifies the host and, if enabled, the rate used for the
    /// "saved ~$X" notice.
    pub fn hook_env(&self, pool: &PoolPricing) -> Vec<(String, String)> {
        let mut env = vec![("HR_HOST".to_string(), self.host.clone())];
        if self.annotate
            && let Some(sku) = pool.github
        {
            env.push(("HR_GH_RATE".into(), format!("{}", self.pricing.rate(sku))));
            env.push(("HR_GH_SKU".into(), sku.label.into()));
        }
        env
    }

    pub async fn record(&self, runner: &str, pool: &PoolConfig, pp: &PoolPricing, info: JobInfo) {
        let record = self.build(runner, pool, pp, info).await;
        info!(
            runner = %runner,
            repo = record.repo.as_deref().unwrap_or("?"),
            actor = record.triggering_actor.as_deref().or(record.actor.as_deref()).unwrap_or("?"),
            result = %record.result,
            "job done in {}, saved {} vs GitHub-hosted",
            ledger::duration(record.duration_secs),
            record.saved_vs_github_usd.map(ledger::usd).unwrap_or_else(|| "?".into()),
        );
        if let Err(e) = ledger::append(&self.ledger, &record) {
            warn!("writing job ledger {}: {e:#}", self.ledger.display());
        }
    }

    async fn build(
        &self,
        runner: &str,
        pool: &PoolConfig,
        pp: &PoolPricing,
        info: JobInfo,
    ) -> JobRecord {
        let meta = &info.meta;
        let get = |k: &str| meta.get(k).filter(|v| !v.is_empty()).cloned();
        let started = timefmt::unix(info.started.unwrap_or_else(SystemTime::now));
        let finished_at = info.finished.unwrap_or_else(SystemTime::now);
        let duration_secs = finished_at
            .duration_since(info.started.unwrap_or(finished_at))
            .unwrap_or_default()
            .as_secs_f64();

        let mut r = JobRecord {
            id: runner.to_string(),
            host: self.host.clone(),
            pool: pool.name.clone(),
            backend: pool.backend.kind().to_string(),
            os: pp.os.clone(),
            arch: pp.arch.clone(),
            cpus: pp.cpus,
            repo: get("repo"),
            workflow: get("workflow"),
            job: info.name.clone().or_else(|| get("job_key")),
            run_id: get("run_id").and_then(|v| v.parse().ok()),
            run_attempt: get("run_attempt").and_then(|v| v.parse().ok()),
            run_number: get("run_number").and_then(|v| v.parse().ok()),
            actor: get("actor"),
            triggering_actor: get("triggering_actor"),
            event: get("event"),
            git_ref: get("ref"),
            sha: get("sha"),
            started: timefmt::rfc3339(started),
            started_at: started,
            finished_at: timefmt::unix(finished_at),
            duration_secs,
            result: info.result.clone().unwrap_or_else(|| "Unknown".into()),
            ..Default::default()
        };

        if let (Some(repo), Some(run_id)) = (&r.repo, r.run_id) {
            let server = get("server_url").unwrap_or_else(|| "https://github.com".into());
            r.url = Some(format!("{server}/{repo}/actions/runs/{run_id}"));
            match self
                .gh
                .find_job(repo, run_id, r.run_attempt.unwrap_or(1), runner)
                .await
            {
                Ok(Some(job)) => {
                    r.job = Some(job.name);
                    r.url = job.html_url.or(r.url.take());
                    r.queued_secs = job
                        .created_at
                        .as_deref()
                        .and_then(timefmt::parse_rfc3339)
                        .map(|created| (started - created).max(0) as f64);
                }
                Ok(None) => {}
                // Usually the app lacks "Actions: Read"; the ledger still has everything else.
                Err(e) => debug!("looking up job details: {e:#}"),
            }
            r.repo_private = self.repo_private(repo).await;
        }

        if let Some((cpu_seconds, utilization)) = hooks::cpu_usage(meta, pp.cpus) {
            r.cpu_seconds = Some(pricing::round_cents(cpu_seconds));
            r.cpu_utilization = Some(pricing::round_cents(utilization));
        }

        let public = r.repo_private == Some(false);
        r.github = pp
            .github
            .map(|sku| self.pricing.compare(sku, duration_secs, public));
        r.depot = pp
            .depot
            .map(|sku| self.pricing.compare(sku, duration_secs, public));
        let (electricity, fee) = self.pricing.own_cost(duration_secs);
        r.electricity_usd = electricity;
        r.self_hosted_fee_usd = fee;
        let own = electricity + fee;
        r.saved_vs_github_usd = r.github.as_ref().map(|c| pricing::round_cents(c.usd - own));
        r.saved_vs_depot_usd = r.depot.as_ref().map(|c| pricing::round_cents(c.usd - own));
        r
    }

    async fn repo_private(&self, repo: &str) -> Option<bool> {
        if let Some(known) = self.repo_private.lock().await.get(repo) {
            return *known;
        }
        match self.gh.repo_private(repo).await {
            Ok(private) => {
                self.repo_private
                    .lock()
                    .await
                    .insert(repo.to_string(), Some(private));
                Some(private)
            }
            Err(e) => {
                debug!("looking up visibility of {repo}: {e:#}");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::time::Duration;

    fn config(dir: &std::path::Path) -> Config {
        std::fs::write(dir.join("token"), "test").unwrap();
        let text = format!(
            r#"
            [github]
            org = "acme"
            [github.auth]
            type = "token"
            file = "{}"
            [host]
            name = "studio-m2"
            [[pool]]
            name = "linux"
            backend = "docker"
            cpus = 4
            arch = "arm64"
            "#,
            dir.join("token").display()
        );
        toml::from_str(&text).unwrap()
    }

    fn info(minutes: u64, repo: &str, actor: &str, result: &str) -> JobInfo {
        let finished = SystemTime::now();
        let meta = [
            ("repo", repo),
            ("workflow", "CI"),
            ("actor", actor),
            ("triggering_actor", actor),
            ("cpu_mode", "usec"),
            ("ncpu", "10"),
            ("cpu_start", "0"),
            ("cpu_end", &(minutes * 60 * 2_000_000).to_string()),
            ("hook_start", "0"),
            ("hook_end", &(minutes * 60).to_string()),
        ];
        JobInfo {
            name: Some("build".into()),
            started: Some(finished - Duration::from_secs(minutes * 60 - 5)),
            finished: Some(finished),
            result: Some(result.into()),
            meta: meta
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[tokio::test]
    async fn prices_a_job_against_hosted_runners() {
        let dir = std::env::temp_dir().join(format!("hr-acct-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = config(&dir);
        let gh = Arc::new(GitHub::from_config(&cfg.github).unwrap());
        let ledger = std::env::var("HOME_RUNNER_DUMP")
            .map(|d| std::path::PathBuf::from(d).join("jobs.jsonl"))
            .unwrap_or_else(|_| dir.join("jobs.jsonl"));
        let acct = Accounting::new(gh, cfg.pricing.clone(), "studio-m2".into(), ledger.clone());
        let pool = &cfg.pools[0];
        let pp = PoolPricing::new(pool, "Linux", "ARM64", 4);
        assert_eq!(pp.github.unwrap().id, "github-linux-arm64-4");
        assert_eq!(pp.depot.unwrap().id, "depot-ubuntu-24.04-arm-4");

        // 10 minutes (minus 5s) on 4 CPUs, using 2 of them.
        let r = acct
            .build(
                "hr-studio-m2-linux-0-abc",
                pool,
                &pp,
                info(10, "acme/app", "octocat", "Succeeded"),
            )
            .await;
        assert_eq!(r.github.as_ref().unwrap().billed_minutes, 10.0);
        assert_eq!(r.github.as_ref().unwrap().usd, 0.08);
        assert!((r.depot.as_ref().unwrap().usd - 0.119).abs() < 0.001);
        assert_eq!(r.cpu_utilization, Some(0.5));
        assert!(r.electricity_usd > 0.0 && r.electricity_usd < 0.001);
        assert!((r.saved_vs_github_usd.unwrap() - (0.08 - r.electricity_usd)).abs() < 1e-9);

        for (i, (mins, repo, actor, result)) in [
            (10, "acme/app", "octocat", "Succeeded"),
            (42, "acme/app", "hubot", "Failed"),
            (7, "acme/api", "octocat", "Succeeded"),
            (95, "acme/mobile", "mona", "Succeeded"),
        ]
        .into_iter()
        .enumerate()
        {
            acct.record(
                &format!("hr-studio-m2-linux-{i}-x"),
                pool,
                &pp,
                info(mins, repo, actor, result),
            )
            .await;
        }
        let records = ledger::load(&[ledger]).unwrap();
        assert_eq!(records.len(), 4);
        std::fs::remove_dir_all(&dir).ok();
    }
}
