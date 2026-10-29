use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::{Method, Response, StatusCode};
use rsa::RsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::signature::{SignatureEncoding, Signer};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::sync::Mutex;

use crate::config::{AuthConfig, GithubConfig, Scope};
use crate::paths::expand_home;

const USER_AGENT: &str = concat!("home-runner/", env!("CARGO_PKG_VERSION"));

pub struct GitHub {
    http: reqwest::Client,
    api: String,
    scope: Scope,
    runner_group_id: u64,
    auth: Auth,
}

enum Auth {
    Token(String),
    App(Box<AppAuth>),
}

struct AppAuth {
    app_id: u64,
    key: RsaPrivateKey,
    installation_id: Mutex<Option<u64>>,
    token: Mutex<Option<(String, Instant)>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Runner {
    pub id: u64,
    pub name: String,
    pub status: String,
    pub busy: bool,
}

pub struct JitConfig {
    pub runner_id: u64,
    pub encoded: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub body: String,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // GitHub errors are JSON with a "message"; fall back to the raw body for proxies and outages.
        let message = serde_json::from_str::<Value>(&self.body)
            .ok()
            .and_then(|v| v.get("message").and_then(Value::as_str).map(String::from))
            .unwrap_or_else(|| self.body.chars().take(300).collect());
        write!(f, "GitHub API returned {}: {}", self.status, message.trim())
    }
}

impl std::error::Error for ApiError {}

pub fn api_status(err: &anyhow::Error) -> Option<StatusCode> {
    err.downcast_ref::<ApiError>().map(|e| e.status)
}

impl GitHub {
    pub fn from_config(cfg: &GithubConfig) -> Result<Self> {
        let auth = match &cfg.auth {
            AuthConfig::Token { file, env } => {
                Auth::Token(read_token(file.as_deref(), env.as_deref())?)
            }
            AuthConfig::App {
                app_id,
                private_key_path,
                installation_id,
            } => {
                let path = expand_home(private_key_path);
                let pem = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading GitHub App key {}", path.display()))?;
                Auth::App(Box::new(AppAuth {
                    app_id: *app_id,
                    key: parse_key(&pem)?,
                    installation_id: Mutex::new(*installation_id),
                    token: Mutex::new(None),
                }))
            }
        };
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            api: cfg.api_url.trim_end_matches('/').to_string(),
            scope: cfg.scope()?,
            runner_group_id: cfg.runner_group_id.unwrap_or(1),
            auth,
        })
    }

    pub fn scope_label(&self) -> String {
        match &self.scope {
            Scope::Org(org) => format!("org {org}"),
            Scope::Repo(repo) => format!("repo {repo}"),
        }
    }

    fn scope_url(&self) -> String {
        match &self.scope {
            Scope::Org(org) => format!("{}/orgs/{org}", self.api),
            Scope::Repo(repo) => format!("{}/repos/{repo}", self.api),
        }
    }

    /// Registers a single-use runner. The returned config is all the runner needs; it deregisters
    /// itself after one job.
    pub async fn generate_jitconfig(&self, name: &str, labels: &[String]) -> Result<JitConfig> {
        #[derive(Deserialize)]
        struct RunnerId {
            id: u64,
        }
        #[derive(Deserialize)]
        struct Resp {
            runner: RunnerId,
            encoded_jit_config: String,
        }
        let url = format!("{}/actions/runners/generate-jitconfig", self.scope_url());
        let body = json!({
            "name": name,
            "runner_group_id": self.runner_group_id,
            "labels": labels,
            "work_folder": "_work",
        });
        let resp: Resp = self
            .api(Method::POST, &url, Some(body))
            .await?
            .json()
            .await?;
        Ok(JitConfig {
            runner_id: resp.runner.id,
            encoded: resp.encoded_jit_config,
        })
    }

    pub async fn list_runners(&self) -> Result<Vec<Runner>> {
        #[derive(Deserialize)]
        struct Page {
            runners: Vec<Runner>,
        }
        let mut all = Vec::new();
        for page in 1.. {
            let url = format!(
                "{}/actions/runners?per_page=100&page={page}",
                self.scope_url()
            );
            let resp: Page = self.api(Method::GET, &url, None).await?.json().await?;
            let n = resp.runners.len();
            all.extend(resp.runners);
            if n < 100 {
                break;
            }
        }
        Ok(all)
    }

    /// Already-gone runners count as deleted.
    pub async fn delete_runner(&self, id: u64) -> Result<()> {
        let url = format!("{}/actions/runners/{id}", self.scope_url());
        match self.api(Method::DELETE, &url, None).await {
            Ok(_) => Ok(()),
            Err(e) if api_status(&e) == Some(StatusCode::NOT_FOUND) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn latest_runner_release(&self) -> Result<Release> {
        // Unauthenticated on purpose: an installation token can't necessarily read other orgs' repos,
        // and this is only called every few hours.
        let url = format!("{}/repos/actions/runner/releases/latest", self.api);
        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }

    async fn api(&self, method: Method, url: &str, body: Option<Value>) -> Result<Response> {
        let token = self.token().await?;
        let result = self.send(method, url, &token, body).await;
        if let Err(e) = &result
            && api_status(e) == Some(StatusCode::UNAUTHORIZED)
            && let Auth::App(app) = &self.auth
        {
            *app.token.lock().await = None;
        }
        result
    }

    async fn send(
        &self,
        method: Method,
        url: &str,
        bearer: &str,
        body: Option<Value>,
    ) -> Result<Response> {
        let mut req = self
            .http
            .request(method, url)
            .bearer_auth(bearer)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req.send().await?;
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(ApiError { status, body }.into())
    }

    async fn token(&self) -> Result<String> {
        match &self.auth {
            Auth::Token(token) => Ok(token.clone()),
            Auth::App(app) => self.installation_token(app).await,
        }
    }

    async fn installation_token(&self, app: &AppAuth) -> Result<String> {
        let mut cached = app.token.lock().await;
        if let Some((token, expires)) = cached.as_ref()
            && Instant::now() + Duration::from_secs(300) < *expires
        {
            return Ok(token.clone());
        }

        let jwt = app_jwt(app.app_id, &app.key)?;
        let installation_id = {
            let mut id = app.installation_id.lock().await;
            match *id {
                Some(id) => id,
                None => {
                    #[derive(Deserialize)]
                    struct Installation {
                        id: u64,
                    }
                    let url = format!("{}/installation", self.scope_url());
                    let inst: Installation = self
                        .send(Method::GET, &url, &jwt, None)
                        .await
                        .with_context(|| {
                            format!("finding the GitHub App installation for {} (is the app installed there?)", self.scope_label())
                        })?
                        .json()
                        .await?;
                    *id = Some(inst.id);
                    inst.id
                }
            }
        };

        #[derive(Deserialize)]
        struct Token {
            token: String,
        }
        let url = format!(
            "{}/app/installations/{installation_id}/access_tokens",
            self.api
        );
        let token: Token = self
            .send(Method::POST, &url, &jwt, None)
            .await
            .context("creating a GitHub App installation token")?
            .json()
            .await?;
        // Installation tokens last an hour.
        *cached = Some((
            token.token.clone(),
            Instant::now() + Duration::from_secs(55 * 60),
        ));
        Ok(token.token)
    }
}

fn app_jwt(app_id: u64, key: &RsaPrivateKey) -> Result<String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    // Backdate iat to tolerate clock drift; GitHub caps exp at 10 minutes.
    let claims = json!({ "iat": now - 60, "exp": now + 540, "iss": app_id.to_string() });
    let claims = URL_SAFE_NO_PAD.encode(claims.to_string());
    let signing_input = format!("{header}.{claims}");
    let signature = SigningKey::<Sha256>::new(key.clone()).sign(signing_input.as_bytes());
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}

fn parse_key(pem: &str) -> Result<RsaPrivateKey> {
    use rsa::pkcs1::DecodeRsaPrivateKey;
    use rsa::pkcs8::DecodePrivateKey;
    RsaPrivateKey::from_pkcs1_pem(pem)
        .or_else(|_| RsaPrivateKey::from_pkcs8_pem(pem))
        .context("GitHub App key is not an RSA private key in PEM format")
}

fn read_token(file: Option<&Path>, env: Option<&str>) -> Result<String> {
    let token = match (file, env) {
        (Some(file), _) => {
            let path = expand_home(file);
            std::fs::read_to_string(&path)
                .with_context(|| format!("reading token file {}", path.display()))?
        }
        (None, Some(var)) => {
            std::env::var(var).with_context(|| format!("environment variable {var} is not set"))?
        }
        (None, None) => std::env::var("HOME_RUNNER_TOKEN")
            .context("auth type \"token\" needs `file` or `env` (or HOME_RUNNER_TOKEN set)")?,
    };
    let token = token.trim().to_string();
    ensure!(!token.is_empty(), "GitHub token is empty");
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Set HOME_RUNNER_TEST_KEY to a PEM file to print a JWT for verification with openssl.
    #[test]
    fn jwt_shape() {
        let Ok(path) = std::env::var("HOME_RUNNER_TEST_KEY") else {
            return;
        };
        let key = parse_key(&std::fs::read_to_string(path).unwrap()).unwrap();
        let jwt = app_jwt(42, &key).unwrap();
        assert_eq!(jwt.split('.').count(), 3);
        std::fs::write(std::env::temp_dir().join("home-runner-test.jwt"), jwt).unwrap();
    }
}
