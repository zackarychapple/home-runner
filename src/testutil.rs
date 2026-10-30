//! Test scaffolding: an in-process GitHub API, a fake backend, and a ready-made supervisor context,
//! so the pool loops, reconcile and selftest run against the real `GitHub` client with no network.
// The tests that use the fake runner spawn `sh`, so they only exist on Unix.
#![cfg_attr(not(unix), allow(dead_code))]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::accounting::{Accounting, PoolPricing};
use crate::backend::Backend;
use crate::backend::fake::{FakeBackend, FakeState};
use crate::config::{AuthConfig, GithubConfig, PoolConfig, PricingConfig};
use crate::daemon::{Ctx, Pool, Timing};
use crate::github::GitHub;

#[derive(Debug, Clone)]
pub struct MockRunner {
    pub id: u64,
    pub name: String,
    pub status: String,
    pub busy: bool,
}

#[derive(Default)]
pub struct MockState {
    next_id: u64,
    pub runners: Vec<MockRunner>,
    /// Bodies of every generate-jitconfig request.
    pub jit_requests: Vec<Value>,
    pub deleted: Vec<u64>,
    /// Runner groups by id, as GitHub's JSON.
    pub groups: HashMap<u64, Value>,
    /// The next n generate-jitconfig calls fail with a 500.
    pub fail_jit: u32,
}

pub struct MockGitHub {
    pub url: String,
    pub state: Arc<Mutex<MockState>>,
    token_file: PathBuf,
}

static UNIQUE: AtomicU32 = AtomicU32::new(0);

pub fn temp_dir(what: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hr-test-{what}-{}-{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

impl MockGitHub {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(MockState {
            next_id: 100,
            ..Default::default()
        }));
        let served = state.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream, served.clone()));
            }
        });
        let token_file = temp_dir("token").join("token");
        std::fs::write(&token_file, "ghp_test\n").unwrap();
        Self {
            url,
            state,
            token_file,
        }
    }

    pub fn github_config(&self) -> GithubConfig {
        GithubConfig {
            org: Some("acme".into()),
            repo: None,
            api_url: self.url.clone(),
            runner_group_id: None,
            auth: AuthConfig::Token {
                file: Some(self.token_file.clone()),
                env: None,
            },
        }
    }

    pub fn client(&self) -> Arc<GitHub> {
        Arc::new(GitHub::from_config(&self.github_config()).unwrap())
    }

    pub fn add_runner(&self, name: &str, busy: bool) -> u64 {
        let mut state = self.state.lock().unwrap();
        state.next_id += 1;
        let id = state.next_id;
        state.runners.push(MockRunner {
            id,
            name: name.into(),
            status: "offline".into(),
            busy,
        });
        id
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .runners
            .iter()
            .map(|r| r.name.clone())
            .collect();
        names.sort();
        names
    }

    pub fn jit_requests(&self) -> usize {
        self.state.lock().unwrap().jit_requests.len()
    }
}

async fn serve(mut stream: tokio::net::TcpStream, state: Arc<Mutex<MockState>>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let (head_end, content_length) = loop {
        let n = stream.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).to_ascii_lowercase();
            let len = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            break (i + 4, len);
        }
    };
    while buf.len() < head_end + content_length {
        let n = stream.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let body: Value =
        serde_json::from_slice(&buf[head_end..head_end + content_length]).unwrap_or(Value::Null);
    let mut first = head.lines().next().unwrap_or("").split_whitespace();
    let (method, target) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    let (status, payload) = route(&state, method, path, query, body);
    let payload = payload.to_string();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn route(
    state: &Mutex<MockState>,
    method: &str,
    path: &str,
    query: &str,
    body: Value,
) -> (&'static str, Value) {
    let mut state = state.lock().unwrap();
    let not_found = || ("404 Not Found", json!({ "message": "Not Found" }));
    let Some(rest) = path.strip_prefix("/orgs/acme/actions/") else {
        return not_found();
    };
    match (method, rest) {
        ("POST", "runners/generate-jitconfig") => {
            if state.fail_jit > 0 {
                state.fail_jit -= 1;
                return (
                    "500 Internal Server Error",
                    json!({ "message": "Server Error" }),
                );
            }
            state.jit_requests.push(body.clone());
            state.next_id += 1;
            let id = state.next_id;
            state.runners.push(MockRunner {
                id,
                name: body["name"].as_str().unwrap_or("").into(),
                status: "offline".into(),
                busy: false,
            });
            (
                "201 Created",
                json!({ "runner": { "id": id }, "encoded_jit_config": format!("jit-{id}") }),
            )
        }
        ("GET", "runners") => {
            let param = |key: &str| {
                query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
                    .and_then(|v| v.parse::<usize>().ok())
            };
            let (per_page, page) = (param("per_page").unwrap_or(30), param("page").unwrap_or(1));
            let runners: Vec<Value> = state
                .runners
                .iter()
                .skip((page - 1) * per_page)
                .take(per_page)
                .map(|r| json!({ "id": r.id, "name": r.name, "status": r.status, "busy": r.busy }))
                .collect();
            (
                "200 OK",
                json!({ "total_count": state.runners.len(), "runners": runners }),
            )
        }
        ("DELETE", rest) if rest.starts_with("runners/") => {
            let id: u64 = rest["runners/".len()..].parse().unwrap_or(0);
            match state.runners.iter().position(|r| r.id == id) {
                Some(i) => {
                    state.runners.remove(i);
                    state.deleted.push(id);
                    ("204 No Content", Value::Null)
                }
                None => not_found(),
            }
        }
        ("GET", rest) if rest.starts_with("runner-groups/") => {
            let id: u64 = rest["runner-groups/".len()..].parse().unwrap_or(0);
            match state.groups.get(&id) {
                Some(group) => ("200 OK", group.clone()),
                None => not_found(),
            }
        }
        _ => not_found(),
    }
}

/// A supervisor context wired to a mock GitHub and a fake backend. The pool is named `t`, so
/// runners are `hr-test-t-<slot>-<id>`.
pub struct Harness {
    pub mock: MockGitHub,
    pub ctx: Arc<Ctx>,
    pub pool: Arc<Pool>,
    pub fake: Arc<Mutex<FakeState>>,
    pub ledger: PathBuf,
}

impl Harness {
    /// `script` is what the fake runner executes (`sh -c`).
    pub async fn new(script: &str) -> Self {
        let mock = MockGitHub::start().await;
        let gh = mock.client();
        let ledger = temp_dir("ledger").join("jobs.jsonl");
        let accounting = Arc::new(Accounting::new(
            gh.clone(),
            PricingConfig::default(),
            "test".into(),
            ledger.clone(),
        ));
        let mut ctx = Ctx::new(gh, accounting.clone(), "hr-test-".into());
        ctx.timing = Timing {
            min_backoff: Duration::from_millis(20),
            max_backoff: Duration::from_millis(80),
            quick_exit: Duration::from_secs(120),
        };
        let cfg: PoolConfig =
            toml::from_str("name = \"t\"\nbackend = \"host\"\ncount = 1\n").unwrap();
        let fake = Arc::new(Mutex::new(FakeState {
            script: script.into(),
            ..Default::default()
        }));
        let backend = Backend::Fake(FakeBackend::new(fake.clone()));
        let pricing = PoolPricing::new(&cfg, "Linux", "ARM64", 2);
        let pool = Arc::new(Pool {
            prefix: "hr-test-t-".into(),
            labels: vec!["self-hosted".into(), "Linux".into(), "ARM64".into()],
            hook_env: accounting.hook_env(&pricing),
            pricing,
            cfg,
            backend,
            ready: Default::default(),
        });
        Self {
            mock,
            ctx: Arc::new(ctx),
            pool,
            fake,
            ledger,
        }
    }

    /// How long a runner must stay up before the slot counts as recovered. Call before starting
    /// any slot.
    pub fn set_quick_exit(&mut self, quick_exit: Duration) {
        let ctx = Arc::get_mut(&mut self.ctx).expect("set timing before sharing the context");
        ctx.timing.quick_exit = quick_exit;
    }

    pub fn resources(&self) -> Vec<String> {
        let mut r = self.fake.lock().unwrap().resources.clone();
        r.sort();
        r
    }
}

/// Polls until `check` holds, panicking with `what` after a few seconds.
pub async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..400 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for: {what}");
}
