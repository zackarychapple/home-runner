use std::collections::HashSet;
use std::fs::File;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::OnceCell;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::backend::Backend;
use crate::config::{Config, PoolConfig};
use crate::github::GitHub;
use crate::power;
use crate::runner_dist::RunnerDist;

const MIN_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(300);
/// A runner that exits this fast without taking a job is broken, not finished.
const QUICK_EXIT: Duration = Duration::from_secs(120);

pub struct Pool {
    pub cfg: PoolConfig,
    pub backend: Backend,
    pub labels: Vec<String>,
    /// Every runner/VM/container in this pool is named `<prefix><slot>-<id>`.
    pub prefix: String,
    ready: OnceCell<()>,
}

impl Pool {
    /// Runs `prepare` once; a failure is retried on the next call.
    async fn ready(&self) -> Result<()> {
        self.ready
            .get_or_try_init(|| self.backend.prepare())
            .await
            .map(|_| ())
    }
}

struct Ctx {
    gh: Arc<GitHub>,
    host_prefix: String,
    /// Names of runners a slot is currently responsible for. Reconcile never touches these.
    active: Mutex<HashSet<String>>,
}

/// Marks a runner name as owned by a slot for as long as the claim lives.
struct Claim<'a> {
    active: &'a Mutex<HashSet<String>>,
    name: String,
}

impl<'a> Claim<'a> {
    fn new(active: &'a Mutex<HashSet<String>>, name: &str) -> Self {
        active.lock().unwrap().insert(name.to_string());
        Self {
            active,
            name: name.to_string(),
        }
    }
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.active.lock().unwrap().remove(&self.name);
    }
}

enum Outcome {
    Completed,
    Shutdown,
}

pub fn build(config: &Config) -> Result<(Arc<GitHub>, Vec<Arc<Pool>>)> {
    let gh = Arc::new(GitHub::from_config(&config.github)?);
    let data_dir = config.data_dir();
    let dist = Arc::new(RunnerDist::new(gh.clone(), data_dir.join("cache")));
    let host_prefix = config.runner_prefix();
    let pools = config
        .pools
        .iter()
        .map(|cfg| {
            let backend = Backend::new(&cfg.backend, &data_dir, dist.clone());
            let labels = labels(&backend, cfg);
            Arc::new(Pool {
                prefix: format!("{host_prefix}{}-", cfg.name),
                cfg: cfg.clone(),
                backend,
                labels,
                ready: OnceCell::new(),
            })
        })
        .collect();
    Ok((gh, pools))
}

fn labels(backend: &Backend, pool: &PoolConfig) -> Vec<String> {
    let mut labels: Vec<String> = Vec::new();
    let all = std::iter::once("self-hosted".to_string())
        .chain(backend.platform_labels())
        .chain(pool.labels.iter().cloned());
    for label in all {
        if !labels.iter().any(|l| l.eq_ignore_ascii_case(&label)) {
            labels.push(label);
        }
    }
    labels
}

/// Prevents two supervisors (say, the service and a manual `run`) from fighting over the same runners.
fn lock_instance(config: &Config) -> Result<File> {
    let dir = config.data_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("home-runner.lock");
    let file = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    file.try_lock().map_err(|_| {
        anyhow!(
            "another home-runner is already running on this machine (lock {}); stop it first",
            path.display()
        )
    })?;
    Ok(file)
}

pub async fn run(config: Config, shutdown: CancellationToken) -> Result<()> {
    let _lock = lock_instance(&config)?;
    let _awake = config.host.prevent_sleep.then(power::prevent_sleep);
    let (gh, pools) = build(&config)?;
    let ctx = Arc::new(Ctx {
        gh,
        host_prefix: config.runner_prefix(),
        active: Mutex::default(),
    });
    info!(
        host = %config.host_name(),
        scope = %ctx.gh.scope_label(),
        "home-runner {} starting",
        env!("CARGO_PKG_VERSION")
    );
    for pool in &pools {
        info!(
            pool = %pool.cfg.name,
            backend = pool.cfg.backend.kind(),
            count = pool.cfg.count,
            labels = %pool.labels.join(","),
            "pool configured"
        );
    }

    // Anything left from a previous run (crash, reboot, power cut) is garbage now.
    if let Err(e) = reconcile(&ctx, &pools).await {
        warn!("initial cleanup failed, will retry: {e:#}");
    }

    let mut slots = JoinSet::new();
    for pool in &pools {
        for slot in 0..pool.cfg.count {
            slots.spawn(slot_loop(ctx.clone(), pool.clone(), slot, shutdown.clone()));
        }
    }

    let reconciler = {
        let (ctx, pools, shutdown) = (ctx.clone(), pools.clone(), shutdown.clone());
        let interval = Duration::from_secs(config.host.reconcile_interval_secs.max(30));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    _ = shutdown.cancelled() => break,
                }
                if let Err(e) = reconcile(&ctx, &pools).await {
                    warn!("reconcile failed: {e:#}");
                }
            }
        })
    };

    shutdown.cancelled().await;
    info!("stopping runners");
    let drain = async { while slots.join_next().await.is_some() {} };
    if tokio::time::timeout(Duration::from_secs(90), drain)
        .await
        .is_err()
    {
        warn!("some runners did not stop within 90s; the next start will clean them up");
    }
    reconciler.abort();
    info!("stopped");
    Ok(())
}

async fn slot_loop(ctx: Arc<Ctx>, pool: Arc<Pool>, slot: usize, shutdown: CancellationToken) {
    let mut backoff = MIN_BACKOFF;
    while !shutdown.is_cancelled() {
        match run_once(&ctx, &pool, slot, &shutdown).await {
            Ok(Outcome::Completed) => backoff = MIN_BACKOFF,
            Ok(Outcome::Shutdown) => break,
            Err(e) => {
                let delay = backoff + jitter();
                error!(pool = %pool.cfg.name, slot, "{e:#}; retrying in {}s", delay.as_secs());
                backoff = (backoff * 2).min(MAX_BACKOFF);
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = shutdown.cancelled() => break,
                }
            }
        }
    }
}

/// One runner, one job: register → launch → wait → tear down → deregister.
async fn run_once(
    ctx: &Ctx,
    pool: &Pool,
    slot: usize,
    shutdown: &CancellationToken,
) -> Result<Outcome> {
    tokio::select! {
        r = pool.ready() => r.with_context(|| format!("preparing the {} backend", pool.cfg.backend.kind()))?,
        _ = shutdown.cancelled() => return Ok(Outcome::Shutdown),
    }
    let name = format!("{}{slot}-{}", pool.prefix, short_id());
    let _claim = Claim::new(&ctx.active, &name);
    let jit = ctx
        .gh
        .generate_jitconfig(&name, &pool.labels)
        .await
        .context("registering runner with GitHub")?;
    info!(runner = %name, "registered, waiting for a job");

    let result = drive(pool, &name, &jit.encoded, shutdown).await;

    if let Err(e) = pool.backend.cleanup(&name).await {
        warn!(runner = %name, "cleanup failed, reconcile will retry: {e:#}");
    }
    if let Err(e) = ctx.gh.delete_runner(jit.runner_id).await {
        warn!(runner = %name, "deregistering failed, reconcile will retry: {e:#}");
    }
    result
}

async fn drive(
    pool: &Pool,
    name: &str,
    jit_config: &str,
    shutdown: &CancellationToken,
) -> Result<Outcome> {
    let launched = Instant::now();
    let mut running = tokio::select! {
        r = pool.backend.launch(name, jit_config) => r.context("launching runner")?,
        _ = shutdown.cancelled() => return Ok(Outcome::Shutdown),
    };

    // The timeout clock starts when a job starts, so idle runners can wait indefinitely.
    let timeout = Duration::from_secs(pool.cfg.job_timeout_minutes * 60);
    let mut job_started = running.job_started.clone();
    let deadline = async move {
        let _ = job_started.wait_for(|started| *started).await;
        tokio::time::sleep(timeout).await;
    };

    enum Event {
        Exited(Result<std::process::ExitStatus>),
        TimedOut,
        Shutdown,
    }
    let event = tokio::select! {
        status = running.wait() => Event::Exited(status),
        _ = deadline => Event::TimedOut,
        _ = shutdown.cancelled() => Event::Shutdown,
    };

    match event {
        Event::Exited(status) => {
            let status = status?;
            let elapsed = launched.elapsed();
            // A runner that never picked up a job had a bad config, lost its registration or failed to
            // start; back off instead of hammering GitHub with registrations.
            if !*running.job_started.borrow() && elapsed < QUICK_EXIT {
                bail!(
                    "runner exited after {}s without running a job ({status})",
                    elapsed.as_secs()
                );
            }
            info!(runner = %name, %status, minutes = elapsed.as_secs() / 60, "runner finished");
            Ok(Outcome::Completed)
        }
        Event::TimedOut => {
            warn!(runner = %name, "job exceeded {} minutes; killing it", pool.cfg.job_timeout_minutes);
            running.kill().await;
            Ok(Outcome::Completed)
        }
        Event::Shutdown => {
            running.kill().await;
            Ok(Outcome::Shutdown)
        }
    }
}

/// Deletes GitHub registrations and backend resources that carry this machine's prefix but that
/// no slot owns: leftovers from crashes, reboots and failed cleanups.
async fn reconcile(ctx: &Ctx, pools: &[Arc<Pool>]) -> Result<()> {
    let runners = ctx.gh.list_runners().await?;
    for runner in runners
        .iter()
        .filter(|r| r.name.starts_with(&ctx.host_prefix))
    {
        if ctx.active.lock().unwrap().contains(&runner.name) {
            continue;
        }
        if runner.busy {
            warn!(runner = %runner.name, "orphaned runner is still busy; will retry");
            continue;
        }
        info!(runner = %runner.name, "removing stale runner registration");
        if let Err(e) = ctx.gh.delete_runner(runner.id).await {
            warn!(runner = %runner.name, "{e:#}");
        }
    }
    for pool in pools {
        let names = match pool.backend.list(&pool.prefix).await {
            Ok(names) => names,
            Err(e) => {
                warn!(pool = %pool.cfg.name, "listing {} resources failed: {e:#}", pool.cfg.backend.kind());
                continue;
            }
        };
        for name in names {
            if ctx.active.lock().unwrap().contains(&name) {
                continue;
            }
            info!(pool = %pool.cfg.name, "removing orphaned {} {name}", pool.cfg.backend.kind());
            if let Err(e) = pool.backend.cleanup(&name).await {
                warn!("{e:#}");
            }
        }
    }
    Ok(())
}

pub async fn status(path: &Path) -> Result<()> {
    let config = Config::load(path)?;
    let gh = GitHub::from_config(&config.github)?;
    let prefix = config.runner_prefix();
    println!("host {} → {}", config.host_name(), gh.scope_label());
    for pool in &config.pools {
        println!(
            "  pool {:<20} {:<16} x{}",
            pool.name,
            pool.backend.kind(),
            pool.count
        );
    }
    let mut runners: Vec<_> = gh
        .list_runners()
        .await?
        .into_iter()
        .filter(|r| r.name.starts_with(&prefix))
        .collect();
    runners.sort_by(|a, b| a.name.cmp(&b.name));
    println!();
    if runners.is_empty() {
        println!("no runners registered for this host (is home-runner running?)");
        return Ok(());
    }
    println!("{:<52} {:<8} JOB", "RUNNER", "STATUS");
    for r in runners {
        println!(
            "{:<52} {:<8} {}",
            r.name,
            r.status,
            if r.busy { "busy" } else { "idle" }
        );
    }
    Ok(())
}

pub async fn cleanup(path: &Path) -> Result<()> {
    let config = Config::load(path)?;
    let _lock = lock_instance(&config)?;
    let (gh, pools) = build(&config)?;
    let ctx = Ctx {
        gh,
        host_prefix: config.runner_prefix(),
        active: Mutex::default(),
    };
    reconcile(&ctx, &pools).await?;
    println!("cleanup complete");
    Ok(())
}

pub async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            },
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn short_id() -> String {
    let n = nanos();
    format!("{:06x}", ((n ^ (n >> 24)) as u32) & 0xff_ffff)
}

fn jitter() -> Duration {
    Duration::from_millis((nanos() % 2000) as u64)
}
