//! `home-runner profile`: where jobs spend their time and whether the caches are helping, from the
//! step timings, cache sizes, sccache counters, resource samples and startup timings in the ledger.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::ledger::{JobRecord, duration};

#[derive(Debug, Serialize)]
pub struct Profile {
    pub jobs: Vec<JobProfile>,
    pub steps: Vec<StepProfile>,
    pub startup: Vec<StartupProfile>,
}

#[derive(Debug, Default, Serialize)]
pub struct JobProfile {
    /// `repo · workflow / job`
    pub name: String,
    pub pool: String,
    pub runs: usize,
    pub median_secs: f64,
    pub p95_secs: f64,
    pub max_secs: f64,
    pub median_queue_secs: Option<f64>,
    pub avg_cpu: Option<f64>,
    pub median_mem_peak_mb: Option<f64>,
    pub mem_total_mb: Option<f64>,
    /// Runs that began with a warm `/cache`, among runs that reported cache data.
    pub warm_runs: usize,
    pub cold_runs: usize,
    pub median_warm_secs: Option<f64>,
    pub median_cold_secs: Option<f64>,
    pub sccache_hit_rate: Option<f64>,
    /// Average MiB each cache directory grew by per run: what had to be fetched or built.
    pub avg_growth_mb: BTreeMap<String, f64>,
}

#[derive(Debug, Default, Serialize)]
pub struct StepProfile {
    pub job: String,
    pub step: String,
    pub runs: usize,
    pub avg_secs: f64,
    pub p95_secs: f64,
    pub total_secs: f64,
    /// Share of the time all these jobs spent running.
    pub share_of_jobs: f64,
}

#[derive(Debug, Default, Serialize)]
pub struct StartupProfile {
    pub pool: String,
    pub runs: usize,
    pub median_register_secs: Option<f64>,
    pub median_launch_secs: Option<f64>,
    pub median_ready_secs: Option<f64>,
}

fn sorted(mut values: Vec<f64>) -> Vec<f64> {
    values.sort_by(f64::total_cmp);
    values
}

fn quantile(values: &[f64], q: f64) -> Option<f64> {
    let sorted = sorted(values.to_vec());
    (!sorted.is_empty()).then(|| sorted[((sorted.len() - 1) as f64 * q).round() as usize])
}

fn median(values: &[f64]) -> Option<f64> {
    quantile(values, 0.5)
}

fn mean(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

fn job_name(r: &JobRecord) -> String {
    format!(
        "{} · {} / {}",
        r.repo.as_deref().unwrap_or("?"),
        r.workflow.as_deref().unwrap_or("?"),
        r.job.as_deref().unwrap_or("?")
    )
}

pub fn build(records: &[JobRecord]) -> Profile {
    let mut by_job: BTreeMap<(String, String), Vec<&JobRecord>> = BTreeMap::new();
    for r in records {
        by_job
            .entry((job_name(r), r.pool.clone()))
            .or_default()
            .push(r);
    }

    let mut jobs: Vec<JobProfile> = by_job
        .into_iter()
        .map(|((name, pool), runs)| {
            let secs: Vec<f64> = runs.iter().map(|r| r.duration_secs).collect();
            let with_cache: Vec<&&JobRecord> = runs.iter().filter(|r| r.cache.is_some()).collect();
            let secs_where = |warm: bool| -> Vec<f64> {
                with_cache
                    .iter()
                    .filter(|r| r.cache.as_ref().is_some_and(|c| c.warm == warm))
                    .map(|r| r.duration_secs)
                    .collect()
            };
            let (warm, cold) = (secs_where(true), secs_where(false));
            let (mut hits, mut misses) = (0u64, 0u64);
            let mut growth: BTreeMap<String, Vec<f64>> = BTreeMap::new();
            for c in with_cache.iter().filter_map(|r| r.cache.as_ref()) {
                hits += c.sccache_hits.unwrap_or(0);
                misses += c.sccache_misses.unwrap_or(0);
                for dir in c.end_kb.keys() {
                    // Growth needs a start size; a cold start counts as everything being new.
                    let before = c.start_kb.get(dir).copied().unwrap_or(0);
                    let grew = c.end_kb[dir].saturating_sub(before);
                    growth
                        .entry(dir.clone())
                        .or_default()
                        .push(grew as f64 / 1024.0);
                }
            }
            let mem_peaks: Vec<f64> = runs
                .iter()
                .filter_map(|r| r.resources.as_ref()?.mem_peak_kb)
                .map(|kb| kb as f64 / 1024.0)
                .collect();
            JobProfile {
                name,
                pool,
                runs: runs.len(),
                median_secs: median(&secs).unwrap_or(0.0),
                p95_secs: quantile(&secs, 0.95).unwrap_or(0.0),
                max_secs: secs.iter().copied().fold(0.0, f64::max),
                median_queue_secs: median(
                    &runs
                        .iter()
                        .filter_map(|r| r.queued_secs)
                        .collect::<Vec<_>>(),
                ),
                avg_cpu: mean(
                    &runs
                        .iter()
                        .filter_map(|r| r.cpu_utilization)
                        .collect::<Vec<_>>(),
                ),
                median_mem_peak_mb: median(&mem_peaks),
                mem_total_mb: runs
                    .iter()
                    .find_map(|r| r.resources.as_ref()?.mem_total_kb)
                    .map(|kb| kb as f64 / 1024.0),
                warm_runs: warm.len(),
                cold_runs: cold.len(),
                median_warm_secs: median(&warm),
                median_cold_secs: median(&cold),
                sccache_hit_rate: (hits + misses > 0).then(|| hits as f64 / (hits + misses) as f64),
                avg_growth_mb: growth
                    .into_iter()
                    .filter_map(|(dir, v)| Some((dir, mean(&v)?)))
                    .collect(),
            }
        })
        .collect();
    // Time spent is what's worth optimising, so biggest total first.
    jobs.sort_by(|a, b| {
        (b.median_secs * b.runs as f64).total_cmp(&(a.median_secs * a.runs as f64))
    });

    let total_secs: f64 = records.iter().map(|r| r.duration_secs).sum();
    let mut by_step: BTreeMap<(String, String), Vec<f64>> = BTreeMap::new();
    for r in records {
        let job = job_name(r);
        for s in &r.steps {
            // Steps that were skipped have no time.
            if s.conclusion.as_deref() != Some("skipped") {
                by_step
                    .entry((job.clone(), s.name.clone()))
                    .or_default()
                    .push(s.secs);
            }
        }
    }
    let mut steps: Vec<StepProfile> = by_step
        .into_iter()
        .map(|((job, step), secs)| {
            let total: f64 = secs.iter().sum();
            StepProfile {
                job,
                step,
                runs: secs.len(),
                avg_secs: total / secs.len() as f64,
                p95_secs: quantile(&secs, 0.95).unwrap_or(0.0),
                total_secs: total,
                share_of_jobs: if total_secs > 0.0 {
                    total / total_secs
                } else {
                    0.0
                },
            }
        })
        .collect();
    steps.sort_by(|a, b| b.total_secs.total_cmp(&a.total_secs));

    let mut by_pool: BTreeMap<String, Vec<&JobRecord>> = BTreeMap::new();
    for r in records.iter().filter(|r| r.startup.is_some()) {
        by_pool
            .entry(format!("{}/{}", r.host, r.pool))
            .or_default()
            .push(r);
    }
    let startup = by_pool
        .into_iter()
        .map(|(pool, runs)| {
            let col = |f: fn(&crate::ledger::Startup) -> Option<f64>| {
                median(
                    &runs
                        .iter()
                        .filter_map(|r| r.startup.as_ref().and_then(f))
                        .collect::<Vec<_>>(),
                )
            };
            StartupProfile {
                pool,
                runs: runs.len(),
                median_register_secs: col(|s| s.register_secs),
                median_launch_secs: col(|s| s.launch_secs),
                median_ready_secs: col(|s| s.ready_secs),
            }
        })
        .collect();

    Profile {
        jobs,
        steps,
        startup,
    }
}

fn opt(v: Option<f64>, f: impl Fn(f64) -> String) -> String {
    v.map(f).unwrap_or_else(|| "-".into())
}

fn pct(v: f64) -> String {
    format!("{:.0}%", v * 100.0)
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

pub fn print(profile: &Profile, top_jobs: usize, top_steps: usize) {
    if profile.jobs.is_empty() {
        println!("no jobs recorded yet");
        return;
    }
    println!("JOBS (most total time first)");
    println!(
        "{:<58}  {:>4}  {:>7}  {:>7}  {:>7}  {:>6}  {:>4}  {:>10}",
        "REPO · WORKFLOW / JOB", "RUNS", "MEDIAN", "P95", "MAX", "QUEUE", "CPU", "MEM PEAK"
    );
    for j in profile.jobs.iter().take(top_jobs) {
        println!(
            "{:<58}  {:>4}  {:>7}  {:>7}  {:>7}  {:>6}  {:>4}  {:>10}",
            fit(&j.name, 58),
            j.runs,
            duration(j.median_secs),
            duration(j.p95_secs),
            duration(j.max_secs),
            opt(j.median_queue_secs, duration),
            opt(j.avg_cpu, pct),
            match (j.median_mem_peak_mb, j.mem_total_mb) {
                (Some(m), Some(t)) => format!("{:.1}/{:.0}G", m / 1024.0, t / 1024.0),
                (Some(m), None) => format!("{:.1}G", m / 1024.0),
                _ => "-".into(),
            },
        );
    }

    let cached: Vec<&JobProfile> = profile
        .jobs
        .iter()
        .filter(|j| j.warm_runs + j.cold_runs > 0)
        .take(top_jobs)
        .collect();
    if !cached.is_empty() {
        println!("\nCACHES");
        println!(
            "{:<58}  {:>9}  {:>7}  {:>7}  {:>8}  GREW PER RUN (MiB)",
            "REPO · WORKFLOW / JOB", "WARM/COLD", "WARM", "COLD", "SCCACHE"
        );
        for j in cached {
            let growth: Vec<String> = j
                .avg_growth_mb
                .iter()
                .filter(|(_, mb)| **mb >= 1.0)
                .map(|(dir, mb)| format!("{dir} {mb:.0}"))
                .collect();
            println!(
                "{:<58}  {:>9}  {:>7}  {:>7}  {:>8}  {}",
                fit(&j.name, 58),
                format!("{}/{}", j.warm_runs, j.cold_runs),
                opt(j.median_warm_secs, duration),
                opt(j.median_cold_secs, duration),
                opt(j.sccache_hit_rate, pct),
                growth.join(", "),
            );
        }
    }

    if !profile.steps.is_empty() {
        println!("\nSLOWEST STEPS (by total time)");
        println!(
            "{:<40}  {:<38}  {:>4}  {:>7}  {:>7}  {:>5}",
            "JOB", "STEP", "RUNS", "AVG", "P95", "SHARE"
        );
        for s in profile.steps.iter().take(top_steps) {
            println!(
                "{:<40}  {:<38}  {:>4}  {:>7}  {:>7}  {:>5}",
                fit(s.job.rsplit(" / ").next().unwrap_or(&s.job), 40),
                fit(&s.step, 38),
                s.runs,
                duration(s.avg_secs),
                duration(s.p95_secs),
                pct(s.share_of_jobs),
            );
        }
    } else {
        println!(
            "\nno step timings recorded yet (they need the GitHub app's \"Actions: Read\" permission)"
        );
    }

    if !profile.startup.is_empty() {
        println!("\nRUNNER STARTUP (median)");
        println!(
            "{:<32}  {:>4}  {:>9}  {:>7}  {:>7}",
            "HOST/POOL", "RUNS", "REGISTER", "LAUNCH", "READY"
        );
        for s in &profile.startup {
            println!(
                "{:<32}  {:>4}  {:>9}  {:>7}  {:>7}",
                fit(&s.pool, 32),
                s.runs,
                opt(s.median_register_secs, |v| format!("{v:.1}s")),
                opt(s.median_launch_secs, |v| format!("{v:.1}s")),
                opt(s.median_ready_secs, |v| format!("{v:.1}s")),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{CacheStats, Resources, StepRecord};

    fn job(secs: f64, warm: bool, hits: u64, misses: u64) -> JobRecord {
        JobRecord {
            repo: Some("acme/app".into()),
            workflow: Some("CI".into()),
            job: Some("build".into()),
            pool: "linux".into(),
            duration_secs: secs,
            steps: vec![
                StepRecord {
                    name: "pnpm install".into(),
                    number: 2,
                    conclusion: Some("success".into()),
                    secs: secs / 2.0,
                },
                StepRecord {
                    name: "Skipped".into(),
                    number: 3,
                    conclusion: Some("skipped".into()),
                    secs: 0.0,
                },
            ],
            cache: Some(CacheStats {
                warm,
                sccache_hits: Some(hits),
                sccache_misses: Some(misses),
                start_kb: [("pnpm-store".to_string(), 1024)].into(),
                end_kb: [("pnpm-store".to_string(), 3072)].into(),
                ..Default::default()
            }),
            resources: Some(Resources {
                mem_peak_kb: Some(2 * 1024 * 1024),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn separates_warm_from_cold_and_ranks_steps() {
        let records = [
            job(300.0, false, 0, 100),
            job(100.0, true, 90, 10),
            job(120.0, true, 95, 5),
        ];
        let p = build(&records);
        assert_eq!(p.jobs.len(), 1);
        let j = &p.jobs[0];
        assert_eq!((j.warm_runs, j.cold_runs), (2, 1));
        assert_eq!(j.median_cold_secs, Some(300.0));
        assert_eq!(j.median_warm_secs, Some(120.0));
        assert!((j.sccache_hit_rate.unwrap() - 185.0 / 300.0).abs() < 1e-9);
        assert_eq!(j.avg_growth_mb["pnpm-store"], 2.0);
        assert_eq!(j.median_mem_peak_mb, Some(2048.0));
        // Skipped steps don't count; the install step is half of every job.
        assert_eq!(p.steps.len(), 1);
        assert!((p.steps[0].share_of_jobs - 0.5).abs() < 1e-9);
    }

    #[test]
    fn handles_old_records_without_new_fields() {
        let old = JobRecord {
            duration_secs: 10.0,
            ..Default::default()
        };
        let p = build(&[old]);
        assert_eq!(p.jobs[0].runs, 1);
        assert!(p.steps.is_empty() && p.startup.is_empty());
        assert_eq!(p.jobs[0].median_warm_secs, None);
    }
}
