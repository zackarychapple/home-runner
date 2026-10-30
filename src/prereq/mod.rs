//! Job prerequisites: what a worker must have installed before it may take jobs.
//!
//! An operator declares a versioned *profile* (`[[profile]]` in the config) and attaches it to a
//! pool. Before a runner is registered, the app starts a throwaway worker the way a job would,
//! runs a probe inside it as the runner user, and only lets the pool take jobs when every
//! requirement passes. Nothing is checked on the supervising machine itself.
//!
//! * [`probe`] builds the probe script and turns its output into a [`Report`].
//! * [`install`] builds a derived image that has the missing pieces, for OCI pools.
//! * [`run`] ties the two to a backend, and renders reports for `doctor` and `prereq`.
//!
//! The probe reports facts and the judgment (version matching, error classification) happens in
//! Rust, so every state (ready, missing, wrong version, failed install, ...) is tested with canned
//! probe output instead of a real worker.

pub mod install;
pub mod native;
pub mod probe;
pub mod run;

use std::collections::HashSet;

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{BackendConfig, PoolConfig};

/// A named, versioned list of requirements. Bump `version` to invalidate readiness everywhere the
/// profile is used.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    pub name: String,
    #[serde(default = "one")]
    pub version: u32,
    /// How missing requirements get fixed. Defaults to `image` for container pools (a derived
    /// image is built) and `preinstalled` for everything else (the operator installs them on the
    /// golden image or machine, and the app only verifies).
    #[serde(default)]
    pub install: Option<InstallMode>,
    /// Install or repair on startup when the probe fails, instead of waiting for the operator to
    /// run `home-runner prereq install`. Only the operator's own config is ever run.
    #[serde(default)]
    pub auto_install: bool,
    /// How long the probe may take, including pulling the PostgreSQL fixture image.
    #[serde(default = "default_probe_timeout")]
    pub probe_timeout_secs: u64,
    #[serde(default, rename = "require")]
    pub requirements: Vec<Requirement>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallMode {
    /// The app builds a derived image from the pool's image with the requirements installed.
    Image,
    /// The requirements are already on the image or machine; the app only verifies them.
    Preinstalled,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Requirement {
    /// An executable on `PATH`, optionally pinned to a version.
    Command(CommandReq),
    /// Debian/Ubuntu packages (shared libraries, headers).
    Apt(AptReq),
    /// A usable Docker daemon for service containers: start a PostgreSQL container, run a command
    /// in it, connect to it through its published port, and clean it up.
    Docker(DockerReq),
    /// Pinned Playwright versions, their exact browser revisions and OS libraries.
    Playwright(PlaywrightReq),
    /// The pinned Cargo Tauri CLI, CEF distribution and Apple toolchain on a macOS (Tart) worker.
    TauriCef(native::TauriCefReq),
    /// A logged-in graphical session where real keyboard and pointer input is proven to reach an
    /// app (Tart workers only).
    MacosGui(native::MacosGuiReq),
    /// The runner user can script Finder over Apple events, as Tauri's DMG window layout does
    /// (Tart workers; the Automation permission is granted once in the golden image).
    MacosFinder(native::MacosFinderReq),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandReq {
    pub name: String,
    /// Required version: `26` matches 26.x.y, `1.7.12` matches exactly that. Compared with the
    /// first version number in the first line of the command's output.
    pub version: Option<String>,
    #[serde(default = "default_version_args")]
    pub version_args: Vec<String>,
    /// A shell script that installs it, run as root while building the derived image. It comes
    /// from this config file and nowhere else.
    pub install: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AptReq {
    pub packages: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DockerReq {
    /// The database image started to prove containers work. It needs `pg_isready` and `psql`.
    #[serde(default = "default_pg_image")]
    pub image: String,
    /// How many one-second checks to allow before PostgreSQL counts as not ready. A count, so
    /// it behaves the same on a slow machine.
    #[serde(default = "default_ready_attempts")]
    pub ready_attempts: u32,
    /// Where job processes reach ports the daemon publishes. Defaults to the host in a `tcp://`
    /// `DOCKER_HOST`, else 127.0.0.1. Set it when the daemon is remote or nested.
    pub endpoint_host: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlaywrightReq {
    /// Exact `playwright-core` versions, one per workspace that pins its own. Their browser
    /// revisions coexist in one cache.
    pub versions: Vec<String>,
    #[serde(default = "default_browsers")]
    pub browsers: Vec<String>,
    /// Root-owned, read-only location of the runtimes and browsers. Jobs get
    /// `PLAYWRIGHT_BROWSERS_PATH=<root>/<os>-<arch>/browsers`.
    #[serde(default = "default_playwright_root")]
    pub root: String,
}

fn one() -> u32 {
    1
}
fn default_probe_timeout() -> u64 {
    900
}
fn default_version_args() -> Vec<String> {
    vec!["--version".into()]
}
fn default_pg_image() -> String {
    "postgres:17-alpine".into()
}
fn default_ready_attempts() -> u32 {
    60
}
fn default_browsers() -> Vec<String> {
    vec!["chromium".into()]
}
fn default_playwright_root() -> String {
    "/opt/home-runner/playwright".into()
}

impl Requirement {
    /// Ids of the probe results this requirement expects, in the order the probe reports them.
    pub fn check_ids(&self) -> Vec<String> {
        match self {
            Requirement::Command(c) => vec![format!("command:{}", c.name)],
            Requirement::Apt(a) => a.packages.iter().map(|p| format!("apt:{p}")).collect(),
            Requirement::Docker(_) => [
                "cli", "daemon", "image", "run", "exec", "ready", "query", "port", "cleanup",
            ]
            .map(|s| format!("docker:{s}"))
            .to_vec(),
            Requirement::Playwright(p) => {
                let mut ids = vec!["playwright:platform".to_string()];
                for v in &p.versions {
                    for b in &p.browsers {
                        ids.push(format!("playwright:{v}:{b}"));
                    }
                }
                ids
            }
            Requirement::TauriCef(_) => native::tauri_cef_ids(),
            Requirement::MacosGui(_) => native::gui_ids(),
            Requirement::MacosFinder(_) => native::finder_ids(),
        }
    }
}

impl ProfileConfig {
    pub fn validate(&self) -> Result<()> {
        let name = &self.name;
        ensure!(
            !name.is_empty()
                && name.len() <= 20
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "profile name {name:?} must be 1-20 lowercase letters, digits or dashes"
        );
        ensure!(
            !self.requirements.is_empty(),
            "profile {name:?} has no [[profile.require]] entries"
        );
        ensure!(
            self.probe_timeout_secs >= 30,
            "profile {name:?}: probe_timeout_secs must be at least 30"
        );
        let mut ids = HashSet::new();
        let mut playwright = 0;
        for req in &self.requirements {
            match req {
                Requirement::Command(c) => {
                    ensure!(
                        is_word(&c.name, |ch| ch.is_ascii_alphanumeric()
                            || "._+-".contains(ch)),
                        "profile {name:?}: command name {:?} may only contain letters, digits and ._+-",
                        c.name
                    );
                    ensure!(
                        !c.version_args.is_empty() || c.version.is_none(),
                        "profile {name:?}: {} needs version_args to check its version",
                        c.name
                    );
                }
                Requirement::Apt(a) => {
                    ensure!(
                        !a.packages.is_empty(),
                        "profile {name:?}: an apt requirement needs packages"
                    );
                    for p in &a.packages {
                        ensure!(
                            is_word(p, |ch| ch.is_ascii_lowercase()
                                || ch.is_ascii_digit()
                                || "+.-".contains(ch)),
                            "profile {name:?}: {p:?} is not a Debian package name"
                        );
                    }
                }
                Requirement::Docker(d) => {
                    ensure!(
                        is_word(&d.image, |ch| ch.is_ascii_alphanumeric()
                            || "._-/:@".contains(ch)),
                        "profile {name:?}: docker image {:?} is not a valid reference",
                        d.image
                    );
                    ensure!(
                        d.ready_attempts >= 1,
                        "profile {name:?}: ready_attempts must be at least 1"
                    );
                    if let Some(host) = &d.endpoint_host {
                        ensure!(
                            is_word(host, |ch| ch.is_ascii_alphanumeric()
                                || ".-:[]".contains(ch)),
                            "profile {name:?}: endpoint_host {host:?} is not a host name or address"
                        );
                    }
                }
                Requirement::Playwright(p) => {
                    playwright += 1;
                    ensure!(
                        !p.versions.is_empty(),
                        "profile {name:?}: playwright needs at least one version"
                    );
                    for v in &p.versions {
                        ensure!(
                            is_exact_version(v),
                            "profile {name:?}: playwright version {v:?} must be exact, like 1.56.1"
                        );
                    }
                    for b in &p.browsers {
                        ensure!(
                            ["chromium", "firefox", "webkit"].contains(&b.as_str()),
                            "profile {name:?}: unknown browser {b:?} (chromium, firefox or webkit)"
                        );
                    }
                    ensure!(
                        !p.browsers.is_empty(),
                        "profile {name:?}: playwright needs at least one browser"
                    );
                    ensure!(
                        p.root.starts_with('/')
                            && is_word(&p.root, |ch| ch.is_ascii_alphanumeric()
                                || "._-/".contains(ch)),
                        "profile {name:?}: playwright root {:?} must be an absolute path",
                        p.root
                    );
                }
                Requirement::TauriCef(r) => r.validate(name)?,
                Requirement::MacosGui(r) => r.validate(name)?,
                Requirement::MacosFinder(_) => {}
            }
            for id in req.check_ids() {
                ensure!(
                    ids.insert(id.clone()),
                    "profile {name:?} requires {id} more than once"
                );
            }
        }
        ensure!(
            playwright <= 1,
            "profile {name:?}: list every Playwright version in one playwright requirement"
        );
        Ok(())
    }

    pub fn docker(&self) -> Option<&DockerReq> {
        self.requirements.iter().find_map(|r| match r {
            Requirement::Docker(d) => Some(d),
            _ => None,
        })
    }

    pub fn macos_gui(&self) -> Option<&native::MacosGuiReq> {
        self.requirements.iter().find_map(|r| match r {
            Requirement::MacosGui(g) => Some(g),
            _ => None,
        })
    }

    /// Whether any requirement only makes sense on a macOS worker.
    pub fn needs_macos(&self) -> bool {
        self.requirements.iter().any(|r| {
            matches!(
                r,
                Requirement::TauriCef(_) | Requirement::MacosGui(_) | Requirement::MacosFinder(_)
            )
        })
    }

    pub fn playwright(&self) -> Option<&PlaywrightReq> {
        self.requirements.iter().find_map(|r| match r {
            Requirement::Playwright(p) => Some(p),
            _ => None,
        })
    }

    /// Identifies what readiness was verified against: the name, the version and every
    /// requirement. Changing any of them means a worker has to be verified (and rebuilt) again.
    pub fn fingerprint(&self) -> String {
        #[derive(Serialize)]
        struct View<'a> {
            name: &'a str,
            version: u32,
            requirements: &'a [Requirement],
        }
        let json = serde_json::to_string(&View {
            name: &self.name,
            version: self.version,
            requirements: &self.requirements,
        })
        .expect("profiles serialize");
        hex(&Sha256::digest(json.as_bytes()))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn is_word(s: &str, allowed: impl Fn(char) -> bool) -> bool {
    !s.is_empty() && s.chars().all(allowed)
}

fn is_exact_version(v: &str) -> bool {
    let (core, pre) = v.split_once('-').unwrap_or((v, "a"));
    let parts: Vec<_> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        && is_word(pre, |c| c.is_ascii_alphanumeric() || c == '.')
}

/// A pool's profile, resolved against the pool's backend.
#[derive(Debug, Clone)]
pub struct Provision {
    pub profile: ProfileConfig,
    pub mode: InstallMode,
    /// The pool's own image (container pools in `image` mode).
    pub base_image: Option<String>,
    /// The image jobs run in once it has the requirements.
    pub derived_image: Option<String>,
    /// `linux-arm64`, `macos-arm64`, ...: the key the browser cache is stored under.
    pub platform_key: String,
    /// `arm64` or `amd64`, the architecture container images must have.
    pub image_arch: &'static str,
    pub fingerprint: String,
    /// Whether the probe starts the pool's in-job Docker daemon first, as a job would.
    pub start_docker: bool,
    /// Where `install` writes its build files.
    pub work_dir: std::path::PathBuf,
}

impl Provision {
    /// `os` and `arch` are the pool's runner labels (`Linux`/`macOS`/`Windows`, `ARM64`/`X64`).
    pub fn new(
        profile: &ProfileConfig,
        pool: &PoolConfig,
        os: &str,
        arch: &str,
        work_dir: std::path::PathBuf,
    ) -> Result<Self> {
        let oci = match &pool.backend {
            BackendConfig::AppleContainer(c) | BackendConfig::Docker(c) => Some(c),
            _ => None,
        };
        let oci_image = oci.map(|c| c.image.clone());
        if profile.docker().is_some()
            && let Some(c) = oci
            && !c.docker_in_job
        {
            bail!(
                "pool {:?}: profile {:?} needs a Docker daemon, so set docker_in_job = true on the \
                 pool and use an image with dockerd",
                pool.name,
                profile.name
            );
        }
        if profile.needs_macos() {
            ensure!(
                matches!(
                    pool.backend,
                    BackendConfig::Tart(_) | BackendConfig::Host(_)
                ),
                "pool {:?}: profile {:?} has macOS native requirements (tauri-cef, macos-gui, macos-finder), so \
                 it needs a `tart` pool",
                pool.name,
                profile.name
            );
        }
        if profile.macos_gui().is_some() {
            ensure!(
                matches!(pool.backend, BackendConfig::Tart(_)),
                "pool {:?}: profile {:?} sends real keyboard and pointer events, which must only \
                 ever go to a throwaway Tart VM, never the desktop of a `host` pool",
                pool.name,
                profile.name
            );
        }
        let start_docker = profile.docker().is_some() && oci.is_some();
        let mode = profile.install.unwrap_or(if oci_image.is_some() {
            InstallMode::Image
        } else {
            InstallMode::Preinstalled
        });
        if mode == InstallMode::Image && oci_image.is_none() {
            bail!(
                "pool {:?}: profile {:?} has install = \"image\", which needs a container pool; \
                 use install = \"preinstalled\" and put the requirements on the {} image or machine",
                pool.name,
                profile.name,
                pool.backend.kind()
            );
        }
        let image_arch = if arch.eq_ignore_ascii_case("arm64") {
            "arm64"
        } else {
            "amd64"
        };
        let platform_key = format!("{}-{}", os.to_ascii_lowercase(), arch.to_ascii_lowercase());
        let mut fingerprint = profile.fingerprint();
        let (base_image, derived_image) = match (mode, oci_image) {
            (InstallMode::Image, Some(base)) => {
                // The derived image depends on the base image's name and platform too.
                let mut h = Sha256::new();
                h.update(fingerprint.as_bytes());
                h.update(base.as_bytes());
                h.update(image_arch.as_bytes());
                fingerprint = hex(&h.finalize());
                let tag = format!(
                    "home-runner/prereq-{}-{}:{}",
                    pool.name,
                    profile.name,
                    &fingerprint[..12]
                );
                (Some(base), Some(tag))
            }
            _ => (None, None),
        };
        Ok(Self {
            profile: profile.clone(),
            mode,
            base_image,
            derived_image,
            platform_key,
            image_arch,
            fingerprint,
            start_docker,
            work_dir,
        })
    }

    /// The pool as jobs will run it: the derived image replaces the configured one.
    pub fn effective_pool(&self, pool: &PoolConfig) -> PoolConfig {
        let mut pool = pool.clone();
        if let (Some(image), BackendConfig::AppleContainer(c) | BackendConfig::Docker(c)) =
            (&self.derived_image, &mut pool.backend)
        {
            c.image = image.clone();
            // It only exists locally; there is nothing to pull.
            c.pull = false;
        }
        pool
    }

    /// Environment the runner passes on to every job.
    pub fn job_env(&self) -> Vec<(String, String)> {
        let mut env = Vec::new();
        if let Some(p) = self.profile.playwright() {
            env.push((
                "PLAYWRIGHT_BROWSERS_PATH".into(),
                format!(
                    "{}/{}/browsers",
                    p.root.trim_end_matches('/'),
                    self.platform_key
                ),
            ));
        }
        for req in &self.profile.requirements {
            match req {
                Requirement::TauriCef(r) => env.extend(r.job_env()),
                Requirement::MacosGui(g) => env.extend(g.job_env()),
                _ => {}
            }
        }
        env
    }

    pub fn can_install(&self) -> bool {
        self.mode == InstallMode::Image
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn profile(toml: &str) -> ProfileConfig {
        let p: ProfileConfig = toml::from_str(toml).unwrap();
        p.validate().unwrap();
        p
    }

    pub const WITH_TAP: &str = r#"
        name = "with-tap"
        version = 3
        [[require]]
        kind = "command"
        name = "node"
        version = "26"
        [[require]]
        kind = "command"
        name = "actionlint"
        version = "1.7.12"
        install = "curl -fsSL https://example.test/actionlint.tgz | tar -xz -C /usr/local/bin actionlint"
        [[require]]
        kind = "apt"
        packages = ["libasound2-dev", "pkg-config"]
        [[require]]
        kind = "docker"
        [[require]]
        kind = "playwright"
        versions = ["1.56.1", "1.49.0"]
    "#;

    pub fn pool(extra: &str) -> PoolConfig {
        toml::from_str(&format!(
            "name = \"linux\"\nbackend = \"apple-container\"\nimage = \"base:1\"\ndocker_in_job = true\n{extra}"
        ))
        .unwrap()
    }

    #[test]
    fn parses_the_with_tap_profile() {
        let p = profile(WITH_TAP);
        assert_eq!(p.requirements.len(), 5);
        assert_eq!(p.docker().unwrap().image, "postgres:17-alpine");
        assert_eq!(p.playwright().unwrap().browsers, ["chromium"]);
        let ids: Vec<_> = p.requirements.iter().flat_map(|r| r.check_ids()).collect();
        assert!(ids.contains(&"apt:pkg-config".to_string()));
        assert!(ids.contains(&"playwright:1.49.0:chromium".to_string()));
    }

    #[test]
    fn rejects_unsafe_or_vague_requirements() {
        let bad = |extra: &str| {
            let p: ProfileConfig =
                toml::from_str(&format!("name = \"p\"\n[[require]]\n{extra}")).unwrap();
            p.validate().unwrap_err().to_string()
        };
        assert!(bad("kind = \"command\"\nname = \"a b; rm -rf /\"").contains("command name"));
        assert!(bad("kind = \"apt\"\npackages = [\"x$(id)\"]").contains("Debian package"));
        assert!(bad("kind = \"playwright\"\nversions = [\"^1.56\"]").contains("exact"));
        assert!(
            bad("kind = \"playwright\"\nversions = [\"1.56.1\"]\nbrowsers = [\"edge\"]")
                .contains("browser")
        );
        assert!(bad("kind = \"docker\"\nimage = \"pg; id\"").contains("valid reference"));
        assert!(
            toml::from_str::<ProfileConfig>("name = \"p\"\n[[require]]\nkind = \"nope\"").is_err()
        );
    }

    #[test]
    fn the_fingerprint_tracks_everything_that_affects_readiness() {
        let base = profile(WITH_TAP);
        assert_eq!(base.fingerprint(), profile(WITH_TAP).fingerprint());
        let mut bumped = base.clone();
        bumped.version += 1;
        assert_ne!(base.fingerprint(), bumped.fingerprint());
        let mut changed = base.clone();
        if let Requirement::Command(c) = &mut changed.requirements[0] {
            c.version = Some("27".into());
        }
        assert_ne!(base.fingerprint(), changed.fingerprint());
        // How it's installed doesn't change what a ready worker looks like.
        let mut auto = base.clone();
        auto.auto_install = true;
        assert_eq!(base.fingerprint(), auto.fingerprint());
    }

    #[test]
    fn container_pools_get_a_derived_image_keyed_by_profile_and_base() {
        let p = profile(WITH_TAP);
        let a = Provision::new(&p, &pool(""), "Linux", "ARM64", "/tmp".into()).unwrap();
        assert_eq!(a.mode, InstallMode::Image);
        let tag = a.derived_image.clone().unwrap();
        assert!(
            tag.starts_with("home-runner/prereq-linux-with-tap:"),
            "{tag}"
        );
        assert_eq!(a.platform_key, "linux-arm64");
        assert_eq!(a.image_arch, "arm64");

        // A different base image or architecture is a different derived image.
        let mut other = pool("");
        if let BackendConfig::AppleContainer(c) = &mut other.backend {
            c.image = "base:2".into();
        }
        let b = Provision::new(&p, &other, "Linux", "ARM64", "/tmp".into()).unwrap();
        assert_ne!(a.derived_image, b.derived_image);
        let c = Provision::new(&p, &pool(""), "Linux", "X64", "/tmp".into()).unwrap();
        assert_ne!(a.derived_image, c.derived_image);
        assert_eq!(c.image_arch, "amd64");

        // Jobs run in the derived image, which exists only locally.
        let effective = a.effective_pool(&pool(""));
        match effective.backend {
            BackendConfig::AppleContainer(c) => {
                assert_eq!(Some(c.image), a.derived_image);
                assert!(!c.pull);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn other_backends_verify_but_the_app_does_not_install_for_them() {
        let p = profile(WITH_TAP);
        let tart: PoolConfig =
            toml::from_str("name = \"mac\"\nbackend = \"tart\"\nimage = \"i\"\nssh_key = \"k\"")
                .unwrap();
        let prov = Provision::new(&p, &tart, "macOS", "ARM64", "/tmp".into()).unwrap();
        assert_eq!(prov.mode, InstallMode::Preinstalled);
        assert!(!prov.can_install());
        assert!(prov.derived_image.is_none());

        let mut forced = p.clone();
        forced.install = Some(InstallMode::Image);
        let err = Provision::new(&forced, &tart, "macOS", "ARM64", "/tmp".into()).unwrap_err();
        assert!(err.to_string().contains("needs a container pool"));
    }

    const NATIVE: &str = r#"
        name = "native"
        [[require]]
        kind = "command"
        name = "node"
        version = "26"
        [[require]]
        kind = "tauri-cef"
        tools_dir = "/opt/home-runner/native/tools"
        tauri_revision = "78f49ba3993fed5a1b921235c547731006ad2a74"
        cli_version = "3.0.0-alpha.5"
        cef_path = "/opt/home-runner/native/cef/dist"
        cef_archive = "cef_binary_154.0.34+g14c5a08+chromium-154.0.8037.98_macosarm64_minimal.tar.bz2"
        cef_sha1 = "b7081f6609e5edccf07be96b5d7977329fbe1bdf"
        [[require]]
        kind = "macos-gui"
    "#;

    fn tart_pool() -> PoolConfig {
        toml::from_str("name = \"mac\"\nbackend = \"tart\"\nimage = \"i\"\nssh_key = \"k\"")
            .unwrap()
    }

    #[test]
    fn native_requirements_export_what_jobs_need_and_are_verified_on_tart() {
        let p = profile(NATIVE);
        let prov = Provision::new(&p, &tart_pool(), "macOS", "ARM64", "/tmp".into()).unwrap();
        assert_eq!(prov.mode, InstallMode::Preinstalled);
        let env = prov.job_env();
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(
            get("WITH_TAP_NATIVE_TOOLS_DIR"),
            Some("/opt/home-runner/native/tools")
        );
        assert_eq!(
            get("WITH_TAP_CEF_PATH"),
            Some("/opt/home-runner/native/cef/dist")
        );
        assert_eq!(
            get("HOME_RUNNER_INPUT"),
            Some("/usr/local/libexec/home-runner/hr-input")
        );
    }

    #[test]
    fn native_requirements_never_land_on_a_container_pool_or_the_hosts_desktop() {
        let p = profile(NATIVE);
        let err = Provision::new(&p, &pool(""), "Linux", "ARM64", "/tmp".into()).unwrap_err();
        assert!(err.to_string().contains("needs a `tart` pool"), "{err}");
        let host: PoolConfig = toml::from_str("name = \"h\"\nbackend = \"host\"").unwrap();
        let err = Provision::new(&p, &host, "macOS", "ARM64", "/tmp".into()).unwrap_err();
        assert!(
            err.to_string()
                .contains("never the desktop of a `host` pool"),
            "{err}"
        );
    }

    #[test]
    fn readiness_is_tied_to_the_pinned_cli_cef_archive_and_helper() {
        let base = profile(NATIVE);
        assert_eq!(base.fingerprint(), profile(NATIVE).fingerprint());
        for (from, to) in [
            (
                "78f49ba3993fed5a1b921235c547731006ad2a74",
                "88f49ba3993fed5a1b921235c547731006ad2a74",
            ),
            (
                "b7081f6609e5edccf07be96b5d7977329fbe1bdf",
                "c7081f6609e5edccf07be96b5d7977329fbe1bdf",
            ),
            (
                "154.0.34+g14c5a08+chromium-154.0.8037.98",
                "154.0.35+g14c5a08+chromium-154.0.8037.98",
            ),
            ("3.0.0-alpha.5", "3.0.0-alpha.6"),
        ] {
            assert_ne!(
                base.fingerprint(),
                profile(&NATIVE.replace(from, to)).fingerprint(),
                "{from}"
            );
        }
        // A new release of the helper changes the revision profiles default to.
        assert!(base.fingerprint().len() == 64);
        let gui = base.macos_gui().unwrap();
        assert_eq!(gui.revision, native::helper_revision());
    }

    #[test]
    fn jobs_are_pointed_at_the_per_platform_browser_cache() {
        let p = profile(WITH_TAP);
        let prov = Provision::new(&p, &pool(""), "Linux", "X64", "/tmp".into()).unwrap();
        assert_eq!(
            prov.job_env(),
            [(
                "PLAYWRIGHT_BROWSERS_PATH".to_string(),
                "/opt/home-runner/playwright/linux-x64/browsers".to_string()
            )]
        );
        let none = profile("name = \"p\"\n[[require]]\nkind = \"command\"\nname = \"git\"");
        let prov = Provision::new(&none, &pool(""), "Linux", "X64", "/tmp".into()).unwrap();
        assert!(prov.job_env().is_empty());
    }
}
