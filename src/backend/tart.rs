use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::process::Command;
use tracing::info;

use super::{ProbeOutput, Running, probe_output};
use crate::config::TartConfig;
use crate::hooks;
use crate::paths::expand_home;
use crate::proc::{self, output, pipe_output};
use crate::runner_dist::RunnerDist;

/// macOS jobs in a Tart VM cloned (copy-on-write) from a golden image and deleted after one job.
pub struct TartBackend {
    cfg: TartConfig,
    /// Where each slot's `cache_size` disk image lives on the host.
    cache_dir: PathBuf,
    dist: Arc<RunnerDist>,
}

impl TartBackend {
    pub fn new(cfg: TartConfig, data_dir: &Path, dist: Arc<RunnerDist>) -> Self {
        Self {
            cfg,
            cache_dir: data_dir.join("tart-cache"),
            dist,
        }
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

    pub fn cpus(&self) -> u32 {
        // Cirrus Labs images default to 4 vCPUs.
        self.cfg.cpus.unwrap_or(4)
    }

    pub async fn launch(
        &self,
        name: &str,
        jit_config: &str,
        hook_env: &[(String, String)],
    ) -> Result<Running> {
        let (vm, ip) = self.boot(name, true).await?;
        // The hooks call `home-runner cpu-sample` inside the VM to read macOS CPU counters.
        self.copy_self(&ip).await?;

        let asset = self.dist.asset("osx", "arm64").await?;
        let mut job = self
            .ssh(&ip)
            .arg(runner_script(
                self.cfg.runner_dir.as_deref(),
                &asset.url,
                &asset.version,
                jit_config,
                hook_env,
                self.cfg.cache_size.is_some(),
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| proc::spawn_error("ssh", e))?;
        let watch = pipe_output(&mut job, name);
        Ok(Running::new(job, Some(vm), false, watch))
    }

    /// Clones the image into a VM called `name`, boots it and waits until SSH answers. The cache
    /// disk is only attached for jobs.
    async fn boot(&self, name: &str, cache: bool) -> Result<(tokio::process::Child, String)> {
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

        let mut run = vec!["run".to_string(), name.into(), "--no-graphics".into()];
        if let (true, Some(size)) = (cache, &self.cfg.cache_size) {
            let disk = self.cache_disk(name, size)?;
            run.push(format!("--disk={}", disk.display()));
        }
        let mut vm = Command::new("tart")
            .args(&run)
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
        Ok((vm, ip))
    }

    /// Boots a throwaway clone and runs `script` in it over SSH as the runner user. Needs one of
    /// the two macOS VM slots, so stop other macOS VMs first if both are in use.
    pub async fn probe(&self, name: &str, script: &str, timeout: Duration) -> Result<ProbeOutput> {
        let run = async {
            let (_vm, ip) = self.boot(name, false).await?;
            let out = self
                .ssh(&ip)
                .arg(format!("/bin/sh -c {}", sh_quote(script)))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|e| proc::spawn_error("ssh", e))?;
            Ok(probe_output(out))
        };
        match tokio::time::timeout(timeout, run).await {
            Ok(result) => result,
            Err(_) => bail!("the probe did not finish within {}s", timeout.as_secs()),
        }
    }

    /// Creates the slot's cache disk the first time it's needed, as a sparse file that only takes
    /// up what has been written. Runner names are `<prefix><slot>-<id>`, so every VM a slot
    /// launches gets `<prefix><slot>-cache.img`. The VM formats it on first boot.
    fn cache_disk(&self, runner: &str, size: &str) -> Result<PathBuf> {
        let slot = runner.rsplit_once('-').map_or(runner, |(slot, _)| slot);
        let path = self.cache_dir.join(format!("{slot}-cache.img"));
        if !path.exists() {
            let bytes = parse_size(size)?;
            std::fs::create_dir_all(&self.cache_dir)?;
            let file = std::fs::File::create_new(&path)
                .with_context(|| format!("creating cache disk {}", path.display()))?;
            file.set_len(bytes)?;
            info!(disk = %path.display(), size, "creating cache disk");
        }
        Ok(path)
    }

    async fn copy_self(&self, ip: &str) -> Result<()> {
        let exe = std::fs::File::open(std::env::current_exe()?)?;
        let status = self
            .ssh(ip)
            .arg("cat > /tmp/home-runner && chmod +x /tmp/home-runner")
            .stdin(Stdio::from(exe))
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .status()
            .await
            .map_err(|e| proc::spawn_error("ssh", e))?;
        anyhow::ensure!(
            status.success(),
            "copying home-runner into the VM failed ({status})"
        );
        Ok(())
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
        let version = output("tart", &["--version"]).await.map_err(missing_tart)?;
        if self.cfg.ssh_password.is_some() {
            output("sshpass", &["-V"])
                .await
                .context("ssh_password needs sshpass")?;
        }
        if let Some(key) = &self.cfg.ssh_key {
            let key = expand_home(key);
            anyhow::ensure!(key.exists(), "ssh_key {} does not exist", key.display());
        }
        if let Some(size) = &self.cfg.cache_size {
            parse_size(size)?;
        }
        let pulled = self.vm_names().await?.contains(&self.cfg.image);
        Ok(format!(
            "tart {version}; image {} {}{}",
            self.cfg.image,
            if pulled {
                "(present)"
            } else {
                "(not pulled yet; first start will download it)"
            },
            match &self.cfg.cache_size {
                Some(size) => format!(
                    "; {size} cache disk per slot in {}",
                    self.cache_dir.display()
                ),
                None => String::new(),
            }
        ))
    }

    async fn vm_names(&self) -> Result<Vec<String>> {
        let json = output("tart", &["list", "--format", "json"])
            .await
            .map_err(missing_tart)?;
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

/// A blank disk is this slot's new cache disk, so format it; the VM's own disk is never blank.
/// macOS mounts it at /Volumes/HRCache, which the image links to /cache. HFS+, because an APFS
/// volume on a VM's internal disk is keyed to that VM's boot disk, and the next job's fresh clone
/// can't unlock it. HFS+ is as fast here for build trees (many small files). A disk that's nearly
/// full is emptied, so a job never fails for lack of space (that job just starts cold).
const SH_MOUNT_CACHE: &str = r#"for d in $(diskutil list physical | awk '/^\/dev\/disk/ {print $1}'); do
  diskutil list "$d" | grep -q '^ *1:' || diskutil eraseDisk JHFS+ HRCache GPT "$d" >/dev/null
done
i=0
until mount | grep -q ' on /Volumes/HRCache '; do
  i=$((i + 1))
  [ "$i" -lt 30 ] || { echo "home-runner: the cache disk did not mount" >&2; exit 1; }
  diskutil mount HRCache >/dev/null 2>&1 || sleep 1
done
c=/Volumes/HRCache
[ -w "$c" ] || sudo -n chown "$(id -u):$(id -g)" "$c"
used=$(df -P "$c" | awk 'NR==2 {sub("%","",$5); print $5}')
if [ "${used:-0}" -ge 85 ]; then
  echo "home-runner: the cache disk is ${used}% full, clearing it"
  find "$c" -mindepth 1 -maxdepth 1 ! -name '.*' -exec rm -rf {} +
fi
"#;

/// Without a cache disk, an image's /cache link would dangle, so give it a plain directory.
const SH_CACHE_DIR: &str = r#"if [ -L /cache ] && [ ! -e /cache/ ]; then
  sudo -n mkdir -p "$(readlink /cache)" && sudo -n chown "$(id -u):$(id -g)" "$(readlink /cache)"
fi
"#;

/// Sets up the cache and the runner, loads the image's environment (`~/.home-runner/env`, see
/// images/macos-latest; sourced once /cache is ready so it can create directories there), then
/// runs one job with the hooks installed and prints what they recorded. Without a cache it uses a
/// runner baked into the image if there is one. With a cache, the runner itself lives on the cache
/// disk, so its work folder is a real directory there: checkouts become incremental fetches and
/// build output carries over. It must not be a symlink, because `actions/checkout` registers its
/// credentials under the path the runner reports, and git only applies them when the path it sees
/// is the same. The runner tarball is kept too, and unpacked again when the version changes.
fn runner_script(
    runner_dir: Option<&str>,
    url: &str,
    version: &str,
    jit_config: &str,
    hook_env: &[(String, String)],
    cache: bool,
) -> String {
    let exports: String = hook_env
        .iter()
        .map(|(k, v)| format!("export {k}={}\n", sh_quote(v)))
        .collect();
    let setup = if cache {
        format!(
            r#"{SH_MOUNT_CACHE}RUNNER_DIR="{runner_dir}"
t=$c/actions-runner-dist/actions-runner-osx-arm64-{version}.tar.gz
if [ "$(cat "$RUNNER_DIR/.home-runner-version" 2>/dev/null)" != "{version}" ]; then
  if [ ! -f "$t" ]; then
    rm -rf "$c/actions-runner-dist" && mkdir -p "$c/actions-runner-dist"
    curl -fsSL -o "$t.part" "{url}" && mv "$t.part" "$t"
  fi
  mkdir -p "$RUNNER_DIR"
  find "$RUNNER_DIR" -mindepth 1 -maxdepth 1 ! -name _work -exec rm -rf {{}} +
  tar -xzf "$t" -C "$RUNNER_DIR"
  echo "{version}" > "$RUNNER_DIR/.home-runner-version"
fi
mkdir -p "$RUNNER_DIR/_work"
"#,
            runner_dir = runner_dir.unwrap_or("/Volumes/HRCache/actions-runner"),
        )
    } else {
        format!(
            r#"{SH_CACHE_DIR}RUNNER_DIR="{runner_dir}"
if [ ! -x "$RUNNER_DIR/run.sh" ]; then
  mkdir -p "$RUNNER_DIR"
  curl -fsSL "{url}" | tar -xz -C "$RUNNER_DIR"
fi
"#,
            runner_dir = runner_dir.unwrap_or("$HOME/actions-runner"),
        )
    };
    format!(
        r#"set -e
{setup}[ ! -f "$HOME/.home-runner/env" ] || . "$HOME/.home-runner/env"
d=/tmp/home-runner-state
{exports}export HR_CPU_MODE=exe HR_EXE=/tmp/home-runner
{hooks}set +e
cd "$RUNNER_DIR"
./run.sh --jitconfig '{jit_config}'
rc=$?
sync
{emit}
exit $rc"#,
        hooks = hooks::sh_install_fragment(),
        emit = hooks::SH_EMIT,
    )
}

/// Tart moved from Cirrus Labs to github.com/openai/tart. The `cirruslabs/cli` Homebrew tap stopped
/// being updated and no longer installs on current Homebrew (it fails on `depends_on macos:`), so
/// point at what does work.
const TART_INSTALL: &str = "tart is not installed. Install it with `brew install openai/tools/tart` \
    (the old `brew install cirruslabs/cli/tart` fails on current Homebrew), or download tart.tar.gz \
    from https://github.com/openai/tart/releases and put its `tart.app/Contents/MacOS/tart` on PATH";

fn missing_tart(e: anyhow::Error) -> anyhow::Error {
    if e.to_string().contains("not found on PATH") {
        anyhow::anyhow!(TART_INSTALL)
    } else {
        e
    }
}

/// Parses sizes like "100G", "512M" or "1T" (powers of 1024; a trailing "B" or "iB" is fine).
fn parse_size(size: &str) -> Result<u64> {
    let s = size.trim().to_ascii_uppercase();
    let s = s.trim_end_matches("IB").trim_end_matches('B');
    let (digits, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let shift = match unit {
        "" => 0,
        "K" => 10,
        "M" => 20,
        "G" => 30,
        "T" => 40,
        _ => bail!("cache_size {size:?}: use a size like \"100G\""),
    };
    let n: u64 = digits
        .parse()
        .with_context(|| format!("cache_size {size:?}: use a size like \"100G\""))?;
    n.checked_mul(1 << shift)
        .filter(|&b| b > 0)
        .with_context(|| format!("cache_size {size:?} is out of range"))
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_tart_says_how_to_install_it() {
        let missing = missing_tart(anyhow::anyhow!("`tart` was not found on PATH"));
        let text = missing.to_string();
        assert!(text.contains("brew install openai/tools/tart"), "{text}");
        assert!(text.contains("releases"));
        // Other failures pass through untouched.
        let other = missing_tart(anyhow::anyhow!("`tart list` failed (exit status: 1): boom"));
        assert!(other.to_string().contains("boom"));
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("100G").unwrap(), 100 << 30);
        assert_eq!(parse_size("512mib").unwrap(), 512 << 20);
        assert_eq!(parse_size("1TB").unwrap(), 1 << 40);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert!(parse_size("").is_err());
        assert!(parse_size("0G").is_err());
        assert!(parse_size("ten gigs").is_err());
        assert!(parse_size("99999999999T").is_err());
    }

    #[test]
    fn cache_script_puts_a_real_work_folder_on_the_cache_disk() {
        let script = runner_script(None, "https://x/r.tgz", "2.330.0", "jit", &[], true);
        let mount = script.find("eraseDisk JHFS+").unwrap();
        let clear = script.find("is ${used}% full").unwrap();
        let unpack = script.find("tar -xzf").unwrap();
        let env = script.find(".home-runner/env").unwrap();
        assert!(mount < clear && clear < unpack && unpack < env);
        assert!(script.contains("RUNNER_DIR=\"/Volumes/HRCache/actions-runner\""));
        assert!(script.contains("! -name _work"));
        assert!(
            !script.contains("ln -s"),
            "the work folder must not be a symlink"
        );
        let plain = runner_script(None, "u", "v", "j", &[], false);
        assert!(!plain.contains("eraseDisk"));
        assert!(plain.contains("RUNNER_DIR=\"$HOME/actions-runner\""));
    }

    /// Set HOME_RUNNER_DUMP to a directory to write the VM scripts there for manual testing.
    #[test]
    fn dump_runner_script() {
        if let Ok(dir) = std::env::var("HOME_RUNNER_DUMP") {
            for (file, cache) in [("tart-cache.sh", true), ("tart.sh", false)] {
                let script = runner_script(None, "URL", "VERSION", "JIT", &[], cache);
                std::fs::write(Path::new(&dir).join(file), script).unwrap();
            }
        }
    }
}
