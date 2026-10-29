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
        launchd::install(&exe, &config_path, &config.log_dir())
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
    return launchd::uninstall();
    #[cfg(windows)]
    return windows::uninstall();
    #[cfg(not(any(target_os = "macos", windows)))]
    anyhow::bail!("no service to uninstall on this platform")
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
