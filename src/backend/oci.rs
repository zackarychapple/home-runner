use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::Command;
use tracing::{debug, info, warn};

use super::{Running, arch_label};
use crate::config::OciConfig;
use crate::hooks;
use crate::proc::{self, output, pipe_output};

#[derive(Clone, Copy)]
pub enum OciKind {
    /// Apple's `container` CLI: every container is its own lightweight VM.
    AppleContainer,
    Docker,
}

/// What `dockerd` needs inside an Apple `container` VM: mounting, netfilter and bridge setup, and
/// device nodes. Every container is its own VM, so these stay inside it.
const DOCKER_CAPS: &[&str] = &[
    "CAP_SYS_ADMIN",
    "CAP_NET_ADMIN",
    "CAP_NET_RAW",
    "CAP_MKNOD",
    "CAP_SYS_RESOURCE",
    "CAP_SETFCAP",
];

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

    pub fn cpus(&self) -> u32 {
        // Apple's container defaults to 4 CPUs per container; Docker shares every host CPU.
        self.cfg.cpus.unwrap_or(match self.kind {
            OciKind::AppleContainer => 4,
            OciKind::Docker => super::host_cpus(),
        })
    }

    pub async fn launch(
        &self,
        name: &str,
        jit_config: &str,
        hook_env: &[(String, String)],
    ) -> Result<Running> {
        let volume = match self.cfg.cache_size {
            Some(_) => Some(self.cache_volume(name).await?),
            None => None,
        };
        let args = self.run_args(name, volume.as_deref(), jit_config, hook_env);
        let mut child = Command::new(self.program())
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| proc::spawn_error(self.program(), e))?;
        let watch = pipe_output(&mut child, name);
        Ok(Running::new(child, None, false, watch))
    }

    /// The arguments for `run`. `volume` is the slot's cache volume, if the pool has one.
    fn run_args(
        &self,
        name: &str,
        volume: Option<&str>,
        jit_config: &str,
        hook_env: &[(String, String)],
    ) -> Vec<String> {
        let mut args: Vec<String> = vec!["run".into(), "--rm".into(), "--name".into(), name.into()];
        if let Some(cpus) = self.cfg.cpus {
            args.extend(["--cpus".into(), cpus.to_string()]);
        }
        if let Some(memory) = &self.cfg.memory {
            args.extend(["--memory".into(), memory.clone()]);
        }
        args.extend(self.arch_args());
        if let Some(volume) = volume {
            args.extend(["--volume".into(), format!("{volume}:{}", self.work_dir())]);
        }
        if self.cfg.docker_in_job {
            match self.kind {
                OciKind::AppleContainer => {
                    for cap in DOCKER_CAPS {
                        args.extend(["--cap-add".into(), (*cap).into()]);
                    }
                }
                OciKind::Docker => args.push("--privileged".into()),
            }
        }
        // Apple's containers are whole VMs, so /proc/stat is the job's; Docker shares a VM, so use
        // the container's own cgroup.
        let cpu_mode = match self.kind {
            OciKind::AppleContainer => "procstat",
            OciKind::Docker => "usec",
        };
        let mut env: Vec<(String, String)> = hook_env.to_vec();
        env.push(("HR_CPU_MODE".into(), cpu_mode.into()));
        env.push(("HR_RUNNER".into(), self.cfg.runner_path.clone()));
        if self.cfg.docker_in_job {
            env.push(("HR_DOCKER".into(), "1".into()));
        }
        for (k, v) in env {
            args.extend(["--env".into(), format!("{k}={v}")]);
        }
        args.extend(self.cfg.extra_args.iter().cloned());
        args.push(self.cfg.image.clone());
        args.extend([
            "/bin/sh".into(),
            "-c".into(),
            wrapper_script(self.cfg.cache_size.is_some(), self.cfg.docker_in_job),
            "home-runner".into(),
            jit_config.into(),
        ]);
        args
    }

    /// The runner's work folder (`_work` next to run.sh). The cache volume is mounted here, as the
    /// real directory rather than behind a symlink: `actions/checkout` registers its credentials
    /// under the logical path, and git only matches them when the path it sees is the same one.
    fn work_dir(&self) -> String {
        let runner = std::path::Path::new(&self.cfg.runner_path);
        let dir = runner
            .parent()
            .map_or_else(String::new, |p| p.display().to_string());
        format!("{dir}/_work")
    }

    /// Creates the slot's cache volume the first time it's needed. Runner names are
    /// `<prefix><slot>-<id>`, so every runner a slot launches maps to `<prefix><slot>-cache`.
    async fn cache_volume(&self, runner: &str) -> Result<String> {
        let slot = runner.rsplit_once('-').map_or(runner, |(slot, _)| slot);
        let volume = format!("{slot}-cache");
        let inspect = ["volume", "inspect", volume.as_str()];
        if output(self.program(), &inspect).await.is_ok() {
            return Ok(volume);
        }
        let mut create = vec!["volume".to_string(), "create".into()];
        if let (OciKind::AppleContainer, Some(size)) = (self.kind, &self.cfg.cache_size) {
            create.extend(["-s".into(), size.clone()]);
        }
        create.push(volume.clone());
        info!(%volume, "creating cache volume");
        output(self.program(), &create)
            .await
            .with_context(|| format!("creating cache volume {volume}"))?;
        Ok(volume)
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

/// Makes a fresh volume writable by the runner user and empties one that is nearly full, so a job
/// never fails for lack of space (that job just starts cold). The volume is the runner's work
/// folder, so checkouts become incremental fetches and package managers can hardlink from a store
/// on the same filesystem instead of copying it. `/cache`, where the image keeps its package
/// stores and build caches, is a symlink to a hidden folder inside it. (`_work` itself must not be
/// the symlink; see `work_dir`.)
const SH_CACHE_PREP: &str = r#"w="$(dirname "$HR_RUNNER")/_work"
[ -w "$w" ] || sudo -n chown "$(id -u):$(id -g)" "$w"
used=$(df -P "$w" | awk 'NR==2 {sub("%","",$5); print $5}')
if [ "${used:-0}" -ge 85 ]; then
  echo "home-runner: $w is ${used}% full, clearing it"
  # Docker's layers are root-owned.
  sudo -n rm -rf "$w/.cache/docker"
  find "$w" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
fi
mkdir -p "$w/.cache"
if [ ! -L /cache ]; then
  sudo -n rmdir /cache 2>/dev/null
  sudo -n ln -s "$w/.cache" /cache
fi
"#;

/// Starts a Docker daemon for the job and waits until it answers. With a cache volume its images
/// and build cache live there, so the next job's pulls and builds start warm; containers,
/// networks and volumes are cleared at the start of every job.
///
/// `/proc/sys` is read-only inside a container until remounted, and dockerd needs it writable to
/// enable IP forwarding. A daemon that doesn't come up is reported but doesn't fail the job:
/// steps that don't use Docker should still run.
fn sh_docker_up(cache: bool) -> String {
    // The real path, not the `/cache` symlink: runc refuses a rootfs that is reached through one.
    let root = if cache {
        r#""$(dirname "$HR_RUNNER")/_work/.cache/docker""#
    } else {
        "/var/lib/docker"
    };
    format!(
        r#"if command -v dockerd >/dev/null 2>&1; then
  sudo -n mount -o remount,rw /proc/sys 2>/dev/null || true
  sudo -n sh -c 'dockerd --host=unix:///var/run/docker.sock --data-root "$1" >"$2" 2>&1 &' sh {root} "$d/dockerd.log"
  i=0
  until docker info >/dev/null 2>&1; do
    i=$((i+1))
    if [ "$i" -ge 60 ]; then
      echo "home-runner: dockerd did not answer within 60s; its log ends:"
      tail -n 20 "$d/dockerd.log"
      break
    fi
    sleep 1
  done
  # Only images and build cache carry over. Containers, networks and volumes a previous job left
  # (it may have been killed mid-way) would collide with this job's names.
  docker ps -aq | xargs -r docker rm -f >/dev/null 2>&1
  docker network prune -f >/dev/null 2>&1
  docker volume prune -af >/dev/null 2>&1
else
  echo "home-runner: docker_in_job is on but this image has no dockerd, so jobs that use Docker will fail"
fi
"#
    )
}

/// Stops dockerd cleanly so the cache volume isn't left mid-write. Skipped when the supervisor
/// kills the container (timeout, shutdown); dockerd recovers on the next start.
const SH_DOCKER_DOWN: &str = r#"if [ -f /var/run/docker.pid ]; then
  dp=$(cat /var/run/docker.pid)
  sudo -n kill "$dp" 2>/dev/null
  i=0
  while [ -d "/proc/$dp" ] && [ "$i" -lt 30 ]; do i=$((i+1)); sleep 1; done
fi
"#;

/// Installs the job hooks, runs the runner (JIT config in $1), then prints what the hooks recorded
/// so it survives the container being removed.
fn wrapper_script(cache: bool, docker: bool) -> String {
    format!(
        "d=/tmp/home-runner\n{}{}{}\"$HR_RUNNER\" --jitconfig \"$1\"\nrc=$?\n{}{}\nexit $rc\n",
        if cache { SH_CACHE_PREP } else { "" },
        hooks::sh_install_fragment(),
        if docker {
            sh_docker_up(cache)
        } else {
            String::new()
        },
        if docker { SH_DOCKER_DOWN } else { "" },
        hooks::SH_EMIT
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(kind: OciKind, extra: &str) -> OciBackend {
        let cfg: OciConfig = toml::from_str(&format!("image = \"img\"\n{extra}")).unwrap();
        OciBackend::new(kind, cfg)
    }

    fn pairs(args: &[String], flag: &str) -> Vec<String> {
        args.windows(2)
            .filter(|w| w[0] == flag)
            .map(|w| w[1].clone())
            .collect()
    }

    #[test]
    fn docker_in_job_adds_capabilities_to_apple_container_vms() {
        let b = backend(
            OciKind::AppleContainer,
            "docker_in_job = true\ncache_size = \"40G\"",
        );
        let args = b.run_args("hr-x-p-0-abc", Some("hr-x-p-0-cache"), "JIT", &[]);
        let caps = pairs(&args, "--cap-add");
        assert_eq!(caps.len(), DOCKER_CAPS.len());
        assert!(caps.contains(&"CAP_SYS_ADMIN".to_string()));
        assert!(caps.contains(&"CAP_NET_ADMIN".to_string()));
        assert!(!args.contains(&"--privileged".to_string()));
        assert!(pairs(&args, "--env").contains(&"HR_DOCKER=1".to_string()));
        assert_eq!(
            pairs(&args, "--volume"),
            ["hr-x-p-0-cache:/home/runner/_work"]
        );
        assert_eq!(args[args.len() - 1], "JIT");
    }

    #[test]
    fn docker_in_job_is_privileged_on_the_docker_backend() {
        let b = backend(OciKind::Docker, "docker_in_job = true");
        let args = b.run_args("n", None, "JIT", &[]);
        assert!(args.contains(&"--privileged".to_string()));
        assert!(pairs(&args, "--cap-add").is_empty());
        assert!(pairs(&args, "--env").contains(&"HR_DOCKER=1".to_string()));
    }

    #[test]
    fn a_pool_without_docker_in_job_gets_no_extra_privileges() {
        for kind in [OciKind::AppleContainer, OciKind::Docker] {
            let args = backend(kind, "").run_args("n", None, "JIT", &[]);
            assert!(!args.contains(&"--privileged".to_string()));
            assert!(pairs(&args, "--cap-add").is_empty());
            assert!(
                !pairs(&args, "--env")
                    .iter()
                    .any(|e| e.starts_with("HR_DOCKER"))
            );
            let script = args.iter().find(|a| a.contains("HR_RUNNER")).unwrap();
            assert!(!script.contains("dockerd"));
        }
    }

    #[test]
    fn wrapper_starts_dockerd_with_its_data_in_the_cache_volume() {
        let cached = wrapper_script(true, true);
        assert!(cached.contains("--data-root"));
        assert!(
            cached.contains(
                "sh \"$(dirname \"$HR_RUNNER\")/_work/.cache/docker\" \"$d/dockerd.log\""
            )
        );
        assert!(cached.contains("kill \"$dp\""));
        assert!(cached.contains("docker network prune -f"));
        assert!(cached.contains("docker volume prune -af"));
        // The daemon is up before the runner starts and stopped after it exits.
        let up = cached.find("dockerd --host").unwrap();
        let run = cached.find("\"$HR_RUNNER\" --jitconfig").unwrap();
        let down = cached.find("kill \"$dp\"").unwrap();
        assert!(up < run && run < down);
        // Clearing a full cache removes root-owned docker layers first.
        assert!(cached.contains("sudo -n rm -rf \"$w/.cache/docker\""));
        assert!(wrapper_script(false, true).contains("sh /var/lib/docker "));
        assert!(!wrapper_script(true, false).contains("dockerd"));
    }

    /// Every combination of the wrapper must at least be valid shell.
    #[cfg(unix)]
    #[test]
    fn wrapper_scripts_are_valid_shell() {
        for (cache, docker) in [(false, false), (true, false), (false, true), (true, true)] {
            let script = wrapper_script(cache, docker);
            let status = std::process::Command::new("sh")
                .args(["-n", "-c", &script])
                .status()
                .unwrap();
            assert!(status.success(), "cache={cache} docker={docker}\n{script}");
        }
    }

    /// Set HOME_RUNNER_DUMP to a directory to write the container wrapper there for manual testing.
    #[test]
    fn dump_wrapper() {
        if let Ok(dir) = std::env::var("HOME_RUNNER_DUMP") {
            for (name, cache, docker) in [
                ("wrapper.sh", true, false),
                ("wrapper-docker.sh", true, true),
            ] {
                std::fs::write(
                    std::path::Path::new(&dir).join(name),
                    super::wrapper_script(cache, docker),
                )
                .unwrap();
            }
        }
    }
}
