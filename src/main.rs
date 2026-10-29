mod accounting;
mod backend;
mod config;
mod daemon;
mod doctor;
mod github;
mod hooks;
mod init;
mod ledger;
mod logging;
mod paths;
mod power;
mod pricing;
mod proc;
mod runner_dist;
mod service;
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
    /// Check GitHub access and that every pool's backend is ready.
    Doctor {
        #[command(flatten)]
        config: ConfigArg,
    },
    /// Run the supervisor in the foreground.
    Run {
        #[command(flatten)]
        config: ConfigArg,
    },
    /// Show this machine's runners as GitHub sees them.
    Status {
        #[command(flatten)]
        config: ConfigArg,
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
    /// Open the dashboard: money saved, job counts and other job stats, updating live.
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
        Command::Doctor { config } => runtime()?.block_on(doctor::run(&config.path())),
        Command::Run { config } => run_foreground(&config.path()),
        Command::Status { config } => runtime()?.block_on(daemon::status(&config.path())),
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
        #[cfg(feature = "ui")]
        Command::Ui { config, query } => ui::run(query.paths(&config), query.since),
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
