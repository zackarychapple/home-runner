use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::github::{GitHub, Release};

const RELEASE_TTL: Duration = Duration::from_secs(6 * 3600);

/// Tracks the latest actions/runner release and keeps a local copy for the host backend.
/// Falls back to whatever is cached when GitHub is unreachable so a flaky network doesn't stop jobs.
pub struct RunnerDist {
    gh: Arc<GitHub>,
    http: reqwest::Client,
    cache_dir: PathBuf,
    release: Mutex<Option<(Instant, Release)>>,
    download: Mutex<()>,
}

pub struct RunnerAsset {
    pub version: String,
    pub name: String,
    pub url: String,
}

/// The (os, arch) pair used in actions/runner asset names for this machine.
pub fn host_os_arch() -> (&'static str, &'static str) {
    let os = match std::env::consts::OS {
        "macos" => "osx",
        "windows" => "win",
        _ => "linux",
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        _ => "x64",
    };
    (os, arch)
}

impl RunnerDist {
    pub fn new(gh: Arc<GitHub>, cache_dir: PathBuf) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .expect("building HTTP client");
        Self {
            gh,
            http,
            cache_dir,
            release: Mutex::new(None),
            download: Mutex::new(()),
        }
    }

    async fn release(&self) -> Result<Release> {
        let mut cached = self.release.lock().await;
        if let Some((at, release)) = cached.as_ref()
            && at.elapsed() < RELEASE_TTL
        {
            return Ok(release.clone());
        }
        match self.gh.latest_runner_release().await {
            Ok(release) => {
                *cached = Some((Instant::now(), release.clone()));
                Ok(release)
            }
            Err(e) => match cached.as_ref() {
                Some((_, release)) => {
                    warn!(
                        "checking for a new runner release failed, keeping {}: {e:#}",
                        release.tag_name
                    );
                    Ok(release.clone())
                }
                None => Err(e.context("looking up the latest actions/runner release")),
            },
        }
    }

    pub async fn asset(&self, os: &str, arch: &str) -> Result<RunnerAsset> {
        let release = self.release().await?;
        let version = release.tag_name.trim_start_matches('v').to_string();
        let ext = if os == "win" { "zip" } else { "tar.gz" };
        let name = format!("actions-runner-{os}-{arch}-{version}.{ext}");
        let asset = release
            .assets
            .iter()
            .find(|a| a.name == name)
            .with_context(|| format!("runner release {version} has no asset {name}"))?;
        Ok(RunnerAsset {
            version,
            url: asset.browser_download_url.clone(),
            name,
        })
    }

    /// Path to a local runner archive for (os, arch), downloading the latest if needed.
    pub async fn local_archive(&self, os: &str, arch: &str) -> Result<PathBuf> {
        let _one_download_at_a_time = self.download.lock().await;
        let prefix = format!("actions-runner-{os}-{arch}-");
        let asset = match self.asset(os, arch).await {
            Ok(asset) => asset,
            Err(e) => {
                return match newest_cached(&self.cache_dir, &prefix) {
                    Some(path) => {
                        warn!("using cached runner {}: {e:#}", path.display());
                        Ok(path)
                    }
                    None => Err(e),
                };
            }
        };
        let path = self.cache_dir.join(&asset.name);
        if path.exists() {
            return Ok(path);
        }
        tokio::fs::create_dir_all(&self.cache_dir).await?;
        info!(version = %asset.version, "downloading GitHub Actions runner");
        let partial = self.cache_dir.join(format!("{}.part", asset.name));
        tokio::time::timeout(
            Duration::from_secs(30 * 60),
            download(&self.http, &asset.url, &partial),
        )
        .await
        .context("runner download timed out")??;
        tokio::fs::rename(&partial, &path).await?;
        prune(&self.cache_dir, &prefix, &asset.name);
        Ok(path)
    }
}

async fn download(http: &reqwest::Client, url: &str, dest: &Path) -> Result<()> {
    let mut resp = http.get(url).send().await?.error_for_status()?;
    let mut file = tokio::fs::File::create(dest).await?;
    while let Some(chunk) = resp.chunk().await? {
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(())
}

fn cached_archives(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(prefix) && !n.ends_with(".part"))
        })
        .collect()
}

fn newest_cached(dir: &Path, prefix: &str) -> Option<PathBuf> {
    cached_archives(dir, prefix)
        .into_iter()
        .max_by_key(|p| p.metadata().and_then(|m| m.modified()).ok())
}

fn prune(dir: &Path, prefix: &str, keep: &str) {
    for path in cached_archives(dir, prefix) {
        if path.file_name().and_then(|n| n.to_str()) != Some(keep) {
            let _ = std::fs::remove_file(path);
        }
    }
}
