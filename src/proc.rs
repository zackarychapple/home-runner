use std::collections::HashMap;
use std::io;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Result, anyhow, bail};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tracing::info;

use crate::hooks::MARKER;

/// Runs a short-lived command and returns trimmed stdout, or an error that includes stderr.
pub async fn output<S: AsRef<str>>(program: &str, args: &[S]) -> Result<String> {
    let out = Command::new(program)
        .args(args.iter().map(AsRef::as_ref))
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| spawn_error(program, e))?;
    if !out.status.success() {
        let args: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
        bail!(
            "`{program} {}` failed ({}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn spawn_error(program: &str, err: io::Error) -> anyhow::Error {
    if err.kind() == io::ErrorKind::NotFound {
        anyhow!("`{program}` was not found on PATH")
    } else {
        anyhow!(err).context(format!("starting {program}"))
    }
}

/// What the supervisor learns about a runner's job from its console output.
#[derive(Debug, Clone, Default)]
pub struct JobInfo {
    /// Display name from "Running job: …".
    pub name: Option<String>,
    pub started: Option<SystemTime>,
    pub finished: Option<SystemTime>,
    /// From "Job … completed with result: …".
    pub result: Option<String>,
    /// `key=value` pairs the job hooks emitted (see `hooks::MARKER`).
    pub meta: HashMap<String, String>,
}

/// Shared view of a runner's progress, fed by [`pipe_output`].
#[derive(Clone)]
pub struct JobWatch {
    info: Arc<Mutex<JobInfo>>,
    started: watch::Receiver<bool>,
    open_streams: watch::Receiver<usize>,
}

impl JobWatch {
    pub fn has_started(&self) -> bool {
        *self.started.borrow()
    }

    pub async fn wait_started(&mut self) {
        let _ = self.started.wait_for(|s| *s).await;
    }

    /// Lets the forwarders finish reading output the process wrote just before exiting.
    pub async fn drained(&mut self, timeout: Duration) {
        let _ = tokio::time::timeout(timeout, self.open_streams.wait_for(|n| *n == 0)).await;
    }

    pub fn snapshot(&self) -> JobInfo {
        self.info.lock().unwrap().clone()
    }
}

struct Forwarder {
    source: String,
    info: Arc<Mutex<JobInfo>>,
    started: watch::Sender<bool>,
    open_streams: watch::Sender<usize>,
}

impl Forwarder {
    fn observe(&self, line: &str) -> bool {
        if let Some(i) = line.find(MARKER) {
            if let Some((k, v)) = line[i + MARKER.len()..].split_once('=') {
                let mut info = self.info.lock().unwrap();
                info.meta.insert(k.trim().to_string(), v.trim().to_string());
            }
            return false;
        }
        if let Some(i) = line.find("Running job: ") {
            let mut info = self.info.lock().unwrap();
            info.name = Some(line[i + "Running job: ".len()..].trim().to_string());
            info.started = Some(SystemTime::now());
            drop(info);
            self.started.send_replace(true);
        } else if let Some(i) = line.find(" completed with result: ") {
            let mut info = self.info.lock().unwrap();
            info.result = Some(
                line[i + " completed with result: ".len()..]
                    .trim()
                    .to_string(),
            );
            info.finished = Some(SystemTime::now());
        }
        true
    }
}

/// Forwards a child's stdout/stderr into our log, tagged with `source`, and tracks job progress.
pub fn pipe_output(child: &mut Child, source: &str) -> JobWatch {
    let (started_tx, started) = watch::channel(false);
    let (open_tx, open_streams) = watch::channel(0usize);
    let info = Arc::new(Mutex::new(JobInfo::default()));
    let fwd = Arc::new(Forwarder {
        source: source.to_string(),
        info: info.clone(),
        started: started_tx,
        open_streams: open_tx,
    });
    if let Some(out) = child.stdout.take() {
        fwd.open_streams.send_modify(|n| *n += 1);
        tokio::spawn(forward(out, fwd.clone()));
    }
    if let Some(err) = child.stderr.take() {
        fwd.open_streams.send_modify(|n| *n += 1);
        tokio::spawn(forward(err, fwd.clone()));
    }
    JobWatch {
        info,
        started,
        open_streams,
    }
}

async fn forward<R: AsyncRead + Unpin>(reader: R, fwd: Arc<Forwarder>) {
    // Read raw bytes: Windows consoles don't always emit UTF-8, and a decode error must never stop
    // us draining the pipe (a full pipe would hang the runner).
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf);
                let line = line.trim_end();
                if line.is_empty() {
                    continue;
                }
                if fwd.observe(line) {
                    info!(target: "runner", source = %fwd.source, "{line}");
                }
            }
        }
    }
    fwd.open_streams.send_modify(|n| *n = n.saturating_sub(1));
}

/// Kills a child and everything it spawned (the runner forks Runner.Listener and Runner.Worker).
pub async fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // Host-backend children are started in their own process group, so -pid targets the group.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    let _ = child.kill().await;
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tracks_job_progress_from_runner_output() {
        let script = "echo '2026-10-29 10:00:00Z: Listening for Jobs'; \
                      echo '2026-10-29 10:00:01Z: Running job: build (ubuntu, 22)'; \
                      echo '2026-10-29 10:00:09Z: Job build (ubuntu, 22) completed with result: Failed'; \
                      printf '\\377 not utf-8\\n' >&2; \
                      echo '::home-runner:: repo=acme/app'; echo '::home-runner:: workflow=CI = main'";
        let mut child = Command::new("sh")
            .args(["-c", script])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut watch = pipe_output(&mut child, "test");
        child.wait().await.unwrap();
        watch.drained(Duration::from_secs(5)).await;
        let info = watch.snapshot();
        assert!(watch.has_started());
        assert_eq!(info.name.as_deref(), Some("build (ubuntu, 22)"));
        assert_eq!(info.result.as_deref(), Some("Failed"));
        assert!(info.started.is_some() && info.finished.is_some());
        assert_eq!(info.meta["repo"], "acme/app");
        assert_eq!(info.meta["workflow"], "CI = main");
    }
}
