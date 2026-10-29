use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::Command;
use tracing::{debug, info, warn};

use super::{Running, arch_label};
use crate::config::OciConfig;
use crate::proc::{self, output, pipe_output};

#[derive(Clone, Copy)]
pub enum OciKind {
    /// Apple's `container` CLI: every container is its own lightweight VM.
    AppleContainer,
    Docker,
}

/// Linux jobs in a container that is created for one job and removed after it.
pub struct OciBackend {
    kind: OciKind,
    cfg: OciConfig,
}

impl OciBackend {
    pub fn new(kind: OciKind, cfg: OciConfig) -> Self {
        Self { kind, cfg }
    }

    fn program(&self) -> &'static str {
        match self.kind {
            OciKind::AppleContainer => "container",
            OciKind::Docker => "docker",
        }
    }

    /// Normalised to OCI arch names.
    fn arch(&self) -> Option<&'static str> {
        self.cfg.arch.as_deref().map(|a| match a {
            "x64" | "x86_64" | "amd64" => "amd64",
            _ => "arm64",
        })
    }

    fn arch_args(&self) -> Vec<String> {
        match (self.kind, self.arch()) {
            (_, None) => vec![],
            (OciKind::AppleContainer, Some(arch)) => vec!["--arch".into(), arch.into()],
            (OciKind::Docker, Some(arch)) => vec!["--platform".into(), format!("linux/{arch}")],
        }
    }

    pub fn platform_labels(&self) -> Vec<String> {
        let arch = self.arch().unwrap_or(std::env::consts::ARCH);
        vec!["Linux".into(), arch_label(arch).into()]
    }

    pub async fn prepare(&self) -> Result<()> {
        if let OciKind::AppleContainer = self.kind {
            ensure_apple_container_running().await?;
        }
        if self.cfg.pull {
            let mut args = vec!["image".to_string(), "pull".into()];
            if let OciKind::Docker = self.kind {
                args.extend(self.arch_args());
            }
            args.push(self.cfg.image.clone());
            info!(image = %self.cfg.image, "pulling runner image");
            if let Err(e) = output(self.program(), &args).await {
                warn!("image pull failed, will use the local copy if there is one: {e:#}");
            }
        }
        Ok(())
    }

    pub async fn launch(&self, name: &str, jit_config: &str) -> Result<Running> {
        let mut args: Vec<String> = vec!["run".into(), "--rm".into(), "--name".into(), name.into()];
        if let Some(cpus) = self.cfg.cpus {
            args.extend(["--cpus".into(), cpus.to_string()]);
        }
        if let Some(memory) = &self.cfg.memory {
            args.extend(["--memory".into(), memory.clone()]);
        }
        args.extend(self.arch_args());
        args.extend(self.cfg.extra_args.iter().cloned());
        args.push(self.cfg.image.clone());
        args.extend([
            self.cfg.runner_path.clone(),
            "--jitconfig".into(),
            jit_config.into(),
        ]);

        let mut child = Command::new(self.program())
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| proc::spawn_error(self.program(), e))?;
        let job_started = pipe_output(&mut child, name);
        Ok(Running::new(child, None, false, job_started))
    }

    pub async fn cleanup(&self, name: &str) -> Result<()> {
        let args: &[&str] = match self.kind {
            OciKind::AppleContainer => &["delete", "--force", name],
            OciKind::Docker => &["rm", "--force", name],
        };
        // "No such container" is the normal case after `--rm`; only a missing CLI is a real error.
        if let Err(e) = output(self.program(), args).await {
            if e.to_string().contains("not found on PATH") {
                return Err(e);
            }
            debug!("{e:#}");
        }
        Ok(())
    }

    pub async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let names = match self.kind {
            OciKind::Docker => output("docker", &["ps", "--all", "--format", "{{.Names}}"]).await?,
            OciKind::AppleContainer => {
                match output("container", &["list", "--all", "--quiet"]).await {
                    Ok(out) => out,
                    // Older CLIs without --quiet: the first column of the table is the container ID (= name).
                    Err(_) => output("container", &["list", "--all"])
                        .await?
                        .lines()
                        .filter_map(|l| l.split_whitespace().next())
                        .collect::<Vec<_>>()
                        .join("\n"),
                }
            }
        };
        Ok(names
            .lines()
            .map(str::trim)
            .filter(|n| n.starts_with(prefix))
            .map(String::from)
            .collect())
    }

    pub async fn check(&self) -> Result<String> {
        match self.kind {
            OciKind::AppleContainer => {
                let version = output("container", &["--version"]).await?;
                output("container", &["system", "status"]).await.context(
                    "container services are not running; run `container system start` once",
                )?;
                Ok(format!("{version}; image {}", self.cfg.image))
            }
            OciKind::Docker => {
                let version = output("docker", &["info", "--format", "{{.ServerVersion}}"])
                    .await
                    .context("Docker daemon is not reachable")?;
                Ok(format!("Docker {version}; image {}", self.cfg.image))
            }
        }
    }
}

/// Apple's container services stop on reboot until something starts them again.
async fn ensure_apple_container_running() -> Result<()> {
    if output("container", &["system", "status"]).await.is_ok() {
        return Ok(());
    }
    info!("starting Apple container services");
    let start = output("container", &["system", "start"]);
    match tokio::time::timeout(Duration::from_secs(180), start).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(e.context(
            "`container system start` failed; run it once in a terminal to finish first-time setup",
        )),
        Err(_) => anyhow::bail!(
            "`container system start` timed out; run it once in a terminal to finish first-time setup"
        ),
    }
}
