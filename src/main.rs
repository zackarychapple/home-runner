mod accounting;
mod backend;
mod config;
mod daemon;
mod doctor;
mod fleet;
mod github;
mod health;
mod hooks;
mod init;
mod ledger;
mod live;
mod logging;
mod paths;
mod power;
mod prereq;
mod pricing;
mod proc;
mod profile;
mod runner_dist;
mod selftest;
mod service;
#[cfg(unix)]
mod setup;
#[cfg(test)]
mod testutil;
mod timefmt;
#[cfg(feature = "ui")]
mod ui;

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    name = "home-runner",
    version,
    about = "Turn spare Macs and PCs into ephemeral GitHub Actions runners"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct ConfigArg {
    /// Path to config.toml [default: ~/.home-runner/config.toml, or
    /// %ProgramData%\home-runner\config.toml on Windows]
    #[arg(long, short)]
    config: Option<PathBuf>,
}

/// Which pools `drain` and `resume` act on; none means the whole machine.
#[derive(Args)]
struct PoolArgs {
    /// A pool to act on (repeatable).
    #[arg(long = "pool", value_name = "NAME")]
    pool: Vec<String>,
    /// Act on every pool with this backend (host, docker, apple-container, tart); repeatable.
    #[arg(long = "backend", value_name = "KIND")]
    backend: Vec<String>,
}

impl PoolArgs {
    fn resolve(&self, config: &config::Config) -> anyhow::Result<Vec<String>> {
        let mut names = Vec::new();
        for name in &self.pool {
            anyhow::ensure!(
                config.pools.iter().any(|p| &p.name == name),
                "no pool named {name:?}; configured: {}",
                config
                    .pools
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            names.push(name.clone());
        }
        for kind in &self.backend {
            let matching: Vec<_> = config
                .pools
                .iter()
                .filter(|p| p.backend.kind() == kind)
                .map(|p| p.name.clone())
                .collect();
            anyhow::ensure!(!matching.is_empty(), "no pool uses the {kind:?} backend");
            names.extend(matching);
        }
        names.sort();
        names.dedup();
        Ok(names)
    }
}

impl ConfigArg {
    fn path(&self) -> PathBuf {
        self.config
            .clone()
            .unwrap_or_else(paths::default_config_path)
    }
}

#[derive(Subcommand)]
enum Command {
    /// Write a starter config tailored to this machine.
    Init {
        #[command(flatten)]
        config: ConfigArg,
        /// Overwrite an existing config file.
        #[arg(long)]
        force: bool,
    },
    /// Check GitHub access, that every pool's backend is ready, and how isolated each pool is.
    Doctor {
        #[command(flatten)]
        config: ConfigArg,
        /// Exit non-zero on warnings too (open runner groups, readable credentials, …).
        #[arg(long)]
        strict: bool,
    },
    /// Check, install and repair the job prerequisites (`[[profile]]`) of each pool that sets
    /// `profile`: tools, libraries, Playwright browsers and Docker, verified inside a worker.
    Prereq {
        #[command(subcommand)]
        action: PrereqAction,
    },
    /// Start a runner of each pool, wait for GitHub to see it online, then tear it down and check
    /// nothing was left behind. Stop the service first.
    Selftest {
        #[command(flatten)]
        config: ConfigArg,
        /// Only test this pool.
        #[arg(long)]
        pool: Option<String>,
        /// How long to wait for a runner to come online.
        #[arg(long, default_value = "5m")]
        timeout: String,
    },
    /// Run the supervisor in the foreground.
    Run {
        #[command(flatten)]
        config: ConfigArg,
    },
    /// Show what the supervisor is doing and this machine's runners as GitHub sees them.
    Status {
        #[command(flatten)]
        config: ConfigArg,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Stop taking jobs: idle runners are removed, running jobs finish. For upgrades and
    /// maintenance; `resume` undoes it. With --pool or --backend only those pools stop, which
    /// frees their VMs and containers while the others keep taking jobs.
    Drain {
        #[command(flatten)]
        config: ConfigArg,
        #[command(flatten)]
        pools: PoolArgs,
        /// Resume by itself after this long, e.g. 2h.
        #[arg(long = "for", value_name = "DURATION")]
        duration: Option<String>,
        /// Why, shown in `status`, the UI and the fleet view.
        #[arg(long)]
        reason: Option<String>,
        /// Wait until no job is running.
        #[arg(long)]
        wait: bool,
        /// With --wait, give up after this long.
        #[arg(long, default_value = "6h", requires = "wait")]
        timeout: String,
    },
    /// Take jobs again after `drain`.
    Resume {
        #[command(flatten)]
        config: ConfigArg,
        #[command(flatten)]
        pools: PoolArgs,
    },
    /// Remove idle runner registrations, VMs and containers this machine left behind.
    /// Stop the service first.
    Cleanup {
        #[command(flatten)]
        config: ConfigArg,
    },
    /// Install as a background service (launchd agent on macOS, Windows service on Windows).
    Install {
        #[command(flatten)]
        config: ConfigArg,
        /// Windows only: account the service runs as, e.g. `.\ci`. Defaults to LocalSystem.
        #[arg(long)]
        account: Option<String>,
        /// Windows only: password for --account.
        #[arg(long, requires = "account")]
        password: Option<String>,
    },
    /// Remove the background service.
    Uninstall,
    /// Put `home-runner` on your shell's PATH and, on a Mac, the desktop app in /Applications.
    /// `install` does this too; run it on its own after replacing the binary somewhere else.
    #[cfg(unix)]
    Setup,
    /// List jobs this machine has run: repo, workflow, who triggered it, time, CPU and savings.
    Jobs {
        #[command(flatten)]
        config: ConfigArg,
        #[command(flatten)]
        query: LedgerQuery,
        /// Show at most this many (most recent) jobs.
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long, value_enum, default_value = "table")]
        format: ledger::Format,
    },
    /// Summarise time, CPU and money saved versus GitHub-hosted and Depot runners.
    Savings {
        #[command(flatten)]
        config: ConfigArg,
        #[command(flatten)]
        query: LedgerQuery,
        /// Break the totals down by this.
        #[arg(long, value_enum, default_value = "repo")]
        by: ledger::GroupBy,
    },
    /// Show where jobs spend their time (steps, queueing, runner startup), whether the caches are
    /// warm and hitting, and how much memory jobs use.
    Profile {
        #[command(flatten)]
        config: ConfigArg,
        #[command(flatten)]
        query: LedgerQuery,
        /// Only jobs whose workflow or job name contains this.
        #[arg(long)]
        job: Option<String>,
        /// How many jobs and steps to list.
        #[arg(long, default_value_t = 15)]
        top: usize,
        #[arg(long, value_enum, default_value = "table")]
        format: ProfileFormat,
    },
    /// Join a shared fleet, or see how it is doing.
    Fleet {
        #[command(subcommand)]
        command: FleetCommand,
    },
    /// Open the desktop app: running jobs, history, savings stats, the fleet and settings.
    #[cfg(feature = "ui")]
    Ui {
        #[command(flatten)]
        config: ConfigArg,
        #[command(flatten)]
        query: LedgerQuery,
    },
    /// List the hosted-runner prices used for savings estimates.
    Prices,
    /// Print machine-wide CPU counters (used by the job hooks).
    #[command(hide = true)]
    CpuSample,
    /// Entry point used by the Windows service manager.
    #[command(hide = true)]
    Service {
        #[command(flatten)]
        config: ConfigArg,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Init { config, force } => init::write(&config.path(), force),
        Command::Doctor { config, strict } => {
            runtime()?.block_on(doctor::run(&config.path(), strict))
        }
        Command::Prereq { action } => {
            let _log = logging::init_with_default(None, "warn");
            match action {
                PrereqAction::Inspect { config, pool } => {
                    runtime()?.block_on(prereq::run::inspect(&config.path(), pool.as_deref()))
                }
                PrereqAction::Install {
                    config,
                    pool,
                    dry_run,
                    force,
                } => runtime()?.block_on(prereq::run::install_cmd(
                    &config.path(),
                    pool.as_deref(),
                    dry_run,
                    force,
                )),
            }
        }
        Command::Selftest {
            config,
            pool,
            timeout,
        } => {
            // The runner's console output would bury the step results; HOME_RUNNER_LOG=info shows it.
            let _log = logging::init_with_default(None, "warn");
            let timeout = timefmt::parse_since(&timeout)?
                .ok_or_else(|| anyhow::anyhow!("--timeout must be a duration like 5m"))?;
            runtime()?.block_on(selftest::run(&config.path(), pool.as_deref(), timeout))
        }
        Command::Run { config } => run_foreground(&config.path()),
        Command::Status { config, json } => {
            runtime()?.block_on(daemon::status(&config.path(), json))
        }
        Command::Drain {
            config,
            pools,
            duration,
            reason,
            wait,
            timeout,
        } => {
            let config = config::Config::load(&config.path())?;
            let timeout = timefmt::parse_since(&timeout)?
                .ok_or_else(|| anyhow::anyhow!("--timeout must be a duration like 6h"))?;
            let duration = duration
                .map(|d| {
                    timefmt::parse_since(&d)?
                        .ok_or_else(|| anyhow::anyhow!("--for must be a duration like 2h"))
                })
                .transpose()?;
            let request = health::DrainRequest {
                pools: pools.resolve(&config)?,
                duration,
                reason,
                wait: wait.then_some(timeout),
            };
            runtime()?.block_on(health::drain(&config, request))
        }
        Command::Resume { config, pools } => {
            let config = config::Config::load(&config.path())?;
            health::resume(&config, &pools.resolve(&config)?)
        }
        Command::Cleanup { config } => {
            let _log = logging::init(None);
            runtime()?.block_on(daemon::cleanup(&config.path()))
        }
        Command::Install {
            config,
            account,
            password,
        } => service::install(&config.path(), account, password),
        Command::Uninstall => service::uninstall(),
        #[cfg(unix)]
        Command::Setup => setup::run(&std::env::current_exe()?),
        Command::Jobs {
            config,
            query,
            limit,
            format,
        } => {
            let records = query.load(&config)?;
            ledger::print_jobs(&records, limit, format)
        }
        Command::Savings { config, query, by } => {
            let records = query.load(&config)?;
            let period = match query.since.as_str() {
                "all" => "all time".to_string(),
                since => format!("in the last {since}"),
            };
            ledger::print_savings(&records, by, &period);
            Ok(())
        }
        Command::Profile {
            config,
            query,
            job,
            top,
            format,
        } => {
            let mut records = query.load(&config)?;
            if let Some(needle) = job.map(|j| j.to_lowercase()) {
                records.retain(|r| {
                    [&r.workflow, &r.job].iter().any(|f| {
                        f.as_deref()
                            .is_some_and(|f| f.to_lowercase().contains(&needle))
                    })
                });
            }
            let profile = profile::build(&records);
            match format {
                ProfileFormat::Table => profile::print(&profile, top, top),
                ProfileFormat::Json => println!("{}", serde_json::to_string_pretty(&profile)?),
            }
            Ok(())
        }
        Command::Fleet { command } => runtime()?.block_on(fleet_command(command)),
        #[cfg(feature = "ui")]
        Command::Ui { config, query } => {
            let ledgers = query.paths(&config);
            ui::run(config.path(), ledgers, query.since)
        }
        Command::Prices => {
            print_prices();
            Ok(())
        }
        Command::CpuSample => {
            if let Some((busy, total)) = hooks::cpu_ticks() {
                println!("{busy} {total}");
            }
            Ok(())
        }
        Command::Service { config } => service::run_service(config.path()),
    }
}

#[derive(Subcommand)]
enum PrereqAction {
    /// Probe a throwaway worker of each pool as the runner user and report what is missing.
    Inspect {
        #[command(flatten)]
        config: ConfigArg,
        /// Only this pool.
        #[arg(long)]
        pool: Option<String>,
    },
    /// Build the derived image that has the missing requirements, then verify it. Safe to
    /// repeat; does nothing when the image is already up to date.
    #[command(alias = "repair")]
    Install {
        #[command(flatten)]
        config: ConfigArg,
        /// Only this pool.
        #[arg(long)]
        pool: Option<String>,
        /// Print what would be built instead of building it.
        #[arg(long)]
        dry_run: bool,
        /// Rebuild even if the image is up to date, without the build cache.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum FleetCommand {
    /// Join with an invite code: stores this machine's token and adds a [fleet] section to the config.
    Join {
        #[command(flatten)]
        config: ConfigArg,
        /// The fleet's Worker URL, e.g. https://fleet.example.workers.dev
        #[arg(long)]
        url: String,
        #[arg(long)]
        invite: String,
        /// Where this machine is, as the rest of the fleet will see it, e.g. "Zack's house".
        #[arg(long)]
        house: String,
        /// How much job detail to share.
        #[arg(long, value_enum, default_value = "repos")]
        share: config::Share,
    },
    /// Fleet-wide totals, houses and machines.
    Status {
        #[command(flatten)]
        config: ConfigArg,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ProfileFormat {
    Table,
    Json,
}

#[derive(Args)]
struct LedgerQuery {
    /// How far back to look: 24h, 7d, 4w, or all.
    #[arg(long, default_value = "30d")]
    since: String,
    /// Only jobs whose repo contains this.
    #[arg(long)]
    repo: Option<String>,
    /// Only jobs triggered by this user.
    #[arg(long)]
    actor: Option<String>,
    /// Read these ledgers instead of this machine's (repeat to combine machines).
    #[arg(long = "ledger")]
    ledgers: Vec<PathBuf>,
}

impl LedgerQuery {
    fn paths(&self, config: &ConfigArg) -> Vec<PathBuf> {
        if self.ledgers.is_empty() {
            // The ledger lives next to the config; don't require valid GitHub credentials to read it.
            let data_dir = config::Config::load(&config.path())
                .map(|c| c.data_dir())
                .unwrap_or_else(|_| paths::default_data_dir());
            vec![data_dir.join(ledger::LEDGER_FILE)]
        } else {
            self.ledgers.clone()
        }
    }

    fn load(&self, config: &ConfigArg) -> Result<Vec<ledger::JobRecord>> {
        let paths = self.paths(config);
        let since = timefmt::parse_since(&self.since)?.map(|d| timefmt::now() - d.as_secs() as i64);
        let filter = ledger::Filter {
            since,
            repo: self.repo.clone(),
            actor: self.actor.clone(),
        };
        Ok(filter.apply(ledger::load(&paths)?))
    }
}

async fn fleet_command(command: FleetCommand) -> Result<()> {
    match command {
        FleetCommand::Join {
            config,
            url,
            invite,
            house,
            share,
        } => {
            let name =
                fleet::join_and_configure(&config.path(), &url, &invite, &house, share).await?;
            println!(
                "joined fleet {name:?} as {house:?}, sharing {}",
                share.as_str()
            );
            println!("added [fleet] to {}", config.path().display());
            println!("restart home-runner (`home-runner install` again) to start reporting");
            Ok(())
        }
        FleetCommand::Status { config } => {
            let config = config::Config::load(&config.path())?;
            let Some(client) = fleet::Client::from_config(&config)? else {
                anyhow::bail!("this machine hasn't joined a fleet (see `home-runner fleet join`)");
            };
            let view = client.view().await?;
            let m = &view.month;
            println!("fleet {}", view.fleet);
            println!(
                "  this month: {} jobs, {:.1}h, saved {} vs GitHub ({} vs Depot), power {}",
                m.jobs,
                m.runner_secs / 3600.0,
                ledger::usd(m.saved_vs_github_usd),
                ledger::usd(m.saved_vs_depot_usd),
                ledger::usd(m.own_usd)
            );
            for house in &view.houses {
                println!(
                    "\n  {}  ({} saved this month)",
                    house.house,
                    ledger::usd(house.saved_month_usd)
                );
                for h in &house.hosts {
                    let slots: Vec<_> = h.pools.iter().flat_map(|p| &p.slots).collect();
                    let busy = slots
                        .iter()
                        .filter(|s| s.phase == health::Phase::Busy)
                        .count();
                    println!(
                        "    {:<22} {:<8} {}/{} busy  {}",
                        h.host,
                        if h.online { "online" } else { "offline" },
                        busy,
                        slots.len(),
                        if h.online {
                            format!("v{}", h.version)
                        } else {
                            format!("last seen {}", timefmt::short(h.last_seen))
                        }
                    );
                }
            }
            Ok(())
        }
    }
}

fn print_prices() {
    println!(
        "Hosted runner list prices (as of {}), USD per minute.",
        pricing::PRICES_AS_OF
    );
    println!("GitHub rounds each job up to a whole minute; Depot bills per second.");
    println!(
        "Override any rate in [pricing.rates]; pick one per pool with compare_github / compare_depot.\n"
    );
    for sku in pricing::SKUS {
        println!(
            "  {:<28} {:<30} {:>3} CPU  ${:.3}{}",
            sku.id,
            sku.label,
            sku.cpus,
            sku.usd_per_minute,
            if sku.free_for_public {
                "  (free for public repos)"
            } else {
                ""
            }
        );
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}

fn run_foreground(path: &Path) -> Result<()> {
    let config = config::Config::load(path)?;
    let _log = logging::init(Some(&config.log_dir()));
    let shutdown = CancellationToken::new();
    runtime()?.block_on(async {
        let token = shutdown.clone();
        tokio::spawn(async move {
            daemon::wait_for_signal().await;
            tracing::info!("shutdown requested");
            token.cancel();
        });
        let result = daemon::run(config, shutdown).await;
        if let Err(e) = &result {
            tracing::error!("{e:#}");
        }
        result
    })
}
