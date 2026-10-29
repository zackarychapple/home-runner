//! Job hooks the runner executes inside the job environment (ACTIONS_RUNNER_HOOK_JOB_STARTED /
//! _COMPLETED). They record who and what the job was for the ledger, sample CPU counters, and post a
//! "saved ~$X" notice on the run. They never fail the job.
//!
//! Hooks write `key=value` lines to `$HR_STATE_DIR/job.env`. The host backend reads that file
//! directly; VM and container backends print it after the runner exits, prefixed with
//! [`MARKER`], and the supervisor picks it out of the runner's output.
//!
//! CPU modes (`HR_CPU_MODE`): `procstat` (VM-wide /proc/stat jiffies), `usec` (the container's
//! cgroup), `exe` (`home-runner cpu-sample`: machine-wide counters on macOS and Windows).

use std::collections::HashMap;

pub const MARKER: &str = "::home-runner:: ";

const SH_COMMON: &str = r#"#!/bin/sh
# Installed by home-runner. Records job metadata for its savings ledger; never fails the job.
d="${HR_STATE_DIR:-/tmp/home-runner}"
mkdir -p "$d" 2>/dev/null
hr_cpu() {
  case "$HR_CPU_MODE" in
    usec) awk '/^usage_usec/ {printf "%.0f\n", $2}' /sys/fs/cgroup/cpu.stat 2>/dev/null ;;
    procstat) awk '/^cpu / {printf "%.0f %.0f\n", $2+$3+$4+$7+$8+$9, $2+$3+$4+$5+$6+$7+$8+$9}' /proc/stat 2>/dev/null ;;
    exe) "$HR_EXE" cpu-sample 2>/dev/null ;;
  esac
}
"#;

const SH_STARTED: &str = r#"{
  echo "repo=$GITHUB_REPOSITORY"
  echo "workflow=$GITHUB_WORKFLOW"
  echo "job_key=$GITHUB_JOB"
  echo "run_id=$GITHUB_RUN_ID"
  echo "run_attempt=$GITHUB_RUN_ATTEMPT"
  echo "run_number=$GITHUB_RUN_NUMBER"
  echo "actor=$GITHUB_ACTOR"
  echo "triggering_actor=$GITHUB_TRIGGERING_ACTOR"
  echo "event=$GITHUB_EVENT_NAME"
  echo "ref=$GITHUB_REF_NAME"
  echo "sha=$GITHUB_SHA"
  echo "server_url=$GITHUB_SERVER_URL"
  echo "ncpu=$(getconf _NPROCESSORS_ONLN 2>/dev/null)"
  echo "cpu_mode=${HR_CPU_MODE:-none}"
  echo "cpu_start=$(hr_cpu)"
  echo "hook_start=$(date +%s)"
} 2>/dev/null > "$d/job.env"
exit 0
"#;

const SH_COMPLETED: &str = r#"{
  echo "cpu_end=$(hr_cpu)"
  echo "hook_end=$(date +%s)"
} 2>/dev/null >> "$d/job.env"
if [ -n "$HR_GH_RATE" ]; then
  s=$(sed -n 's/^hook_start=//p' "$d/job.env" 2>/dev/null)
  awk -v s="${s:-0}" -v n="$(date +%s)" -v r="$HR_GH_RATE" -v sku="$HR_GH_SKU" -v host="$HR_HOST" 'BEGIN {
    if (s == 0) exit
    m = int((n - s + 59) / 60); if (m < 1) m = 1
    printf "::notice title=home-runner::Ran on %s. About $%.2f saved vs %s (%d billable min).\n", host, m * r, sku, m
  }' 2>/dev/null
fi
exit 0
"#;

const PS_COMMON: &str = r#"# Installed by home-runner. Records job metadata for its savings ledger; never fails the job.
$ErrorActionPreference = 'SilentlyContinue'
$d = if ($env:HR_STATE_DIR) { $env:HR_STATE_DIR } else { Join-Path $env:TEMP 'home-runner' }
New-Item -ItemType Directory -Force -Path $d | Out-Null
$f = Join-Path $d 'job.env'
$now = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
$cpu = if ($env:HR_EXE) { (& $env:HR_EXE cpu-sample) -join '' } else { '' }
"#;

const PS_STARTED: &str = r#"[IO.File]::WriteAllLines($f, [string[]]@(
  "repo=$env:GITHUB_REPOSITORY",
  "workflow=$env:GITHUB_WORKFLOW",
  "job_key=$env:GITHUB_JOB",
  "run_id=$env:GITHUB_RUN_ID",
  "run_attempt=$env:GITHUB_RUN_ATTEMPT",
  "run_number=$env:GITHUB_RUN_NUMBER",
  "actor=$env:GITHUB_ACTOR",
  "triggering_actor=$env:GITHUB_TRIGGERING_ACTOR",
  "event=$env:GITHUB_EVENT_NAME",
  "ref=$env:GITHUB_REF_NAME",
  "sha=$env:GITHUB_SHA",
  "server_url=$env:GITHUB_SERVER_URL",
  "ncpu=$env:NUMBER_OF_PROCESSORS",
  "cpu_mode=$env:HR_CPU_MODE",
  "cpu_start=$cpu",
  "hook_start=$now"
))
exit 0
"#;

const PS_COMPLETED: &str = r#"[IO.File]::AppendAllLines($f, [string[]]@("cpu_end=$cpu", "hook_end=$now"))
if ($env:HR_GH_RATE) {
  $s = (Get-Content $f | Where-Object { $_ -like 'hook_start=*' } | Select-Object -First 1) -replace '^hook_start=', ''
  if ($s) {
    $inv = [Globalization.CultureInfo]::InvariantCulture
    $m = [Math]::Max(1, [Math]::Ceiling(($now - [long]$s) / 60))
    $usd = $m * [double]::Parse($env:HR_GH_RATE, $inv)
    Write-Output ([string]::Format($inv, '::notice title=home-runner::Ran on {0}. About ${1:0.00} saved vs {2} ({3} billable min).', $env:HR_HOST, $usd, $env:HR_GH_SKU, $m))
  }
}
exit 0
"#;

pub fn sh_started() -> String {
    format!("{SH_COMMON}{SH_STARTED}")
}

pub fn sh_completed() -> String {
    format!("{SH_COMMON}{SH_COMPLETED}")
}

pub fn ps_started() -> String {
    format!("{PS_COMMON}{PS_STARTED}")
}

pub fn ps_completed() -> String {
    format!("{PS_COMMON}{PS_COMPLETED}")
}

/// A POSIX script fragment that installs both hooks under `$d` (which it expects to be set) and
/// exports the runner's hook variables. Used inside containers and VMs.
pub fn sh_install_fragment() -> String {
    format!(
        r#"mkdir -p "$d/hooks"
cat > "$d/hooks/started.sh" <<'HR_STARTED_EOF'
{}HR_STARTED_EOF
cat > "$d/hooks/completed.sh" <<'HR_COMPLETED_EOF'
{}HR_COMPLETED_EOF
chmod +x "$d/hooks/started.sh" "$d/hooks/completed.sh"
export HR_STATE_DIR="$d"
export ACTIONS_RUNNER_HOOK_JOB_STARTED="$d/hooks/started.sh"
export ACTIONS_RUNNER_HOOK_JOB_COMPLETED="$d/hooks/completed.sh"
"#,
        sh_started(),
        sh_completed()
    )
}

/// Prints the hook output with [`MARKER`] so the supervisor can read it from the runner's stdout.
pub const SH_EMIT: &str = r#"[ -f "$d/job.env" ] && sed 's/^/::home-runner:: /' "$d/job.env""#;

/// Parses `key=value` lines (tolerating a UTF-8 BOM and CRLF from PowerShell).
pub fn parse_env(text: &str) -> HashMap<String, String> {
    text.trim_start_matches('\u{feff}')
        .lines()
        .filter_map(|line| line.trim_end_matches('\r').split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

/// Cumulative (busy, total) CPU ticks for the whole machine, printed by `home-runner cpu-sample`.
/// Values wrap on macOS (32-bit counters); consumers take wrapping deltas.
pub fn cpu_ticks() -> Option<(u64, u64)> {
    #[cfg(target_os = "macos")]
    #[allow(deprecated)]
    unsafe {
        let mut info: libc::host_cpu_load_info = std::mem::zeroed();
        let mut count = (std::mem::size_of::<libc::host_cpu_load_info>()
            / std::mem::size_of::<libc::integer_t>())
            as libc::mach_msg_type_number_t;
        let rc = libc::host_statistics(
            libc::mach_host_self(),
            libc::HOST_CPU_LOAD_INFO,
            &mut info as *mut _ as libc::host_info_t,
            &mut count,
        );
        if rc != 0 {
            return None;
        }
        let [user, system, idle, nice] = info.cpu_ticks;
        let busy = user.wrapping_add(system).wrapping_add(nice);
        Some((busy as u64, busy.wrapping_add(idle) as u64))
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::GetSystemTimes;
        let mut idle: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        if GetSystemTimes(&mut idle, &mut kernel, &mut user) == 0 {
            return None;
        }
        let v = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
        // Kernel time includes idle time.
        let total = v(kernel) + v(user);
        Some((total - v(idle), total))
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let fields: Vec<u64> = stat
            .lines()
            .next()?
            .split_whitespace()
            .skip(1)
            .filter_map(|f| f.parse().ok())
            .collect();
        let get = |i: usize| fields.get(i).copied().unwrap_or(0);
        let busy = get(0) + get(1) + get(2) + get(5) + get(6) + get(7);
        Some((busy, busy + get(3) + get(4)))
    }
}

/// CPU used during the job, from the hook's start/end samples: (cpu_seconds, utilization 0..1).
pub fn cpu_usage(meta: &HashMap<String, String>, cpus_limit: u32) -> Option<(f64, f64)> {
    let start = meta.get("cpu_start")?;
    let end = meta.get("cpu_end")?;
    let wall =
        meta.get("hook_end")?.parse::<f64>().ok()? - meta.get("hook_start")?.parse::<f64>().ok()?;
    if wall <= 0.0 {
        return None;
    }
    let nums = |s: &str| -> Vec<u64> {
        s.split_whitespace()
            .filter_map(|n| n.parse().ok())
            .collect()
    };
    let (start, end) = (nums(start), nums(end));
    let delta = |i: usize| -> Option<f64> {
        let (a, b) = (*start.get(i)?, *end.get(i)?);
        // macOS counters are 32-bit and wrap.
        Some(if b >= a { b - a } else { b + (1u64 << 32) - a } as f64)
    };
    let ncpu: f64 = meta
        .get("ncpu")
        .and_then(|n| n.parse().ok())
        .unwrap_or(f64::from(cpus_limit));
    match meta.get("cpu_mode").map(String::as_str) {
        Some("usec") => {
            let cpu_secs = delta(0)? / 1e6;
            let capacity = wall * f64::from(cpus_limit).min(ncpu).max(1.0);
            Some((cpu_secs, (cpu_secs / capacity).min(1.0)))
        }
        Some("procstat" | "exe") => {
            let (busy, total) = (delta(0)?, delta(1)?);
            if total <= 0.0 {
                return None;
            }
            let utilization = (busy / total).min(1.0);
            Some((utilization * wall * ncpu, utilization))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn computes_cpu_from_ticks_and_cgroups() {
        let ticks = meta(&[
            ("cpu_mode", "procstat"),
            ("ncpu", "4"),
            ("cpu_start", "100 1000"),
            ("cpu_end", "300 1400"),
            ("hook_start", "1000"),
            ("hook_end", "1100"),
        ]);
        let (secs, util) = cpu_usage(&ticks, 4).unwrap();
        assert!((util - 0.5).abs() < 1e-9 && (secs - 200.0).abs() < 1e-9);

        let cgroup = meta(&[
            ("cpu_mode", "usec"),
            ("ncpu", "8"),
            ("cpu_start", "0"),
            ("cpu_end", "60000000"),
            ("hook_start", "0"),
            ("hook_end", "60"),
        ]);
        let (secs, util) = cpu_usage(&cgroup, 2).unwrap();
        assert!((secs - 60.0).abs() < 1e-9 && (util - 0.5).abs() < 1e-9);

        let wrapped = meta(&[
            ("cpu_mode", "exe"),
            ("cpu_start", "4294967200 4294967000"),
            ("cpu_end", "4 400"),
            ("hook_start", "0"),
            ("hook_end", "10"),
            ("ncpu", "1"),
        ]);
        assert!(cpu_usage(&wrapped, 1).is_some());
    }

    #[test]
    fn parses_powershell_output() {
        let env = parse_env("\u{feff}repo=acme/app\r\nactor=octocat\r\nworkflow=CI = main\r\n");
        assert_eq!(env["repo"], "acme/app");
        assert_eq!(env["workflow"], "CI = main");
    }

    #[test]
    fn samples_this_machine() {
        let (busy, total) = cpu_ticks().unwrap();
        assert!(total >= busy || cfg!(target_os = "macos"));
    }
}

#[cfg(test)]
mod dump {
    /// Set HOME_RUNNER_DUMP to a directory to write the hook scripts there for manual testing.
    #[test]
    fn dump_hooks() {
        if let Ok(dir) = std::env::var("HOME_RUNNER_DUMP") {
            let dir = std::path::Path::new(&dir);
            std::fs::write(dir.join("started.sh"), super::sh_started()).unwrap();
            std::fs::write(dir.join("completed.sh"), super::sh_completed()).unwrap();
            std::fs::write(dir.join("started.ps1"), super::ps_started()).unwrap();
            std::fs::write(dir.join("completed.ps1"), super::ps_completed()).unwrap();
        }
    }
}
