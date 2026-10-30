//! A backend for tests: "runners" are `sh -c <script>` processes and resources are names in a list.
#![cfg_attr(not(unix), allow(dead_code))]

use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use tokio::process::Command;

use super::Running;
use crate::proc::{self, pipe_output};

#[derive(Default)]
pub struct FakeState {
    /// What the fake runner executes.
    pub script: String,
    /// Names the backend currently "holds", as `list` reports them.
    pub resources: Vec<String>,
    pub launched: Vec<String>,
    pub cleaned: Vec<String>,
    /// The next n launches fail.
    pub launch_failures: u32,
}

pub struct FakeBackend {
    state: Arc<Mutex<FakeState>>,
}

impl FakeBackend {
    pub fn new(state: Arc<Mutex<FakeState>>) -> Self {
        Self { state }
    }

    pub async fn launch(&self, name: &str, _jit_config: &str) -> Result<Running> {
        let script = {
            let mut state = self.state.lock().unwrap();
            if state.launch_failures > 0 {
                state.launch_failures -= 1;
                bail!("fake launch failure");
            }
            state.resources.push(name.to_string());
            state.launched.push(name.to_string());
            state.script.clone()
        };
        let mut child = Command::new("sh")
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| proc::spawn_error("sh", e))?;
        let watch = pipe_output(&mut child, name);
        Ok(Running::new(child, None, false, watch))
    }

    pub async fn cleanup(&self, name: &str) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        state.resources.retain(|r| r != name);
        state.cleaned.push(name.to_string());
        Ok(())
    }

    pub async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let state = self.state.lock().unwrap();
        Ok(state
            .resources
            .iter()
            .filter(|r| r.starts_with(prefix))
            .cloned()
            .collect())
    }
}
