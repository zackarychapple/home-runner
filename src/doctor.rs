use std::path::Path;

use anyhow::{Result, bail};

use crate::config::Config;
use crate::daemon;

pub async fn run(path: &Path) -> Result<()> {
    let config = Config::load(path)?;
    println!("✓ config {}", path.display());
    println!(
        "  host name {:?}, runners will be named {}<pool>-…",
        config.host_name(),
        config.runner_prefix()
    );

    let (gh, pools) = daemon::build(&config)?;
    let mut failures = 0;

    match gh.list_runners().await {
        Ok(runners) => println!(
            "✓ GitHub: can manage runners for {} ({} registered)",
            gh.scope_label(),
            runners.len()
        ),
        Err(e) => {
            failures += 1;
            println!("✗ GitHub: {e:#}");
        }
    }

    for pool in &pools {
        match pool.backend.check().await {
            Ok(info) => println!(
                "✓ pool {} ({} x{}): {info}",
                pool.cfg.name,
                pool.cfg.backend.kind(),
                pool.cfg.count
            ),
            Err(e) => {
                failures += 1;
                println!(
                    "✗ pool {} ({}): {e:#}",
                    pool.cfg.name,
                    pool.cfg.backend.kind()
                );
            }
        }
        println!("    runs-on: [{}]", pool.labels.join(", "));
    }

    if failures > 0 {
        bail!("{failures} check(s) failed");
    }
    println!("\nall good — `home-runner install` to run it in the background");
    Ok(())
}
