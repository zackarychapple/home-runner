use std::io;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tracing::info;

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

/// Forwards a child's stdout/stderr into our log, tagged with `source`. The returned receiver
/// flips to true once the runner reports it has picked up a job.
pub fn pipe_output(child: &mut Child, source: &str) -> watch::Receiver<bool> {
    let (tx, rx) = watch::channel(false);
    let tx = Arc::new(tx);
    if let Some(out) = child.stdout.take() {
        tokio::spawn(forward(out, source.to_string(), tx.clone()));
    }
    if let Some(err) = child.stderr.take() {
        tokio::spawn(forward(err, source.to_string(), tx));
    }
    rx
}

async fn forward<R: AsyncRead + Unpin>(
    reader: R,
    source: String,
    job_started: Arc<watch::Sender<bool>>,
) {
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
                if line.contains("Running job:") {
                    job_started.send_replace(true);
                }
                info!(target: "runner", source = %source, "{line}");
            }
        }
    }
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
