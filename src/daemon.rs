use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use tokio::sync::{Notify, OnceCell, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::accounting::{Accounting, PoolPricing};
use crate::backend::Backend;
use crate::config::{Config, PoolConfig};
use crate::github::GitHub;
use crate::health::{self, FleetStatus, Health, Notifier, Phase, Slots};
use crate::live::{self, Live};
use crate::runner_dist::RunnerDist;
use crate::{fleet, hooks, power, timefmt};

/// How the slot loops pace themselves. Tests shrink these.
#[derive(Clone, Copy)]
pub struct Timing {
    pub min_backoff: Duration,
    pub max_backoff: Duration,
    /// A runner that exits this fast without taking a job is broken, not finished.
    pub quick_exit: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            min_backoff: Duration::from_secs(5),
            max_backoff: Duration::from_secs(300),
            quick_exit: Duration::from_secs(120),
        }
    }
}

/// How often the supervisor rewrites `state.json`, checks the drain flag and evaluates alerts.
const HEALTH_TICK: Duration = Duration::from_secs(5);

pub struct Pool {
    pub cfg: PoolConfig,
    pub backend: Backend,
    pub labels: Vec<String>,
    /// Every runner/VM/container in this pool is named `<prefix><slot>-<id>`.
    pub prefix: String,
    pub pricing: PoolPricing,
    pub(crate) hook_env: Vec<(String, String)>,
    pub(crate) ready: OnceCell<()>,
}

impl Pool {
    /// Runs `prepare` once; a failure is retried on the next call.
    pub(crate) async fn ready(&self) -> Result<()> {
        self.ready
            .get_or_try_init(|| self.backend.prepare())
            .await
            .map(|_| ())
    }
}

pub struct Ctx {
    pub gh: Arc<GitHub>,
    accounting: Arc<Accounting>,
    host_prefix: String,
    /// Names of runners a slot is currently responsible for. Reconcile never touches these.
    active: Mutex<HashSet<String>>,
    pub slots: Slots,
    /// `true` while draining: slots deregister idle runners and don't start new ones.
    pub drain: watch::Sender<bool>,
    /// Unix time GitHub last answered an API call.
    github_ok_at: AtomicI64,
    pub timing: Timing,
    /// Each slot's current runner and job.
    pub live: Live,
    /// Signalled after each job is written to the ledger, so the fleet hears about it soon.
    pub job_done: Arc<Notify>,
    fleet: Mutex<Option<FleetStatus>>,
    /// The state most recently written to `state.json`.
    latest: Mutex<Option<health::State>>,
}

impl Ctx {
    pub fn new(gh: Arc<GitHub>, accounting: Arc<Accounting>, host_prefix: String) -> Self {
        Self {
            gh,
            accounting,
            host_prefix,
            active: Mutex::default(),
            slots: Slots::default(),
            drain: watch::channel(false).0,
            github_ok_at: AtomicI64::new(timefmt::now()),
            timing: Timing::default(),
            live: Live::default(),
            job_done: Arc::new(Notify::new()),
            fleet: Mutex::default(),
            latest: Mutex::default(),
        }
    }

    fn github_ok(&self) {
        self.github_ok_at.store(timefmt::now(), Ordering::Relaxed);
    }

    pub fn latest_state(&self) -> Option<health::State> {
        self.latest.lock().unwrap().clone()
    }

    pub fn update_fleet_status(&self, f: impl FnOnce(&mut FleetStatus)) {
        f(self
            .fleet
            .lock()
            .unwrap()
            .get_or_insert_with(Default::default));
    }
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
    /// The runner was idle when a drain began and was removed.
    Drained,
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
pub fn lock_instance(config: &Config) -> Result<File> {
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
    let ctx = Arc::new(Ctx::new(gh, accounting, config.runner_prefix()));
    let data_dir = config.data_dir();
    ctx.drain.send_replace(health::drain_requested(&data_dir));
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
    for pool in &pools {
        for slot in 0..pool.cfg.count {
            ctx.slots.register(&pool.cfg.name, slot);
            slots.spawn(slot_loop(ctx.clone(), pool.clone(), slot, shutdown.clone()));
        }
    }

    let monitor = tokio::spawn(monitor(
        ctx.clone(),
        config.clone(),
        pools.clone(),
        data_dir.clone(),
        shutdown.clone(),
    ));

    let reporter = match fleet::Reporter::new(&config, ctx.clone()) {
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
    monitor.abort();
    if let Some(reporter) = reporter {
        let _ = tokio::time::timeout(Duration::from_secs(15), reporter).await;
    }
    health::remove_state(&data_dir);
    info!("stopped");
    Ok(())
}

/// Once per tick: pick up drain and stop requests, publish `state.json`, and send alerts.
async fn monitor(
    ctx: Arc<Ctx>,
    config: Config,
    pools: Vec<Arc<Pool>>,
    data_dir: std::path::PathBuf,
    shutdown: CancellationToken,
) {
    let rate = |id: &str, list: f64| config.pricing.rates.get(id).copied().unwrap_or(list);
    let pool_info: Vec<health::PoolState> = pools
        .iter()
        .map(|p| health::PoolState {
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
        .collect();
    let mut last_ticks = hooks::cpu_ticks();
    let host = config.host_name();
    let started_at = timefmt::now();
    let mut health = Health::new(&host, &config.alerts);
    let notifier = match Notifier::new(&host, &config.alerts) {
        Ok(n) => n,
        Err(e) => {
            warn!("alerts disabled: {e:#}");
            None
        }
    };
    let mut ticker = tokio::time::interval(HEALTH_TICK);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = shutdown.cancelled() => return,
        }
        let draining = health::drain_requested(&data_dir);
        if ctx.drain.send_replace(draining) != draining {
            info!(
                "{}",
                if draining {
                    "draining: no new jobs will start"
                } else {
                    "resumed"
                }
            );
        }

        ctx.live.take_stop_requests(&data_dir);

        let now = timefmt::now();
        let ticks = hooks::cpu_ticks();
        let host_cpu = match (last_ticks, ticks) {
            (Some((b0, t0)), Some((b1, t1))) if t1 > t0 => {
                Some((b1.wrapping_sub(b0) as f32 / (t1 - t0) as f32).min(1.0))
            }
            _ => None,
        };
        last_ticks = ticks;
        let mut slots = ctx.slots.snapshot();
        for s in &mut slots {
            let Some(pool) = pools.iter().find(|p| p.cfg.name == s.pool) else {
                continue;
            };
            let Some((runner, info)) = ctx.live.job(&s.pool, s.slot) else {
                continue;
            };
            if let Some(mut info) = info {
                // Host-backend jobs keep their metadata on disk and share this machine's CPU.
                let host = matches!(pool.backend, Backend::Host(_));
                if host {
                    let extra: HashMap<_, _> = pool.backend.collect(&runner).await;
                    for (k, v) in extra {
                        info.meta.entry(k).or_insert(v);
                    }
                }
                let cpu = ctx.live.sample_cpu(
                    &s.pool,
                    s.slot,
                    &info.meta,
                    pool.pricing.cpus,
                    host_cpu.filter(|_| host),
                );
                s.job = Some(live::live_job(&info, cpu));
            }
            s.runner = Some(runner);
        }
        let github_ok_at = ctx.github_ok_at.load(Ordering::Relaxed);
        for alert in health.evaluate(now, &slots, github_ok_at) {
            if alert.firing {
                warn!("{}", alert.message);
            } else {
                info!("{}", alert.message);
            }
            if let Some(notifier) = &notifier
                && let Err(e) = notifier.send(&alert).await
            {
                warn!("{e:#}");
            }
        }
        let state = health::State {
            version: env!("CARGO_PKG_VERSION").to_string(),
            host: host.clone(),
            pid: std::process::id(),
            started_at,
            updated_at: now,
            draining,
            github_ok_at,
            slots,
            alerts: health.firing(),
            scope: ctx.gh.scope_label(),
            electricity_usd_per_kwh: config.pricing.electricity_usd_per_kwh,
            job_watts: config.pricing.job_watts,
            pools: pool_info.clone(),
            fleet: ctx.fleet.lock().unwrap().clone(),
        };
        if let Err(e) = health::write_state(&data_dir, &state) {
            warn!("{e:#}");
        }
        *ctx.latest.lock().unwrap() = Some(state);
    }
}

pub async fn slot_loop(ctx: Arc<Ctx>, pool: Arc<Pool>, slot: usize, shutdown: CancellationToken) {
    let timing = ctx.timing;
    let name = pool.cfg.name.as_str();
    let mut backoff = timing.min_backoff;
    let mut drain = ctx.drain.subscribe();
    while !shutdown.is_cancelled() {
        if *drain.borrow() {
            ctx.slots.phase(name, slot, Phase::Paused);
            tokio::select! {
                _ = drain.wait_for(|draining| !*draining) => {}
                _ = shutdown.cancelled() => break,
            }
            continue;
        }
        match run_once(&ctx, &pool, slot, &shutdown).await {
            Ok(Outcome::Completed) => {
                backoff = timing.min_backoff;
                ctx.slots.job_done(name, slot);
            }
            Ok(Outcome::Drained) => backoff = timing.min_backoff,
            Ok(Outcome::Shutdown) => break,
            Err(e) => {
                let delay = backoff + jitter(timing.min_backoff);
                error!(pool = %pool.cfg.name, slot, "{e:#}; retrying in {}s", delay.as_secs());
                ctx.slots.failed(name, slot, format!("{e:#}"));
                ctx.slots
                    .retry_at(name, slot, timefmt::now() + delay.as_secs() as i64);
                backoff = (backoff * 2).min(timing.max_backoff);
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = drain.wait_for(|draining| *draining) => {}
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
    ctx.slots.phase(&pool.cfg.name, slot, Phase::Starting);
    tokio::select! {
        r = pool.ready() => r.with_context(|| format!("preparing the {} backend", pool.cfg.backend.kind()))?,
        _ = shutdown.cancelled() => return Ok(Outcome::Shutdown),
    }
    let name = format!("{}{slot}-{}", pool.prefix, short_id());
    let _claim = Claim::new(&ctx.active, &name);
    // Cancelled when someone asks to stop this runner's job (see `live::request_stop`).
    let stop = CancellationToken::new();
    ctx.live.start(&pool.cfg.name, slot, &name, stop.clone());
    let result = run_runner(ctx, pool, slot, &name, shutdown, &stop).await;
    ctx.live.end(&pool.cfg.name, slot);
    result
}

async fn run_runner(
    ctx: &Ctx,
    pool: &Pool,
    slot: usize,
    name: &str,
    shutdown: &CancellationToken,
    stop: &CancellationToken,
) -> Result<Outcome> {
    let jit = ctx
        .gh
        .generate_jitconfig(name, &pool.labels, pool.cfg.runner_group_id)
        .await
        .context("registering runner with GitHub")?;
    ctx.github_ok();
    info!(runner = %name, "registered, waiting for a job");

    let result = drive(ctx, pool, slot, name, &jit.encoded, shutdown, stop).await;

    if let Err(e) = pool.backend.cleanup(name).await {
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
    slot: usize,
    name: &str,
    jit_config: &str,
    shutdown: &CancellationToken,
    stop: &CancellationToken,
) -> Result<Outcome> {
    let launched = Instant::now();
    let mut running = tokio::select! {
        r = pool.backend.launch(name, jit_config, &pool.hook_env) => r.context("launching runner")?,
        _ = shutdown.cancelled() => return Ok(Outcome::Shutdown),
    };
    ctx.live.watch(&pool.cfg.name, slot, running.watch.clone());
    ctx.slots.phase(&pool.cfg.name, slot, Phase::Idle);

    enum Event {
        Exited(Result<std::process::ExitStatus>),
        TimedOut,
        Stopped,
        Shutdown,
        Drained,
    }

    // The timeout clock starts when a job starts, so idle runners can wait indefinitely.
    let timeout = Duration::from_secs(pool.cfg.job_timeout_minutes * 60);
    let mut watch = running.watch.clone();
    let deadline = {
        let mut watch = watch.clone();
        async move {
            watch.wait_started().await;
            ctx.slots.phase(&pool.cfg.name, slot, Phase::Busy);
            tokio::time::sleep(timeout).await;
        }
    };
    // A runner that has stayed up this long isn't crash-looping, which clears the slot's failures.
    // Never finishes; it only marks the slot.
    let proven = async {
        tokio::time::sleep(ctx.timing.quick_exit).await;
        ctx.slots.proven(&pool.cfg.name, slot);
        std::future::pending::<Event>().await
    };
    // A drain removes runners that are still waiting; one that has a job keeps it.
    let drained_while_idle = {
        let probe = watch.clone();
        let mut drain = ctx.drain.subscribe();
        async move {
            loop {
                if *drain.borrow() && !probe.has_started() {
                    return;
                }
                tokio::select! {
                    _ = drain.changed() => {}
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }
        }
    };

    let event = tokio::select! {
        event = proven => event,
        status = running.wait() => Event::Exited(status),
        _ = deadline => Event::TimedOut,
        _ = drained_while_idle => Event::Drained,
        _ = stop.cancelled() => Event::Stopped,
        _ = shutdown.cancelled() => Event::Shutdown,
    };

    // `forced` is the ledger result when the runner never got to report one.
    let (outcome, forced) = match event {
        Event::Exited(Ok(status)) => {
            let elapsed = launched.elapsed();
            // A runner that never picked up a job had a bad config, lost its registration or failed
            // to start; back off instead of hammering GitHub with registrations.
            if !watch.has_started() && elapsed < ctx.timing.quick_exit {
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
        Event::Drained => {
            info!(runner = %name, "draining: removing the idle runner");
            running.kill().await;
            (Ok(Outcome::Drained), Some("Interrupted"))
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
pub async fn reconcile(ctx: &Ctx, pools: &[Arc<Pool>]) -> Result<()> {
    let runners = ctx.gh.list_runners().await?;
    ctx.github_ok();
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

pub async fn status(path: &Path, json: bool) -> Result<()> {
    let config = Config::load(path)?;
    let gh = GitHub::from_config(&config.github)?;
    let prefix = config.runner_prefix();
    let local = health::read_state(&config.data_dir()).filter(|s| s.is_fresh(timefmt::now()));
    let draining = health::drain_requested(&config.data_dir());
    let runners = gh.list_runners().await.map(|mut runners| {
        runners.retain(|r| r.name.starts_with(&prefix));
        runners.sort_by(|a, b| a.name.cmp(&b.name));
        runners
    });

    if json {
        let pools: Vec<_> = config
            .pools
            .iter()
            .map(|p| serde_json::json!({ "name": p.name, "backend": p.backend.kind(), "count": p.count }))
            .collect();
        let github = match &runners {
            Ok(runners) => serde_json::json!({
                "ok": true,
                "runners": runners.iter().map(|r| serde_json::json!({
                    "name": r.name, "status": r.status, "busy": r.busy,
                })).collect::<Vec<_>>(),
            }),
            Err(e) => serde_json::json!({ "ok": false, "error": format!("{e:#}") }),
        };
        let out = serde_json::json!({
            "host": config.host_name(),
            "scope": gh.scope_label(),
            "draining": draining,
            "pools": pools,
            "supervisor": local,
            "github": github,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!("host {} → {}", config.host_name(), gh.scope_label());
    for pool in &config.pools {
        println!(
            "  pool {:<20} {:<16} x{}",
            pool.name,
            pool.backend.kind(),
            pool.count
        );
    }
    let ledger = crate::ledger::load(&[config.ledger_path()]).unwrap_or_default();
    if let Some(summary) = crate::ledger::summary_line(&ledger) {
        println!("  lifetime: {summary}");
    }
    println!();
    match &local {
        None => println!(
            "supervisor: not running (no recent state in {})",
            config.data_dir().display()
        ),
        Some(state) => {
            let now = timefmt::now();
            println!(
                "supervisor: pid {}, up {}{}",
                state.pid,
                crate::ledger::duration((now - state.started_at) as f64),
                if state.draining { ", DRAINING" } else { "" }
            );
            for s in &state.slots {
                println!(
                    "  {:<20} slot {}  {:<8} for {}{}",
                    s.pool,
                    s.slot,
                    format!("{:?}", s.phase).to_lowercase(),
                    crate::ledger::duration((now - s.since).max(0) as f64),
                    match (&s.last_error, s.consecutive_failures) {
                        (Some(e), n) if n > 0 => format!(
                            "  ({n} failure(s); last: {})",
                            e.lines().next().unwrap_or("")
                        ),
                        _ => String::new(),
                    }
                );
            }
            for alert in &state.alerts {
                println!("  ALERT {alert}");
            }
        }
    }
    println!();
    let runners = runners?;
    if runners.is_empty() {
        println!(
            "no runners registered for this host ({})",
            if draining {
                "draining"
            } else {
                "is home-runner running?"
            }
        );
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
    let ctx = Ctx::new(gh, accounting, config.runner_prefix());
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

pub fn short_id() -> String {
    let n = nanos();
    format!("{:06x}", ((n ^ (n >> 24)) as u32) & 0xff_ffff)
}

/// Up to 40% of the minimum backoff (2s at the default 5s), so slots don't retry in lockstep.
fn jitter(min_backoff: Duration) -> Duration {
    let max_ms = (min_backoff.as_millis() * 2 / 5).max(1);
    Duration::from_millis((nanos() % max_ms) as u64)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::ledger;
    use crate::testutil::{Harness, eventually};

    /// A runner that picks up one job and finishes it.
    const ONE_JOB: &str = "echo 'Running job: build'; sleep 0.2; \
                           echo 'Job build completed with result: Succeeded'";
    const IDLE: &str = "exec sleep 30";

    fn phase(h: &Harness) -> Phase {
        h.ctx.slots.snapshot()[0].phase
    }

    /// Runs the slot loop on a task and returns what's needed to stop it.
    fn start_slot(h: &Harness) -> (CancellationToken, tokio::task::JoinHandle<()>) {
        h.ctx.slots.register("t", 0);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(slot_loop(
            h.ctx.clone(),
            h.pool.clone(),
            0,
            shutdown.clone(),
        ));
        (shutdown, task)
    }

    async fn stop(shutdown: CancellationToken, task: tokio::task::JoinHandle<()>) {
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("slot loop didn't stop")
            .unwrap();
    }

    #[tokio::test]
    async fn reconcile_removes_only_what_nobody_owns() {
        let h = Harness::new(IDLE).await;
        h.mock.add_runner("hr-test-t-0-stale1", false);
        h.mock.add_runner("hr-test-t-1-claimed", false);
        h.mock.add_runner("hr-test-t-2-busy", true);
        h.mock.add_runner("hr-other-t-0-theirs", false);
        h.mock.add_runner("some-runner-i-made-by-hand", false);
        {
            let mut fake = h.fake.lock().unwrap();
            fake.resources = [
                "hr-test-t-0-stale1",
                "hr-test-t-1-claimed",
                "hr-other-t-0-theirs",
            ]
            .map(String::from)
            .to_vec();
        }
        let _claim = Claim::new(&h.ctx.active, "hr-test-t-1-claimed");

        reconcile(&h.ctx, std::slice::from_ref(&h.pool))
            .await
            .unwrap();

        // Gone: the stale registration. Kept: the claimed one, a busy orphan (it may be finishing
        // a job), other hosts' runners and anything without our prefix.
        assert_eq!(
            h.mock.names(),
            [
                "hr-other-t-0-theirs",
                "hr-test-t-1-claimed",
                "hr-test-t-2-busy",
                "some-runner-i-made-by-hand",
            ]
        );
        // The backend's stale VM is cleaned; the claimed one and another host's stay.
        assert_eq!(
            h.resources(),
            ["hr-other-t-0-theirs", "hr-test-t-1-claimed"]
        );
        assert_eq!(h.fake.lock().unwrap().cleaned, ["hr-test-t-0-stale1"]);
    }

    #[tokio::test]
    async fn reconcile_pages_through_every_runner() {
        let h = Harness::new(IDLE).await;
        for i in 0..230 {
            h.mock.add_runner(&format!("hr-test-t-0-s{i:03}"), false);
        }
        reconcile(&h.ctx, std::slice::from_ref(&h.pool))
            .await
            .unwrap();
        assert!(h.mock.names().is_empty());
    }

    #[tokio::test]
    async fn one_job_register_run_record_clean_up() {
        let h = Harness::new(ONE_JOB).await;
        h.ctx.slots.register("t", 0);
        let outcome = run_once(&h.ctx, &h.pool, 0, &CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(outcome, Outcome::Completed));

        // Registered once, with the pool's labels and the default runner group.
        let state = h.mock.state.lock().unwrap();
        assert_eq!(state.jit_requests.len(), 1);
        let req = &state.jit_requests[0];
        assert_eq!(
            req["labels"],
            serde_json::json!(["self-hosted", "Linux", "ARM64"])
        );
        assert_eq!(req["runner_group_id"], 1);
        assert!(req["name"].as_str().unwrap().starts_with("hr-test-t-0-"));
        // Deregistered and cleaned up afterwards.
        assert!(state.runners.is_empty());
        assert_eq!(state.deleted.len(), 1);
        drop(state);
        assert!(h.resources().is_empty());
        let fake = h.fake.lock().unwrap();
        assert_eq!(fake.cleaned, fake.launched);
        drop(fake);

        let records = ledger::load(std::slice::from_ref(&h.ledger)).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].result, "Succeeded");
        assert_eq!(records[0].pool, "t");
    }

    #[tokio::test]
    async fn retries_failures_without_leaking_registrations() {
        let h = Harness::new(IDLE).await;
        h.fake.lock().unwrap().launch_failures = 2;
        let (shutdown, task) = start_slot(&h);

        eventually("the third attempt to reach idle", || {
            phase(&h) == Phase::Idle
        })
        .await;
        // Two failed attempts were registered and then deregistered; only the live one remains.
        assert_eq!(h.mock.jit_requests(), 3);
        assert_eq!(h.mock.names().len(), 1);
        let slot = &h.ctx.slots.snapshot()[0];
        assert_eq!(
            slot.consecutive_failures, 2,
            "idle alone doesn't clear the failures"
        );
        assert_eq!(
            slot.last_error.as_deref(),
            Some("launching runner: fake launch failure")
        );

        stop(shutdown, task).await;
        assert!(
            h.mock.names().is_empty(),
            "shutdown deregisters the idle runner"
        );
        assert!(h.resources().is_empty());
    }

    #[tokio::test]
    async fn a_runner_that_stays_up_clears_the_failures() {
        let mut h = Harness::new(IDLE).await;
        h.set_quick_exit(Duration::from_millis(300));
        h.fake.lock().unwrap().launch_failures = 2;
        let (shutdown, task) = start_slot(&h);
        eventually("idle", || phase(&h) == Phase::Idle).await;
        assert_eq!(h.ctx.slots.snapshot()[0].consecutive_failures, 2);
        assert!(!h.ctx.slots.snapshot()[0].is_healthy());
        eventually("proven", || h.ctx.slots.snapshot()[0].is_healthy()).await;
        assert_eq!(h.ctx.slots.snapshot()[0].consecutive_failures, 0);
        stop(shutdown, task).await;
    }

    #[tokio::test]
    async fn a_runner_that_exits_without_a_job_is_a_failure() {
        let h = Harness::new("exit 0").await;
        let (shutdown, task) = start_slot(&h);
        eventually("repeated failures", || {
            h.ctx.slots.snapshot()[0].consecutive_failures >= 3
        })
        .await;
        let slot = &h.ctx.slots.snapshot()[0];
        assert!(
            slot.last_error
                .as_deref()
                .unwrap()
                .contains("without running a job")
        );
        stop(shutdown, task).await;
        assert!(h.mock.names().is_empty());
    }

    #[tokio::test]
    async fn registration_errors_back_off_and_recover() {
        let h = Harness::new(IDLE).await;
        h.mock.state.lock().unwrap().fail_jit = 2;
        let (shutdown, task) = start_slot(&h);
        eventually("recovery", || phase(&h) == Phase::Idle).await;
        assert_eq!(
            h.mock.jit_requests(),
            1,
            "only the successful call is recorded"
        );
        stop(shutdown, task).await;
    }

    #[tokio::test]
    async fn draining_removes_idle_runners_and_resuming_brings_them_back() {
        let h = Harness::new(IDLE).await;
        let (shutdown, task) = start_slot(&h);
        eventually("idle", || phase(&h) == Phase::Idle).await;
        assert_eq!(h.mock.names().len(), 1);

        h.ctx.drain.send_replace(true);
        eventually("paused", || phase(&h) == Phase::Paused).await;
        assert!(
            h.mock.names().is_empty(),
            "the idle runner must not be able to take a job"
        );
        assert!(h.resources().is_empty());
        let registered = h.mock.jit_requests();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            h.mock.jit_requests(),
            registered,
            "no registrations while draining"
        );

        h.ctx.drain.send_replace(false);
        eventually("idle again", || phase(&h) == Phase::Idle).await;
        assert_eq!(h.mock.names().len(), 1);
        stop(shutdown, task).await;
    }

    #[tokio::test]
    async fn draining_lets_a_running_job_finish() {
        // Starts a job, runs for a while, then completes.
        let h = Harness::new(
            "echo 'Running job: slow'; sleep 1; echo 'Job slow completed with result: Succeeded'",
        )
        .await;
        let (shutdown, task) = start_slot(&h);
        eventually("busy", || phase(&h) == Phase::Busy).await;

        h.ctx.drain.send_replace(true);
        eventually("paused after the job", || phase(&h) == Phase::Paused).await;

        let records = ledger::load(std::slice::from_ref(&h.ledger)).unwrap();
        assert_eq!(
            records.len(),
            1,
            "the job was recorded, so it wasn't killed"
        );
        assert_eq!(records[0].result, "Succeeded");
        assert_eq!(h.ctx.slots.snapshot()[0].jobs_done, 1);
        assert!(h.mock.names().is_empty());
        stop(shutdown, task).await;
    }

    #[tokio::test]
    async fn shutdown_during_a_job_records_it_as_interrupted() {
        let h = Harness::new("echo 'Running job: slow'; exec sleep 30").await;
        let (shutdown, task) = start_slot(&h);
        eventually("busy", || phase(&h) == Phase::Busy).await;
        stop(shutdown, task).await;
        let records = ledger::load(std::slice::from_ref(&h.ledger)).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].result, "Interrupted");
        assert!(h.mock.names().is_empty());
        assert!(h.resources().is_empty());
    }
}
