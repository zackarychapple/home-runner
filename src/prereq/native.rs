//! Native macOS prerequisites for Tart workers: the pinned Tauri/CEF toolchain (`tauri-cef`) and
//! an interactive login session whose native input is proven to work (`macos-gui`).
//!
//! Like every requirement, the probe script only gathers facts inside the worker as the runner
//! user, one `HRP` line per check; the judgment (does the CLI revision match, is the archive the
//! pinned one, which stage of the input proof failed) is done here on plain text, so each state
//! is tested with canned output. The app verifies these on Tart pools and never installs them from
//! a job: `images/macos-latest/native/install-native.sh` and `images/macos-latest/gui/` build them
//! into the golden image.

use std::collections::HashMap;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::probe::{Check, EvalCtx, Status, sh_quote};

/// The helper's source. Its SHA-256 is the revision a worker's helper must report.
const HELPER_SOURCE: &str = include_str!("../../images/macos-latest/gui/hr-input.swift");

/// The pinned Cargo Tauri CLI and CEF distribution, provisioned read-only on the image.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TauriCefReq {
    /// Root of the provisioned CLI: `bin/cargo-tauri` and `tauri-revision`. Read-only for the
    /// runner user.
    pub tools_dir: String,
    /// The variable jobs find `tools_dir` in.
    #[serde(default = "default_tools_env")]
    pub tools_env: String,
    /// The immutable Tauri source revision the CLI was built from.
    pub tauri_revision: String,
    /// What the CLI reports for itself, e.g. `3.0.0-alpha.5`.
    pub cli_version: String,
    /// The extracted, architecture-matched CEF distribution (it holds `archive.json`).
    pub cef_path: String,
    /// The variable jobs find `cef_path` in.
    #[serde(default = "default_cef_env")]
    pub cef_env: String,
    /// The published archive file name, e.g. `cef_binary_…_macosarm64_minimal.tar.bz2`. The
    /// archive itself stays beside the extracted tree (`<cef_path>/../<name>`) for verification,
    /// together with `<name>.tree.sha256`, a manifest of every extracted file.
    pub cef_archive: String,
    /// Its SHA-1, as published in the CEF builds index. It authorizes the artifact.
    pub cef_sha1: String,
    #[serde(default = "default_rust_target")]
    pub rust_target: String,
    /// Validation workers hold no signing identities. Set this on a release worker that must.
    #[serde(default)]
    pub allow_signing_identities: bool,
}

/// A logged-in graphical session in which a real keyboard and pointer event provably reaches a
/// disposable test app.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MacosGuiReq {
    /// The app-owned, signed input helper (`images/macos-latest/gui`). Jobs get its path in
    /// `helper_env`, so suites use it instead of compiling a helper that has no Accessibility grant.
    #[serde(default = "default_helper")]
    pub helper: String,
    #[serde(default = "default_helper_env")]
    pub helper_env: String,
    /// The source revision the helper must report. Defaults to this app's own copy, so a new
    /// release of the helper invalidates readiness until the image has it.
    #[serde(default = "helper_revision")]
    pub revision: String,
}

fn default_tools_env() -> String {
    "WITH_TAP_NATIVE_TOOLS_DIR".into()
}
fn default_cef_env() -> String {
    "WITH_TAP_CEF_PATH".into()
}
fn default_rust_target() -> String {
    "aarch64-apple-darwin".into()
}
fn default_helper() -> String {
    "/usr/local/libexec/home-runner/hr-input".into()
}
fn default_helper_env() -> String {
    "HOME_RUNNER_INPUT".into()
}
pub fn helper_revision() -> String {
    Sha256::digest(HELPER_SOURCE.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub const TAURI_CEF_IDS: [&str; 14] = [
    "platform",
    "xcode",
    "clang",
    "swift",
    "cmake",
    "ninja",
    "rustfmt",
    "clippy",
    "rust-target",
    "cli",
    "cef",
    "cef-archive",
    "cef-tree",
    "signing",
];
pub const GUI_IDS: [&str; 4] = ["session", "helper", "access", "input"];

pub fn tauri_cef_ids() -> Vec<String> {
    TAURI_CEF_IDS
        .iter()
        .map(|s| format!("native:{s}"))
        .collect()
}
pub fn gui_ids() -> Vec<String> {
    GUI_IDS.iter().map(|s| format!("gui:{s}")).collect()
}

fn word(s: &str, allowed: impl Fn(char) -> bool) -> bool {
    !s.is_empty() && s.chars().all(allowed)
}
fn hex(s: &str, len: usize) -> bool {
    s.len() == len && s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}
fn absolute(s: &str) -> bool {
    s.starts_with('/') && word(s, |c| c.is_ascii_alphanumeric() || "._-/".contains(c))
}
fn env_name(s: &str) -> bool {
    word(s, |c| {
        c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'
    }) && !s.starts_with(|c: char| c.is_ascii_digit())
}

impl TauriCefReq {
    pub fn validate(&self, profile: &str) -> Result<()> {
        ensure!(
            absolute(&self.tools_dir),
            "profile {profile:?}: tauri-cef tools_dir {:?} must be an absolute path",
            self.tools_dir
        );
        ensure!(
            absolute(&self.cef_path),
            "profile {profile:?}: tauri-cef cef_path {:?} must be an absolute path",
            self.cef_path
        );
        ensure!(
            hex(&self.tauri_revision, 40),
            "profile {profile:?}: tauri_revision must be a full 40-character commit hash (lowercase), not a branch or tag"
        );
        ensure!(
            hex(&self.cef_sha1, 40),
            "profile {profile:?}: cef_sha1 must be the archive's 40-character SHA-1 (lowercase)"
        );
        ensure!(
            word(&self.cli_version, |c| c.is_ascii_alphanumeric()
                || "._+-".contains(c)),
            "profile {profile:?}: cli_version {:?} is not a version",
            self.cli_version
        );
        ensure!(
            word(&self.cef_archive, |c| c.is_ascii_alphanumeric()
                || "._+-".contains(c))
                && self.cef_archive.ends_with(".tar.bz2")
                && self.cef_archive.contains("_macosarm64_"),
            "profile {profile:?}: cef_archive {:?} must be a macosarm64 .tar.bz2 distribution name",
            self.cef_archive
        );
        ensure!(
            word(&self.rust_target, |c| c.is_ascii_alphanumeric()
                || "_-".contains(c)),
            "profile {profile:?}: rust_target {:?} is not a target triple",
            self.rust_target
        );
        for (what, name) in [("tools_env", &self.tools_env), ("cef_env", &self.cef_env)] {
            ensure!(
                env_name(name),
                "profile {profile:?}: {what} {name:?} is not an environment variable name"
            );
        }
        ensure!(
            self.tools_env != self.cef_env,
            "profile {profile:?}: tools_env and cef_env must differ"
        );
        Ok(())
    }

    /// The CEF version the archive name carries: `cef_binary_<version>_macosarm64_minimal…`.
    fn cef_version(&self) -> Option<&str> {
        self.cef_archive
            .strip_prefix("cef_binary_")?
            .split("_macos")
            .next()
    }

    pub fn job_env(&self) -> Vec<(String, String)> {
        vec![
            (self.tools_env.clone(), self.tools_dir.clone()),
            (self.cef_env.clone(), self.cef_path.clone()),
        ]
    }
}

impl MacosGuiReq {
    pub fn validate(&self, profile: &str) -> Result<()> {
        ensure!(
            absolute(&self.helper),
            "profile {profile:?}: macos-gui helper {:?} must be an absolute path",
            self.helper
        );
        ensure!(
            env_name(&self.helper_env),
            "profile {profile:?}: helper_env {:?} is not an environment variable name",
            self.helper_env
        );
        ensure!(
            hex(&self.revision, 64),
            "profile {profile:?}: macos-gui revision must be a SHA-256 (lowercase hex)"
        );
        Ok(())
    }

    pub fn job_env(&self) -> Vec<(String, String)> {
        vec![(self.helper_env.clone(), self.helper.clone())]
    }
}

// ---------------------------------------------------------------------------------------------
// The probe scripts

const HELPERS: &str = r#"hr_sha() { if command -v shasum >/dev/null 2>&1; then shasum -a "$1" "$2" | awk '{print $1}'; else "sha$1sum" "$2" | awk '{print $1}'; fi; }
hr_shacheck() { if command -v shasum >/dev/null 2>&1; then shasum -a 256 -c "$1"; else sha256sum -c "$1"; fi; }
hr_tool() {
  id=$1; shift
  if command -v "$1" >/dev/null 2>&1; then
    out=$("$@" 2>&1); rc=$?
    if [ "$rc" -eq 0 ]; then hr_emit "$id" ok "$(printf '%s\n' "$out" | head -n 1)"
    else hr_emit "$id" fail "exit $rc: $out"; fi
  else
    hr_emit "$id" missing "not found on PATH"
  fi
}
hr_ro() { w=no; for p in "$@"; do [ ! -w "$p" ] || w=yes; done; printf '%s' "$w"; }
"#;

const TAURI_CEF: &str = r#"hr_native_probe() {
  tools=@TOOLS@; cefdir=@CEFDIR@; archive=@ARCHIVE@; target=@TARGET@
  parent=$(dirname "$cefdir")
  tr=$(sysctl -n sysctl.proc_translated 2>/dev/null); tr=${tr:-0}
  hr_emit native:platform ok "os=$(uname -s) | arch=$(uname -m) | translated=$tr"

  if sel=$(xcode-select -p 2>&1); then
    case "$sel" in */Contents/Developer) kind=xcode ;; *) kind=clt ;; esac
    ver=$(xcodebuild -version 2>/dev/null | tr '\n' ' ')
    lic=ok
    if [ "$kind" = xcode ]; then xcodebuild -license check >/dev/null 2>&1 || lic=unaccepted; fi
    sdk=$(xcrun --sdk macosx --show-sdk-path 2>/dev/null)
    hr_emit native:xcode ok "path=$sel | kind=$kind | version=$ver | license=$lic | sdk=$sdk"
  else
    hr_emit native:xcode missing "$sel"
  fi
  hr_tool native:clang clang --version
  hr_tool native:swift swift --version
  hr_tool native:cmake cmake --version
  hr_tool native:ninja ninja --version
  hr_tool native:rustfmt rustfmt --version
  hr_tool native:clippy cargo clippy --version
  if sysroot=$(rustc --print sysroot 2>&1); then
    if [ -d "$sysroot/lib/rustlib/$target" ]; then hr_emit native:rust-target ok "$target"
    else hr_emit native:rust-target missing "the $target standard library is not installed"; fi
  else
    hr_emit native:rust-target missing "rustc is not usable: $sysroot"
  fi

  bin="$tools/bin/cargo-tauri"
  if [ ! -x "$bin" ]; then
    hr_emit native:cli missing "no executable at $bin"
  else
    rev=$(head -n 1 "$tools/tauri-revision" 2>/dev/null)
    ver=$("$bin" --version 2>&1 | head -n 1)
    hr_emit native:cli ok "rev=$rev | version=$ver | writable=$(hr_ro "$tools" "$tools/bin" "$bin" "$tools/tauri-revision")"
  fi

  aj="$cefdir/archive.json"
  if [ ! -f "$aj" ]; then
    hr_emit native:cef missing "no archive.json in $cefdir"
  else
    jn=$(sed -n 's/.*"name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$aj" | head -n 1)
    js=$(sed -n 's/.*"sha1"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$aj" | head -n 1)
    hdr="$cefdir/include/cef_version.h"
    fw="$cefdir/Chromium Embedded Framework.framework/Chromium Embedded Framework"
    ver=$(sed -n 's/^#define CEF_VERSION "\([^"]*\)".*/\1/p' "$hdr" 2>/dev/null | head -n 1)
    archs=$(lipo -archs "$fw" 2>/dev/null)
    h=no; [ -f "$hdr" ] && h=yes
    f=no; [ -f "$fw" ] && f=yes
    wr=no; [ -f "$cefdir/CMakeLists.txt" ] && [ -f "$cefdir/libcef_dll/CMakeLists.txt" ] && wr=yes
    hr_emit native:cef ok "name=$jn | sha1=$js | version=$ver | archs=$archs | headers=$h | framework=$f | wrapper=$wr | writable=$(hr_ro "$cefdir" "$aj" "$cefdir/include" "$fw")"
  fi

  arc="$parent/$archive"
  if [ -f "$arc" ]; then hr_emit native:cef-archive ok "sha1=$(hr_sha 1 "$arc") | file=$arc"
  else hr_emit native:cef-archive missing "no archive at $arc"; fi

  man="$parent/$archive.tree.sha256"
  if [ ! -f "$man" ]; then
    hr_emit native:cef-tree missing "no manifest at $man"
  elif [ ! -d "$cefdir" ]; then
    hr_emit native:cef-tree missing "no distribution at $cefdir"
  else
    out=$(cd "$cefdir" && hr_shacheck "$man" 2>&1); rc=$?
    bad=$(printf '%s\n' "$out" | grep -m 1 -i 'failed\|no such file')
    if [ "$rc" -eq 0 ]; then hr_emit native:cef-tree ok "files=$(wc -l < "$man" | tr -d ' ')"
    else hr_emit native:cef-tree fail "${bad:-exit $rc}"; fi
  fi

  if command -v security >/dev/null 2>&1; then
    out=$(security find-identity -v -p codesigning 2>&1)
    n=$(printf '%s\n' "$out" | sed -n 's/^ *\([0-9][0-9]*\) valid identit.*/\1/p' | head -n 1)
    hr_emit native:signing ok "identities=${n:-0}"
  else
    hr_emit native:signing ok "identities=0 | security=missing"
  fi
}
hr_native_probe
"#;

const GUI: &str = r#"hr_gui_probe() {
  helper=@HELPER@
  me=$(id -un)
  vm=$(sysctl -n kern.hv_vmm_present 2>/dev/null); vm=${vm:-0}
  console=$(stat -f %Su /dev/console 2>/dev/null)
  if pgrep -x WindowServer >/dev/null 2>&1; then ws=yes; else ws=no; fi
  hr_emit gui:session ok "user=$me | console=$console | windowserver=$ws | vm=$vm"

  if [ ! -x "$helper" ]; then
    hr_emit gui:helper missing "no executable at $helper"
    return
  fi
  ver=$("$helper" version 2>&1 | head -n 1)
  id=$(codesign -dv "$helper" 2>&1 | sed -n 's/^Identifier=//p' | head -n 1)
  sig=ok; codesign --verify --strict "$helper" >/dev/null 2>&1 || sig=invalid
  hr_emit gui:helper ok "version=$ver | identifier=$id | signature=$sig | writable=$(hr_ro "$helper" "$(dirname "$helper")")"

  out=$("$helper" access 2>&1); rc=$?
  if [ "$rc" -eq 0 ]; then hr_emit gui:access ok "$out"; else hr_emit gui:access fail "exit $rc: $out"; fi

  out=$("$helper" selftest 2>&1); rc=$?
  last=$(printf '%s\n' "$out" | tail -n 1)
  if [ "$rc" -eq 0 ]; then hr_emit gui:input ok "$last"; else hr_emit gui:input fail "${last:-exit $rc}"; fi
}
hr_gui_probe
"#;

pub fn tauri_cef_script(r: &TauriCefReq) -> String {
    let body = TAURI_CEF
        .replace("@TOOLS@", &sh_quote(r.tools_dir.trim_end_matches('/')))
        .replace("@CEFDIR@", &sh_quote(r.cef_path.trim_end_matches('/')))
        .replace("@ARCHIVE@", &sh_quote(&r.cef_archive))
        .replace("@TARGET@", &sh_quote(&r.rust_target));
    format!("{HELPERS}{body}")
}

pub fn gui_script(r: &MacosGuiReq) -> String {
    format!("{HELPERS}{}", GUI.replace("@HELPER@", &sh_quote(&r.helper)))
}

// ---------------------------------------------------------------------------------------------
// Judging what the probe saw

/// `a=1 | b=two words` → `{a: "1", b: "two words"}`.
fn fields(detail: &str) -> HashMap<&str, &str> {
    detail
        .split(" | ")
        .filter_map(|part| part.split_once('='))
        .map(|(k, v)| (k.trim(), v.trim()))
        .collect()
}

fn build_hint(ctx: &EvalCtx) -> String {
    format!(
        "rebuild the golden image for pool {} with `images/macos-latest/build.sh NATIVE=true` (README: \
         Native prerequisites); the app never installs these from a job",
        ctx.pool
    )
}

pub fn judge_tauri_cef(
    r: &TauriCefReq,
    id: &str,
    status: &str,
    detail: &str,
    ctx: &EvalCtx,
) -> Check {
    let name = id.strip_prefix("native:").unwrap_or(id);
    let rebuild = build_hint(ctx);
    let repair = match name {
        "platform" => {
            "run this pool on the macOS ARM64 Tart image, natively (not under Rosetta)".to_string()
        }
        "xcode" => "select and license Xcode in the image: `sudo xcode-select -s \
            /Applications/Xcode.app && sudo xcodebuild -license accept && sudo xcodebuild \
            -runFirstLaunch` (images/macos-latest/provision.sh does this)"
            .to_string(),
        "clang" | "swift" => "install and license Xcode or the Command Line Tools in the image \
            (`sudo xcodebuild -license accept`)"
            .to_string(),
        "cmake" | "ninja" => "install CMake and Ninja in the image: \
            `images/macos-latest/build.sh BREW_PACKAGES=\"cmake ninja\"`"
            .to_string(),
        "rustfmt" | "clippy" => format!(
            "add the components to the pinned toolchain in the image: `rustup component add \
             rustfmt clippy` (`RUST_COMPONENTS` in {}'s image build)",
            ctx.pool
        ),
        "rust-target" => format!(
            "`rustup target add {}` in the image (`RUST_TARGETS` in the image build)",
            r.rust_target
        ),
        _ => rebuild,
    };
    let bad = |status: Status, what: String| Check::new(id, status, what).hint(repair.clone());

    match status {
        "missing" => return bad(Status::Missing, detail.to_string()),
        "ok" => {}
        _ => return bad(Status::Failed, detail.to_string()),
    }
    let f = fields(detail);
    match name {
        "platform" => {
            let (os, arch) = (f.get("os").copied(), f.get("arch").copied());
            let rosetta = f.get("translated") == Some(&"1");
            if os != Some("Darwin") || arch != Some("arm64") || rosetta {
                return bad(
                    Status::Failed,
                    format!(
                        "this worker is {}/{}{}; native validation needs Darwin/arm64 (Apple \
                         Silicon, not under Rosetta)",
                        os.unwrap_or("?"),
                        arch.unwrap_or("?"),
                        if rosetta { " under Rosetta" } else { "" }
                    ),
                );
            }
        }
        "xcode" => {
            if f.get("license") != Some(&"ok") {
                return bad(
                    Status::Failed,
                    format!("the Xcode license has not been accepted ({detail})"),
                );
            }
            if f.get("sdk").is_none_or(|s| s.is_empty()) {
                return bad(
                    Status::Failed,
                    format!("the selected developer directory has no macOS SDK ({detail})"),
                );
            }
        }
        "cli" => {
            let (rev, version) = (
                f.get("rev").copied().unwrap_or(""),
                f.get("version").copied().unwrap_or(""),
            );
            if rev != r.tauri_revision {
                return bad(
                    Status::Wrong,
                    format!(
                        "the CLI was built from {}, need {}",
                        if rev.is_empty() {
                            "an unrecorded revision"
                        } else {
                            rev
                        },
                        r.tauri_revision
                    ),
                );
            }
            if !super::probe::version_matches(&r.cli_version, version) {
                return bad(
                    Status::Wrong,
                    format!("the CLI reports {version:?}, need {}", r.cli_version),
                );
            }
            if f.get("writable") != Some(&"no") {
                return bad(
                    Status::Failed,
                    format!(
                        "{} is writable by the runner user; a job could replace the CLI",
                        r.tools_dir
                    ),
                );
            }
            return Check::new(id, Status::Ok, format!("{version} at {rev}"));
        }
        "cef" => return judge_cef(r, id, &f, &repair),
        "cef-archive" => {
            let got = f.get("sha1").copied().unwrap_or("");
            if got != r.cef_sha1 {
                return bad(
                    Status::Failed,
                    format!(
                        "{} has sha1 {got}, but {} is pinned: the archive is corrupt or not the approved one",
                        r.cef_archive, r.cef_sha1
                    ),
                );
            }
            return Check::new(id, Status::Ok, format!("sha1 {got}"));
        }
        "signing" => {
            let n: u32 = f
                .get("identities")
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            if n > 0 && !r.allow_signing_identities {
                return bad(
                    Status::Failed,
                    format!(
                        "{n} code-signing identit{} in the worker's keychain: native validation \
                         workers must hold no signing credentials",
                        if n == 1 { "y is" } else { "ies are" }
                    ),
                )
                .hint(
                    "build the image without signing material (import it into a temporary keychain \
                     in the signing pool's own job instead); `allow_signing_identities = true` is \
                     only for a release pool",
                );
            }
            return Check::new(id, Status::Ok, format!("{n} signing identities"));
        }
        _ => {}
    }
    Check::new(id, Status::Ok, detail.to_string())
}

fn judge_cef(r: &TauriCefReq, id: &str, f: &HashMap<&str, &str>, repair: &str) -> Check {
    let bad = |what: String| Check::new(id, Status::Failed, what).hint(repair.to_string());
    let get = |k: &str| f.get(k).copied().unwrap_or("");
    if get("name") != r.cef_archive {
        return bad(format!(
            "the distribution was extracted from {:?}, need {}",
            get("name"),
            r.cef_archive
        ));
    }
    if get("sha1") != r.cef_sha1 {
        return bad(format!(
            "archive.json records sha1 {:?}, but {} is pinned",
            get("sha1"),
            r.cef_sha1
        ));
    }
    if let Some(want) = r.cef_version()
        && !get("version").starts_with(want)
    {
        return bad(format!(
            "the headers are CEF {:?}, the archive is {want}",
            get("version")
        ));
    }
    for (key, what) in [
        ("headers", "include/cef_version.h"),
        ("framework", "Chromium Embedded Framework.framework"),
        ("wrapper", "the libcef_dll wrapper sources"),
    ] {
        if get(key) != "yes" {
            return bad(format!("the distribution has no {what}"));
        }
    }
    if get("archs") != "arm64" {
        return bad(format!(
            "the framework is built for {:?}, this worker needs arm64",
            get("archs")
        ));
    }
    if get("writable") != "no" {
        return bad(format!(
            "{} is writable by the runner user; a job could change the runtime",
            r.cef_path
        ));
    }
    Check::new(
        id,
        Status::Ok,
        format!("{} ({})", get("version"), get("archs")),
    )
}

pub fn judge_gui(r: &MacosGuiReq, id: &str, status: &str, detail: &str, ctx: &EvalCtx) -> Check {
    let name = id.strip_prefix("gui:").unwrap_or(id);
    let setup = format!(
        "build the golden image for pool {} with `images/macos-latest/build.sh GUI=true` and grant \
         Accessibility once in the VM window (README: Native GUI workers)",
        ctx.pool
    );
    let fail = |status: Status, what: String, hint: &str| {
        Check::new(id, status, what).hint(hint.to_string())
    };
    if status == "missing" {
        return fail(Status::Missing, detail.to_string(), &setup);
    }
    if status != "ok" && name != "input" {
        return fail(Status::Failed, detail.to_string(), &setup);
    }
    let f = fields(detail);
    match name {
        "session" => {
            let user = f.get("user").copied().unwrap_or("");
            let console = f.get("console").copied().unwrap_or("");
            if f.get("vm") != Some(&"1") {
                return fail(
                    Status::Failed,
                    "this worker is not a virtual machine: input would go to a real desktop".into(),
                    "run native GUI jobs only in Tart workers, never on the host (`host` pools \
                     cannot use macos-gui)",
                );
            }
            let login = "turn on automatic login for the runner user in the golden image \
                (System Settings > Users & Groups > Automatic login; needs FileVault off)";
            if matches!(console, "" | "root" | "_mbsetupuser") {
                return fail(
                    Status::Failed,
                    format!(
                        "no session: nobody is logged in at the console (console owner {console:?})"
                    ),
                    login,
                );
            }
            if console != user {
                return fail(
                    Status::Failed,
                    format!(
                        "wrong session: the console belongs to {console}, but jobs run as {user}"
                    ),
                    login,
                );
            }
            if f.get("windowserver") != Some(&"yes") {
                return fail(
                    Status::Failed,
                    "no session: the WindowServer is not running".into(),
                    login,
                );
            }
            Check::new(
                id,
                Status::Ok,
                format!("{user} is logged in at the console"),
            )
        }
        "helper" => {
            let version = f.get("version").copied().unwrap_or("");
            let got = version.strip_prefix("rev=").unwrap_or("");
            if got != r.revision {
                return fail(
                    Status::Wrong,
                    format!(
                        "the helper is revision {}, this home-runner needs {}: its Accessibility \
                         grant may not apply to a new build",
                        if got.is_empty() {
                            "unknown"
                        } else {
                            &got[..got.len().min(12)]
                        },
                        &r.revision[..12.min(r.revision.len())]
                    ),
                    &format!(
                        "{setup}. A helper signed with the same identity keeps its grant; an ad-hoc signed one needs a new grant"
                    ),
                );
            }
            if f.get("signature") != Some(&"ok") {
                return fail(
                    Status::Failed,
                    "the helper's code signature does not verify".into(),
                    &setup,
                );
            }
            if f.get("writable") != Some(&"no") {
                return fail(
                    Status::Failed,
                    format!(
                        "{} is writable by the runner user: a job could replace it",
                        r.helper
                    ),
                    &setup,
                );
            }
            Check::new(
                id,
                Status::Ok,
                format!(
                    "rev {} ({})",
                    &got[..12.min(got.len())],
                    f.get("identifier").copied().unwrap_or("?")
                ),
            )
        }
        "access" => {
            #[derive(Deserialize)]
            struct Access {
                #[serde(default)]
                session: bool,
                #[serde(default)]
                trusted: bool,
                #[serde(default)]
                post: bool,
                #[serde(default)]
                detail: String,
            }
            let Ok(a) = serde_json::from_str::<Access>(detail) else {
                return fail(
                    Status::Failed,
                    format!("the helper's access report is not understood: {detail}"),
                    &setup,
                );
            };
            if !a.session {
                return fail(
                    Status::Failed,
                    format!(
                        "the helper cannot reach the login session from the job context{}",
                        if a.detail.is_empty() {
                            String::new()
                        } else {
                            format!(" ({})", a.detail)
                        }
                    ),
                    "the runner must be able to enter the login session: keep passwordless sudo \
                     for the runner user in the image (the helper uses `sudo -n launchctl asuser`)",
                );
            }
            if !a.trusted || !a.post {
                return fail(
                    Status::Failed,
                    format!(
                        "permission not granted: Accessibility trusted={}, event posting={} for {}",
                        a.trusted, a.post, r.helper
                    ),
                    &format!(
                        "{setup}. If it was granted before, it was revoked or the helper was replaced by a differently signed build"
                    ),
                );
            }
            Check::new(
                id,
                Status::Ok,
                "Accessibility and event posting are granted",
            )
        }
        _ => {
            // gui:input: the helper's own staged proof.
            #[derive(Deserialize)]
            struct Proof {
                ok: bool,
                #[serde(default)]
                stage: String,
                #[serde(default)]
                detail: String,
            }
            match serde_json::from_str::<Proof>(detail) {
                Ok(p) if p.ok && status == "ok" => Check::new(id, Status::Ok, p.detail),
                Ok(p) => {
                    let (what, hint) = match p.stage.as_str() {
                        "vm" => ("not a virtual machine", "run in a Tart worker only"),
                        "session" => (
                            "no login session",
                            "see gui:session; the worker must auto-login",
                        ),
                        "permission" => ("posting permission is missing", setup.as_str()),
                        "focus" => (
                            "the test app could not be activated",
                            "something else holds the focus, or the session is locked or at a \
                             prompt: check the VM image for login items, screen lock and \
                             first-run dialogs",
                        ),
                        "delivery" => (
                            "the event was posted but never reached the app",
                            "the grant belongs to a different helper identity than the one that \
                             ran, or the session is locked: re-grant the helper in the VM window",
                        ),
                        _ => ("input proof failed", setup.as_str()),
                    };
                    fail(Status::Failed, format!("{what}: {}", p.detail), hint)
                }
                Err(_) => fail(
                    Status::Failed,
                    format!("the input proof did not report a result: {detail}"),
                    &setup,
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::InstallMode;
    use super::*;

    pub const SHA1: &str = "b7081f6609e5edccf07be96b5d7977329fbe1bdf";
    pub const REV: &str = "78f49ba3993fed5a1b921235c547731006ad2a74";
    pub const ARCHIVE: &str =
        "cef_binary_154.0.34+g14c5a08+chromium-154.0.8037.98_macosarm64_minimal.tar.bz2";

    pub fn tauri() -> TauriCefReq {
        toml::from_str(&format!(
            "tools_dir = \"/opt/home-runner/native/tools\"\n\
             tauri_revision = \"{REV}\"\ncli_version = \"3.0.0-alpha.5\"\n\
             cef_path = \"/opt/home-runner/native/cef/dist\"\n\
             cef_archive = \"{ARCHIVE}\"\ncef_sha1 = \"{SHA1}\""
        ))
        .unwrap()
    }

    fn ctx() -> EvalCtx<'static> {
        EvalCtx {
            pool: "mac",
            mode: InstallMode::Preinstalled,
            image_arch: "arm64",
            platform_key: "macos-arm64",
        }
    }

    fn ok_cef() -> String {
        format!(
            "name={ARCHIVE} | sha1={SHA1} | version=154.0.34+g14c5a08+chromium-154.0.8037.98 | \
             archs=arm64 | headers=yes | framework=yes | wrapper=yes | writable=no"
        )
    }

    fn judge(id: &str, status: &str, detail: &str) -> Check {
        judge_tauri_cef(&tauri(), &format!("native:{id}"), status, detail, &ctx())
    }

    #[test]
    fn the_defaults_match_what_with_tap_consumes() {
        let r = tauri();
        r.validate("p").unwrap();
        assert_eq!(
            r.job_env(),
            [
                (
                    "WITH_TAP_NATIVE_TOOLS_DIR".to_string(),
                    "/opt/home-runner/native/tools".to_string()
                ),
                (
                    "WITH_TAP_CEF_PATH".to_string(),
                    "/opt/home-runner/native/cef/dist".to_string()
                ),
            ]
        );
        assert_eq!(
            r.cef_version(),
            Some("154.0.34+g14c5a08+chromium-154.0.8037.98")
        );
    }

    #[test]
    fn pins_must_be_immutable_and_specific() {
        let bad = |edit: &dyn Fn(&mut TauriCefReq)| {
            let mut r = tauri();
            edit(&mut r);
            r.validate("p").unwrap_err().to_string()
        };
        assert!(bad(&|r| r.tauri_revision = "main".into()).contains("40-character commit"));
        assert!(bad(&|r| r.cef_sha1 = "B7081F".into()).contains("SHA-1"));
        assert!(bad(&|r| r.cef_path = "relative/cef".into()).contains("absolute"));
        assert!(
            bad(&|r| r.cef_archive = "cef_binary_x_macosx64_minimal.tar.bz2".into())
                .contains("macosarm64")
        );
        assert!(bad(&|r| r.tools_env = "lower".into()).contains("environment variable"));
        assert!(bad(&|r| r.cef_env = r.tools_env.clone()).contains("differ"));
        assert!(bad(&|r| r.tools_dir = "/x; rm -rf /".into()).contains("absolute"));
    }

    #[test]
    fn a_fully_provisioned_worker_passes_every_native_check() {
        for (id, status, detail) in [
            ("platform", "ok", "os=Darwin | arch=arm64 | translated=0"),
            (
                "xcode",
                "ok",
                "path=/Applications/Xcode.app/Contents/Developer | kind=xcode | version=Xcode 26.0 Build version 17A | license=ok | sdk=/sdk",
            ),
            ("clang", "ok", "Apple clang version 21"),
            (
                "cli",
                "ok",
                &format!("rev={REV} | version=tauri-cli 3.0.0-alpha.5 | writable=no"),
            ),
            ("cef", "ok", &ok_cef()),
            ("cef-archive", "ok", &format!("sha1={SHA1} | file=/x")),
            ("cef-tree", "ok", "files=1500"),
            ("signing", "ok", "identities=0"),
        ] {
            let c = judge(id, status, detail);
            assert_eq!(c.status, Status::Ok, "{id}: {c:?}");
        }
    }

    #[test]
    fn a_wrong_or_unrecorded_cli_revision_is_reported_with_both_revisions() {
        let c = judge(
            "cli",
            "ok",
            "rev=deadbeef | version=tauri-cli 3.0.0-alpha.5 | writable=no",
        );
        assert_eq!(c.status, Status::Wrong);
        assert!(
            c.detail.contains("deadbeef") && c.detail.contains(REV),
            "{}",
            c.detail
        );
        assert!(c.hint.unwrap().contains("build.sh NATIVE=true"));
        let none = judge(
            "cli",
            "ok",
            "rev= | version=tauri-cli 3.0.0-alpha.5 | writable=no",
        );
        assert!(none.detail.contains("unrecorded"));
        // A generic or differently versioned CLI at the right revision marker is still wrong.
        let v = judge(
            "cli",
            "ok",
            &format!("rev={REV} | version=tauri-cli 2.9.1 | writable=no"),
        );
        assert_eq!(v.status, Status::Wrong);
        let rw = judge(
            "cli",
            "ok",
            &format!("rev={REV} | version=tauri-cli 3.0.0-alpha.5 | writable=yes"),
        );
        assert_eq!(rw.status, Status::Failed);
        assert!(rw.detail.contains("writable"));
        assert_eq!(
            judge("cli", "missing", "no executable").status,
            Status::Missing
        );
    }

    #[test]
    fn cef_version_architecture_and_provenance_mismatches_block_readiness() {
        let tweak = |from: &str, to: &str| judge("cef", "ok", &ok_cef().replace(from, to));
        let wrong_archive = tweak("macosarm64_minimal", "macosx64_minimal");
        assert_eq!(wrong_archive.status, Status::Failed);
        assert!(wrong_archive.detail.contains("extracted from"));
        let sha = tweak(SHA1, "0000000000000000000000000000000000000000");
        assert!(sha.detail.contains("archive.json records sha1"));
        let version = tweak("version=154.0.34+", "version=153.0.1+");
        assert!(
            version.detail.contains("headers are CEF"),
            "{}",
            version.detail
        );
        let arch = tweak("archs=arm64", "archs=x86_64");
        assert!(arch.detail.contains("x86_64") && arch.detail.contains("arm64"));
        let framework = tweak("framework=yes", "framework=no");
        assert!(
            framework
                .detail
                .contains("Chromium Embedded Framework.framework")
        );
        let wrapper = tweak("wrapper=yes", "wrapper=no");
        assert!(wrapper.detail.contains("wrapper"));
        let writable = tweak("writable=no", "writable=yes");
        assert!(writable.detail.contains("writable"));
        for c in [
            wrong_archive,
            sha,
            version,
            arch,
            framework,
            wrapper,
            writable,
        ] {
            assert!(!c.status.is_ok());
            assert!(c.hint.as_deref().unwrap().contains("NATIVE=true"));
        }
        assert_eq!(
            judge("cef", "missing", "no archive.json").status,
            Status::Missing
        );
    }

    #[test]
    fn archive_corruption_is_a_failure_not_a_directory_check() {
        let c = judge(
            "cef-archive",
            "ok",
            "sha1=1111111111111111111111111111111111111111 | file=/x",
        );
        assert_eq!(c.status, Status::Failed);
        assert!(c.detail.contains("corrupt") && c.detail.contains(SHA1));
        let tree = judge("cef-tree", "fail", "./include/cef_version.h: FAILED");
        assert_eq!(tree.status, Status::Failed);
        assert!(tree.detail.contains("cef_version.h"));
        assert_eq!(
            judge("cef-tree", "missing", "no manifest").status,
            Status::Missing
        );
    }

    #[test]
    fn platform_xcode_and_signing_state_are_each_named() {
        let linux = judge("platform", "ok", "os=Linux | arch=aarch64 | translated=0");
        assert!(linux.detail.contains("Linux/aarch64"));
        let rosetta = judge("platform", "ok", "os=Darwin | arch=arm64 | translated=1");
        assert!(rosetta.detail.contains("Rosetta"));
        let lic = judge(
            "xcode",
            "ok",
            "path=/p | kind=xcode | version=x | license=unaccepted | sdk=/sdk",
        );
        assert!(lic.detail.contains("license") && lic.hint.unwrap().contains("-license accept"));
        let nosdk = judge(
            "xcode",
            "ok",
            "path=/p | kind=clt | version= | license=ok | sdk=",
        );
        assert!(nosdk.detail.contains("no macOS SDK"));
        assert!(
            judge(
                "xcode",
                "missing",
                "xcode-select: error: no developer tools were found"
            )
            .hint
            .unwrap()
            .contains("xcode-select -s")
        );
        assert!(
            judge("ninja", "missing", "not found on PATH")
                .hint
                .unwrap()
                .contains("BREW_PACKAGES")
        );
        assert!(
            judge("rust-target", "missing", "x")
                .hint
                .unwrap()
                .contains("rustup target add aarch64-apple-darwin")
        );

        let signed = judge("signing", "ok", "identities=2");
        assert_eq!(signed.status, Status::Failed);
        assert!(signed.detail.contains("2 code-signing identities are"));
        let mut release = tauri();
        release.allow_signing_identities = true;
        let ok = judge_tauri_cef(&release, "native:signing", "ok", "identities=2", &ctx());
        assert_eq!(ok.status, Status::Ok);
    }

    // ---- macos-gui ----

    fn gui() -> MacosGuiReq {
        toml::from_str("").unwrap()
    }
    fn jg(id: &str, status: &str, detail: &str) -> Check {
        judge_gui(&gui(), &format!("gui:{id}"), status, detail, &ctx())
    }

    #[test]
    fn the_gui_helper_revision_tracks_the_embedded_source() {
        let r = gui();
        r.validate("p").unwrap();
        assert_eq!(r.revision, helper_revision());
        assert_eq!(r.revision.len(), 64);
        assert_eq!(
            r.job_env(),
            [(
                "HOME_RUNNER_INPUT".to_string(),
                "/usr/local/libexec/home-runner/hr-input".to_string()
            )]
        );
        // The install script computes the same revision with `shasum -a 256` of the file.
        assert!(HELPER_SOURCE.contains("kern.hv_vmm_present"));
    }

    #[test]
    fn session_failures_are_distinct() {
        let ok = jg(
            "session",
            "ok",
            "user=admin | console=admin | windowserver=yes | vm=1",
        );
        assert_eq!(ok.status, Status::Ok);
        let none = jg(
            "session",
            "ok",
            "user=admin | console=root | windowserver=yes | vm=1",
        );
        assert!(
            none.detail.starts_with("no session: nobody is logged in"),
            "{}",
            none.detail
        );
        assert!(none.hint.unwrap().contains("automatic login"));
        let other = jg(
            "session",
            "ok",
            "user=admin | console=bob | windowserver=yes | vm=1",
        );
        assert!(other.detail.starts_with("wrong session"));
        let ws = jg(
            "session",
            "ok",
            "user=admin | console=admin | windowserver=no | vm=1",
        );
        assert!(ws.detail.contains("WindowServer"));
        let host = jg(
            "session",
            "ok",
            "user=zack | console=zack | windowserver=yes | vm=0",
        );
        assert!(host.detail.contains("not a virtual machine"));
        assert!(host.hint.unwrap().contains("never on the host"));
    }

    #[test]
    fn helper_revision_signature_and_ownership_are_checked() {
        let rev = helper_revision();
        let good = format!(
            "version=rev={rev} | identifier=ai.home-runner.input | signature=ok | writable=no"
        );
        assert_eq!(jg("helper", "ok", &good).status, Status::Ok);
        let old = jg("helper", "ok", &good.replace(&rev, &"a".repeat(64)));
        assert_eq!(old.status, Status::Wrong);
        assert!(old.detail.contains("aaaaaaaaaaaa") && old.detail.contains(&rev[..12]));
        assert!(old.hint.unwrap().contains("GUI=true"));
        assert_eq!(
            jg(
                "helper",
                "ok",
                &good.replace("signature=ok", "signature=invalid")
            )
            .status,
            Status::Failed
        );
        assert!(
            jg("helper", "ok", &good.replace("writable=no", "writable=yes"))
                .detail
                .contains("writable")
        );
        assert_eq!(
            jg("helper", "missing", "no executable at /x").status,
            Status::Missing
        );
    }

    #[test]
    fn revoked_or_unreachable_permission_is_reported_apart_from_session_and_focus() {
        let ok = jg(
            "access",
            "ok",
            r#"{"session":true,"trusted":true,"post":true,"vm":true,"detail":"x"}"#,
        );
        assert_eq!(ok.status, Status::Ok);
        let revoked = jg(
            "access",
            "ok",
            r#"{"session":true,"trusted":false,"post":false,"vm":true}"#,
        );
        assert!(
            revoked.detail.starts_with("permission not granted"),
            "{}",
            revoked.detail
        );
        assert!(revoked.hint.unwrap().contains("revoked"));
        let session = jg(
            "access",
            "ok",
            r#"{"session":false,"trusted":false,"post":false,"vm":true,"detail":"no sudo"}"#,
        );
        assert!(session.detail.contains("cannot reach the login session"));
        assert!(session.hint.unwrap().contains("passwordless sudo"));
        assert_eq!(jg("access", "ok", "garbage").status, Status::Failed);
        assert_eq!(jg("access", "fail", "exit 1: boom").status, Status::Failed);
    }

    #[test]
    fn each_stage_of_the_input_proof_fails_with_its_own_message() {
        let proof = |stage: &str| {
            jg(
                "input",
                "fail",
                &format!(r#"{{"ok":false,"stage":"{stage}","detail":"d"}}"#),
            )
        };
        assert_eq!(
            jg("input", "ok", r#"{"ok":true,"stage":"done","detail":"keyboard, click and drag reached the test app"}"#).status,
            Status::Ok
        );
        assert!(proof("session").detail.starts_with("no login session"));
        assert!(
            proof("permission")
                .detail
                .starts_with("posting permission is missing")
        );
        let focus = proof("focus");
        assert!(
            focus
                .detail
                .starts_with("the test app could not be activated")
        );
        assert!(focus.hint.unwrap().contains("holds the focus"));
        let delivery = proof("delivery");
        assert!(
            delivery
                .detail
                .starts_with("the event was posted but never reached")
        );
        assert!(proof("vm").detail.contains("not a virtual machine"));
        // A "pass" the helper did not actually report is not a pass.
        assert_eq!(
            jg(
                "input",
                "ok",
                r#"{"ok":false,"stage":"focus","detail":"d"}"#
            )
            .status,
            Status::Failed
        );
        assert_eq!(jg("input", "fail", "exit 2").status, Status::Failed);
    }

    // ---- the scripts, run against fake tools ----

    #[cfg(unix)]
    mod shell {
        use super::*;
        use crate::backend::ProbeOutput;
        use crate::prereq::probe::{evaluate, script};
        use crate::prereq::{ProfileConfig, Requirement};
        use crate::testutil::temp_dir;
        use std::os::unix::fs::PermissionsExt;
        use std::path::{Path, PathBuf};
        use std::process::Command;

        fn root() -> bool {
            unsafe { libc::geteuid() == 0 }
        }

        struct Fixture {
            dir: PathBuf,
        }

        impl Fixture {
            fn new() -> Self {
                let dir = temp_dir("native");
                std::fs::create_dir_all(dir.join("bin")).unwrap();
                Self { dir }
            }
            fn tool(&self, name: &str, body: &str) {
                let path = self.dir.join("bin").join(name);
                std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            fn file(&self, rel: &str, text: &str) -> PathBuf {
                let path = self.dir.join(rel);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, text).unwrap();
                path
            }
            /// Makes `rel` read-only for everyone, as the image build does.
            fn lock(&self, rel: &str) {
                fn walk(path: &Path) {
                    if path.is_dir() {
                        for entry in std::fs::read_dir(path).unwrap() {
                            walk(&entry.unwrap().path());
                        }
                        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o555))
                            .unwrap();
                    } else {
                        let exec =
                            std::fs::metadata(path).unwrap().permissions().mode() & 0o111 != 0;
                        let mode = if exec { 0o555 } else { 0o444 };
                        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                            .unwrap();
                    }
                }
                walk(&self.dir.join(rel));
            }
            fn run(&self, p: &ProfileConfig) -> Vec<Check> {
                // The scripts start many short processes; one at a time keeps the rest of the
                // suite's own process-heavy tests from being starved.
                static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());
                let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
                let script = script(p, "macos-arm64");
                let out = Command::new("sh")
                    .args(["-c", &script])
                    .env_clear()
                    .env(
                        "PATH",
                        format!("{}:/usr/bin:/bin", self.dir.join("bin").display()),
                    )
                    .env("HOME", &self.dir)
                    .env("d", self.dir.join("state"))
                    .output()
                    .unwrap();
                evaluate(
                    p,
                    &ctx(),
                    &ProbeOutput {
                        stdout: String::from_utf8_lossy(&out.stdout).into(),
                        stderr: String::from_utf8_lossy(&out.stderr).into(),
                        code: out.status.code(),
                    },
                )
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = Command::new("chmod")
                    .args(["-R", "u+w"])
                    .arg(&self.dir)
                    .status();
            }
        }

        fn profile_for(fx: &Fixture, extra: &str) -> ProfileConfig {
            let tools = fx.dir.join("native/tools");
            let cef = fx.dir.join("native/cef/dist");
            let p: ProfileConfig = toml::from_str(&format!(
                "name = \"p\"\n[[require]]\nkind = \"tauri-cef\"\ntools_dir = {:?}\ncef_path = {:?}\n\
                 tauri_revision = \"{REV}\"\ncli_version = \"3.0.0-alpha.5\"\ncef_archive = \"{ARCHIVE}\"\n\
                 cef_sha1 = \"{SHA1}\"\n{extra}",
                tools.display().to_string(),
                cef.display().to_string()
            ))
            .unwrap();
            p.validate().unwrap();
            p
        }

        /// A worker that satisfies every native check. The archive is a real file and the profile
        /// pins its real SHA-1, so the checksum and manifest checks are real hashes.
        fn provisioned() -> (Fixture, ProfileConfig) {
            let fx = Fixture::new();
            fx.tool(
                "uname",
                "case \"$1\" in -s) echo Darwin ;; -m) echo arm64 ;; esac",
            );
            fx.tool("sysctl", "echo 0");
            fx.tool(
                "xcode-select",
                "echo /Applications/Xcode.app/Contents/Developer",
            );
            fx.tool("xcodebuild", "case \"$1\" in -version) echo 'Xcode 26.0'; echo 'Build version 17A' ;; esac; exit 0");
            fx.tool("xcrun", "echo /sdk/MacOSX.sdk");
            fx.tool("clang", "echo 'Apple clang version 21'");
            fx.tool("swift", "echo 'Apple Swift version 6'");
            fx.tool("cmake", "echo 'cmake version 4.0.0'");
            fx.tool("ninja", "echo 1.12.1");
            fx.tool("rustfmt", "echo 'rustfmt 1.8.0'");
            fx.tool(
                "cargo",
                "[ \"$1\" = clippy ] && echo 'clippy 0.1.99'; exit 0",
            );
            fx.tool("rustc", &format!("echo {}/sysroot", fx.dir.display()));
            std::fs::create_dir_all(fx.dir.join("sysroot/lib/rustlib/aarch64-apple-darwin"))
                .unwrap();
            fx.tool("lipo", "echo arm64");
            fx.tool("security", "echo '     0 valid identities found'");

            let cli = fx.file(
                "native/tools/bin/cargo-tauri",
                "#!/bin/sh\necho 'tauri-cli 3.0.0-alpha.5'\n",
            );
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
            fx.file("native/tools/tauri-revision", &format!("{REV}\n"));

            let archive = fx.file(&format!("native/cef/{ARCHIVE}"), "archive bytes");
            let sha1 = hash(&archive, "1");
            let dist = "native/cef/dist";
            fx.file(&format!("{dist}/archive.json"), &format!("{{\n  \"type\": \"minimal\",\n  \"name\": \"{ARCHIVE}\",\n  \"sha1\": \"{sha1}\"\n}}\n"));
            fx.file(
                &format!("{dist}/include/cef_version.h"),
                "#define CEF_VERSION \"154.0.34+g14c5a08+chromium-154.0.8037.98\"\n",
            );
            fx.file(
                &format!(
                    "{dist}/Chromium Embedded Framework.framework/Chromium Embedded Framework"
                ),
                "framework",
            );
            fx.file(&format!("{dist}/CMakeLists.txt"), "x");
            fx.file(&format!("{dist}/libcef_dll/CMakeLists.txt"), "x");
            let listing = Command::new("sh")
                .current_dir(fx.dir.join(dist))
                .args(["-c", "find . -type f -print0 | LC_ALL=C sort -z | xargs -0 shasum -a 256 2>/dev/null || find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum"])
                .output()
                .unwrap();
            fx.file(
                &format!("native/cef/{ARCHIVE}.tree.sha256"),
                &String::from_utf8(listing.stdout).unwrap(),
            );

            let mut p = profile_for(&fx, "");
            if let Requirement::TauriCef(r) = &mut p.requirements[0] {
                r.cef_sha1 = sha1;
            }
            fx.lock("native");
            (fx, p)
        }

        fn hash(path: &Path, bits: &str) -> String {
            let out = Command::new("sh")
                .args([
                    "-c",
                    &format!("shasum -a {bits} \"$0\" 2>/dev/null || sha{bits}sum \"$0\""),
                    path.to_str().unwrap(),
                ])
                .output()
                .unwrap();
            String::from_utf8(out.stdout)
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .to_string()
        }

        fn find<'a>(checks: &'a [Check], id: &str) -> &'a Check {
            checks
                .iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("no {id}: {checks:#?}"))
        }

        #[test]
        fn a_provisioned_worker_is_ready_end_to_end() {
            if root() {
                return;
            }
            let (fx, p) = provisioned();
            let checks = fx.run(&p);
            let bad: Vec<_> = checks.iter().filter(|c| !c.status.is_ok()).collect();
            assert!(bad.is_empty(), "{bad:#?}");
            assert!(
                checks
                    .iter()
                    .any(|c| c.id == "native:cef-tree" && c.status == Status::Ok)
            );
            assert_eq!(
                checks
                    .iter()
                    .filter(|c| c.id.starts_with("native:"))
                    .count(),
                TAURI_CEF_IDS.len()
            );
        }

        #[test]
        fn a_changed_file_in_the_extracted_tree_is_caught_by_the_manifest() {
            if root() {
                return;
            }
            let (fx, p) = provisioned();
            let header = fx.dir.join("native/cef/dist/include/cef_version.h");
            std::fs::set_permissions(
                header.parent().unwrap(),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            std::fs::set_permissions(&header, std::fs::Permissions::from_mode(0o644)).unwrap();
            std::fs::write(
                &header,
                "#define CEF_VERSION \"154.0.34+g14c5a08+chromium-154.0.8037.98\"\n// tampered\n",
            )
            .unwrap();
            let checks = fx.run(&p);
            let tree = find(&checks, "native:cef-tree");
            assert_eq!(tree.status, Status::Failed, "{tree:?}");
            assert!(tree.detail.contains("cef_version.h"), "{}", tree.detail);
        }

        #[test]
        fn a_corrupt_archive_missing_tool_and_wrong_revision_each_block_readiness() {
            if root() {
                return;
            }
            let (fx, p) = provisioned();
            // Corrupt the archive: its hash no longer matches the pin.
            let archive = fx.dir.join(format!("native/cef/{ARCHIVE}"));
            std::fs::set_permissions(
                archive.parent().unwrap(),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            std::fs::set_permissions(&archive, std::fs::Permissions::from_mode(0o644)).unwrap();
            std::fs::write(&archive, "truncated").unwrap();
            // Remove a build tool and record another revision.
            std::fs::remove_file(fx.dir.join("bin/ninja")).unwrap();
            let marker = fx.dir.join("native/tools/tauri-revision");
            std::fs::set_permissions(
                marker.parent().unwrap(),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o644)).unwrap();
            std::fs::write(&marker, "1111111111111111111111111111111111111111\n").unwrap();
            let checks = fx.run(&p);
            assert_eq!(find(&checks, "native:cef-archive").status, Status::Failed);
            assert_eq!(find(&checks, "native:ninja").status, Status::Missing);
            assert_eq!(find(&checks, "native:cli").status, Status::Wrong);
            // Everything else is still reported on its own.
            assert_eq!(find(&checks, "native:clang").status, Status::Ok);
        }

        #[test]
        fn a_writable_toolchain_or_a_signing_identity_is_refused() {
            if root() {
                return;
            }
            let (fx, p) = provisioned();
            std::fs::set_permissions(
                fx.dir.join("native/tools"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            fx.tool("security", "echo '     1 valid identities found'");
            let checks = fx.run(&p);
            assert!(find(&checks, "native:cli").detail.contains("writable"));
            assert!(
                find(&checks, "native:signing")
                    .detail
                    .contains("1 code-signing identity is")
            );
        }

        #[test]
        fn a_non_macos_worker_stops_at_the_platform() {
            let (fx, p) = {
                let (fx, p) = provisioned();
                fx.tool(
                    "uname",
                    "case \"$1\" in -s) echo Linux ;; -m) echo aarch64 ;; esac",
                );
                (fx, p)
            };
            let checks = fx.run(&p);
            assert_eq!(find(&checks, "native:platform").status, Status::Failed);
            assert_eq!(find(&checks, "native:cef").status, Status::Skipped);
        }

        /// Set HOME_RUNNER_DUMP to a directory to write the probe scripts there for manual runs
        /// in a real worker.
        #[test]
        fn dump_native_probe_scripts() {
            if let Ok(dir) = std::env::var("HOME_RUNNER_DUMP") {
                let p: ProfileConfig = toml::from_str(&format!(
                    "name = \"n\"\n[[require]]\nkind = \"tauri-cef\"\ntools_dir = \"/opt/home-runner/native/tools\"\n\
                     cef_path = \"/opt/home-runner/native/cef/dist\"\ntauri_revision = \"{REV}\"\n\
                     cli_version = \"3.0.0-alpha.5\"\ncef_archive = \"{ARCHIVE}\"\ncef_sha1 = \"{SHA1}\"\n\
                     [[require]]\nkind = \"macos-gui\"\n"
                ))
                .unwrap();
                std::fs::write(
                    Path::new(&dir).join("native-probe.sh"),
                    script(&p, "macos-arm64"),
                )
                .unwrap();
            }
        }

        fn gui_profile(fx: &Fixture) -> ProfileConfig {
            let p: ProfileConfig = toml::from_str(&format!(
                "name = \"g\"\n[[require]]\nkind = \"macos-gui\"\nhelper = {:?}\n",
                fx.dir.join("libexec/hr-input").display().to_string()
            ))
            .unwrap();
            p.validate().unwrap();
            p
        }

        fn gui_worker(
            rev: &str,
            access: &str,
            selftest: &str,
            selftest_rc: i32,
        ) -> (Fixture, ProfileConfig) {
            let fx = Fixture::new();
            fx.tool(
                "id",
                "case \"$1\" in -un) echo admin ;; -u) echo 501 ;; esac",
            );
            fx.tool("sysctl", "echo 1");
            fx.tool("stat", "echo admin");
            fx.tool("pgrep", "exit 0");
            fx.tool(
                "codesign",
                "case \"$1\" in -dv) echo 'Identifier=ai.home-runner.input' >&2 ;; esac; exit 0",
            );
            let helper = fx.file(
                "libexec/hr-input",
                &format!(
                    "#!/bin/sh\ncase \"$1\" in version) echo 'rev={rev}' ;; access) echo '{access}' ;; selftest) echo 'noise'; echo '{selftest}'; exit {selftest_rc} ;; esac\n"
                ),
            );
            std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
            fx.lock("libexec");
            let p = gui_profile(&fx);
            (fx, p)
        }

        const ACCESS_OK: &str = r#"{"session":true,"trusted":true,"post":true,"vm":true,"detail":"console session of admin"}"#;
        const INPUT_OK: &str = r#"{"ok":true,"stage":"done","detail":"keyboard, click and drag reached the test app (pid 7)"}"#;

        #[test]
        fn a_worker_whose_input_proof_passes_is_ready() {
            if root() {
                return;
            }
            let (fx, p) = gui_worker(&helper_revision(), ACCESS_OK, INPUT_OK, 0);
            let checks = fx.run(&p);
            let bad: Vec<_> = checks.iter().filter(|c| !c.status.is_ok()).collect();
            assert!(bad.is_empty(), "{bad:#?}");
            assert_eq!(
                checks.iter().filter(|c| c.id.starts_with("gui:")).count(),
                GUI_IDS.len()
            );
        }

        #[test]
        fn revoked_permission_blocks_qualification_instead_of_skipping() {
            if root() {
                return;
            }
            let revoked = r#"{"session":true,"trusted":false,"post":false,"vm":true}"#;
            let proof =
                r#"{"ok":false,"stage":"permission","detail":"Accessibility trusted=false"}"#;
            let (fx, p) = gui_worker(&helper_revision(), revoked, proof, 2);
            let checks = fx.run(&p);
            let access = find(&checks, "gui:access");
            assert_eq!(access.status, Status::Failed);
            assert!(access.detail.starts_with("permission not granted"));
            // The proof is not attempted as a pass, and nothing here counts as ready.
            assert!(checks.iter().any(|c| !c.status.is_ok()));
            assert_eq!(find(&checks, "gui:input").status, Status::Skipped);
        }

        #[test]
        fn a_helper_from_another_revision_stops_before_any_input_is_sent() {
            if root() {
                return;
            }
            let (fx, p) = gui_worker(&"b".repeat(64), ACCESS_OK, INPUT_OK, 0);
            let checks = fx.run(&p);
            assert_eq!(find(&checks, "gui:helper").status, Status::Wrong);
            assert_eq!(find(&checks, "gui:access").status, Status::Skipped);
            assert_eq!(find(&checks, "gui:input").status, Status::Skipped);
        }

        #[test]
        fn no_logged_in_session_is_reported_as_a_session_problem() {
            if root() {
                return;
            }
            let (fx, p) = gui_worker(&helper_revision(), ACCESS_OK, INPUT_OK, 0);
            fx.tool("stat", "echo root");
            let checks = fx.run(&p);
            let session = find(&checks, "gui:session");
            assert_eq!(session.status, Status::Failed);
            assert!(session.detail.contains("nobody is logged in"));
            assert_eq!(find(&checks, "gui:input").status, Status::Skipped);
        }

        #[test]
        fn a_focus_failure_keeps_the_pool_unready_with_the_failing_stage() {
            if root() {
                return;
            }
            let proof = r#"{"ok":false,"stage":"focus","detail":"target 7 did not come to the front (frontmost PID 3)"}"#;
            let (fx, p) = gui_worker(&helper_revision(), ACCESS_OK, proof, 2);
            let checks = fx.run(&p);
            let input = find(&checks, "gui:input");
            assert_eq!(input.status, Status::Failed);
            assert!(
                input
                    .detail
                    .starts_with("the test app could not be activated"),
                "{}",
                input.detail
            );
        }
    }
}
