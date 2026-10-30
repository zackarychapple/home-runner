use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use tokio::sync::{Notify, OnceCell};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::accounting::{Accounting, PoolPricing};
use crate::backend::Backend;
use crate::config::{Config, PoolConfig};
use crate::github::GitHub;
use crate::runner_dist::RunnerDist;
use crate::state::{self, Board, Phase, Slot};
use crate::{fleet, hooks, power, timefmt};

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
    pub pricing: PoolPricing,
    hook_env: Vec<(String, String)>,
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
    accounting: Arc<Accounting>,
    host_prefix: String,
    /// Names of runners a slot is currently responsible for. Reconcile never touches these.
    active: Mutex<HashSet<String>>,
    board: Arc<Board>,
    /// Signalled after each job is written to the ledger, so the fleet hears about it soon.
    job_done: Arc<Notify>,
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

pub struct Built {
    pub gh: Arc<GitHub>,
    pub accounting: Arc<Accounting>,
    pub pools: Vec<Arc<Pool>>,
}

pub fn build(config: &Config) -> Result<Built> {
    let gh = Arc::new(GitHub::from_config(&config.github)?);
    let data_dir = config.data_dir();
    let dist = Arc::new(RunnerDist::new(gh.clone(), data_dir.join("cache")));
    let accounting = Arc::new(Accounting::new(
        gh.clone(),
        config.pricing.clone(),
        config.host_name(),
        config.ledger_path(),
    ));
    let host_prefix = config.runner_prefix();
    let pools = config
        .pools
        .iter()
        .map(|cfg| {
            let backend = Backend::new(&cfg.backend, &data_dir, dist.clone());
            let labels = labels(&backend, cfg);
            let platform = backend.platform_labels();
            let pricing = PoolPricing::new(cfg, &platform[0], &platform[1], backend.cpus());
            Arc::new(Pool {
                prefix: format!("{host_prefix}{}-", cfg.name),
                hook_env: accounting.hook_env(&pricing),
                cfg: cfg.clone(),
                backend,
                labels,
                pricing,
                ready: OnceCell::new(),
            })
        })
        .collect();
    Ok(Built {
        gh,
        accounting,
        pools,
    })
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
    let Built {
        gh,
        accounting,
        pools,
    } = build(&config)?;
    let mut board = Board::new(&config.data_dir(), base_state(&config, &gh, &pools));
    for (p, pool) in pools.iter().enumerate() {
        for slot in 0..pool.cfg.count {
            board.slots.push(Slot::new(p, slot));
        }
    }
    let board = Arc::new(board);
    let ctx = Arc::new(Ctx {
        gh,
        accounting,
        host_prefix: config.runner_prefix(),
        active: Mutex::default(),
        board: board.clone(),
        job_done: Arc::new(Notify::new()),
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
            compare = %pool.pricing.github.map(|s| s.label).unwrap_or("-"),
            "pool configured"
        );
    }

    // Anything left from a previous run (crash, reboot, power cut) is garbage now.
    if let Err(e) = reconcile(&ctx, &pools).await {
        warn!("initial cleanup failed, will retry: {e:#}");
    }

    let mut slots = JoinSet::new();
    for slot in &board.slots {
        let pool = pools[slot.pool].clone();
        slots.spawn(slot_loop(ctx.clone(), pool, slot.clone(), shutdown.clone()));
    }

    let publisher = {
        let (board, pools, stop) = (board.clone(), pools.clone(), CancellationToken::new());
        let task = tokio::spawn(publish(board, pools, stop.clone()));
        (task, stop)
    };

    let reporter = match fleet::Reporter::new(&config, board.clone()) {
        Ok(Some(reporter)) => Some(tokio::spawn(
            reporter.run(ctx.job_done.clone(), shutdown.clone()),
        )),
        Ok(None) => None,
        Err(e) => {
            warn!("fleet reporting is off: {e:#}");
            None
        }
    };

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
    if let Some(reporter) = reporter {
        let _ = tokio::time::timeout(Duration::from_secs(15), reporter).await;
    }
    publisher.1.cancel();
    let _ = publisher.0.await;
    info!("stopped");
    Ok(())
}

/// The parts of the published state that don't change while running.
fn base_state(config: &Config, gh: &GitHub, pools: &[Arc<Pool>]) -> state::State {
    let rate = |id: &str, list: f64| config.pricing.rates.get(id).copied().unwrap_or(list);
    state::State {
        host: config.host_name(),
        version: env!("CARGO_PKG_VERSION").into(),
        pid: std::process::id(),
        started_at: timefmt::now(),
        updated_at: timefmt::now(),
        running: true,
        scope: gh.scope_label(),
        electricity_usd_per_kwh: config.pricing.electricity_usd_per_kwh,
        job_watts: config.pricing.job_watts,
        pools: pools
            .iter()
            .map(|p| state::PoolState {
                name: p.cfg.name.clone(),
                backend: p.cfg.backend.kind().into(),
                labels: p.labels.clone(),
                os: p.pricing.os.clone(),
                arch: p.pricing.arch.clone(),
                cpus: p.pricing.cpus,
                github_label: p.pricing.github.map(|s| s.label.to_string()),
                github_usd_per_minute: p.pricing.github.map(|s| rate(s.id, s.usd_per_minute)),
                slots: vec![],
            })
            .collect(),
        reconcile: None,
        fleet: None,
    }
}

/// Rewrites `state.json` every couple of seconds and acts on the GUI's stop requests.
async fn publish(board: Arc<Board>, pools: Vec<Arc<Pool>>, stop: CancellationToken) {
    let mut last_ticks = hooks::cpu_ticks();
    let mut warned = false;
    loop {
        board.take_stop_requests();
        // Host-backend jobs keep their metadata on disk and share this machine's CPU.
        let mut extra = HashMap::new();
        for runner in board.busy_runners() {
            if let Some(pool) = pools.iter().find(|p| runner.starts_with(&p.prefix))
                && matches!(pool.backend, Backend::Host(_))
            {
                extra.insert(runner.clone(), pool.backend.collect(&runner).await);
            }
        }
        let ticks = hooks::cpu_ticks();
        let host_cpu = match (last_ticks, ticks) {
            (Some((b0, t0)), Some((b1, t1))) if t1 > t0 => {
                Some((b1.wrapping_sub(b0) as f32 / (t1 - t0) as f32).min(1.0))
            }
            _ => None,
        };
        last_ticks = ticks;

        let mut current = board.collect(&extra, host_cpu);
        let stopping = stop.is_cancelled();
        if stopping {
            current.running = false;
            for slot in current.pools.iter_mut().flat_map(|p| p.slots.iter_mut()) {
                slot.phase = Phase::Stopped;
                slot.job = None;
            }
        }
        match state::write(&board.path, &current) {
            Ok(()) => warned = false,
            Err(e) if !warned => {
                warn!("writing {}: {e}", board.path.display());
                warned = true;
            }
            Err(_) => {}
        }
        if stopping {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(state::PUBLISH_SECS)) => {}
            _ = stop.cancelled() => {}
        }
    }
}

async fn slot_loop(ctx: Arc<Ctx>, pool: Arc<Pool>, status: Arc<Slot>, shutdown: CancellationToken) {
    let slot = status.slot;
    let mut backoff = MIN_BACKOFF;
    while !shutdown.is_cancelled() {
        let result = run_once(&ctx, &pool, &status, &shutdown).await;
        status.end_runner();
        match result {
            Ok(Outcome::Completed) => {
                backoff = MIN_BACKOFF;
                status.ok();
            }
            Ok(Outcome::Shutdown) => break,
            Err(e) => {
                let delay = backoff + jitter();
                status.backoff(format!("{e:#}"), timefmt::now() + delay.as_secs() as i64);
                error!(pool = %pool.cfg.name, slot, "{e:#}; retrying in {}s", delay.as_secs());
                backoff = (backoff * 2).min(MAX_BACKOFF);
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = shutdown.cancelled() => break,
                }
            }
        }
    }
    status.set(Phase::Stopped);
}

/// One runner, one job: register → launch → wait → tear down → deregister.
async fn run_once(
    ctx: &Ctx,
    pool: &Pool,
    status: &Slot,
    shutdown: &CancellationToken,
) -> Result<Outcome> {
    let slot = status.slot;
    if pool.ready.get().is_none() {
        status.set(Phase::Preparing);
    }
    tokio::select! {
        r = pool.ready() => r.with_context(|| format!("preparing the {} backend", pool.cfg.backend.kind()))?,
        _ = shutdown.cancelled() => return Ok(Outcome::Shutdown),
    }
    let name = format!("{}{slot}-{}", pool.prefix, short_id());
    let _claim = Claim::new(&ctx.active, &name);
    // Cancelled when someone asks to stop this runner's job (see `state::request_stop`).
    let stop = CancellationToken::new();
    status.start_runner(&name, stop.clone());
    status.set(Phase::Registering);
    let jit = ctx
        .gh
        .generate_jitconfig(&name, &pool.labels)
        .await
        .context("registering runner with GitHub")?;
    info!(runner = %name, "registered, waiting for a job");

    let result = drive(ctx, pool, status, &name, &jit.encoded, shutdown, &stop).await;
    status.set(Phase::Finishing);

    if let Err(e) = pool.backend.cleanup(&name).await {
        warn!(runner = %name, "cleanup failed, reconcile will retry: {e:#}");
    }
    if let Err(e) = ctx.gh.delete_runner(jit.runner_id).await {
        warn!(runner = %name, "deregistering failed, reconcile will retry: {e:#}");
    }
    result
}

async fn drive(
    ctx: &Ctx,
    pool: &Pool,
    status: &Slot,
    name: &str,
    jit_config: &str,
    shutdown: &CancellationToken,
    stop: &CancellationToken,
) -> Result<Outcome> {
    let launched = Instant::now();
    status.set(Phase::Launching);
    let mut running = tokio::select! {
        r = pool.backend.launch(name, jit_config, &pool.hook_env) => r.context("launching runner")?,
        _ = shutdown.cancelled() => return Ok(Outcome::Shutdown),
    };
    status.watch(running.watch.clone());
    status.set(Phase::Idle);

    // The timeout clock starts when a job starts, so idle runners can wait indefinitely.
    let timeout = Duration::from_secs(pool.cfg.job_timeout_minutes * 60);
    let mut watch = running.watch.clone();
    let deadline = {
        let mut watch = watch.clone();
        async move {
            watch.wait_started().await;
            tokio::time::sleep(timeout).await;
        }
    };

    enum Event {
        Exited(Result<std::process::ExitStatus>),
        TimedOut,
        Stopped,
        Shutdown,
    }
    let event = tokio::select! {
        status = running.wait() => Event::Exited(status),
        _ = deadline => Event::TimedOut,
        _ = stop.cancelled() => Event::Stopped,
        _ = shutdown.cancelled() => Event::Shutdown,
    };

    // `forced` is the ledger result when the runner never got to report one.
    let (outcome, forced) = match event {
        Event::Exited(Ok(status)) => {
            let elapsed = launched.elapsed();
            // A runner that never picked up a job had a bad config, lost its registration or failed
            // to start; back off instead of hammering GitHub with registrations.
            if !watch.has_started() && elapsed < QUICK_EXIT {
                (
                    Err(anyhow!(
                        "runner exited after {}s without running a job ({status})",
                        elapsed.as_secs()
                    )),
                    None,
                )
            } else {
                info!(runner = %name, %status, "runner finished");
                (Ok(Outcome::Completed), None)
            }
        }
        Event::Exited(Err(e)) => (Err(e), Some("VmCrashed")),
        Event::TimedOut => {
            warn!(runner = %name, "job exceeded {} minutes; killing it", pool.cfg.job_timeout_minutes);
            running.kill().await;
            (Ok(Outcome::Completed), Some("TimedOut"))
        }
        Event::Stopped => {
            warn!(runner = %name, "stopping job on request");
            running.kill().await;
            (Ok(Outcome::Completed), Some("Canceled"))
        }
        Event::Shutdown => {
            running.kill().await;
            (Ok(Outcome::Shutdown), Some("Interrupted"))
        }
    };

    if watch.has_started() {
        // Hook output arrives on stdout just before the runner exits; let it drain.
        watch.drained(Duration::from_secs(5)).await;
        let mut info = watch.snapshot();
        if info.result.is_none() {
            info.result = Some(forced.unwrap_or("Unknown").to_string());
        }
        info.meta.extend(pool.backend.collect(name).await);
        ctx.accounting
            .record(name, &pool.cfg, &pool.pricing, info)
            .await;
        ctx.job_done.notify_one();
    }
    outcome
}

/// Deletes GitHub registrations and backend resources that carry this machine's prefix but that
/// no slot owns: leftovers from crashes, reboots and failed cleanups.
async fn reconcile(ctx: &Ctx, pools: &[Arc<Pool>]) -> Result<()> {
    let result = reconcile_inner(ctx, pools).await;
    ctx.board.update(|s| {
        let removed = result.as_ref().map_or(0, |n| *n);
        s.reconcile = Some(state::ReconcileState {
            at: timefmt::now(),
            removed,
            error: result.as_ref().err().map(|e| format!("{e:#}")),
        });
    });
    result.map(|_| ())
}

/// Returns how many leftovers it removed.
async fn reconcile_inner(ctx: &Ctx, pools: &[Arc<Pool>]) -> Result<usize> {
    let mut removed = 0;
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
        match ctx.gh.delete_runner(runner.id).await {
            Ok(()) => removed += 1,
            Err(e) => warn!(runner = %runner.name, "{e:#}"),
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
            match pool.backend.cleanup(&name).await {
                Ok(()) => removed += 1,
                Err(e) => warn!("{e:#}"),
            }
        }
    }
    Ok(removed)
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
    let ledger = crate::ledger::load(&[config.ledger_path()]).unwrap_or_default();
    if let Some(summary) = crate::ledger::summary_line(&ledger) {
        println!("  lifetime: {summary}");
    }
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
    let Built {
        gh,
        accounting,
        pools,
    } = build(&config)?;
    let board = Arc::new(Board::new(
        &config.data_dir(),
        base_state(&config, &gh, &pools),
    ));
    let ctx = Ctx {
        gh,
        accounting,
        host_prefix: config.runner_prefix(),
        active: Mutex::default(),
        board,
        job_done: Arc::new(Notify::new()),
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
