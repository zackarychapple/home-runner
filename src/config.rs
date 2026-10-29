use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::paths;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub github: GithubConfig,
    #[serde(default)]
    pub host: HostConfig,
    #[serde(default, rename = "pool")]
    pub pools: Vec<PoolConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubConfig {
    /// Register org-level runners (recommended).
    pub org: Option<String>,
    /// Or register repo-level runners: "owner/name". Personal accounts must use this.
    pub repo: Option<String>,
    #[serde(default = "default_api_url")]
    pub api_url: String,
    /// Org runner group; 1 is the "Default" group.
    pub runner_group_id: Option<u64>,
    pub auth: AuthConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthConfig {
    /// A GitHub App (recommended): tokens are short-lived and never need rotating.
    App {
        app_id: u64,
        private_key_path: PathBuf,
        installation_id: Option<u64>,
    },
    /// A fine-grained or classic PAT, read from a file or environment variable.
    Token {
        file: Option<PathBuf>,
        env: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub enum Scope {
    Org(String),
    /// "owner/name"
    Repo(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HostConfig {
    /// Unique per machine; prefixes every runner, VM and container this machine owns.
    pub name: Option<String>,
    pub data_dir: Option<PathBuf>,
    pub prevent_sleep: bool,
    pub reconcile_interval_secs: u64,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            name: None,
            data_dir: None,
            prevent_sleep: true,
            reconcile_interval_secs: 120,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PoolConfig {
    pub name: String,
    #[serde(default = "default_count")]
    pub count: usize,
    /// Extra labels; `self-hosted`, the OS and the arch are always added.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Hard limit on a single job, measured from when the job starts.
    #[serde(default = "default_job_timeout")]
    pub job_timeout_minutes: u64,
    #[serde(flatten)]
    pub backend: BackendConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "backend", rename_all = "kebab-case")]
pub enum BackendConfig {
    /// Run the job directly on this machine in a fresh directory. Used for Windows and Intel macOS jobs.
    Host(HostBackendConfig),
    /// Linux jobs in Apple's `container` (one lightweight VM per job). Apple Silicon only.
    AppleContainer(OciConfig),
    /// Linux jobs in Docker (Docker Desktop, Colima, OrbStack).
    Docker(OciConfig),
    /// macOS jobs in a throwaway Tart VM. Apple Silicon only.
    Tart(TartConfig),
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct HostBackendConfig {}

#[derive(Debug, Clone, Deserialize)]
pub struct OciConfig {
    #[serde(default = "default_runner_image")]
    pub image: String,
    /// Path of run.sh inside the image.
    #[serde(default = "default_runner_path")]
    pub runner_path: String,
    pub cpus: Option<u32>,
    /// e.g. "8G"
    pub memory: Option<String>,
    /// "arm64" or "amd64"; defaults to the host architecture.
    pub arch: Option<String>,
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// Pull the image on startup so new runner versions are picked up.
    #[serde(default = "default_true")]
    pub pull: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TartConfig {
    /// Local VM name or OCI image, e.g. "ghcr.io/cirruslabs/macos-sequoia-xcode:latest".
    pub image: String,
    pub cpus: Option<u32>,
    pub memory_mb: Option<u32>,
    #[serde(default = "default_ssh_user")]
    pub ssh_user: String,
    /// Preferred: a key baked into the image's authorized_keys.
    pub ssh_key: Option<PathBuf>,
    /// Works with Cirrus Labs images out of the box (admin/admin); requires `sshpass`.
    pub ssh_password: Option<String>,
    /// Where the runner lives (or gets downloaded to) inside the VM.
    pub runner_dir: Option<String>,
}

impl BackendConfig {
    pub fn kind(&self) -> &'static str {
        match self {
            BackendConfig::Host(_) => "host",
            BackendConfig::AppleContainer(_) => "apple-container",
            BackendConfig::Docker(_) => "docker",
            BackendConfig::Tart(_) => "tart",
        }
    }
}

impl GithubConfig {
    pub fn scope(&self) -> Result<Scope> {
        match (&self.org, &self.repo) {
            (Some(org), None) => Ok(Scope::Org(org.clone())),
            (None, Some(repo)) => {
                ensure!(
                    repo.split('/').count() == 2,
                    "github.repo must look like \"owner/name\""
                );
                Ok(Scope::Repo(repo.clone()))
            }
            _ => bail!("set exactly one of github.org or github.repo"),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "reading {} (run `home-runner init` to create it)",
                path.display()
            )
        })?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn host_name(&self) -> String {
        match &self.host.name {
            Some(name) => sanitize_name(name),
            None => sanitize_name(&gethostname::gethostname().to_string_lossy()),
        }
    }

    pub fn data_dir(&self) -> PathBuf {
        self.host
            .data_dir
            .as_deref()
            .map(paths::expand_home)
            .unwrap_or_else(paths::default_data_dir)
    }

    pub fn log_dir(&self) -> PathBuf {
        self.data_dir().join("logs")
    }

    /// Everything this machine creates starts with this, which is how reconcile finds leftovers.
    pub fn runner_prefix(&self) -> String {
        format!("hr-{}-", self.host_name())
    }

    fn validate(&self) -> Result<()> {
        self.github.scope()?;
        ensure!(!self.pools.is_empty(), "config has no [[pool]] entries");
        let apple_silicon = cfg!(all(target_os = "macos", target_arch = "aarch64"));
        let mut names = HashSet::new();
        let mut macos_vms = 0;
        for pool in &self.pools {
            ensure!(
                !pool.name.is_empty()
                    && pool.name.len() <= 20
                    && pool
                        .name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "pool name {:?} must be 1-20 lowercase letters, digits or dashes",
                pool.name
            );
            ensure!(
                names.insert(&pool.name),
                "duplicate pool name {:?}",
                pool.name
            );
            ensure!(
                pool.count >= 1,
                "pool {:?}: count must be at least 1",
                pool.name
            );
            match &pool.backend {
                BackendConfig::AppleContainer(_) | BackendConfig::Tart(_) => ensure!(
                    apple_silicon,
                    "pool {:?}: the {} backend needs an Apple Silicon Mac",
                    pool.name,
                    pool.backend.kind()
                ),
                BackendConfig::Host(_) | BackendConfig::Docker(_) => {}
            }
            if let BackendConfig::Tart(tart) = &pool.backend {
                macos_vms += pool.count;
                ensure!(
                    tart.ssh_key.is_some() || tart.ssh_password.is_some(),
                    "pool {:?}: set ssh_key or ssh_password so the VM can be reached",
                    pool.name
                );
            }
        }
        // macOS's Virtualization.framework refuses to run more than two macOS guests at once.
        ensure!(
            macos_vms <= 2,
            "macOS allows at most 2 macOS VMs at once, but tart pools ask for {macos_vms}"
        );
        Ok(())
    }
}

/// Lowercase, dash-separated and short enough that full runner names stay well under GitHub's limit.
pub fn sanitize_name(raw: &str) -> String {
    let raw = raw.split('.').next().unwrap_or(raw);
    let mut out = String::new();
    for c in raw.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.trim_matches('-').chars().take(20).collect();
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "host".into() } else { out }
}

fn default_api_url() -> String {
    "https://api.github.com".into()
}
fn default_count() -> usize {
    1
}
fn default_job_timeout() -> u64 {
    360
}
fn default_true() -> bool {
    true
}
fn default_runner_image() -> String {
    "ghcr.io/actions/actions-runner:latest".into()
}
fn default_runner_path() -> String {
    "/home/runner/run.sh".into()
}
fn default_ssh_user() -> String {
    "admin".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_host_names() {
        assert_eq!(
            sanitize_name("Office-MacBook-Pro.local"),
            "office-macbook-pro"
        );
        assert_eq!(sanitize_name("DESKTOP_9QX 7"), "desktop-9qx-7");
        assert_eq!(
            sanitize_name("a-very-long-machine-name-indeed"),
            "a-very-long-machine"
        );
        assert_eq!(sanitize_name("..."), "host");
    }

    #[test]
    fn example_config_parses() {
        let config: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        assert!(matches!(config.github.scope().unwrap(), Scope::Org(_)));
        let kinds: Vec<_> = config.pools.iter().map(|p| p.backend.kind()).collect();
        assert_eq!(kinds, ["apple-container", "apple-container", "tart"]);
        match &config.pools[1].backend {
            BackendConfig::AppleContainer(oci) => {
                assert_eq!(oci.arch.as_deref(), Some("amd64"));
                assert_eq!(oci.cpus, Some(4));
            }
            other => panic!("unexpected backend {other:?}"),
        }
    }
}
