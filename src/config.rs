use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub github: GithubConfig,
    #[serde(default)]
    pub host: HostConfig,
    #[serde(default)]
    pub pricing: PricingConfig,
    #[serde(default)]
    pub alerts: AlertsConfig,
    #[serde(default, rename = "pool")]
    pub pools: Vec<PoolConfig>,
    /// Report savings and status to a shared fleet (see `home-runner fleet join`).
    pub fleet: Option<FleetConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetConfig {
    /// The fleet's Cloudflare Worker, e.g. "https://fleet.example.workers.dev".
    pub url: String,
    /// Shown to the rest of the fleet; groups this machine with others in the same place.
    pub house: String,
    #[serde(default)]
    pub share: Share,
    /// Where the machine token from `fleet join` is kept [default: <data dir>/fleet-token].
    pub token_file: Option<PathBuf>,
    /// Send private repo names as a short hash instead of the name.
    #[serde(default = "default_true")]
    pub hash_private_repos: bool,
    #[serde(default = "default_heartbeat")]
    pub heartbeat_secs: u64,
}

/// How much job detail this machine shares with the fleet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Share {
    /// Time, results and dollars only: no repo, workflow, people, branches or commits.
    Totals,
    /// Adds repo, workflow and job names.
    #[default]
    Repos,
    /// Everything in the ledger, including who triggered each job, branch, commit and run link.
    Full,
}

impl Share {
    pub fn as_str(self) -> &'static str {
        match self {
            Share::Totals => "totals",
            Share::Repos => "repos",
            Share::Full => "full",
        }
    }
}

/// Inputs for the savings estimates in the job ledger.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PricingConfig {
    /// What you pay for power; 0.17 is roughly the US residential average.
    pub electricity_usd_per_kwh: f64,
    /// Extra power a machine draws while running a job (an M-series Mac mini under load is ~20-40 W).
    pub job_watts: f64,
    /// GitHub proposed (then postponed) a per-minute charge for self-hosted runners on private
    /// repos. Set this if it ever applies to you.
    pub github_self_hosted_usd_per_minute: f64,
    /// Post a "saved ~$X" notice on each workflow run.
    pub annotate_jobs: bool,
    /// Override any built-in per-minute price by SKU id, e.g. `github-linux-arm64-4 = 0.008`.
    pub rates: HashMap<String, f64>,
}

impl Default for PricingConfig {
    fn default() -> Self {
        Self {
            electricity_usd_per_kwh: 0.17,
            job_watts: 30.0,
            github_self_hosted_usd_per_minute: 0.0,
            annotate_jobs: true,
            rates: HashMap::new(),
        }
    }
}

/// Where to send a message when the supervisor can't keep a pool running.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AlertsConfig {
    /// Slack/Discord webhook, ntfy topic URL, or any endpoint that accepts a JSON POST.
    pub webhook_url: Option<String>,
    pub format: AlertFormat,
    /// A slot that hasn't had a runner waiting for (or running) a job for this long counts as
    /// stalled. Backoff between retries tops out at 5 minutes, so anything under 5 is noisy.
    pub stalled_minutes: u64,
}

impl Default for AlertsConfig {
    fn default() -> Self {
        Self {
            webhook_url: None,
            format: AlertFormat::Json,
            stalled_minutes: 10,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AlertFormat {
    /// `{"event": "firing"|"resolved", "host", "key", "message", "time"}`
    Json,
    /// `{"text": …}`
    Slack,
    /// `{"content": …}`
    Discord,
    /// Plain-text body with a `Title` header.
    Ntfy,
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
    /// Org runner group for this pool, overriding `github.runner_group_id`. Lets a pool with a
    /// broad label (like `ubuntu-latest`) serve only the repos that group allows.
    pub runner_group_id: Option<u64>,
    /// Hard limit on a single job, measured from when the job starts.
    #[serde(default = "default_job_timeout")]
    pub job_timeout_minutes: u64,
    /// GitHub-hosted runner to price jobs against (a `home-runner prices` id). Defaults to the
    /// smallest one with at least as many CPUs, same OS and arch.
    pub compare_github: Option<String>,
    /// Depot runner to price jobs against.
    pub compare_depot: Option<String>,
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
    /// Give each slot a persistent volume of this size (e.g. "40G") mounted at /cache, for
    /// package stores and build caches that should survive from one job to the next. Each slot
    /// owns its volume, so concurrent jobs never share one. Docker ignores the size.
    pub cache_size: Option<String>,
    /// Run a Docker daemon inside each job's container, so `docker build`, `docker run`,
    /// `container:` jobs, `services:` and Docker-based actions work. The image must contain
    /// `dockerd` (images/ubuntu-latest does). With `cache_size`, the daemon's images and layers
    /// live in the cache volume and stay warm between jobs. Apple `container` VMs get the extra
    /// capabilities the daemon needs; the `docker` backend runs the container `--privileged`,
    /// which gives its jobs root on the Docker host VM, so use it only for repos you trust.
    #[serde(default)]
    pub docker_in_job: bool,
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
    /// Give each slot a persistent disk image of this size (e.g. "100G", sparse on the host),
    /// formatted APFS and mounted at /cache in the VM, for package stores and build caches that
    /// should survive from one job to the next. Needs an image with the /cache link (see
    /// images/macos-latest). Each slot owns its disk, so concurrent jobs never share one.
    pub cache_size: Option<String>,
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
        Self::parse(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Parses and validates config text.
    pub fn parse(text: &str) -> Result<Self> {
        let config: Config = toml::from_str(text)?;
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

    pub fn ledger_path(&self) -> PathBuf {
        self.data_dir().join(crate::ledger::LEDGER_FILE)
    }

    pub fn fleet_token_path(&self) -> PathBuf {
        self.fleet
            .as_ref()
            .and_then(|f| f.token_file.as_deref())
            .map(paths::expand_home)
            .unwrap_or_else(|| self.data_dir().join("fleet-token"))
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
            for (id, provider) in [
                (&pool.compare_github, crate::pricing::Provider::GitHub),
                (&pool.compare_depot, crate::pricing::Provider::Depot),
            ] {
                if let Some(id) = id {
                    ensure!(
                        crate::pricing::find(id).is_some_and(|s| s.provider == provider),
                        "pool {:?}: unknown {provider:?} runner {id:?} (see `home-runner prices`)",
                        pool.name
                    );
                }
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
        if let Some(fleet) = &self.fleet {
            ensure!(
                ["https://", "http://localhost", "http://127.0.0.1"]
                    .iter()
                    .any(|p| fleet.url.starts_with(p)),
                "fleet.url must be an https:// URL"
            );
            ensure!(
                !fleet.house.trim().is_empty(),
                "fleet.house must not be empty"
            );
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
fn default_heartbeat() -> u64 {
    60
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
        assert_eq!(config.alerts.stalled_minutes, 10);
        assert!(config.alerts.webhook_url.is_none());
        match &config.pools[1].backend {
            BackendConfig::AppleContainer(oci) => {
                assert_eq!(oci.arch.as_deref(), Some("amd64"));
                assert_eq!(oci.cpus, Some(4));
            }
            other => panic!("unexpected backend {other:?}"),
        }
    }

    fn parse(extra: &str) -> Result<Config> {
        let text = format!(
            "[github]\norg = \"o\"\n[github.auth]\ntype = \"token\"\nenv = \"X\"\n{extra}\n\
             [[pool]]\nname = \"p\"\nbackend = \"docker\"\ndocker_in_job = true\n"
        );
        let config: Config = toml::from_str(&text)?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn parses_alerts_and_docker_in_job() {
        let config = parse(
            "[alerts]\nwebhook_url = \"https://hooks.example/x\"\nformat = \"slack\"\nstalled_minutes = 15\n",
        )
        .unwrap();
        assert_eq!(config.alerts.format, AlertFormat::Slack);
        assert_eq!(config.alerts.stalled_minutes, 15);
        match &config.pools[0].backend {
            BackendConfig::Docker(oci) => assert!(oci.docker_in_job),
            other => panic!("unexpected backend {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_alert_settings() {
        assert!(parse("[alerts]\nwebook_url = \"typo\"\n").is_err());
    }
}
