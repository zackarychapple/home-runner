use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::process::Command;
use tracing::info;

use super::Running;
use crate::config::TartConfig;
use crate::paths::expand_home;
use crate::proc::{self, output, pipe_output};
use crate::runner_dist::RunnerDist;

/// macOS jobs in a Tart VM cloned (copy-on-write) from a golden image and deleted after one job.
pub struct TartBackend {
    cfg: TartConfig,
    dist: Arc<RunnerDist>,
}

impl TartBackend {
    pub fn new(cfg: TartConfig, dist: Arc<RunnerDist>) -> Self {
        Self { cfg, dist }
    }

    pub fn platform_labels(&self) -> Vec<String> {
        vec!["macOS".into(), "ARM64".into()]
    }

    pub async fn prepare(&self) -> Result<()> {
        if self.cfg.ssh_password.is_some() {
            output("sshpass", &["-V"])
                .await
                .context("ssh_password needs sshpass (`brew install hudochenkov/sshpass/sshpass`), or use ssh_key")?;
        }
        if !self.vm_names().await?.contains(&self.cfg.image) {
            info!(image = %self.cfg.image, "pulling macOS image (this can be tens of GB the first time)");
            output("tart", &["pull", &self.cfg.image]).await?;
        }
        Ok(())
    }

    pub async fn launch(&self, name: &str, jit_config: &str) -> Result<Running> {
        output("tart", &["clone", &self.cfg.image, name]).await?;
        let mut set = vec!["set".to_string(), name.into()];
        if let Some(cpus) = self.cfg.cpus {
            set.extend(["--cpu".into(), cpus.to_string()]);
        }
        if let Some(memory) = self.cfg.memory_mb {
            set.extend(["--memory".into(), memory.to_string()]);
        }
        if set.len() > 2 {
            output("tart", &set).await?;
        }

        let mut vm = Command::new("tart")
            .args(["run", name, "--no-graphics"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| proc::spawn_error("tart", e))?;
        pipe_output(&mut vm, &format!("{name}/vm"));

        let ip_args = ["ip", name, "--wait", "180"];
        let ip = tokio::select! {
            ip = output("tart", &ip_args) => ip?,
            status = vm.wait() => bail!("VM exited while booting ({status:?})"),
        };
        self.wait_for_ssh(&ip).await?;

        let asset = self.dist.asset("osx", "arm64").await?;
        let runner_dir = self
            .cfg
            .runner_dir
            .as_deref()
            .unwrap_or("$HOME/actions-runner");
        let mut job = self
            .ssh(&ip)
            .arg(runner_script(runner_dir, &asset.url, jit_config))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| proc::spawn_error("ssh", e))?;
        let job_started = pipe_output(&mut job, name);
        Ok(Running::new(job, Some(vm), false, job_started))
    }

    pub async fn cleanup(&self, name: &str) -> Result<()> {
        if !self.vm_names().await?.iter().any(|n| n == name) {
            return Ok(());
        }
        let _ = output("tart", &["stop", name, "--timeout", "10"]).await;
        output("tart", &["delete", name]).await?;
        Ok(())
    }

    pub async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        Ok(self
            .vm_names()
            .await?
            .into_iter()
            .filter(|n| n.starts_with(prefix))
            .collect())
    }

    pub async fn check(&self) -> Result<String> {
        let version = output("tart", &["--version"]).await?;
        if self.cfg.ssh_password.is_some() {
            output("sshpass", &["-V"])
                .await
                .context("ssh_password needs sshpass")?;
        }
        if let Some(key) = &self.cfg.ssh_key {
            let key = expand_home(key);
            anyhow::ensure!(key.exists(), "ssh_key {} does not exist", key.display());
        }
        let pulled = self.vm_names().await?.contains(&self.cfg.image);
        Ok(format!(
            "tart {version}; image {} {}",
            self.cfg.image,
            if pulled {
                "(present)"
            } else {
                "(not pulled yet; first start will download it)"
            }
        ))
    }

    async fn vm_names(&self) -> Result<Vec<String>> {
        let json = output("tart", &["list", "--format", "json"]).await?;
        let entries: Vec<Value> =
            serde_json::from_str(&json).context("parsing `tart list` output")?;
        Ok(entries
            .iter()
            .filter_map(|e| e.get("Name").and_then(Value::as_str).map(String::from))
            .collect())
    }

    fn ssh(&self, ip: &str) -> Command {
        let mut cmd = match &self.cfg.ssh_password {
            Some(password) => {
                let mut cmd = Command::new("sshpass");
                cmd.env("SSHPASS", password).args(["-e", "ssh"]);
                cmd
            }
            None => Command::new("ssh"),
        };
        cmd.args([
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "LogLevel=ERROR",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=8",
        ]);
        if let Some(key) = &self.cfg.ssh_key {
            cmd.arg("-i")
                .arg(expand_home(key))
                .args(["-o", "IdentitiesOnly=yes"]);
        }
        cmd.arg(format!("{}@{ip}", self.cfg.ssh_user));
        cmd
    }

    async fn wait_for_ssh(&self, ip: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            let ready = self
                .ssh(ip)
                .arg("true")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .status()
                .await
                .is_ok_and(|s| s.success());
            if ready {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!(
                    "SSH to {ip} did not come up within 3 minutes (check ssh_user / ssh_key / ssh_password)"
                );
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }
}

/// Uses a runner baked into the image if there is one, otherwise downloads it, then runs one job.
fn runner_script(runner_dir: &str, url: &str, jit_config: &str) -> String {
    format!(
        r#"set -e
RUNNER_DIR="{runner_dir}"
if [ ! -x "$RUNNER_DIR/run.sh" ]; then
  mkdir -p "$RUNNER_DIR"
  curl -fsSL "{url}" | tar -xz -C "$RUNNER_DIR"
fi
cd "$RUNNER_DIR"
exec ./run.sh --jitconfig '{jit_config}'"#
    )
}
