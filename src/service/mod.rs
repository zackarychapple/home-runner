#[cfg(target_os = "macos")]
mod launchd;
#[cfg(windows)]
mod windows;

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config::Config;

pub fn install(
    config_path: &Path,
    account: Option<String>,
    password: Option<String>,
) -> Result<()> {
    // Refuse to install a service that would just crash-loop on a bad config.
    let config = Config::load(config_path)?;
    let config_path = std::path::absolute(config_path)?;
    let exe = std::env::current_exe()?;

    #[cfg(target_os = "macos")]
    {
        let _ = (account, password);
        launchd::install(&exe, &config_path, &config.log_dir())?;
        // The service is up; failing to reach the CLI or the app from outside is worth a warning,
        // not an error that makes the install look as if it hadn't worked.
        let _ = crate::setup::run(&exe);
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = config;
        windows::install(&exe, &config_path, account, password)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (config, exe, config_path, account, password);
        anyhow::bail!(
            "install supports macOS and Windows; on Linux run `home-runner run` under systemd"
        )
    }
}

pub fn uninstall() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        launchd::uninstall()?;
        crate::setup::remove()
    }
    #[cfg(windows)]
    return windows::uninstall();
    #[cfg(not(any(target_os = "macos", windows)))]
    anyhow::bail!("no service to uninstall on this platform")
}

/// Restarts the installed service or agent.
#[cfg_attr(not(feature = "ui"), allow(dead_code))]
pub fn restart() -> Result<()> {
    #[cfg(target_os = "macos")]
    return launchd::restart();
    #[cfg(windows)]
    return windows::restart();
    #[cfg(not(any(target_os = "macos", windows)))]
    anyhow::bail!("no service to restart on this platform; restart `home-runner run` yourself")
}

pub fn run_service(config_path: PathBuf) -> Result<()> {
    #[cfg(windows)]
    return windows::run(config_path);
    #[cfg(not(windows))]
    {
        let _ = config_path;
        anyhow::bail!(
            "`service` is only used by the Windows service manager; use `home-runner run`"
        )
    }
}
