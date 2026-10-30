use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use tokio::process::Command;
use tracing::debug;

#[cfg(unix)]
use super::probe_output;
use super::{ProbeOutput, Running, arch_label, os_label};
use crate::hooks;
use crate::proc::{self, pipe_output};
use crate::runner_dist::{RunnerDist, host_os_arch};

/// Runs each job directly on this machine, in a fresh copy of the runner that is deleted afterwards.
/// No isolation beyond that: tools installed by one job remain for the next.
pub struct HostBackend {
    work_root: PathBuf,
    dist: Arc<RunnerDist>,
}

impl HostBackend {
    pub fn new(data_dir: &Path, dist: Arc<RunnerDist>) -> Self {
        Self {
            work_root: data_dir.join("work"),
            dist,
        }
    }

    pub fn platform_labels(&self) -> Vec<String> {
        vec![
            os_label(std::env::consts::OS).into(),
            arch_label(std::env::consts::ARCH).into(),
        ]
    }

    pub async fn prepare(&self) -> Result<()> {
        let (os, arch) = host_os_arch();
        self.dist.local_archive(os, arch).await?;
        Ok(())
    }

    pub async fn launch(
        &self,
        name: &str,
        jit_config: &str,
        hook_env: &[(String, String)],
    ) -> Result<Running> {
        let dir = self.work_root.join(name);
        remove_dir(&dir).await?;
        tokio::fs::create_dir_all(&dir).await?;
        let (os, arch) = host_os_arch();
        let archive = self.dist.local_archive(os, arch).await?;
        extract(&archive, &dir).await?;
        let hooks = install_hooks(&dir).await?;

        let script = dir.join(if cfg!(windows) { "run.cmd" } else { "run.sh" });
        let mut cmd = Command::new(&script);
        cmd.arg("--jitconfig")
            .arg(jit_config)
            .envs(hook_env.iter().map(|(k, v)| (k, v)))
            .envs(hooks)
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Own process group / no console, so we can kill the whole runner tree on timeout.
        #[cfg(unix)]
        cmd.process_group(0);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000 /* CREATE_NO_WINDOW */);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("starting {}", script.display()))?;
        let watch = pipe_output(&mut child, name);
        Ok(Running::new(child, None, true, watch))
    }

    pub async fn collect(&self, name: &str) -> HashMap<String, String> {
        let path = self.work_root.join(name).join("hr-state").join("job.env");
        match tokio::fs::read_to_string(&path).await {
            Ok(text) => hooks::parse_env(&text),
            Err(_) => HashMap::new(),
        }
    }

    /// Runs the probe directly on this machine, as the user the service runs as.
    #[cfg(unix)]
    pub async fn probe(&self, script: &str, timeout: Duration) -> Result<ProbeOutput> {
        let child = Command::new("sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("starting sh for the probe")?;
        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(out) => Ok(probe_output(out?)),
            Err(_) => anyhow::bail!("the probe did not finish within {}s", timeout.as_secs()),
        }
    }

    #[cfg(not(unix))]
    pub async fn probe(&self, _script: &str, _timeout: Duration) -> Result<ProbeOutput> {
        anyhow::bail!("prerequisite probes need a POSIX shell, which Windows host pools don't have")
    }

    pub async fn cleanup(&self, name: &str) -> Result<()> {
        remove_dir(&self.work_root.join(name)).await
    }

    pub async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let mut names = Vec::new();
        let mut entries = match tokio::fs::read_dir(&self.work_root).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(names),
            Err(e) => return Err(e.into()),
        };
        while let Some(entry) = entries.next_entry().await? {
            if let Some(name) = entry.file_name().to_str()
                && name.starts_with(prefix)
            {
                names.push(name.to_string());
            }
        }
        Ok(names)
    }

    pub async fn check(&self) -> Result<String> {
        proc::output(&tar_program(), &["--version"]).await?;
        let (os, arch) = host_os_arch();
        let asset = self.dist.asset(os, arch).await?;
        Ok(format!(
            "jobs run directly on this machine; runner {} ({})",
            asset.version, asset.name
        ))
    }
}

/// Writes the job hooks into the runner directory and returns the env vars that enable them.
/// CPU is sampled machine-wide by this binary (`home-runner cpu-sample`).
async fn install_hooks(dir: &Path) -> Result<Vec<(String, String)>> {
    let hook_dir = dir.join("hr-hooks");
    tokio::fs::create_dir_all(&hook_dir).await?;
    let ext = if cfg!(windows) { "ps1" } else { "sh" };
    let (started, completed) = if cfg!(windows) {
        (hooks::ps_started(), hooks::ps_completed())
    } else {
        (hooks::sh_started(), hooks::sh_completed())
    };
    let started_path = hook_dir.join(format!("started.{ext}"));
    let completed_path = hook_dir.join(format!("completed.{ext}"));
    tokio::fs::write(&started_path, started).await?;
    tokio::fs::write(&completed_path, completed).await?;
    #[cfg(unix)]
    for path in [&started_path, &completed_path] {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).await?;
    }
    let exe = std::env::current_exe()?;
    let path = |p: &Path| p.display().to_string();
    Ok(vec![
        (
            "ACTIONS_RUNNER_HOOK_JOB_STARTED".into(),
            path(&started_path),
        ),
        (
            "ACTIONS_RUNNER_HOOK_JOB_COMPLETED".into(),
            path(&completed_path),
        ),
        ("HR_STATE_DIR".into(), path(&dir.join("hr-state"))),
        ("HR_CPU_MODE".into(), "exe".into()),
        ("HR_EXE".into(), path(&exe)),
    ])
}

fn tar_program() -> String {
    // Windows 10+ ships bsdtar, which also extracts zip. Use it explicitly so a Git-for-Windows
    // GNU tar earlier on PATH can't shadow it.
    if cfg!(windows) {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        format!(r"{root}\System32\tar.exe")
    } else {
        "tar".into()
    }
}

async fn extract(archive: &Path, dest: &Path) -> Result<()> {
    let status = Command::new(tar_program())
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .stdin(Stdio::null())
        .status()
        .await
        .context("running tar")?;
    ensure!(
        status.success(),
        "extracting {} failed ({status})",
        archive.display()
    );
    Ok(())
}

/// Job directories can briefly stay locked after a kill (especially on Windows), so retry.
async fn remove_dir(dir: &Path) -> Result<()> {
    let mut attempt = 0;
    loop {
        match tokio::fs::remove_dir_all(dir).await {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) if attempt < 5 => {
                debug!("removing {} failed, retrying: {e}", dir.display());
                attempt += 1;
                #[cfg(windows)]
                clear_readonly(dir.to_path_buf()).await;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => return Err(e).with_context(|| format!("removing {}", dir.display())),
        }
    }
}

/// Git marks pack files read-only, which blocks deletion on Windows.
#[cfg(windows)]
async fn clear_readonly(dir: PathBuf) {
    fn walk(dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(meta) = entry.metadata() {
                if meta.is_dir() {
                    walk(&path);
                }
                let mut perms = meta.permissions();
                if perms.readonly() {
                    #[allow(clippy::permissions_set_readonly_false)]
                    perms.set_readonly(false);
                    let _ = std::fs::set_permissions(&path, perms);
                }
            }
        }
    }
    let _ = tokio::task::spawn_blocking(move || walk(&dir)).await;
}
