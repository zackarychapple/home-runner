use std::io::IsTerminal;
use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

/// Logs to stderr when attached to a terminal, and to daily-rotated files in `log_dir` when given.
/// Keep the returned guard alive for the life of the process so buffered lines get flushed.
pub fn init(log_dir: Option<&Path>) -> Option<WorkerGuard> {
    init_with_default(log_dir, "info")
}

/// Like [`init`], with a quieter default for commands that print their own results. Set
/// `HOME_RUNNER_LOG` to override.
pub fn init_with_default(log_dir: Option<&Path>, default: &str) -> Option<WorkerGuard> {
    let filter =
        EnvFilter::try_from_env("HOME_RUNNER_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    // Under launchd or the Windows SCM stderr is not a terminal; the log file is the source of truth.
    let stderr = (log_dir.is_none() || std::io::stderr().is_terminal())
        .then(|| fmt::layer().with_writer(std::io::stderr));

    let (file, guard) = match log_dir.map(file_appender) {
        Some(Ok(appender)) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            (
                Some(fmt::layer().with_ansi(false).with_writer(writer)),
                Some(guard),
            )
        }
        Some(Err(e)) => {
            eprintln!("home-runner: cannot open log directory: {e}");
            (None, None)
        }
        None => (None, None),
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(stderr)
        .with(file)
        .init();
    guard
}

fn file_appender(dir: &Path) -> anyhow::Result<RollingFileAppender> {
    std::fs::create_dir_all(dir)?;
    Ok(RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("home-runner")
        .filename_suffix("log")
        .max_log_files(14)
        .build(dir)?)
}
