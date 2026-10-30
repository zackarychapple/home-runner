#[cfg(test)]
pub(crate) mod fake;
mod host;
mod oci;
mod tart;

use std::collections::HashMap;
use std::path::Path;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::process::Child;

use crate::config::BackendConfig;
use crate::proc::{self, JobWatch};
use crate::runner_dist::RunnerDist;

use host::HostBackend;
use oci::{OciBackend, OciKind};
use tart::TartBackend;

/// Where a job runs. Every backend names its VM/container/work dir after the runner, so cleanup
/// and orphan detection only need the name.
pub enum Backend {
    Host(HostBackend),
    Oci(OciBackend),
    Tart(TartBackend),
    #[cfg(test)]
    #[cfg_attr(not(unix), allow(dead_code))]
    Fake(fake::FakeBackend),
}

/// What a probe printed, and how it ended. `code` is `None` when it was killed.
#[derive(Debug, Clone, Default)]
pub struct ProbeOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: Option<i32>,
}

/// An image as the container CLI reports it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImageInfo {
    pub id: String,
    /// The image's default user (`runner`); empty or `None` means root.
    pub user: Option<String>,
    pub labels: HashMap<String, String>,
}

/// Most a probe may print before the rest is dropped.
const PROBE_OUTPUT_LIMIT: usize = 256 * 1024;

pub(crate) fn probe_output(out: std::process::Output) -> ProbeOutput {
    let clip = |bytes: &[u8]| {
        let bytes = &bytes[..bytes.len().min(PROBE_OUTPUT_LIMIT)];
        String::from_utf8_lossy(bytes).into_owned()
    };
    ProbeOutput {
        stdout: clip(&out.stdout),
        stderr: clip(&out.stderr),
        code: out.status.code(),
    }
}

/// A launched runner. When `job` exits, the runner has finished (or failed) its single job.
pub struct Running {
    job: Child,
    vm: Option<Child>,
    tree_kill: bool,
    pub watch: JobWatch,
}

impl Running {
    fn new(job: Child, vm: Option<Child>, tree_kill: bool, watch: JobWatch) -> Self {
        Self {
            job,
            vm,
            tree_kill,
            watch,
        }
    }

    pub async fn wait(&mut self) -> Result<ExitStatus> {
        match self.vm.as_mut() {
            None => Ok(self.job.wait().await?),
            Some(vm) => tokio::select! {
                status = self.job.wait() => Ok(status?),
                status = vm.wait() => Err(anyhow!(
                    "VM exited while the runner was active ({})",
                    status.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string())
                )),
            },
        }
    }

    pub async fn kill(&mut self) {
        if self.tree_kill {
            proc::kill_tree(&mut self.job).await;
        } else {
            let _ = self.job.kill().await;
        }
        if let Some(vm) = self.vm.as_mut() {
            let _ = vm.kill().await;
        }
    }
}

impl Backend {
    pub fn new(config: &BackendConfig, data_dir: &Path, dist: Arc<RunnerDist>) -> Self {
        match config {
            BackendConfig::Host(_) => Backend::Host(HostBackend::new(data_dir, dist)),
            BackendConfig::AppleContainer(c) => {
                Backend::Oci(OciBackend::new(OciKind::AppleContainer, c.clone()))
            }
            BackendConfig::Docker(c) => Backend::Oci(OciBackend::new(OciKind::Docker, c.clone())),
            BackendConfig::Tart(c) => Backend::Tart(TartBackend::new(c.clone(), data_dir, dist)),
        }
    }

    /// The OS and arch labels GitHub would give a runner of this kind.
    pub fn platform_labels(&self) -> Vec<String> {
        match self {
            Backend::Host(b) => b.platform_labels(),
            Backend::Oci(b) => b.platform_labels(),
            Backend::Tart(b) => b.platform_labels(),
            #[cfg(test)]
            Backend::Fake(_) => vec!["Linux".into(), "ARM64".into()],
        }
    }

    /// One-time setup before the first job (downloads, image pulls). Retried until it succeeds.
    pub async fn prepare(&self) -> Result<()> {
        match self {
            Backend::Host(b) => b.prepare().await,
            Backend::Oci(b) => b.prepare().await,
            Backend::Tart(b) => b.prepare().await,
            #[cfg(test)]
            Backend::Fake(_) => Ok(()),
        }
    }

    /// Number of CPUs a job gets, used to pick a comparable hosted runner.
    pub fn cpus(&self) -> u32 {
        match self {
            Backend::Host(_) => host_cpus(),
            Backend::Oci(b) => b.cpus(),
            Backend::Tart(b) => b.cpus(),
            #[cfg(test)]
            Backend::Fake(_) => 2,
        }
    }

    /// Starts a runner for one job. `hook_env` is passed to the job hooks (see `hooks`).
    pub async fn launch(
        &self,
        name: &str,
        jit_config: &str,
        hook_env: &[(String, String)],
    ) -> Result<Running> {
        match self {
            Backend::Host(b) => b.launch(name, jit_config, hook_env).await,
            Backend::Oci(b) => b.launch(name, jit_config, hook_env).await,
            Backend::Tart(b) => b.launch(name, jit_config, hook_env).await,
            #[cfg(test)]
            Backend::Fake(b) => b.launch(name, jit_config).await,
        }
    }

    /// Job metadata the hooks wrote that isn't in the runner's output (host backend only; the
    /// others print it). Call after the runner exits and before `cleanup`.
    pub async fn collect(&self, name: &str) -> HashMap<String, String> {
        match self {
            Backend::Host(b) => b.collect(name).await,
            Backend::Oci(_) | Backend::Tart(_) => HashMap::new(),
            #[cfg(test)]
            Backend::Fake(_) => HashMap::new(),
        }
    }

    /// Idempotent: removing something that doesn't exist succeeds.
    pub async fn cleanup(&self, name: &str) -> Result<()> {
        match self {
            Backend::Host(b) => b.cleanup(name).await,
            Backend::Oci(b) => b.cleanup(name).await,
            Backend::Tart(b) => b.cleanup(name).await,
            #[cfg(test)]
            Backend::Fake(b) => b.cleanup(name).await,
        }
    }

    /// Names of resources this backend currently holds that start with `prefix`.
    pub async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        match self {
            Backend::Host(b) => b.list(prefix).await,
            Backend::Oci(b) => b.list(prefix).await,
            Backend::Tart(b) => b.list(prefix).await,
            #[cfg(test)]
            Backend::Fake(b) => b.list(prefix).await,
        }
    }

    /// Runs `script` (POSIX shell) once in a throwaway worker the way a job would be run, as the
    /// runner user, and returns what it printed. The worker is named `name`, so `cleanup(name)`
    /// removes it however the probe ended. With `start_docker`, a container backend starts the
    /// pool's in-job Docker daemon first.
    pub async fn probe(
        &self,
        name: &str,
        script: &str,
        start_docker: bool,
        timeout: Duration,
    ) -> Result<ProbeOutput> {
        match self {
            Backend::Host(b) => b.probe(script, timeout).await,
            Backend::Oci(b) => b.probe(name, script, start_docker, timeout).await,
            Backend::Tart(b) => b.probe(name, script, timeout).await,
            #[cfg(test)]
            Backend::Fake(b) => b.probe(name).await,
        }
    }

    /// The image `reference` as stored locally, or `None` if it isn't there. Only container
    /// backends have images.
    pub async fn image_info(&self, reference: &str) -> Result<Option<ImageInfo>> {
        match self {
            Backend::Oci(b) => b.image_info(reference).await,
            _ => Ok(None),
        }
    }

    /// Builds `dockerfile` into the image `tag`, using `dir` for the build files.
    pub async fn build_image(
        &self,
        tag: &str,
        dockerfile: &str,
        dir: &Path,
        no_cache: bool,
    ) -> Result<()> {
        match self {
            Backend::Oci(b) => b.build_image(tag, dockerfile, dir, no_cache).await,
            _ => Err(anyhow!("only container pools can build an image")),
        }
    }

    /// A human-readable readiness check for `home-runner doctor`.
    pub async fn check(&self) -> Result<String> {
        match self {
            Backend::Host(b) => b.check().await,
            Backend::Oci(b) => b.check().await,
            Backend::Tart(b) => b.check().await,
            #[cfg(test)]
            Backend::Fake(_) => Ok("fake".into()),
        }
    }
}

pub fn os_label(os: &str) -> &'static str {
    match os {
        "macos" => "macOS",
        "windows" => "Windows",
        _ => "Linux",
    }
}

pub fn arch_label(arch: &str) -> &'static str {
    match arch {
        "aarch64" | "arm64" => "ARM64",
        _ => "X64",
    }
}

pub fn host_cpus() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
}
