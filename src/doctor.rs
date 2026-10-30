use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::Path;

use anyhow::{Result, bail};

use crate::config::{AuthConfig, BackendConfig, Config, PoolConfig};
use crate::daemon;
use crate::github::RunnerGroup;
use crate::paths::expand_home;

pub async fn run(path: &Path, strict: bool) -> Result<()> {
    let config = Config::load(path)?;
    println!("✓ config {}", path.display());
    println!(
        "  host name {:?}, runners will be named {}<pool>-…",
        config.host_name(),
        config.runner_prefix()
    );

    let daemon::Built { gh, pools, .. } = daemon::build(&config)?;
    let mut failures = 0;
    let mut warnings = 0;
    let mut warn = |what: &str, message: String| {
        warnings += 1;
        println!("⚠ {what}: {message}");
    };

    match gh.list_runners().await {
        Ok(runners) => println!(
            "✓ GitHub: can manage runners for {} ({} registered)",
            gh.scope_label(),
            runners.len()
        ),
        Err(e) => {
            failures += 1;
            println!("✗ GitHub: {e:#}");
        }
    }
    for message in audit_secrets(&config.github.auth) {
        warn("credentials", message);
    }

    let mut groups: HashMap<u64, Result<RunnerGroup>> = HashMap::new();
    for pool in &pools {
        match pool.backend.check().await {
            Ok(info) => println!(
                "✓ pool {} ({} x{}): {info}",
                pool.cfg.name,
                pool.cfg.backend.kind(),
                pool.cfg.count
            ),
            Err(e) => {
                failures += 1;
                println!(
                    "✗ pool {} ({}): {e:#}",
                    pool.cfg.name,
                    pool.cfg.backend.kind()
                );
            }
        }
        println!("    runs-on: [{}]", pool.labels.join(", "));
        let compare: Vec<_> = [pool.pricing.github, pool.pricing.depot]
            .into_iter()
            .flatten()
            .map(|s| format!("{} (${}/min)", s.label, s.usd_per_minute))
            .collect();
        println!("    priced against: {}", compare.join(", "));

        let group = match gh.group_id(pool.cfg.runner_group_id) {
            None => None,
            Some(id) => {
                if let Entry::Vacant(slot) = groups.entry(id) {
                    slot.insert(gh.runner_group(id).await);
                }
                match &groups[&id] {
                    Ok(group) => {
                        println!("    runner group: {}", group.describe());
                        Some(group.clone())
                    }
                    Err(e) => {
                        println!(
                            "    note: couldn't read runner group {id} ({e:#}), so its isolation wasn't checked"
                        );
                        None
                    }
                }
            }
        };
        for message in audit_pool(&pool.cfg, group.as_ref()) {
            warn(&format!("pool {}", pool.cfg.name), message);
        }
    }

    if failures > 0 {
        bail!("{failures} check(s) failed");
    }
    if warnings > 0 {
        if strict {
            bail!("{warnings} warning(s), and --strict was given");
        }
        println!(
            "\nworks, with {warnings} warning(s) above — `home-runner install` to run it in the background"
        );
    } else {
        println!("\nall good — `home-runner install` to run it in the background");
    }
    Ok(())
}

/// Ways a pool can let one job affect the next, or let the wrong repo's job in. Warnings, not
/// failures: the config works, it just isn't as isolated as the README recommends.
pub fn audit_pool(pool: &PoolConfig, group: Option<&RunnerGroup>) -> Vec<String> {
    let mut out = Vec::new();

    let carries_over = match &pool.backend {
        BackendConfig::Host(_) => Some(
            "jobs run directly on the machine, and anything one job installs stays for the next",
        ),
        BackendConfig::AppleContainer(c) | BackendConfig::Docker(c) if c.cache_size.is_some() => {
            Some("its /cache volume carries over from one job to the next")
        }
        BackendConfig::Tart(t) if t.cache_size.is_some() => {
            Some("its cache disk carries over from one job to the next")
        }
        _ => None,
    };
    if let (Some(why), Some(group)) = (carries_over, group) {
        if group.allows_public_repositories {
            out.push(format!(
                "runner group {:?} allows public repositories, and {why}: a pull request from a \
                 fork could run code here and leave something behind for later jobs. Turn off \
                 \"Allow public repositories\" on the group",
                group.name
            ));
        }
        if group.visibility != "selected" {
            let who = if group.visibility == "private" {
                "every private repo"
            } else {
                "every repo"
            };
            out.push(format!(
                "runner group {:?}{} is open to {who} in the org, and {why}. Give this pool a \
                 group limited to the repos that should run here (`runner_group_id`)",
                group.name,
                if group.default {
                    " (the Default group)"
                } else {
                    ""
                },
            ));
        }
        let looks_like_release = std::iter::once(&pool.name)
            .chain(&pool.labels)
            .any(|n| n.to_ascii_lowercase().contains("release"));
        if looks_like_release && !group.restricted_to_workflows {
            out.push(format!(
                "this looks like a release pool, but runner group {:?} isn't restricted to \
                 specific workflows, so any workflow in an allowed repo can write to the cache \
                 your release builds read. Set `restricted_to_workflows` on the group to your \
                 release workflow on the default branch",
                group.name
            ));
        }
    }

    match &pool.backend {
        BackendConfig::Docker(c) if c.docker_in_job => out.push(
            "docker_in_job runs each job's container --privileged, which gives it root on the \
             Docker host VM. Only use it for repos you trust"
                .into(),
        ),
        BackendConfig::Tart(t) if t.ssh_password.is_some() => out.push(
            "ssh_password is set. The Cirrus Labs default (admin/admin) is public: bake an SSH key \
             into the image and use `ssh_key` instead"
                .into(),
        ),
        _ => {}
    }
    out
}

/// Credential files other users on the machine can read.
pub fn audit_secrets(auth: &AuthConfig) -> Vec<String> {
    let path = match auth {
        AuthConfig::App {
            private_key_path, ..
        } => Some(expand_home(private_key_path)),
        AuthConfig::Token {
            file: Some(file), ..
        } => Some(expand_home(file)),
        AuthConfig::Token { .. } => None,
    };
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut out = Vec::new();
    #[cfg(unix)]
    if let Some(path) = path
        && let Ok(meta) = std::fs::metadata(&path)
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            out.push(format!(
                "{} is readable by other users (mode {mode:o}); run `chmod 600 {}`",
                path.display(),
                path.display()
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(extra: &str) -> PoolConfig {
        let text = format!("name = \"p\"\n{extra}");
        toml::from_str(&text).unwrap()
    }

    fn group(json: &str) -> RunnerGroup {
        serde_json::from_str(json).unwrap()
    }

    const OPEN: &str = r#"{"id":1,"name":"Default","visibility":"all","default":true,
        "allows_public_repositories":true,"restricted_to_workflows":false}"#;
    const LOCKED: &str = r#"{"id":7,"name":"trusted","visibility":"selected","default":false,
        "allows_public_repositories":false,"restricted_to_workflows":true,
        "selected_workflows":["o/r/.github/workflows/release.yml@refs/heads/main"]}"#;

    #[test]
    fn warns_about_an_open_group_on_a_cached_pool() {
        let p = pool("backend = \"apple-container\"\ncache_size = \"40G\"");
        let warnings = audit_pool(&p, Some(&group(OPEN)));
        assert_eq!(warnings.len(), 2, "{warnings:#?}");
        assert!(warnings[0].contains("allows public repositories"));
        assert!(warnings[0].contains("/cache volume carries over"));
        assert!(warnings[1].contains("(the Default group) is open to every repo"));
    }

    #[test]
    fn warns_about_an_open_group_on_a_host_pool() {
        let p = pool("backend = \"host\"");
        let warnings = audit_pool(&p, Some(&group(OPEN)));
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("run directly on the machine"));
    }

    #[test]
    fn a_locked_group_is_fine_and_so_is_a_pool_without_state() {
        let cached = pool("backend = \"apple-container\"\ncache_size = \"40G\"");
        assert!(audit_pool(&cached, Some(&group(LOCKED))).is_empty());
        // No cache, no shared state: nothing carries over, so the open group doesn't matter here.
        let plain = pool("backend = \"apple-container\"");
        assert!(audit_pool(&plain, Some(&group(OPEN))).is_empty());
        // Repo-scoped runners have no groups to inspect.
        assert!(audit_pool(&cached, None).is_empty());
    }

    #[test]
    fn private_visibility_still_counts_as_broad() {
        let p = pool("backend = \"host\"");
        let g = group(
            r#"{"id":2,"name":"priv","visibility":"private","allows_public_repositories":false}"#,
        );
        let warnings = audit_pool(&p, Some(&g));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("every private repo"));
    }

    #[test]
    fn release_pools_need_workflow_restrictions() {
        let mut p =
            pool("backend = \"tart\"\nimage = \"i\"\nssh_key = \"k\"\ncache_size = \"100G\"");
        p.name = "macos-release".into();
        let g = group(
            r#"{"id":8,"name":"rel","visibility":"selected","allows_public_repositories":false,
                "restricted_to_workflows":false}"#,
        );
        let warnings = audit_pool(&p, Some(&g));
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert!(warnings[0].contains("release pool"));
        let restricted = group(LOCKED);
        assert!(audit_pool(&p, Some(&restricted)).is_empty());
    }

    #[test]
    fn flags_privileged_docker_and_password_ssh() {
        let p = pool("backend = \"docker\"\ndocker_in_job = true");
        assert!(audit_pool(&p, None)[0].contains("--privileged"));
        let t = pool("backend = \"tart\"\nimage = \"i\"\nssh_password = \"admin\"");
        assert!(audit_pool(&t, None)[0].contains("ssh_password"));
        // Apple container VMs are their own VMs; the extra capabilities stay inside.
        let a = pool("backend = \"apple-container\"\ndocker_in_job = true");
        assert!(audit_pool(&a, None).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn flags_credentials_other_users_can_read() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("hr-doctor-key-{}", std::process::id()));
        std::fs::write(&path, "secret").unwrap();
        let auth = AuthConfig::Token {
            file: Some(path.clone()),
            env: None,
        };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let warnings = audit_secrets(&auth);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("mode 644"));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(audit_secrets(&auth).is_empty());
        std::fs::remove_file(&path).unwrap();
        // A missing file isn't this check's problem; loading the config reports it.
        assert!(audit_secrets(&auth).is_empty());
    }
}
