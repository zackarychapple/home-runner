//! Verifying and installing a pool's prerequisites against its backend, and the `home-runner
//! prereq` commands.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};

use super::install::{self, LABEL_BASE_ID, LABEL_FINGERPRINT};
use super::probe::{self, Check, EvalCtx, Status};
use super::{InstallMode, Provision};
use crate::backend::Backend;
use crate::config::Config;
use crate::daemon::{self, Built, Pool};

/// The outcome of one verification.
#[derive(Debug, Clone)]
pub struct Report {
    pub pool: String,
    pub profile: String,
    pub checks: Vec<Check>,
}

impl Report {
    fn single(pool: &str, profile: &str, check: Check) -> Self {
        Self {
            pool: pool.into(),
            profile: profile.into(),
            checks: vec![check],
        }
    }

    pub fn ready(&self) -> bool {
        self.checks.iter().all(|c| c.status.is_ok())
    }

    fn failing(&self) -> impl Iterator<Item = &Check> {
        self.checks.iter().filter(|c| !c.status.is_ok())
    }

    /// Why the pool isn't taking jobs. The first line fits in `status`.
    pub fn failure(&self) -> String {
        let failing: Vec<_> = self.failing().collect();
        let names: Vec<String> = failing
            .iter()
            .take(4)
            .map(|c| format!("{} {}", c.id, word(c.status)))
            .collect();
        let more = failing.len().saturating_sub(names.len());
        let mut out = format!(
            "pool {} is not ready: profile {} has {} unmet requirement(s) ({}{}); \
             see `home-runner doctor` or `home-runner prereq inspect`",
            self.pool,
            self.profile,
            failing.len(),
            names.join(", "),
            if more > 0 {
                format!(", and {more} more")
            } else {
                String::new()
            }
        );
        for check in failing {
            out.push_str(&format!("\n{}", check_lines(check).join("\n")));
        }
        out
    }

    pub fn lines(&self) -> Vec<String> {
        self.checks.iter().flat_map(check_lines).collect()
    }
}

fn word(status: Status) -> &'static str {
    match status {
        Status::Ok => "ok",
        Status::Missing => "missing",
        Status::Wrong => "wrong version",
        Status::Failed => "failed",
        Status::Skipped => "not run",
    }
}

fn check_lines(check: &Check) -> Vec<String> {
    let mut lines = Vec::new();
    match check.status {
        Status::Ok => lines.push(format!("    ✓ {}: {}", check.id, check.detail)),
        Status::Skipped => lines.push(format!("    – {}: {}", check.id, check.detail)),
        status => {
            lines.push(format!(
                "    ✗ {} ({}): {}",
                check.id,
                word(status),
                check.detail
            ));
            if let Some(hint) = &check.hint {
                lines.push(format!("        fix: {hint}"));
            }
            lines.extend(check.diag.iter().map(|d| format!("        | {d}")));
        }
    }
    lines
}

/// Starts a throwaway worker, runs the profile's probe in it as the runner user, and judges what
/// it reports. `probe_name` names the worker, so the caller can always `cleanup` it.
pub async fn verify(
    backend: &Backend,
    base: Option<&Backend>,
    prov: &Provision,
    pool: &str,
    probe_name: &str,
) -> Report {
    let profile = &prov.profile;
    let fail = |id: &str, detail: String, hint: String| {
        Report::single(
            pool,
            &profile.name,
            Check {
                id: id.into(),
                status: Status::Failed,
                detail,
                hint: Some(hint),
                diag: Vec::new(),
            },
        )
    };
    if let Err(problem) = check_image(backend, base, prov, pool).await {
        return fail(
            "image",
            problem,
            format!("run `home-runner prereq install --pool {pool}`"),
        );
    }
    let script = probe::script(profile, &prov.platform_key);
    let timeout = Duration::from_secs(profile.probe_timeout_secs);
    let out = match backend
        .probe(probe_name, &script, prov.start_docker, timeout)
        .await
    {
        Ok(out) => out,
        Err(e) => {
            return fail(
                "probe",
                format!("could not run the probe in a worker: {e:#}"),
                "run `home-runner doctor` to check the pool's backend and image".into(),
            );
        }
    };
    let ctx = EvalCtx {
        pool,
        mode: prov.mode,
        image_arch: prov.image_arch,
        platform_key: &prov.platform_key,
    };
    Report {
        pool: pool.into(),
        profile: profile.name.clone(),
        checks: probe::evaluate(profile, &ctx, &out),
    }
}

/// In `image` mode, jobs run in a derived image that has to exist and still match both the profile
/// and the base image it was built from (a rebuilt base image invalidates it).
async fn check_image(
    backend: &Backend,
    base: Option<&Backend>,
    prov: &Provision,
    pool: &str,
) -> std::result::Result<(), String> {
    let Some(tag) = &prov.derived_image else {
        return Ok(());
    };
    let rebuild = format!("run `home-runner prereq install --pool {pool}`");
    let info = backend
        .image_info(tag)
        .await
        .map_err(|e| format!("{e:#}"))?
        .ok_or_else(|| format!("the prerequisite image {tag} has not been built; {rebuild}"))?;
    if info.labels.get(LABEL_FINGERPRINT) != Some(&prov.fingerprint) {
        return Err(format!(
            "the prerequisite image {tag} was built for a different profile; {rebuild}"
        ));
    }
    if let (Some(base), Some(base_image)) = (base, &prov.base_image)
        && let Some(current) = base
            .image_info(base_image)
            .await
            .map_err(|e| format!("{e:#}"))?
        && info
            .labels
            .get(LABEL_BASE_ID)
            .is_some_and(|built| *built != current.id)
    {
        return Err(format!(
            "the base image {base_image} was rebuilt after {tag} was made from it; {rebuild}"
        ));
    }
    Ok(())
}

/// What a verified pool was verified against: the profile and the images it runs in. A change to
/// either means it has to be verified again.
pub async fn readiness_key(backend: &Backend, prov: &Provision) -> String {
    let id = match &prov.derived_image {
        Some(tag) => backend.image_info(tag).await.ok().flatten().map(|i| i.id),
        None => None,
    };
    format!("{}:{}", prov.fingerprint, id.unwrap_or_default())
}

/// Builds the derived image for a pool in `image` mode. Does nothing if it is already up to date
/// (same profile, same base image), unless `force`.
pub async fn install(pool: &Pool, prov: &Provision, force: bool) -> Result<String> {
    let (Some(base), Some(base_image), Some(tag)) =
        (&pool.base_backend, &prov.base_image, &prov.derived_image)
    else {
        bail!(
            "pool {}: the app doesn't install for {} pools. Put the requirements on the image or \
             machine yourself; `home-runner prereq inspect --pool {}` lists what is missing",
            pool.cfg.name,
            pool.cfg.backend.kind(),
            pool.cfg.name
        );
    };
    base.prepare().await?;
    let Some(base_info) = base.image_info(base_image).await? else {
        bail!("the base image {base_image} isn't present locally; pull or build it first");
    };
    if !force
        && let Some(existing) = pool.backend.image_info(tag).await?
        && existing.labels.get(LABEL_FINGERPRINT) == Some(&prov.fingerprint)
        && existing.labels.get(LABEL_BASE_ID) == Some(&base_info.id)
    {
        return Ok(format!("{tag} is already up to date"));
    }
    let dockerfile = install::dockerfile(
        base_image,
        base_info.user.as_deref(),
        Some(&base_info.id),
        &prov.profile,
        &prov.fingerprint,
    );
    let dir = prov.work_dir.join(&pool.cfg.name);
    base.build_image(tag, &dockerfile, &dir, force).await?;
    Ok(format!("built {tag} from {base_image}"))
}

/// A probe name that the supervisor's reconcile loop doesn't own, for commands that may run beside
/// the service. The caller removes it.
pub fn cli_probe_name(config: &Config, pool: &str) -> String {
    format!("hrp-{}-{pool}-{}", config.host_name(), daemon::short_id())
}

/// `doctor`'s view of one pool: verify it and print each requirement. Returns whether it's ready.
pub async fn doctor(config: &Config, pool: &Pool) -> Option<bool> {
    let prov = pool.prereq.as_ref()?;
    let name = cli_probe_name(config, &pool.cfg.name);
    let report = verify(
        &pool.backend,
        pool.base_backend.as_ref(),
        prov,
        &pool.cfg.name,
        &name,
    )
    .await;
    let _ = pool.backend.cleanup(&name).await;
    println!(
        "    prerequisites (profile {} v{}, {}): {}",
        prov.profile.name,
        prov.profile.version,
        match prov.mode {
            InstallMode::Image => "derived image",
            InstallMode::Preinstalled => "preinstalled",
        },
        if report.ready() { "ready" } else { "NOT READY" }
    );
    for line in report.lines() {
        println!("{line}");
    }
    Some(report.ready())
}

fn select(built: &Built, only: Option<&str>) -> Result<Vec<Arc<Pool>>> {
    let pools: Vec<_> = built
        .pools
        .iter()
        .filter(|p| only.is_none_or(|n| p.cfg.name == n))
        .filter(|p| p.prereq.is_some())
        .cloned()
        .collect();
    if pools.is_empty() {
        match only {
            Some(name) => bail!("pool {name:?} doesn't exist or doesn't set `profile`"),
            None => bail!("no pool sets `profile`; see [[profile]] in the README"),
        }
    }
    Ok(pools)
}

/// `home-runner prereq inspect`: probe each pool with a profile and say what's missing.
pub async fn inspect(path: &Path, only: Option<&str>) -> Result<()> {
    let config = Config::load(path)?;
    let built = daemon::build(&config)?;
    let mut unready = 0;
    for pool in select(&built, only)? {
        println!("pool {} ({})", pool.cfg.name, pool.cfg.backend.kind());
        if let Some(false) = doctor(&config, &pool).await {
            unready += 1;
        }
    }
    if unready > 0 {
        bail!("{unready} pool(s) are not ready");
    }
    Ok(())
}

/// `home-runner prereq install`: build what's missing, then verify. Safe to repeat.
pub async fn install_cmd(
    path: &Path,
    only: Option<&str>,
    dry_run: bool,
    force: bool,
) -> Result<()> {
    let config = Config::load(path)?;
    let built = daemon::build(&config)?;
    let mut unready = 0;
    for pool in select(&built, only)? {
        let prov = pool.prereq.as_ref().expect("selected pools have a profile");
        println!("pool {} ({})", pool.cfg.name, pool.cfg.backend.kind());
        if prov.can_install() {
            if dry_run {
                let text = install::dockerfile(
                    prov.base_image.as_deref().unwrap_or_default(),
                    None,
                    None,
                    &prov.profile,
                    &prov.fingerprint,
                );
                println!(
                    "{text}\n# the script that RUN executes, as root:\n{}",
                    install::script(&prov.profile)
                );
                continue;
            }
            match install(&pool, prov, force).await {
                Ok(message) => println!("    ✓ {message}"),
                Err(e) => {
                    println!("    ✗ install failed: {e:#}");
                    unready += 1;
                    continue;
                }
            }
        } else {
            println!(
                "    nothing for the app to install on a {} pool: put the requirements on its image \
                 or machine. Verifying what is there:",
                pool.cfg.backend.kind()
            );
        }
        if dry_run {
            continue;
        }
        if let Some(false) = doctor(&config, &pool).await {
            unready += 1;
        }
    }
    if unready > 0 {
        bail!("{unready} pool(s) are not ready");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{WITH_TAP, profile};
    use super::*;

    #[test]
    fn a_failure_names_the_pool_the_profile_and_each_unmet_requirement() {
        let report = Report {
            pool: "linux".into(),
            profile: "with-tap".into(),
            checks: vec![
                Check {
                    id: "command:node".into(),
                    status: Status::Ok,
                    detail: "v26.1.0".into(),
                    hint: None,
                    diag: vec![],
                },
                Check {
                    id: "apt:libasound2-dev".into(),
                    status: Status::Missing,
                    detail: "package is not installed".into(),
                    hint: Some("run `home-runner prereq install --pool linux`".into()),
                    diag: vec![],
                },
                Check {
                    id: "docker:run".into(),
                    status: Status::Failed,
                    detail: "boom".into(),
                    hint: None,
                    diag: vec!["container state=created".into()],
                },
                Check {
                    id: "docker:exec".into(),
                    status: Status::Skipped,
                    detail: "not run".into(),
                    hint: None,
                    diag: vec![],
                },
            ],
        };
        assert!(!report.ready());
        let failure = report.failure();
        let first = failure.lines().next().unwrap();
        assert!(
            first.starts_with("pool linux is not ready: profile with-tap has 2 unmet"),
            "{first}"
        );
        assert!(first.contains("apt:libasound2-dev missing"));
        assert!(failure.contains("fix: run `home-runner prereq install --pool linux`"));
        assert!(failure.contains("| container state=created"));
        // A step that didn't run isn't a failure of its own.
        assert!(!first.contains("docker:exec"));
        let _ = profile(WITH_TAP);
    }
}
