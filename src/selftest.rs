//! `home-runner selftest`: proves a pool works end to end without needing a workflow. For each pool
//! it registers a runner, starts it the way the supervisor would, waits for GitHub to report it
//! online, tears it down, and checks nothing was left behind on GitHub or the backend.
//!
//! The runner is a real one, so a queued job whose labels match can be assigned to it while it is
//! online; the teardown lets such a job finish rather than killing it. What the selftest doesn't
//! run itself is a job: that needs a workflow in a repo. A runner that GitHub sees online
//! has already authenticated, pulled its JIT config and opened its job session, which is where
//! upgrades to Tart, Apple `container` or the runner itself tend to break things.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use tracing::warn;

use crate::config::Config;
use crate::daemon::{self, Built, Ctx, Pool};

pub struct Step {
    pub name: &'static str,
    pub elapsed: Duration,
    /// `None` when the step passed.
    pub error: Option<String>,
}

impl Step {
    fn print(&self) {
        let secs = self.elapsed.as_secs_f32();
        match &self.error {
            None => println!("  ✓ {} ({secs:.1}s)", self.name),
            Some(e) => println!("  ✗ {} ({secs:.1}s): {e}", self.name),
        }
    }
}

pub async fn run(path: &Path, only: Option<&str>, timeout: Duration) -> Result<()> {
    let config = Config::load(path)?;
    if let Some(name) = only
        && !config.pools.iter().any(|p| p.name == name)
    {
        bail!("no pool named {name:?} in the config");
    }
    // Slot 0 of each pool is the one the selftest borrows, so the service must not be running.
    let _lock = daemon::lock_instance(&config)?;
    let Built {
        gh,
        accounting,
        pools,
    } = daemon::build(&config)?;
    let ctx = Ctx::new(gh, accounting, config.runner_prefix());
    println!(
        "selftest on {} → {}",
        config.host_name(),
        ctx.gh.scope_label()
    );

    // Start clean, so "nothing left behind" is about this run and not an earlier crash.
    daemon::reconcile(&ctx, &pools).await?;

    let mut failed = 0;
    for pool in pools
        .iter()
        .filter(|p| only.is_none_or(|n| p.cfg.name == n))
    {
        println!(
            "pool {} ({} x{})",
            pool.cfg.name,
            pool.cfg.backend.kind(),
            pool.cfg.count
        );
        let steps = check_pool(&ctx, pool, timeout, Duration::from_secs(2)).await;
        for step in &steps {
            step.print();
        }
        if steps.iter().any(|s| s.error.is_some()) {
            failed += 1;
        }
    }
    if failed > 0 {
        bail!("{failed} pool(s) failed the selftest");
    }
    println!("\nall pools passed");
    Ok(())
}

/// Runs the whole lifecycle for one pool, always tearing down. Stops at the first failing step
/// (but still cleans up) and returns what ran.
pub async fn check_pool(ctx: &Ctx, pool: &Pool, timeout: Duration, poll: Duration) -> Vec<Step> {
    let mut steps = Vec::new();
    macro_rules! step {
        ($name:expr, $body:expr) => {{
            let started = Instant::now();
            let result: Result<_> = $body.await;
            let (value, error) = match result {
                Ok(v) => (Some(v), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            };
            steps.push(Step {
                name: $name,
                elapsed: started.elapsed(),
                error,
            });
            value
        }};
    }

    let name = format!("{}0-st{}", pool.prefix, daemon::short_id());
    let mut registered = None;
    let mut running = None;

    'run: {
        if step!("backend ready", pool.ready()).is_none() {
            break 'run;
        }
        registered = step!("registered with GitHub", async {
            ctx.gh
                .generate_jitconfig(&name, &pool.labels, pool.cfg.runner_group_id)
                .await
        });
        let Some(jit) = &registered else { break 'run };
        running = step!(
            "runner started",
            pool.backend.launch(&name, &jit.encoded, &pool.hook_env)
        );
        let Some(running) = running.as_mut() else {
            break 'run;
        };
        step!("GitHub sees it online", async {
            let deadline = Instant::now() + timeout;
            loop {
                if ctx
                    .gh
                    .list_runners()
                    .await?
                    .iter()
                    .any(|r| r.name == name && r.status == "online")
                {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    bail!("not online after {}s", timeout.as_secs());
                }
                tokio::select! {
                    status = running.wait() => bail!(
                        "the runner exited before GitHub saw it online ({})",
                        status.map(|s| s.to_string()).unwrap_or_else(|e| format!("{e:#}"))
                    ),
                    _ = tokio::time::sleep(poll) => {}
                }
            }
        });
    }

    // Teardown runs whatever happened above.
    let teardown = step!("torn down, nothing left behind", async {
        if let Some(running) = running.as_mut() {
            // The runner is a real one: if a queued job matching its labels was assigned to it
            // while it was online, killing it would fail someone's job. Let that job finish.
            let assigned = ctx
                .gh
                .list_runners()
                .await
                .is_ok_and(|rs| rs.iter().any(|r| r.name == name && r.busy));
            if assigned {
                warn!(runner = %name, "a queued job was assigned to the selftest runner; letting it finish");
                let limit = Duration::from_secs(pool.cfg.job_timeout_minutes * 60);
                let _ = tokio::time::timeout(limit, running.wait()).await;
            }
            running.kill().await;
        }
        let mut problems = Vec::new();
        if let Err(e) = pool.backend.cleanup(&name).await {
            problems.push(format!("backend cleanup: {e:#}"));
        }
        if let Some(jit) = &registered
            && let Err(e) = ctx.gh.delete_runner(jit.runner_id).await
        {
            problems.push(format!("deregistering: {e:#}"));
        }
        match ctx.gh.list_runners().await {
            Ok(runners) if runners.iter().any(|r| r.name == name) => {
                problems.push("the runner is still registered with GitHub".into())
            }
            Ok(_) => {}
            Err(e) => problems.push(format!("listing runners: {e:#}")),
        }
        match pool.backend.list(&pool.prefix).await {
            Ok(names) if names.contains(&name) => {
                problems.push("the backend still holds the runner".into())
            }
            Ok(_) => {}
            Err(e) => problems.push(format!("listing {}: {e:#}", pool.cfg.backend.kind())),
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(problems.join("; ")))
        }
    });
    let _ = teardown;
    steps
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testutil::Harness;

    /// Brings the first runner the mock sees online, like a real runner connecting.
    fn bring_online(h: &Harness) {
        let mock = h.mock.state.clone();
        tokio::spawn(async move {
            for _ in 0..200 {
                tokio::time::sleep(Duration::from_millis(10)).await;
                let mut state = mock.lock().unwrap();
                if let Some(r) = state.runners.first_mut() {
                    r.status = "online".into();
                    return;
                }
            }
        });
    }

    fn failed_step(steps: &[Step]) -> Option<&Step> {
        steps.iter().find(|s| s.error.is_some())
    }

    #[tokio::test]
    async fn passes_when_the_runner_comes_online_and_cleans_up() {
        let h = Harness::new("exec sleep 30").await;
        bring_online(&h);
        let steps = check_pool(
            &h.ctx,
            &h.pool,
            Duration::from_secs(5),
            Duration::from_millis(20),
        )
        .await;
        let names: Vec<_> = steps.iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            [
                "backend ready",
                "registered with GitHub",
                "runner started",
                "GitHub sees it online",
                "torn down, nothing left behind",
            ]
        );
        assert!(
            failed_step(&steps).is_none(),
            "{:?}",
            failed_step(&steps).map(|s| &s.error)
        );
        assert!(h.mock.names().is_empty());
        assert!(h.resources().is_empty());
        // Named so a crash mid-test would still be swept up by reconcile.
        assert!(h.fake.lock().unwrap().launched[0].starts_with("hr-test-t-0-st"));
    }

    #[tokio::test]
    async fn lets_a_job_that_was_assigned_to_the_selftest_runner_finish() {
        let done = crate::testutil::temp_dir("assigned").join("job-finished");
        let h = Harness::new(&format!("sleep 0.5; touch {}", done.display())).await;
        let mock = h.mock.state.clone();
        tokio::spawn(async move {
            for _ in 0..200 {
                tokio::time::sleep(Duration::from_millis(10)).await;
                if let Some(r) = mock.lock().unwrap().runners.first_mut() {
                    // Online and already handed a job.
                    r.status = "online".into();
                    r.busy = true;
                    return;
                }
            }
        });
        let steps = check_pool(
            &h.ctx,
            &h.pool,
            Duration::from_secs(5),
            Duration::from_millis(20),
        )
        .await;
        assert!(
            failed_step(&steps).is_none(),
            "{:?}",
            failed_step(&steps).map(|s| &s.error)
        );
        assert!(
            done.exists(),
            "the assigned job was killed instead of finishing"
        );
        assert!(h.mock.names().is_empty());
    }

    #[tokio::test]
    async fn times_out_but_still_tears_down() {
        let h = Harness::new("exec sleep 30").await;
        let steps = check_pool(
            &h.ctx,
            &h.pool,
            Duration::from_millis(150),
            Duration::from_millis(20),
        )
        .await;
        let failed = failed_step(&steps).unwrap();
        assert_eq!(failed.name, "GitHub sees it online");
        assert!(
            failed
                .error
                .as_deref()
                .unwrap()
                .contains("not online after")
        );
        // The teardown step ran after the failure and succeeded.
        let last = steps.last().unwrap();
        assert_eq!(last.name, "torn down, nothing left behind");
        assert!(last.error.is_none());
        assert!(h.mock.names().is_empty());
        assert!(h.resources().is_empty());
    }

    #[tokio::test]
    async fn reports_a_runner_that_dies_on_start() {
        let h = Harness::new("exit 3").await;
        let steps = check_pool(
            &h.ctx,
            &h.pool,
            Duration::from_secs(5),
            Duration::from_millis(500),
        )
        .await;
        let failed = failed_step(&steps).unwrap();
        assert_eq!(failed.name, "GitHub sees it online");
        assert!(
            failed
                .error
                .as_deref()
                .unwrap()
                .contains("exited before GitHub saw it online")
        );
        assert!(h.mock.names().is_empty());
    }

    #[tokio::test]
    async fn a_registration_failure_stops_early_and_leaves_nothing() {
        let h = Harness::new("exec sleep 30").await;
        h.mock.state.lock().unwrap().fail_jit = 1;
        let steps = check_pool(
            &h.ctx,
            &h.pool,
            Duration::from_secs(1),
            Duration::from_millis(20),
        )
        .await;
        let names: Vec<_> = steps.iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            [
                "backend ready",
                "registered with GitHub",
                "torn down, nothing left behind"
            ]
        );
        let failed = failed_step(&steps).unwrap();
        assert!(failed.error.as_deref().unwrap().contains("500"));
        assert!(h.fake.lock().unwrap().launched.is_empty());
    }
}
