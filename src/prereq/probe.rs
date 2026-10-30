//! The probe: a POSIX shell script that runs inside the worker as the runner user and reports one
//! `HRP<TAB>id<TAB>status<TAB>detail` line per check, and the code that judges those lines.
//!
//! The script only gathers facts (is it there, what did it print, did it start). Deciding whether a
//! version is right, classifying a failure and choosing the repair hint all happen in
//! [`evaluate`], which takes plain text, so every outcome is tested with canned output. The
//! script's own control flow is tested separately against fake `docker` and `node` executables.

use std::collections::HashMap;

use super::{InstallMode, PlaywrightReq, ProfileConfig, Requirement, native};
use crate::backend::ProbeOutput;

/// Output lines that carry results start with this and are tab-separated.
const TAG: &str = "HRP";

/// How a requirement stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Missing,
    /// Present, but not the pinned version.
    Wrong,
    Failed,
    /// Not run because an earlier step of the same requirement failed.
    Skipped,
}

impl Status {
    pub fn is_ok(self) -> bool {
        matches!(self, Status::Ok | Status::Skipped)
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub id: String,
    pub status: Status,
    pub detail: String,
    /// What to do about it.
    pub hint: Option<String>,
    /// Sanitised, bounded container and daemon state collected when a Docker step fails.
    pub diag: Vec<String>,
}

impl Check {
    pub(super) fn new(id: &str, status: Status, detail: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            status,
            detail: detail.into(),
            hint: None,
            diag: Vec::new(),
        }
    }

    pub(super) fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// What the evaluation needs to know about the pool the probe ran for.
pub struct EvalCtx<'a> {
    pub pool: &'a str,
    pub mode: InstallMode,
    /// `arm64` or `amd64`.
    pub image_arch: &'a str,
    /// `linux-arm64`, ...
    pub platform_key: &'a str,
}

pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

const PRELUDE: &str = r#"d="${d:-/tmp/home-runner}"
mkdir -p "$d" 2>/dev/null
hr_clip() { tr '\n\t\r' '   ' | cut -c1-"${HR_CLIP:-600}"; }
hr_emit() { printf 'HRP\t%s\t%s\t%s\n' "$1" "$2" "$(printf '%s' "$3" | hr_clip)"; }
hr_lines() { while IFS= read -r l; do hr_emit "$1" info "$l"; done; }
hr_emit identity ok "user=$(id -un 2>/dev/null) uid=$(id -u 2>/dev/null) os=$(uname -s) arch=$(uname -m)"
"#;

const COMMAND: &str = r#"if command -v @CMD@ >/dev/null 2>&1; then
  @RUN@
else
  hr_emit @ID@ missing "not found on PATH"
fi
"#;

const COMMAND_RUN_VERSION: &str = r#"out=$(@CMD@ @ARGS@ 2>&1); rc=$?
  if [ "$rc" -eq 0 ]; then hr_emit @ID@ ok "$(printf '%s\n' "$out" | head -n 1)"
  else hr_emit @ID@ fail "exit $rc: $out"; fi"#;

const APT: &str = r#"if command -v dpkg-query >/dev/null 2>&1; then
  for p in @PKGS@; do
    if dpkg-query -W -f='${Status}' "$p" 2>/dev/null | grep -q 'install ok installed'; then
      hr_emit "apt:$p" ok "$(dpkg-query -W -f='${Version}' "$p" 2>/dev/null)"
    else
      hr_emit "apt:$p" missing "package is not installed"
    fi
  done
else
  for p in @PKGS@; do hr_emit "apt:$p" fail "dpkg-query not found: apt requirements need a Debian or Ubuntu worker"; done
fi
"#;

/// Proves a container workload works, step by step, and stops at the first step that doesn't. The
/// PostgreSQL password is random per probe and scrubbed from everything reported. Readiness is
/// checked over TCP (`-h 127.0.0.1`): the image's first-boot server listens only on its socket, so
/// a socket check says "ready" before the real server exists.
const DOCKER: &str = r#"hr_pg="hr-probe-pg-$$"
HRPW="$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')"
hr_redact() { sed -E -e "s/$HRPW/***/g" -e 's/([Pp]assword|[Ss]ecret|[Tt]oken)([=: ]+)[^ ]+/\1\2***/g'; }
hr_diag() {
  {
    if docker inspect "$hr_pg" >/dev/null 2>&1; then
      docker inspect -f 'container state={{.State.Status}} exit={{.State.ExitCode}} oom={{.State.OOMKilled}} error={{.State.Error}}' "$hr_pg" 2>&1
      docker logs --tail 30 "$hr_pg" 2>&1
    fi
    if [ -f "$d/dockerd.log" ]; then tail -n 15 "$d/dockerd.log"; fi
  } | hr_redact | cut -c1-300 | head -n 40 | hr_lines docker:diag
}
hr_docker_clean() {
  for c in $(docker ps -aq --filter label=dev.home-runner.probe=1 2>/dev/null); do docker rm -f "$c" >/dev/null 2>&1; done
}
hr_pgping() {
  bash -c 'exec 3<>"/dev/tcp/$1/$2" || exit 3; printf "\000\000\000\010\004\322\026\057" >&3; read -r -n 1 -t 5 r <&3 || exit 4; printf %s "$r"' hr "$1" "$2"
}
hr_docker_probe() {
  command -v docker >/dev/null 2>&1 || { hr_emit docker:cli missing "docker CLI not found on PATH"; return; }
  hr_emit docker:cli ok "$(docker --version 2>&1)"
  out=$(docker info --format '{{.ServerVersion}} {{.OSType}}/{{.Architecture}}' 2>&1) || { hr_emit docker:daemon fail "$out"; hr_diag; return; }
  ep=@ENDPOINT@
  remote=""
  case "$DOCKER_HOST" in
    tcp://*) h=${DOCKER_HOST#tcp://}; h=${h%:*}; [ -n "$ep" ] || ep=$h; remote=" (remote daemon $DOCKER_HOST)" ;;
  esac
  [ -n "$ep" ] || ep=127.0.0.1
  bind=127.0.0.1; [ "$ep" = 127.0.0.1 ] || bind=0.0.0.0
  hr_emit docker:daemon ok "$out, published ports at $ep$remote"
  out=$(docker pull @IMAGE@ 2>&1) || { hr_emit docker:image fail "$out"; return; }
  ia=$(docker image inspect --format '{{.Os}}/{{.Architecture}}' @IMAGE@ 2>&1)
  case "$ia" in
    */@ARCH@) hr_emit docker:image ok "@IMAGERAW@ $ia" ;;
    *) hr_emit docker:image fail "unsupported platform: @IMAGERAW@ is $ia, but this worker is @ARCH@"; return ;;
  esac
  out=$(docker run -d --name "$hr_pg" --label dev.home-runner.probe=1 -e POSTGRES_PASSWORD="$HRPW" -p "$bind::5432" @IMAGE@ 2>&1) || { hr_emit docker:run fail "$out"; hr_diag; return; }
  hr_emit docker:run ok "container $(printf '%.12s' "$out")"
  out=$(docker exec "$hr_pg" sh -c 'echo hr-exec-ok' 2>&1); rc=$?
  case "$out" in *hr-exec-ok*) ;; *) hr_emit docker:exec fail "${out:-exit $rc}"; hr_diag; return ;; esac
  hr_emit docker:exec ok "ran a command inside the container"
  n=0; ready=0
  while [ "$n" -lt @ATTEMPTS@ ]; do
    n=$((n + 1))
    if docker exec "$hr_pg" pg_isready -h 127.0.0.1 -U postgres >/dev/null 2>&1; then ready=1; break; fi
    st=$(docker inspect -f '{{.State.Status}}' "$hr_pg" 2>/dev/null)
    [ "$st" = running ] || break
    sleep "${HR_PROBE_INTERVAL:-1}"
  done
  if [ "$ready" -ne 1 ]; then
    hr_emit docker:ready fail "PostgreSQL did not accept TCP connections after $n check(s); container is ${st:-gone}"
    hr_diag; return
  fi
  hr_emit docker:ready ok "pg_isready answered after $n check(s)"
  out=$(docker exec -e PGPASSWORD="$HRPW" "$hr_pg" psql -h 127.0.0.1 -U postgres -tAc 'select 1' 2>&1) || { hr_emit docker:query fail "$out"; hr_diag; return; }
  [ "$out" = 1 ] || { hr_emit docker:query fail "select 1 returned: $out"; hr_diag; return; }
  hr_emit docker:query ok "psql connected and ran a query"
  line=$(docker port "$hr_pg" 5432/tcp 2>&1 | head -n 1)
  port=${line##*:}
  case "$port" in ''|*[!0-9]*) hr_emit docker:port fail "no published port for 5432/tcp: $line"; hr_diag; return ;; esac
  out=$(hr_pgping "$ep" "$port" 2>&1); rc=$?
  case "$out" in
    N|S) hr_emit docker:port ok "PostgreSQL answered the protocol handshake at $ep:$port" ;;
    *) hr_emit docker:port fail "nothing answered at $ep:$port from the job environment (exit $rc): $out"; hr_diag; return ;;
  esac
  docker rm -f "$hr_pg" >/dev/null 2>&1
  left=$(docker ps -aq --filter label=dev.home-runner.probe=1 2>&1)
  if [ -z "$left" ]; then hr_emit docker:cleanup ok "the probe container was removed"
  else hr_emit docker:cleanup fail "left behind: $left"; fi
}
trap hr_docker_clean EXIT
hr_docker_probe
hr_docker_clean
"#;

const PLAYWRIGHT: &str = r#"hr_pw_js='const [rt, b] = process.argv.slice(1);
(async () => {
  const br = await require(rt)[b].launch({ headless: true });
  const page = await br.newPage();
  await page.setContent("<p id=x>x</p>");
  const display = await page.evaluate(() => getComputedStyle(document.getElementById("x")).display);
  const version = br.version();
  await br.close();
  console.log(b + " " + version + " display=" + display);
})().catch((e) => { console.error(String((e && e.message) || e).split("\n").slice(0, 6).join(" | ")); process.exit(1); });'
hr_pw_check() {
  v=$1; b=$2; id="playwright:$v:$b"
  rt="$root/runtime/$v/node_modules/playwright-core"
  [ -f "$rt/package.json" ] || { hr_emit "$id" missing "playwright-core $v is not installed in $root/runtime/$v"; return; }
  got=$(node -p 'require(process.argv[1]).version' "$rt/package.json" 2>&1)
  [ "$got" = "$v" ] || { hr_emit "$id" fail "$root/runtime/$v holds playwright-core $got, not $v"; return; }
  out=$(PLAYWRIGHT_BROWSERS_PATH="$bp" node -e "$hr_pw_js" "$rt" "$b" 2>&1) && hr_emit "$id" ok "$out" || hr_emit "$id" fail "$out"
}
hr_pw_probe() {
  root=@ROOT@
  case "$(uname -s)" in Linux) o=linux ;; Darwin) o=macos ;; *) o=$(uname -s) ;; esac
  case "$(uname -m)" in aarch64|arm64) a=arm64 ;; x86_64|amd64) a=x64 ;; *) a=$(uname -m) ;; esac
  if [ "$o-$a" != @PLATFORM@ ]; then
    hr_emit playwright:platform fail "this worker is $o-$a, but the pool is declared as @PLATFORM@"; return
  fi
  hr_emit playwright:platform ok "$o-$a"
  bp="$root/$o-$a/browsers"
  if ! command -v node >/dev/null 2>&1; then
@NONODE@
    return
  fi
@CHECKS@
}
hr_pw_probe
"#;

/// The script that checks `profile`. `platform_key` is what the pool declares (`linux-arm64`).
pub fn script(profile: &ProfileConfig, platform_key: &str) -> String {
    let mut out = String::from(PRELUDE);
    for req in &profile.requirements {
        match req {
            Requirement::Command(c) => {
                let id = format!("command:{}", c.name);
                let run = if c.version.is_some() {
                    COMMAND_RUN_VERSION
                        .replace("@ARGS@", &join_quoted(&c.version_args))
                        .replace("@CMD@", &sh_quote(&c.name))
                        .replace("@ID@", &sh_quote(&id))
                } else {
                    format!(
                        "hr_emit {} ok \"$(command -v {})\"",
                        sh_quote(&id),
                        sh_quote(&c.name)
                    )
                };
                out.push_str(
                    &COMMAND
                        .replace("@RUN@", &run)
                        .replace("@CMD@", &sh_quote(&c.name))
                        .replace("@ID@", &sh_quote(&id)),
                );
            }
            Requirement::Apt(a) => {
                out.push_str(&APT.replace("@PKGS@", &join_quoted(&a.packages)));
            }
            Requirement::Docker(d) => {
                out.push_str(
                    &DOCKER
                        .replace(
                            "@ENDPOINT@",
                            &sh_quote(d.endpoint_host.as_deref().unwrap_or("")),
                        )
                        .replace("@IMAGERAW@", &d.image)
                        .replace("@IMAGE@", &sh_quote(&d.image))
                        .replace(
                            "@ARCH@",
                            if platform_key.ends_with("arm64") {
                                "arm64"
                            } else {
                                "amd64"
                            },
                        )
                        .replace("@ATTEMPTS@", &d.ready_attempts.to_string()),
                );
            }
            Requirement::Playwright(p) => out.push_str(&playwright(p, platform_key)),
            Requirement::TauriCef(r) => out.push_str(&native::tauri_cef_script(r)),
            Requirement::MacosGui(r) => out.push_str(&native::gui_script(r)),
            Requirement::MacosFinder(_) => out.push_str(&native::finder_script()),
        }
    }
    out
}

fn playwright(p: &PlaywrightReq, platform_key: &str) -> String {
    let mut no_node = String::new();
    let mut checks = String::new();
    for v in &p.versions {
        for b in &p.browsers {
            no_node.push_str(&format!(
                "    hr_emit {} missing \"node is not installed\"\n",
                sh_quote(&format!("playwright:{v}:{b}"))
            ));
            checks.push_str(&format!("  hr_pw_check {} {}\n", sh_quote(v), sh_quote(b)));
        }
    }
    PLAYWRIGHT
        .replace("@ROOT@", &sh_quote(p.root.trim_end_matches('/')))
        .replace("@PLATFORM@", &sh_quote(platform_key))
        .replace("@NONODE@", no_node.trim_end())
        .replace("@CHECKS@", checks.trim_end())
}

fn join_quoted(items: &[String]) -> String {
    items
        .iter()
        .map(|s| sh_quote(s))
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, PartialEq)]
struct Fact {
    status: String,
    detail: String,
}

/// The probe's result lines. Anything else the worker printed (shell noise, dockerd chatter) is
/// ignored; `docker:diag` lines are collected in order.
fn parse(stdout: &str) -> (HashMap<String, Fact>, Vec<String>, bool) {
    let mut facts = HashMap::new();
    let mut diag = Vec::new();
    let mut any = false;
    for line in stdout.lines() {
        let mut parts = line.trim_end_matches('\r').splitn(4, '\t');
        if parts.next() != Some(TAG) {
            continue;
        }
        let (Some(id), Some(status)) = (parts.next(), parts.next()) else {
            continue;
        };
        any = true;
        let detail = parts.next().unwrap_or("").trim().to_string();
        if id == "docker:diag" {
            diag.push(detail);
        } else {
            facts.insert(
                id.to_string(),
                Fact {
                    status: status.to_string(),
                    detail,
                },
            );
        }
    }
    (facts, diag, any)
}

/// Judges a probe's output against the profile. One [`Check`] per expected result, in order.
pub fn evaluate(profile: &ProfileConfig, ctx: &EvalCtx, out: &ProbeOutput) -> Vec<Check> {
    let (facts, diag, any) = parse(&out.stdout);
    if !any {
        let mut why = match out.code {
            Some(code) => format!("the probe exited with status {code} without reporting anything"),
            None => "the probe was killed without reporting anything".to_string(),
        };
        let tail = tail(&out.stderr, 600);
        if !tail.is_empty() {
            why.push_str(&format!("; it printed: {tail}"));
        }
        return vec![Check::new("probe", Status::Failed, why).hint(
            "check that the worker image has a POSIX shell and that the pool's image and \
             credentials work (`home-runner doctor`)",
        )];
    }

    let mut checks = Vec::new();
    checks.push(match facts.get("identity") {
        Some(f) if f.detail.contains("uid=0 ") || f.detail.ends_with("uid=0") => Check::new(
            "runner-user",
            Status::Failed,
            format!("the worker runs jobs as root ({})", f.detail),
        )
        .hint(
            "the runner refuses to run as root; use an image whose default user is the runner \
             user (the stock actions-runner image uses `runner`)",
        ),
        Some(f) => Check::new("runner-user", Status::Ok, f.detail.clone()),
        None => Check::new(
            "runner-user",
            Status::Failed,
            "the probe did not identify its user",
        ),
    });

    for req in &profile.requirements {
        let mut broken = false;
        let first = checks.len();
        for id in req.check_ids() {
            // Native steps build on each other, so one that follows a failure is not judged on
            // whatever the script happened to print.
            let skip = broken && matches!(req, Requirement::TauriCef(_) | Requirement::MacosGui(_));
            let check = match facts.get(&id) {
                Some(_) if skip => {
                    Check::new(&id, Status::Skipped, "not run: an earlier step failed")
                }
                Some(fact) => judge(req, &id, fact, ctx),
                None if broken => {
                    Check::new(&id, Status::Skipped, "not run: an earlier step failed")
                }
                None => Check::new(
                    &id,
                    Status::Failed,
                    "the probe stopped before reporting this".to_string(),
                ),
            };
            let stops = match req {
                Requirement::Docker(_) => true,
                Requirement::Playwright(_) => id == "playwright:platform",
                Requirement::TauriCef(_) => id == "native:platform",
                // Each step builds on the one before: no point posting input without a session.
                Requirement::MacosGui(_) | Requirement::MacosFinder(_) => true,
                _ => false,
            };
            broken |= stops && !check.status.is_ok();
            checks.push(check);
        }
        // Container and daemon state belongs to the first Docker step that failed.
        if matches!(req, Requirement::Docker(_))
            && let Some(failed) = checks[first..].iter_mut().find(|c| !c.status.is_ok())
        {
            failed.diag = diag.clone();
        }
    }
    checks
}

fn judge(req: &Requirement, id: &str, fact: &Fact, ctx: &EvalCtx) -> Check {
    let detail = fact.detail.clone();
    match (req, fact.status.as_str()) {
        (Requirement::Command(c), "ok") => match &c.version {
            Some(want) if !version_matches(want, &detail) => Check::new(
                id,
                Status::Wrong,
                format!(
                    "found {}, need {want}",
                    first_version(&detail).unwrap_or_else(|| detail.clone())
                ),
            )
            .hint(repair(req, ctx)),
            _ => Check::new(id, Status::Ok, detail),
        },
        (Requirement::Docker(d), "ok") if id == "docker:image" => {
            let arch = detail.rsplit('/').next().unwrap_or("").trim();
            if arch != ctx.image_arch {
                Check::new(
                    id,
                    Status::Failed,
                    format!(
                        "unsupported platform: {} is {detail}, but this worker is {}",
                        d.image, ctx.image_arch
                    ),
                )
                .hint(
                    "use an image with a native build for the worker's architecture; running an \
                     emulated database image is not supported",
                )
            } else {
                Check::new(id, Status::Ok, detail)
            }
        }
        (Requirement::TauriCef(r), status) => native::judge_tauri_cef(r, id, status, &detail, ctx),
        (Requirement::MacosGui(r), status) => native::judge_gui(r, id, status, &detail, ctx),
        (Requirement::MacosFinder(_), status) => native::judge_finder(id, status, &detail, ctx),
        (_, "ok") => Check::new(id, Status::Ok, detail),
        (_, "missing") => Check::new(id, Status::Missing, detail).hint(repair(req, ctx)),
        (Requirement::Docker(d), _) => explain_docker(id, d.image.as_str(), &detail, ctx),
        (Requirement::Playwright(_), _) => explain_playwright(req, id, &detail, ctx),
        _ => Check::new(id, Status::Failed, detail).hint(repair(req, ctx)),
    }
}

/// Turns Docker's raw errors into what is actually wrong.
fn explain_docker(id: &str, image: &str, detail: &str, ctx: &EvalCtx) -> Check {
    let lower = detail.to_ascii_lowercase();
    let unsupported = lower.contains("no matching manifest")
        || lower.contains("does not provide the specified platform")
        || lower.contains("exec format error")
        || lower.contains("platform (linux/") && lower.contains("does not match");
    if unsupported {
        return Check::new(
            id,
            Status::Failed,
            format!(
                "unsupported platform: {image} cannot run on a linux/{} worker ({detail})",
                ctx.image_arch
            ),
        )
        .hint("use an image with a native build for this architecture");
    }
    let hint = if lower.contains("permission denied")
        && (id == "docker:daemon" || lower.contains("docker.sock"))
    {
        "the runner user cannot use the Docker socket: add it to the docker group, or run the \
         daemon the way `docker_in_job = true` does"
    } else if lower.contains("cannot connect to the docker daemon")
        || lower.contains("is the docker daemon running")
    {
        "no daemon is answering: set `docker_in_job = true` on the pool and use an image with \
         dockerd (images/ubuntu-latest), or start the daemon the worker is configured to use"
    } else if id == "docker:exec" {
        "the daemon refused to run a command in the container (exec denied): check seccomp/AppArmor \
         policy and that the daemon was started with the capabilities `docker_in_job` grants"
    } else if id == "docker:port" {
        "published ports bind on the daemon's host. If the daemon is remote or nested, set \
         `endpoint_host` on the docker requirement to the address job processes can reach it at"
    } else if id == "docker:ready" {
        "the database never came up: see the container state and log lines below"
    } else if lower.contains("toomanyrequests") || lower.contains("rate limit") {
        "the registry is rate limiting pulls: pre-pull the image into the worker image or log in"
    } else {
        "see the daemon and container details below"
    };
    Check::new(id, Status::Failed, detail).hint(hint)
}

fn explain_playwright(req: &Requirement, id: &str, detail: &str, ctx: &EvalCtx) -> Check {
    let lower = detail.to_ascii_lowercase();
    let (status, what) = if lower.contains("executable doesn't exist")
        || lower.contains("executable does not exist")
    {
        (
            Status::Missing,
            "the browser revision this Playwright version needs is not installed",
        )
    } else if lower.contains("error while loading shared libraries")
        || lower.contains("missing dependencies")
        || lower.contains("host system is missing")
    {
        (
            Status::Failed,
            "operating-system libraries the browser needs are missing",
        )
    } else if lower.contains("exec format error")
        || lower.contains("cannot execute binary")
        || lower.contains("bad cpu type")
    {
        (
            Status::Failed,
            "the browser is built for a different architecture than this worker",
        )
    } else if lower.contains("eacces") || lower.contains("permission denied") {
        (
            Status::Failed,
            "the runner user cannot read or execute the browser",
        )
    } else {
        (
            Status::Failed,
            "the browser did not launch, open a page and close cleanly",
        )
    };
    let hint = match status {
        Status::Missing => repair(req, ctx),
        _ => match ctx.mode {
            InstallMode::Image => format!(
                "rebuild the image with `home-runner prereq install --pool {}` (installs the \
                 browsers with their libraries)",
                ctx.pool
            ),
            InstallMode::Preinstalled => repair(req, ctx),
        },
    };
    Check::new(id, status, format!("{what}: {detail}")).hint(hint)
}

/// What the operator does to fix a requirement.
fn repair(req: &Requirement, ctx: &EvalCtx) -> String {
    let install = format!("home-runner prereq install --pool {}", ctx.pool);
    match (req, ctx.mode) {
        (Requirement::Command(c), InstallMode::Image) if c.install.is_some() => {
            format!("run `{install}` (runs this requirement's install script as root in a derived image)")
        }
        (Requirement::Command(c), InstallMode::Image) => format!(
            "add an `install` script to the {} requirement (or put it in the base image), then run `{install}`",
            c.name
        ),
        (Requirement::Command(c), InstallMode::Preinstalled) => match &c.version {
            Some(v) => format!("install {} {v} for the runner user on the {} worker image or machine", c.name, ctx.pool),
            None => format!("install {} for the runner user on the {} worker image or machine", c.name, ctx.pool),
        },
        (Requirement::Apt(a), InstallMode::Image) => {
            let _ = a;
            format!("run `{install}`")
        }
        (Requirement::Apt(a), InstallMode::Preinstalled) => format!(
            "install them on the worker image or machine: apt-get install -y {}",
            a.packages.join(" ")
        ),
        (Requirement::Playwright(_), InstallMode::Image) => format!("run `{install}`"),
        (Requirement::Playwright(p), InstallMode::Preinstalled) => format!(
            "install playwright-core@<version> under {root}/runtime/<version> and run its \
             `cli.js install --with-deps <browser>` with PLAYWRIGHT_BROWSERS_PATH={root}/{key}/browsers (README: Playwright)",
            root = p.root.trim_end_matches('/'),
            key = ctx.platform_key
        ),
        (
            Requirement::TauriCef(_) | Requirement::MacosGui(_) | Requirement::MacosFinder(_),
            _,
        ) => {
            "provision the golden image with images/macos-latest/build.sh (README: Native prerequisites)"
                .into()
        }
        (Requirement::Docker(_), _) => {
            "use an image with dockerd and set `docker_in_job = true` on the pool (README: Docker inside jobs)".into()
        }
    }
}

/// The first dotted number in `text` (`gh version 2.63.0 (…)` → `2.63.0`).
pub fn first_version(text: &str) -> Option<String> {
    let line = text.lines().next().unwrap_or("");
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit()
            && (i == 0 || !(bytes[i - 1].is_ascii_digit() || bytes[i - 1] == b'.'))
        {
            let mut j = i;
            while j < bytes.len()
                && (bytes[j].is_ascii_digit()
                    || (bytes[j] == b'.' && bytes.get(j + 1).is_some_and(u8::is_ascii_digit)))
            {
                j += 1;
            }
            return Some(line[i..j].to_string());
        }
        i += 1;
    }
    None
}

/// `26` matches 26.1.0, `1.7.12` matches only 1.7.12 and 1.7.12.x, never 1.7.120.
pub fn version_matches(want: &str, output: &str) -> bool {
    let want = want.trim().trim_start_matches('v');
    let numeric = want.chars().all(|c| c.is_ascii_digit() || c == '.');
    if !numeric {
        return output.lines().next().unwrap_or("").contains(want);
    }
    first_version(output)
        .is_some_and(|found| found == want || found.starts_with(&format!("{want}.")))
}

fn tail(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let skip = flat.chars().count().saturating_sub(max);
    flat.chars().skip(skip).collect()
}

#[cfg(test)]
mod tests {
    use super::super::tests::{WITH_TAP, profile};
    use super::*;

    fn ctx(mode: InstallMode) -> EvalCtx<'static> {
        EvalCtx {
            pool: "linux",
            mode,
            image_arch: "arm64",
            platform_key: "linux-arm64",
        }
    }

    fn output(lines: &[&str]) -> ProbeOutput {
        ProbeOutput {
            stdout: lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
            stderr: String::new(),
            code: Some(0),
        }
    }

    const ID: &str = "HRP\tidentity\tok\tuser=runner uid=1001 os=Linux arch=aarch64";

    fn find<'a>(checks: &'a [Check], id: &str) -> &'a Check {
        checks
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("no {id}: {checks:#?}"))
    }

    fn ready(checks: &[Check]) -> bool {
        checks.iter().all(|c| c.status.is_ok())
    }

    #[test]
    fn matches_versions_on_component_boundaries() {
        assert!(version_matches("26", "v26.1.0"));
        assert!(version_matches("1.7.12", "1.7.12"));
        assert!(version_matches("1.7", "1.7.12"));
        assert!(!version_matches("1.7.1", "1.7.12"));
        assert!(!version_matches("2", "26.0.0"));
        assert!(version_matches("10.33.1", "10.33.1"));
        assert!(version_matches(
            "1.99.0",
            "rustc 1.99.0 (abcdef 2026-10-01)"
        ));
        assert!(version_matches(
            "2.63",
            "gh version 2.63.0 (2025-01-01)\nhttps://github.com/cli/cli"
        ));
        assert!(version_matches("nightly", "rustc 1.99.0-nightly (abc)"));
        assert!(!version_matches("1.7.12", "no version here"));
        assert_eq!(
            first_version("Docker version 29.7.2, build a7d").as_deref(),
            Some("29.7.2")
        );
    }

    #[test]
    fn everything_reported_ok_is_ready() {
        let p = profile(
            "name = \"p\"\n[[require]]\nkind = \"command\"\nname = \"node\"\nversion = \"26\"\n\
             [[require]]\nkind = \"apt\"\npackages = [\"pkg-config\"]",
        );
        let out = output(&[
            "some shell noise",
            ID,
            "HRP\tcommand:node\tok\tv26.1.0",
            "HRP\tapt:pkg-config\tok\t1.8.1-2build1",
        ]);
        let checks = evaluate(&p, &ctx(InstallMode::Image), &out);
        assert!(ready(&checks), "{checks:#?}");
        assert_eq!(checks.len(), 3);
    }

    #[test]
    fn a_missing_tool_is_not_ready_and_says_how_to_fix_it() {
        let p = profile(WITH_TAP);
        let out = output(&[
            ID,
            "HRP\tcommand:node\tok\tv26.1.0",
            "HRP\tcommand:actionlint\tmissing\tnot found on PATH",
            "HRP\tapt:libasound2-dev\tmissing\tpackage is not installed",
            "HRP\tapt:pkg-config\tok\t1.8.1",
        ]);
        let checks = evaluate(&p, &ctx(InstallMode::Image), &out);
        assert!(!ready(&checks));
        let lint = find(&checks, "command:actionlint");
        assert_eq!(lint.status, Status::Missing);
        assert!(
            lint.hint
                .as_deref()
                .unwrap()
                .contains("home-runner prereq install --pool linux")
        );
        // Requirements that report nothing aren't assumed healthy.
        assert_eq!(find(&checks, "playwright:platform").status, Status::Failed);
        assert_eq!(find(&checks, "docker:cli").status, Status::Failed);
    }

    #[test]
    fn a_wrong_version_is_reported_with_both_versions() {
        let p = profile(WITH_TAP);
        let out = output(&[
            ID,
            "HRP\tcommand:node\tok\tv24.9.0",
            "HRP\tcommand:actionlint\tok\t1.7.12",
        ]);
        let checks = evaluate(&p, &ctx(InstallMode::Preinstalled), &out);
        let node = find(&checks, "command:node");
        assert_eq!(node.status, Status::Wrong);
        assert_eq!(node.detail, "found 24.9.0, need 26");
        assert!(node.hint.as_deref().unwrap().contains("install node 26"));
        assert_eq!(find(&checks, "command:actionlint").status, Status::Ok);
    }

    #[test]
    fn a_probe_that_printed_nothing_reports_why() {
        let p = profile(WITH_TAP);
        let out = ProbeOutput {
            stdout: "unrelated".into(),
            stderr: "/bin/sh: 1: exec: not found\n".into(),
            code: Some(127),
        };
        let checks = evaluate(&p, &ctx(InstallMode::Image), &out);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Failed);
        assert!(checks[0].detail.contains("status 127"));
        assert!(checks[0].detail.contains("exec: not found"));
        let killed = ProbeOutput {
            stdout: String::new(),
            stderr: String::new(),
            code: None,
        };
        assert!(
            evaluate(&p, &ctx(InstallMode::Image), &killed)[0]
                .detail
                .contains("killed")
        );
    }

    #[test]
    fn running_as_root_fails_the_worker() {
        let p = profile("name = \"p\"\n[[require]]\nkind = \"command\"\nname = \"git\"");
        let out = output(&[
            "HRP\tidentity\tok\tuser=root uid=0 os=Linux arch=aarch64",
            "HRP\tcommand:git\tok\t/usr/bin/git",
        ]);
        let checks = evaluate(&p, &ctx(InstallMode::Image), &out);
        assert_eq!(find(&checks, "runner-user").status, Status::Failed);
        assert!(!ready(&checks));
    }

    const DOCKER_OK: &[&str] = &[
        "HRP\tdocker:cli\tok\tDocker version 29.7.2",
        "HRP\tdocker:daemon\tok\t29.7.2 linux/aarch64, published ports at 127.0.0.1",
        "HRP\tdocker:image\tok\tpostgres:17-alpine linux/arm64",
        "HRP\tdocker:run\tok\tcontainer abcdef012345",
        "HRP\tdocker:exec\tok\tran a command inside the container",
        "HRP\tdocker:ready\tok\tpg_isready answered after 3 check(s)",
        "HRP\tdocker:query\tok\tpsql connected and ran a query",
        "HRP\tdocker:port\tok\tPostgreSQL answered the protocol handshake at 127.0.0.1:49153",
        "HRP\tdocker:cleanup\tok\tthe probe container was removed",
    ];

    fn docker_profile() -> ProfileConfig {
        profile("name = \"p\"\n[[require]]\nkind = \"docker\"")
    }

    fn docker_out(lines: &[&str]) -> ProbeOutput {
        let mut all = vec![ID];
        all.extend_from_slice(lines);
        output(&all)
    }

    #[test]
    fn a_working_docker_workload_is_ready() {
        let checks = evaluate(
            &docker_profile(),
            &ctx(InstallMode::Image),
            &docker_out(DOCKER_OK),
        );
        assert!(ready(&checks), "{checks:#?}");
    }

    #[test]
    fn a_failed_start_keeps_the_original_error_and_adds_bounded_state() {
        let out = docker_out(&[
            DOCKER_OK[0],
            DOCKER_OK[1],
            DOCKER_OK[2],
            "HRP\tdocker:run\tfail\tdocker: Error response from daemon: failed to create task: OCI runtime create failed",
            "HRP\tdocker:diag\tinfo\tcontainer state=created exit=0 oom=false error=failed to create task",
            "HRP\tdocker:diag\tinfo\tdockerd: level=error msg=\"cannot mount overlay\"",
        ]);
        let checks = evaluate(&docker_profile(), &ctx(InstallMode::Image), &out);
        let run = find(&checks, "docker:run");
        assert_eq!(run.status, Status::Failed);
        assert!(
            run.detail.contains("failed to create task"),
            "the daemon's own error survives"
        );
        assert_eq!(run.diag.len(), 2);
        // Later steps are not blamed; they simply did not run.
        assert_eq!(find(&checks, "docker:exec").status, Status::Skipped);
        assert_eq!(find(&checks, "docker:cleanup").status, Status::Skipped);
        assert!(!ready(&checks));
    }

    #[test]
    fn an_unhealthy_database_points_at_the_container_state() {
        let out = docker_out(&[
            DOCKER_OK[0],
            DOCKER_OK[1],
            DOCKER_OK[2],
            DOCKER_OK[3],
            DOCKER_OK[4],
            "HRP\tdocker:ready\tfail\tPostgreSQL did not accept TCP connections after 60 check(s); container is running",
            "HRP\tdocker:diag\tinfo\tcontainer state=running exit=0 oom=false error=",
            "HRP\tdocker:diag\tinfo\tFATAL: could not write lock file",
        ]);
        let checks = evaluate(&docker_profile(), &ctx(InstallMode::Image), &out);
        let ready_check = find(&checks, "docker:ready");
        assert_eq!(ready_check.status, Status::Failed);
        assert!(
            ready_check
                .hint
                .as_deref()
                .unwrap()
                .contains("container state")
        );
        assert_eq!(ready_check.diag.len(), 2);
    }

    #[test]
    fn exec_denied_and_unreachable_ports_are_told_apart() {
        let denied = docker_out(&[
            DOCKER_OK[0],
            DOCKER_OK[1],
            DOCKER_OK[2],
            DOCKER_OK[3],
            "HRP\tdocker:exec\tfail\tOCI runtime exec failed: exec failed: permission denied: unknown",
        ]);
        let checks = evaluate(&docker_profile(), &ctx(InstallMode::Image), &denied);
        let exec = find(&checks, "docker:exec");
        assert_eq!(exec.status, Status::Failed);
        assert!(exec.hint.as_deref().unwrap().contains("exec denied"));

        let mut lines: Vec<&str> = DOCKER_OK[..7].to_vec();
        lines.push("HRP\tdocker:port\tfail\tnothing answered at 127.0.0.1:49153 from the job environment (exit 3): Connection refused");
        let checks = evaluate(
            &docker_profile(),
            &ctx(InstallMode::Image),
            &docker_out(&lines),
        );
        let port = find(&checks, "docker:port");
        assert!(port.hint.as_deref().unwrap().contains("endpoint_host"));
        assert!(port.detail.contains("Connection refused"));
    }

    #[test]
    fn unsupported_platforms_are_called_out_by_name() {
        let pull = docker_out(&[
            DOCKER_OK[0],
            DOCKER_OK[1],
            "HRP\tdocker:image\tfail\tno matching manifest for linux/arm64/v8 in the manifest list entries",
        ]);
        let checks = evaluate(&docker_profile(), &ctx(InstallMode::Image), &pull);
        let image = find(&checks, "docker:image");
        assert!(
            image.detail.starts_with(
                "unsupported platform: postgres:17-alpine cannot run on a linux/arm64 worker"
            ),
            "{}",
            image.detail
        );

        let wrong = docker_out(&[
            DOCKER_OK[0],
            DOCKER_OK[1],
            "HRP\tdocker:image\tok\tpostgres:17-alpine linux/amd64",
        ]);
        let checks = evaluate(&docker_profile(), &ctx(InstallMode::Image), &wrong);
        let image = find(&checks, "docker:image");
        assert_eq!(image.status, Status::Failed);
        assert!(image.detail.contains("unsupported platform"));

        let exec_format = docker_out(&[
            DOCKER_OK[0],
            DOCKER_OK[1],
            DOCKER_OK[2],
            "HRP\tdocker:run\tfail\tstandard_init_linux.go: exec user process caused: exec format error",
        ]);
        let checks = evaluate(&docker_profile(), &ctx(InstallMode::Image), &exec_format);
        assert!(
            find(&checks, "docker:run")
                .detail
                .contains("unsupported platform")
        );
    }

    #[test]
    fn a_daemon_that_is_down_or_off_limits_gets_a_specific_hint() {
        let down = docker_out(&[
            DOCKER_OK[0],
            "HRP\tdocker:daemon\tfail\tCannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?",
        ]);
        let checks = evaluate(&docker_profile(), &ctx(InstallMode::Image), &down);
        assert!(
            find(&checks, "docker:daemon")
                .hint
                .as_deref()
                .unwrap()
                .contains("docker_in_job")
        );
        let denied = docker_out(&[
            DOCKER_OK[0],
            "HRP\tdocker:daemon\tfail\tpermission denied while trying to connect to the Docker daemon socket at unix:///var/run/docker.sock",
        ]);
        let checks = evaluate(&docker_profile(), &ctx(InstallMode::Image), &denied);
        assert!(
            find(&checks, "docker:daemon")
                .hint
                .as_deref()
                .unwrap()
                .contains("cannot use the Docker socket")
        );
    }

    fn pw_profile() -> ProfileConfig {
        profile(
            "name = \"p\"\n[[require]]\nkind = \"playwright\"\nversions = [\"1.56.1\", \"1.49.0\"]",
        )
    }

    const PW_PLATFORM: &str = "HRP\tplaywright:platform\tok\tlinux-arm64";

    #[test]
    fn independent_playwright_versions_are_each_launched() {
        let out = output(&[
            ID,
            PW_PLATFORM,
            "HRP\tplaywright:1.56.1:chromium\tok\tchromium 141.0.7390.37 display=block",
            "HRP\tplaywright:1.49.0:chromium\tok\tchromium 131.0.6778.33 display=block",
        ]);
        let checks = evaluate(&pw_profile(), &ctx(InstallMode::Image), &out);
        assert!(ready(&checks), "{checks:#?}");
    }

    #[test]
    fn a_missing_revision_fails_only_the_version_that_needs_it() {
        let out = output(&[
            ID,
            PW_PLATFORM,
            "HRP\tplaywright:1.56.1:chromium\tok\tchromium 141 display=block",
            "HRP\tplaywright:1.49.0:chromium\tfail\tbrowserType.launch: Executable doesn't exist at /opt/home-runner/playwright/linux-arm64/browsers/chromium_headless_shell-1148/chrome-linux/headless_shell",
        ]);
        let checks = evaluate(&pw_profile(), &ctx(InstallMode::Image), &out);
        assert_eq!(
            find(&checks, "playwright:1.56.1:chromium").status,
            Status::Ok
        );
        let old = find(&checks, "playwright:1.49.0:chromium");
        assert_eq!(old.status, Status::Missing);
        assert!(old.detail.contains("chromium_headless_shell-1148"));
        assert!(
            old.hint
                .as_deref()
                .unwrap()
                .contains("prereq install --pool linux")
        );
    }

    #[test]
    fn launch_failures_are_classified() {
        let case = |detail: &str| {
            let out = output(&[
                ID,
                PW_PLATFORM,
                &format!("HRP\tplaywright:1.56.1:chromium\tfail\t{detail}"),
                "HRP\tplaywright:1.49.0:chromium\tok\tchromium 131 display=block",
            ]);
            let checks = evaluate(&pw_profile(), &ctx(InstallMode::Preinstalled), &out);
            find(&checks, "playwright:1.56.1:chromium").clone()
        };
        let libs = case(
            "browserType.launch: /x/headless_shell: error while loading shared libraries: libnss3.so: cannot open shared object file",
        );
        assert!(libs.detail.contains("libraries"), "{}", libs.detail);
        let arch = case("spawn /x/headless_shell Exec format error");
        assert!(arch.detail.contains("different architecture"));
        let perm = case("spawn /x/headless_shell EACCES");
        assert!(perm.detail.contains("cannot read or execute"));
        let other = case("Target page, context or browser has been closed");
        assert!(other.detail.contains("did not launch"));
        assert!(
            libs.hint
                .as_deref()
                .unwrap()
                .contains("PLAYWRIGHT_BROWSERS_PATH")
        );
    }

    #[test]
    fn the_wrong_architecture_stops_the_browser_checks() {
        let out = output(&[
            ID,
            "HRP\tplaywright:platform\tfail\tthis worker is linux-x64, but the pool is declared as linux-arm64",
        ]);
        let checks = evaluate(&pw_profile(), &ctx(InstallMode::Image), &out);
        assert_eq!(find(&checks, "playwright:platform").status, Status::Failed);
        assert_eq!(
            find(&checks, "playwright:1.56.1:chromium").status,
            Status::Skipped
        );
        assert!(!ready(&checks));
    }

    // Script generation and the shell itself ------------------------------------------------

    /// Set HOME_RUNNER_DUMP to a directory to write the probe script there for manual testing.
    #[test]
    fn dump_probe_script() {
        if let Ok(dir) = std::env::var("HOME_RUNNER_DUMP") {
            let text = script(&profile(WITH_TAP), "linux-arm64");
            std::fs::write(std::path::Path::new(&dir).join("probe.sh"), text).unwrap();
        }
    }

    #[test]
    fn scripts_quote_everything_they_interpolate() {
        let p = profile(
            "name = \"p\"\n[[require]]\nkind = \"command\"\nname = \"node\"\nversion = \"26\"\n\
             version_args = [\"-v\", \"it's\"]",
        );
        let script = script(&p, "linux-arm64");
        assert!(script.contains("'node' '-v' 'it'\\''s'"), "{script}");
    }

    #[cfg(unix)]
    mod shell {
        use super::*;
        use crate::testutil::temp_dir;
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::os::unix::fs::PermissionsExt;
        use std::path::{Path, PathBuf};
        use std::process::Command;

        fn host_platform_key() -> String {
            let os = if cfg!(target_os = "macos") {
                "macos"
            } else {
                "linux"
            };
            let arch = if cfg!(target_arch = "aarch64") {
                "arm64"
            } else {
                "x64"
            };
            format!("{os}-{arch}")
        }

        struct Fixture {
            dir: PathBuf,
        }

        impl Fixture {
            fn new() -> Self {
                let dir = temp_dir("probe");
                std::fs::create_dir_all(dir.join("bin")).unwrap();
                Self { dir }
            }

            fn tool(&self, name: &str, body: &str) {
                let path = self.dir.join("bin").join(name);
                std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }

            /// Runs the probe for `profile` with only the fake tools (and the system basics) on PATH.
            fn run(&self, p: &ProfileConfig, env: &[(&str, &str)]) -> Vec<Check> {
                let script = script(p, &host_platform_key());
                let path = format!("{}:/usr/bin:/bin", self.dir.join("bin").display());
                let out = Command::new("sh")
                    .args(["-c", &script])
                    .env_clear()
                    .env("PATH", path)
                    .env("HOME", &self.dir)
                    .env("d", self.dir.join("state"))
                    .env("FAKE_DIR", &self.dir)
                    .env("HR_PROBE_INTERVAL", "0")
                    .envs(env.iter().copied())
                    .output()
                    .unwrap();
                let probe = ProbeOutput {
                    stdout: String::from_utf8_lossy(&out.stdout).into(),
                    stderr: String::from_utf8_lossy(&out.stderr).into(),
                    code: out.status.code(),
                };
                evaluate(
                    p,
                    &EvalCtx {
                        pool: "linux",
                        mode: InstallMode::Image,
                        image_arch: if cfg!(target_arch = "aarch64") {
                            "arm64"
                        } else {
                            "amd64"
                        },
                        platform_key: &host_platform_key(),
                    },
                    &probe,
                )
            }
        }

        #[test]
        fn commands_apt_packages_and_versions_are_checked_in_the_worker() {
            let fx = Fixture::new();
            fx.tool("node", "echo v26.1.0");
            fx.tool("pnpm", "echo 9.15.0");
            fx.tool("broken", "echo boom >&2; exit 3");
            fx.tool(
                "dpkg-query",
                r#"case "$*" in
  *'${Status}'*) [ "$3" = pkg-config ] && echo "install ok installed" || echo "unknown ok not-installed" ;;
  *'${Version}'*) echo 1.8.1 ;;
esac"#,
            );
            let p = profile(
                "name = \"p\"\n\
                 [[require]]\nkind = \"command\"\nname = \"node\"\nversion = \"26\"\n\
                 [[require]]\nkind = \"command\"\nname = \"pnpm\"\nversion = \"10.33.1\"\n\
                 [[require]]\nkind = \"command\"\nname = \"broken\"\nversion = \"1\"\n\
                 [[require]]\nkind = \"command\"\nname = \"gone\"\nversion = \"1\"\n\
                 [[require]]\nkind = \"apt\"\npackages = [\"pkg-config\", \"libasound2-dev\"]",
            );
            let checks = fx.run(&p, &[]);
            assert_eq!(find(&checks, "command:node").status, Status::Ok);
            let pnpm = find(&checks, "command:pnpm");
            assert_eq!(
                (pnpm.status, pnpm.detail.as_str()),
                (Status::Wrong, "found 9.15.0, need 10.33.1")
            );
            let broken = find(&checks, "command:broken");
            assert_eq!(broken.status, Status::Failed);
            assert!(broken.detail.contains("exit 3: boom"), "{}", broken.detail);
            assert_eq!(find(&checks, "command:gone").status, Status::Missing);
            assert_eq!(find(&checks, "apt:pkg-config").status, Status::Ok);
            assert_eq!(find(&checks, "apt:libasound2-dev").status, Status::Missing);
        }

        const FAKE_DOCKER: &str = r#"
state="$FAKE_DIR/container"
case "$1" in
  --version) echo "Docker version 99.0.0, build fake" ;;
  info)
    if [ "$SCENARIO" = daemon-down ]; then
      echo "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?" >&2; exit 1
    fi
    echo "99.0.0 linux/aarch64" ;;
  pull)
    if [ "$SCENARIO" = wrong-platform ]; then echo "no matching manifest for linux/arm64/v8 in the manifest list entries" >&2; exit 1; fi
    echo pulled ;;
  image) echo "linux/$FAKE_ARCH" ;;
  run)
    if [ "$SCENARIO" = failed-start ]; then echo "docker: Error response from daemon: failed to create task for container" >&2; echo created > "$state"; exit 125; fi
    for a in "$@"; do case "$a" in POSTGRES_PASSWORD=*) echo "${a#POSTGRES_PASSWORD=}" > "$FAKE_DIR/password" ;; esac; done
    echo running > "$state"; echo 0123456789abcdef0123 ;;
  exec)
    case "$*" in
      *pg_isready*) if [ "$SCENARIO" = unhealthy ]; then exit 2; fi; exit 0 ;;
      *psql*) echo 1 ;;
      *) if [ "$SCENARIO" = exec-denied ]; then echo "OCI runtime exec failed: exec failed: permission denied" >&2; exit 126; fi; echo hr-exec-ok ;;
    esac ;;
  inspect)
    [ -f "$state" ] || { echo "Error: No such object" >&2; exit 1; }
    case "$*" in
      *'{{.State.Status}}'*'ExitCode'*) echo "container state=$(cat "$state") exit=1 oom=false error=" ;;
      *'{{.State.Status}}'*) cat "$state" ;;
    esac ;;
  logs)
    echo "initdb: using POSTGRES_PASSWORD=$(cat "$FAKE_DIR/password" 2>/dev/null) for the superuser"
    echo "LOG: database system is ready to accept connections password=hunter2"
    i=0; while [ $i -lt 100 ]; do echo "log line $i"; i=$((i+1)); done ;;
  port) echo "127.0.0.1:$FAKE_PORT" ;;
  rm) rm -f "$state" ;;
  ps) [ -f "$state" ] && echo 0123456789ab ;;
  *) echo "fake docker: unhandled $*" >&2; exit 99 ;;
esac
"#;

        fn arch() -> &'static str {
            if cfg!(target_arch = "aarch64") {
                "arm64"
            } else {
                "amd64"
            }
        }

        /// A listener that answers PostgreSQL's SSLRequest with `N`, like a server without TLS.
        fn fake_postgres() -> (u16, std::thread::JoinHandle<()>) {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let handle = std::thread::spawn(move || {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut request = [0u8; 8];
                    if stream.read_exact(&mut request).is_ok() && request[4..] == [4, 210, 22, 47] {
                        let _ = stream.write_all(b"N");
                    }
                }
            });
            (port, handle)
        }

        /// A port nothing listens on.
        fn closed_port() -> u16 {
            TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port()
        }

        fn run_docker(scenario: &str, port: u16) -> (Fixture, Vec<Check>) {
            let fx = Fixture::new();
            fx.tool("docker", FAKE_DOCKER);
            let checks = fx.run(
                &docker_profile(),
                &[
                    ("SCENARIO", scenario),
                    ("FAKE_PORT", &port.to_string()),
                    ("FAKE_ARCH", arch()),
                ],
            );
            (fx, checks)
        }

        #[test]
        fn a_healthy_docker_workload_passes_every_step_and_cleans_up() {
            let (port, server) = fake_postgres();
            let (fx, checks) = run_docker("ok", port);
            server.join().unwrap();
            for id in DOCKER_IDS {
                assert_eq!(find(&checks, id).status, Status::Ok, "{id}: {checks:#?}");
            }
            assert!(
                !fx.dir.join("container").exists(),
                "the container must be gone"
            );
        }

        const DOCKER_IDS: [&str; 9] = [
            "docker:cli",
            "docker:daemon",
            "docker:image",
            "docker:run",
            "docker:exec",
            "docker:ready",
            "docker:query",
            "docker:port",
            "docker:cleanup",
        ];

        #[test]
        fn a_daemon_that_is_not_running_stops_at_the_daemon() {
            let (_fx, checks) = run_docker("daemon-down", closed_port());
            let daemon = find(&checks, "docker:daemon");
            assert_eq!(daemon.status, Status::Failed);
            assert!(
                daemon
                    .detail
                    .contains("Cannot connect to the Docker daemon")
            );
            assert_eq!(find(&checks, "docker:run").status, Status::Skipped);
        }

        #[test]
        fn a_container_that_fails_to_start_reports_state_and_cleans_up() {
            let (fx, checks) = run_docker("failed-start", closed_port());
            let run = find(&checks, "docker:run");
            assert_eq!(run.status, Status::Failed);
            assert!(run.detail.contains("failed to create task"));
            assert!(
                run.diag
                    .iter()
                    .any(|l| l.contains("container state=created")),
                "{:?}",
                run.diag
            );
            assert!(
                !fx.dir.join("container").exists(),
                "the half-created container must be removed"
            );
        }

        #[test]
        fn an_unhealthy_database_is_reported_with_scrubbed_and_bounded_logs() {
            let (fx, checks) = run_docker("unhealthy", closed_port());
            let ready_check = find(&checks, "docker:ready");
            assert_eq!(ready_check.status, Status::Failed);
            assert!(
                ready_check.detail.contains("60 check(s)"),
                "{}",
                ready_check.detail
            );
            assert!(!ready_check.diag.is_empty());
            // Bounded: the fake logs 100 lines whatever it is asked for.
            assert!(ready_check.diag.len() <= 40, "{}", ready_check.diag.len());
            // Scrubbed: neither the random fixture password nor a password=… pair survives.
            let password = std::fs::read_to_string(fx.dir.join("password")).unwrap();
            let all = ready_check.diag.join("\n");
            assert!(
                !password.trim().is_empty() && !all.contains(password.trim()),
                "{all}"
            );
            assert!(!all.contains("hunter2"), "{all}");
            assert!(all.contains("***"));
            assert!(!fx.dir.join("container").exists());
        }

        #[test]
        fn exec_denied_is_reported_at_the_exec_step() {
            let (fx, checks) = run_docker("exec-denied", closed_port());
            let exec = find(&checks, "docker:exec");
            assert_eq!(exec.status, Status::Failed);
            assert!(exec.detail.contains("permission denied"));
            assert_eq!(find(&checks, "docker:ready").status, Status::Skipped);
            assert!(!fx.dir.join("container").exists());
        }

        #[test]
        fn a_published_port_the_job_cannot_reach_is_reported_after_the_database_is_up() {
            let (fx, checks) = run_docker("ok", closed_port());
            assert_eq!(find(&checks, "docker:query").status, Status::Ok);
            let port = find(&checks, "docker:port");
            assert_eq!(port.status, Status::Failed);
            assert!(port.detail.contains("127.0.0.1:"), "{}", port.detail);
            assert!(port.hint.as_deref().unwrap().contains("endpoint_host"));
            assert!(!fx.dir.join("container").exists());
        }

        #[test]
        fn an_image_for_the_wrong_platform_is_reported_explicitly() {
            let (_fx, checks) = run_docker("wrong-platform", closed_port());
            let image = find(&checks, "docker:image");
            assert!(
                image.detail.contains("unsupported platform"),
                "{}",
                image.detail
            );
            let other = if arch() == "arm64" { "amd64" } else { "arm64" };
            let fx = Fixture::new();
            fx.tool("docker", FAKE_DOCKER);
            let checks = fx.run(
                &docker_profile(),
                &[("SCENARIO", "ok"), ("FAKE_PORT", "1"), ("FAKE_ARCH", other)],
            );
            assert!(
                find(&checks, "docker:image")
                    .detail
                    .contains("unsupported platform")
            );
            assert_eq!(find(&checks, "docker:run").status, Status::Skipped);
        }

        #[test]
        fn a_remote_daemon_is_probed_at_its_own_address() {
            let fx = Fixture::new();
            fx.tool("docker", FAKE_DOCKER);
            let checks = fx.run(
                &docker_profile(),
                &[
                    ("SCENARIO", "ok"),
                    ("FAKE_PORT", &closed_port().to_string()),
                    ("FAKE_ARCH", arch()),
                    ("DOCKER_HOST", "tcp://127.0.0.1:2375"),
                ],
            );
            let daemon = find(&checks, "docker:daemon");
            assert!(
                daemon.detail.contains("remote daemon tcp://127.0.0.1:2375"),
                "{}",
                daemon.detail
            );
            assert!(daemon.detail.contains("published ports at 127.0.0.1"));
        }

        fn pw_fixture(version: &str, scenario: &str) -> (Fixture, Vec<Check>) {
            let fx = Fixture::new();
            let root = fx.dir.join("pw");
            let p = profile(&format!(
                "name = \"p\"\n[[require]]\nkind = \"playwright\"\nversions = [\"{version}\", \"1.49.0\"]\nroot = \"{}\"",
                root.display()
            ));
            // 1.56.x is installed; 1.49.0's runtime was never installed.
            let pkg = root
                .join("runtime")
                .join(version)
                .join("node_modules/playwright-core");
            std::fs::create_dir_all(&pkg).unwrap();
            std::fs::write(pkg.join("package.json"), "{}").unwrap();
            fx.tool(
                "node",
                r#"case "$1" in
  -p) echo "$FAKE_PW_VERSION" ;;
  -e) case "$SCENARIO" in
        ok) echo "chromium 141.0.7390.37 display=block" ;;
        missing) echo "browserType.launch: Executable doesn't exist at $PLAYWRIGHT_BROWSERS_PATH/chromium_headless_shell-1194/chrome-linux/headless_shell" >&2; exit 1 ;;
        libs) echo "headless_shell: error while loading shared libraries: libnss3.so" >&2; exit 1 ;;
      esac ;;
esac"#,
            );
            let checks = fx.run(&p, &[("SCENARIO", scenario), ("FAKE_PW_VERSION", version)]);
            (fx, checks)
        }

        #[test]
        fn playwright_is_launched_per_version_with_the_per_platform_cache() {
            let (_fx, checks) = pw_fixture("1.56.1", "ok");
            assert_eq!(find(&checks, "playwright:platform").status, Status::Ok);
            let new = find(&checks, "playwright:1.56.1:chromium");
            assert_eq!(new.status, Status::Ok, "{new:?}");
            assert!(new.detail.contains("display=block"));
            // The second version's runtime was never installed, and that is reported alone.
            let old = find(&checks, "playwright:1.49.0:chromium");
            assert_eq!(old.status, Status::Missing);
            assert!(
                old.detail
                    .contains("playwright-core 1.49.0 is not installed")
            );
        }

        #[test]
        fn a_missing_browser_revision_and_missing_libraries_are_distinguished() {
            let (fx, checks) = pw_fixture("1.56.1", "missing");
            let missing = find(&checks, "playwright:1.56.1:chromium");
            assert_eq!(missing.status, Status::Missing);
            // The probe handed the browser cache path for this platform to Playwright.
            let want = format!("{}/pw/{}/browsers", fx.dir.display(), host_platform_key());
            assert!(
                missing.detail.contains(&want),
                "{} vs {want}",
                missing.detail
            );
            let (_fx, checks) = pw_fixture("1.56.1", "libs");
            let libs = find(&checks, "playwright:1.56.1:chromium");
            assert_eq!(libs.status, Status::Failed);
            assert!(libs.detail.contains("libraries"));
        }

        #[test]
        fn a_version_other_than_the_pinned_one_does_not_count() {
            let (_fx, checks) = pw_fixture("1.56.1", "ok");
            // The runtime directory exists but reports a different version.
            let fx = Fixture::new();
            let root = fx.dir.join("pw");
            let pkg = root.join("runtime/1.56.1/node_modules/playwright-core");
            std::fs::create_dir_all(&pkg).unwrap();
            std::fs::write(pkg.join("package.json"), "{}").unwrap();
            fx.tool("node", "case \"$1\" in -p) echo 1.40.0 ;; esac");
            let p = profile(&format!(
                "name = \"p\"\n[[require]]\nkind = \"playwright\"\nversions = [\"1.56.1\"]\nroot = \"{}\"",
                root.display()
            ));
            let mismatch = fx.run(&p, &[]);
            let check = find(&mismatch, "playwright:1.56.1:chromium");
            assert_eq!(check.status, Status::Failed);
            assert!(
                check
                    .detail
                    .contains("holds playwright-core 1.40.0, not 1.56.1")
            );
            let _ = checks;
        }

        #[test]
        fn the_wrong_architecture_is_caught_before_any_browser_runs() {
            let fx = Fixture::new();
            fx.tool("node", "echo should-not-run");
            let p = profile(
                "name = \"p\"\n[[require]]\nkind = \"playwright\"\nversions = [\"1.56.1\"]",
            );
            let wrong = if host_platform_key().ends_with("arm64") {
                "linux-x64"
            } else {
                "linux-arm64"
            };
            let script = script(&p, wrong);
            let out = Command::new("sh")
                .args(["-c", &script])
                .env_clear()
                .env(
                    "PATH",
                    format!("{}:/usr/bin:/bin", fx.dir.join("bin").display()),
                )
                .env("d", fx.dir.join("state"))
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                stdout.contains("playwright:platform\tfail\tthis worker is"),
                "{stdout}"
            );
            assert!(!stdout.contains("should-not-run"));
        }

        #[test]
        fn the_script_is_valid_shell_for_every_requirement_kind() {
            let p = profile(WITH_TAP);
            let status = Command::new("sh")
                .args(["-n", "-c", &script(&p, "linux-arm64")])
                .status()
                .unwrap();
            assert!(status.success());
            let _ = Path::new("/");
        }
    }
}
