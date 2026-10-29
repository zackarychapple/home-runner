use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config::sanitize_name;

pub fn write(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "{} already exists (use --force to overwrite)",
            path.display()
        );
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let host = sanitize_name(&gethostname::gethostname().to_string_lossy());
    std::fs::write(path, template(&host)).with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {}", path.display());
    println!("next: set github.org and github.auth, then run `home-runner doctor`");
    Ok(())
}

fn template(host: &str) -> String {
    let pools = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => APPLE_SILICON_POOLS,
        ("macos", _) => INTEL_MAC_POOLS,
        ("windows", _) => WINDOWS_POOLS,
        _ => LINUX_POOLS,
    };
    let key_path = if cfg!(windows) {
        r"C:\\ProgramData\\home-runner\\github-app.pem"
    } else {
        "~/.home-runner/github-app.pem"
    };
    format!(
        r#"[github]
# Org-level runners (recommended), or `repo = "owner/name"` for a personal account.
org = "your-org"

[github.auth]
# GitHub App with org permission "Self-hosted runners: Read and write".
type = "app"
app_id = 0
private_key_path = "{key_path}"
# Or a personal access token stored in a file:
# type = "token"
# file = "~/.home-runner/token"

[host]
# Must be unique per machine.
name = "{host}"
prevent_sleep = true
{pools}"#
    )
}

const APPLE_SILICON_POOLS: &str = r#"
# Linux arm64 jobs, one lightweight VM per job (Apple `container`).
[[pool]]
name = "linux-arm64"
backend = "apple-container"
count = 2
labels = ["home"]
cpus = 4
memory = "8G"

# Linux x64 jobs through Rosetta.
[[pool]]
name = "linux-x64"
backend = "apple-container"
count = 1
labels = ["home"]
arch = "amd64"
cpus = 4
memory = "8G"

# macOS jobs in a throwaway Tart VM (`brew install cirruslabs/cli/tart`).
[[pool]]
name = "macos"
backend = "tart"
count = 1
labels = ["home"]
image = "ghcr.io/cirruslabs/macos-sequoia-xcode:latest"
cpus = 4
memory_mb = 8192
ssh_user = "admin"
ssh_password = "admin"
"#;

const INTEL_MAC_POOLS: &str = r#"
# Linux x64 jobs in Docker (Colima, OrbStack or Docker Desktop).
[[pool]]
name = "linux-x64"
backend = "docker"
count = 1
labels = ["home"]
cpus = 2
memory = "4g"

# macOS x64 jobs directly on this Mac (no VM isolation on Intel).
[[pool]]
name = "macos-x64"
backend = "host"
count = 1
labels = ["home"]
"#;

const WINDOWS_POOLS: &str = r#"
# Windows x64 jobs directly on this PC, in a fresh runner directory per job.
[[pool]]
name = "windows"
backend = "host"
count = 1
labels = ["home"]

# Linux jobs through Docker Desktop (the service account must be able to reach Docker).
# [[pool]]
# name = "linux-x64"
# backend = "docker"
# count = 1
# labels = ["home"]
"#;

const LINUX_POOLS: &str = r#"
[[pool]]
name = "linux"
backend = "docker"
count = 1
labels = ["home"]
"#;

#[cfg(test)]
mod tests {
    #[test]
    fn template_parses() {
        let text = super::template("test-host");
        let config: crate::config::Config = toml::from_str(&text).unwrap();
        assert!(!config.pools.is_empty());
    }
}
