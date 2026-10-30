use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::paths::home_dir;

const LABEL: &str = "dev.home-runner";

/// Installs a per-user LaunchAgent. Tart and Apple `container` keep their VMs in the user's
/// session, so this runs as you rather than as root; enable automatic login so it starts at boot.
pub fn install(exe: &Path, config: &Path, log_dir: &Path) -> Result<()> {
    let plist = plist_path();
    std::fs::create_dir_all(plist.parent().unwrap())?;
    std::fs::create_dir_all(log_dir)?;
    std::fs::write(&plist, plist_xml(exe, config, log_dir))
        .with_context(|| format!("writing {}", plist.display()))?;

    let domain = gui_domain()?;
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{LABEL}")])
        .stderr(Stdio::null())
        .status();
    // bootout returns before the old process has exited; bootstrap fails until it has.
    let mut last_error = String::new();
    for _ in 0..15 {
        let out = Command::new("launchctl")
            .arg("bootstrap")
            .arg(&domain)
            .arg(&plist)
            .output()?;
        if out.status.success() {
            println!("installed launchd agent {LABEL} ({})", plist.display());
            println!("logs: {}", log_dir.display());
            println!(
                "tip: enable automatic login so it starts after a reboot without anyone signing in"
            );
            return Ok(());
        }
        last_error = String::from_utf8_lossy(&out.stderr).trim().to_string();
        std::thread::sleep(Duration::from_secs(2));
    }
    bail!("launchctl bootstrap failed: {last_error}")
}

pub fn uninstall() -> Result<()> {
    let domain = gui_domain()?;
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{LABEL}")])
        .status();
    let plist = plist_path();
    if plist.exists() {
        std::fs::remove_file(&plist)?;
    }
    println!("removed launchd agent {LABEL}");
    Ok(())
}

/// Restarts the running agent so it picks up config changes.
pub fn restart() -> Result<()> {
    let target = format!("{}/{LABEL}", gui_domain()?);
    let out = Command::new("launchctl")
        .args(["kickstart", "-k", &target])
        .output()?;
    if !out.status.success() {
        bail!(
            "launchctl kickstart failed (is the agent installed? run `home-runner install`): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn plist_path() -> PathBuf {
    home_dir()
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

fn gui_domain() -> Result<String> {
    let out = Command::new("id")
        .arg("-u")
        .output()
        .context("running id -u")?;
    Ok(format!(
        "gui/{}",
        String::from_utf8_lossy(&out.stdout).trim()
    ))
}

fn plist_xml(exe: &Path, config: &Path, log_dir: &Path) -> String {
    let xml = |p: &Path| {
        p.display()
            .to_string()
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>run</string>
    <string>--config</string>
    <string>{config}</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>ExitTimeOut</key><integer>120</integer>
  <key>ProcessType</key><string>Interactive</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>StandardOutPath</key><string>{log}/launchd.out.log</string>
  <key>StandardErrorPath</key><string>{log}/launchd.err.log</string>
</dict>
</plist>
"#,
        exe = xml(exe),
        config = xml(config),
        log = xml(log_dir),
    )
}
