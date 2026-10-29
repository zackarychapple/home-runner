mod backend;
mod config;
mod daemon;
mod doctor;
mod github;
mod init;
mod logging;
mod paths;
mod power;
mod proc;
mod runner_dist;
mod service;

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
        Command::Service { config } => service::run_service(config.path()),
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
