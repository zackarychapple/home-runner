# home-runner

[![ci](https://github.com/zackarychapple/home-runner/actions/workflows/ci.yml/badge.svg)](https://github.com/zackarychapple/home-runner/actions/workflows/ci.yml)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

> **Status: alpha.** The supervisor, GitHub integration and host backend have been tested; the Apple
> `container` and Tart backends and the Windows service have not yet run real jobs. Issues and PRs welcome.

Turn spare Macs and PCs into GitHub Actions runners for **Linux, macOS and Windows** jobs.

Each machine runs one small Rust supervisor. It keeps a pool of **single-use runners** online. A runner registers with GitHub, runs exactly one job in a fresh VM, container or directory, and is then destroyed and replaced. A crash, reboot or power cut leaves nothing permanently broken: on startup, and every two minutes after that, home-runner removes anything carrying its name that it no longer owns.

```
┌──── each machine ─────────────────────────────────────────────────────┐
│ home-runner (launchd agent / Windows service, restarts on failure)    │
│   pool "linux-arm64" x2 ─► Apple container VM ─► run.sh --jitconfig   │
│   pool "macos"       x1 ─► Tart macOS VM      ─► run.sh --jitconfig   │
│   reconcile loop: delete stale registrations, VMs, containers, dirs   │
└──────────────────────────────── outbound HTTPS only ──► github.com ───┘
```

## What runs where

| Machine | Linux jobs | macOS jobs | Windows jobs |
|---|---|---|---|
| Apple Silicon Mac (M1–M4) | `apple-container`: arm64 native, x64 through Rosetta | `tart`: throwaway VM, max 2 per Mac | — |
| Intel Mac | `docker` (Colima / OrbStack / Docker Desktop) | `host`: directly on the Mac | — |
| Windows PC | `docker` (Docker Desktop), optional | — | `host`: directly on the PC |

`host` pools run each job in a new copy of the runner that is deleted afterwards. They don't isolate jobs from each other: software a job installs stays on the machine. Only use them for **private repos** you trust.

## 1. One-time GitHub setup

Create a GitHub App under your org's **Settings → Developer settings → GitHub Apps → New**:

- Disable webhooks; home-runner doesn't use them.
- Permissions:
  - Org-wide runners: **Organization → Self-hosted runners: Read and write**.
  - Repo-level runners (personal accounts): **Repository → Administration: Read and write**.
  - Optional, for the job ledger: **Repository → Actions: Read**. This adds each job's direct URL and queue time. **Metadata: Read** (on by default) lets home-runner tell public repos from private ones.
- Install it on the org, note the **App ID**, and generate a **private key** (`.pem`).

Copy the `.pem` to each machine. Alternatively, use a fine-grained PAT with the same permission and set `type = "token"`, `file = "…"`.

Under **Org → Settings → Actions → Runner groups → Default**, leave "Allow public repositories" **off**.

## 2. Install on each machine

Download the binary from the latest release:

- macOS (Intel and Apple Silicon): `home-runner-macos-universal.tar.gz`
- Windows: `home-runner-windows-x64.exe`

Or build it yourself with `cargo build --release`.

### Apple Silicon Mac

```sh
brew install cirruslabs/cli/tart          # macOS jobs
# Apple container: install the signed .pkg from github.com/apple/container/releases, then:
container system start                    # once, interactively (installs the Linux kernel)
brew install hudochenkov/sshpass/sshpass  # only if you use ssh_password with Tart

sudo install -m 755 home-runner /usr/local/bin/
home-runner init                          # writes ~/.home-runner/config.toml for this machine
$EDITOR ~/.home-runner/config.toml        # org, app_id, key path; adjust pool sizes
home-runner doctor
home-runner install                       # LaunchAgent: starts at login, restarts on crash
```

Then, in **System Settings**:
- Turn on automatic login for this user. The agent runs in your user session, which Tart and Apple container need. Automatic login requires FileVault to be off.
- Under Energy, turn on "Start up automatically after a power failure".

home-runner keeps the Mac awake itself (`prevent_sleep`).

Pre-pull the macOS image before the first job (about 30–60 GB): `tart pull ghcr.io/cirruslabs/macos-sequoia-xcode:latest`.

### Intel Mac

Same steps, without Tart or Apple container. Install Docker through Colima or OrbStack and make sure it starts at login. `home-runner init` writes a `docker` Linux pool and a `host` macOS pool.

### Windows PC

From an **Administrator** PowerShell:

```powershell
mkdir "C:\Program Files\home-runner"
copy home-runner-windows-x64.exe "C:\Program Files\home-runner\home-runner.exe"
cd "C:\Program Files\home-runner"
.\home-runner.exe init           # writes C:\ProgramData\home-runner\config.toml
notepad C:\ProgramData\home-runner\config.toml
.\home-runner.exe doctor
.\home-runner.exe install        # auto-start service, restarts on failure
# Recommended: run jobs as a dedicated local user instead of LocalSystem:
# .\home-runner.exe install --account .\ci --password '...'
```

It's fine if this PC is only online some days. When the service stops (shutdown or sleep), it deregisters its runners. Windows jobs then **wait in the queue** until the PC comes back; GitHub fails a job that has been queued for 24 hours.

## 3. Use them in workflows

Runners always get `self-hosted` plus their OS and arch, plus any `labels` you set in the pool:

```yaml
jobs:
  linux:
    runs-on: [self-hosted, Linux, ARM64, home]
  linux-x64:
    runs-on: [self-hosted, Linux, X64, home]
  mac:
    runs-on: [self-hosted, macOS, ARM64, home]
  windows:
    runs-on: [self-hosted, Windows, X64, home]
```

`home-runner doctor` prints the exact `runs-on` for each pool.

### An `ubuntu-latest`-like image with warm caches

`images/ubuntu-latest/` builds an image that looks like GitHub's `ubuntu-latest` (Ubuntu 24.04, `gh`, a C toolchain, passwordless `sudo`) with the toolchains already installed, so setup steps find them instead of downloading them: Node in the runner tool cache (`actions/setup-node` resolves it locally), pnpm, a pinned Rust toolchain with sccache, and any extra apt packages. Pin versions to match your repos:

```sh
./images/ubuntu-latest/build.sh arm64 --build-arg NODE_VERSION=26 --build-arg RUST_TOOLCHAIN=1.99.0
```

```toml
[[pool]]
name = "ubuntu-latest"
backend = "apple-container"
runner_group_id = 7              # a group limited to the repos that should run here
count = 2
labels = ["home", "ubuntu-latest"]
image = "home-runner/ubuntu-latest:arm64"
pull = false                     # a local image; there is nothing to pull
cache_size = "40G"               # persistent /cache volume per slot
cpus = 8
memory = "16G"
```

Use the labels in the workflow: GitHub always sends a bare `runs-on: ubuntu-latest` to its own runners, even when a self-hosted runner carries that label.

```yaml
runs-on: [self-hosted, home, ubuntu-latest]
```

**Caches.** With `cache_size`, each slot gets its own persistent volume (`<pool prefix><slot>-cache`) mounted as the runner's work folder (`/home/runner/_work`) with `/cache` linking into it, and the image points the pnpm store, npm cache, Cargo registry, sccache and Turbo cache at it. The work folder is on the volume too, so checkouts are incremental fetches and pnpm hardlinks from its store instead of copying. If the repo uses Turbo, list `CARGO_HOME`, `RUSTC_WRAPPER` and `SCCACHE_*` in `globalPassThroughEnv`, or Turbo's strict env mode hides them from Cargo. Jobs after the first start warm without any `actions/cache` round trips, so skip those on self-hosted runners (`if: runner.environment == 'github-hosted'`). A slot owns its volume, so concurrent jobs never write to the same one. When a volume passes 85% full it's emptied before the next job and that job starts cold. To reset by hand: `container volume rm <name>` while the service is stopped.

Anything a job leaves in `/cache` is visible to the next job on that slot, so give a cached pool a runner group limited to repos you trust. `runner_group_id` on a pool overrides `github.runner_group_id`. Rebuild the image to pick up new versions.

### A `macos-latest`-like Tart image with warm caches

`images/macos-latest/` does the same for macOS: it clones Cirrus Labs' macOS + Xcode image, installs Node (in the runner tool cache), pnpm, a pinned Rust toolchain with both Apple targets (for universal builds) and sccache, points the caches at `/cache`, and authorizes your SSH key. It builds a local VM named `home-runner-macos-latest` and needs a free macOS VM slot (macOS allows two VMs at a time), so stop the service or a pool first if two are running.

```sh
./images/macos-latest/build.sh RUST_TOOLCHAIN=1.99.0 NODE_VERSION=26 PNPM_VERSION=10.33.1
```

```toml
[[pool]]
name = "macos"
backend = "tart"
runner_group_id = 7
count = 1
labels = ["home", "macos-latest"]
image = "home-runner-macos-latest"
ssh_key = "~/.home-runner/tart_ed25519"
cache_size = "150G"              # sparse disk image per slot, mounted at /cache
cpus = 8
memory_mb = 16384
```

```yaml
runs-on: [self-hosted, home, macos-latest]
```

**Caches.** With `cache_size`, each slot gets a sparse disk image (`~/.home-runner/tart-cache/<pool prefix><slot>-cache.img`) attached to its VM. The VM formats it on first use and mounts it at `/cache`; the pnpm store, Cargo registry, sccache, Turbo cache, CEF downloads (`~/Library/Caches/tauri-cef`), the runner itself and its work folder live there. The work folder is a real directory on the disk (`/Volumes/HRCache/actions-runner/_work`), not a symlink, because `actions/checkout` registers its credentials under the path the runner reports and git only applies them when the path it sees matches. It's a disk rather than a shared folder because build trees are many small files: a shared folder (virtiofs) measured 8–23x slower for that than the VM's own disk, and the cache disk measured the same as the VM's own disk. It's HFS+ because an APFS volume on a VM's internal disk is keyed to that VM's boot disk, which is a fresh clone every job. `actions/checkout` cleans untracked files by default, which deletes build output (`target/`, `node_modules`) from the work folder; use `clean: false` and a `git clean -ffdx -e target -e node_modules` step to keep it. The same 85% rule and trust rule as above apply. To reset by hand, stop the service and delete the image file.

**Keeping release builds trustworthy.** A build cache is keyed by what goes into a build, but its contents are whatever a previous job wrote, so a job from any branch that can reach the pool could leave poisoned objects that a later release build reuses. Give release builds their own pool (so their own cache) in a runner group restricted to the release workflow on your default branch (`restricted_to_workflows` with `selected_workflows` like `org/repo/.github/workflows/release.yml@refs/heads/main`). Release builds stay warm, because only release builds write that cache. Keep signing material out of `/cache`: import it into a temporary keychain under `/tmp`, since the work folder (including `$RUNNER_TEMP`) is on the cache.

## Job log and savings

Every job is written to a ledger on the machine that ran it (`~/.home-runner/jobs.jsonl`, one JSON object per line). Each entry records:
- repo, workflow, job, run link, who triggered it, event, branch and commit
- when it started, how long it took and how long it waited in GitHub's queue
- the result, and CPU used
- what the same job would have cost on the closest GitHub-hosted and Depot runners

```console
$ home-runner jobs --since 7d
STARTED (UTC)     REPO          WORKFLOW / JOB      ACTOR     POOL         TIME  RESULT     CPU   SAVED
2026-10-29 09:12  acme/mobile   CI / build          mona      linux       1h34m  Succeeded  50%   $0.75
2026-10-29 10:05  acme/app      CI / build          hubot     linux      41m55s  Failed     50%   $0.33

$ home-runner savings --by repo          # or --by actor | pool | host | workflow | day | month
  GitHub-hosted would have cost   $1.23
  Depot would have cost           $1.84
  your cost (power + fees)        $0.01
  saved vs GitHub                 $1.22
  ...
```

- `--repo`, `--actor`, `--since 24h|7d|4w|all` filter either command.
- `--format csv|json` exports for spreadsheets or dashboards.
- To report across the whole fleet, copy each machine's ledger and pass them all, e.g. `--ledger mini.jsonl --ledger pc.jsonl`.
- With `annotate_jobs = true`, each workflow run also shows a notice such as *"Ran on studio-m2. About $0.79 saved vs GitHub macOS 3-core M1 (13 billable min)."*

**How the numbers are estimated**

- **Job details:** GitHub's runner job hooks (`ACTIONS_RUNNER_HOOK_JOB_STARTED/COMPLETED`) run small scripts inside each job. They record the `GITHUB_*` variables and sample CPU counters, and they never fail the job.
- **CPU:**
  - Apple container VMs: the VM's `/proc/stat`.
  - Docker: the container's cgroup.
  - Tart VMs and host jobs: machine-wide counters through `home-runner cpu-sample`. For host jobs this includes anything else the machine was doing at the same time.
- **Comparable runner:** by default, the smallest hosted runner with the same OS and arch and at least as many CPUs. Pin a specific one per pool with `compare_github` / `compare_depot`. `home-runner prices` lists every id and rate.
- **Billing rules:** GitHub rounds every job up to a whole minute; Depot bills per second. Standard GitHub runners are free for public repos, so those jobs count as $0 saved.
- **Prices:** list prices as of 2026-10-29, from [GitHub](https://docs.github.com/en/billing/reference/actions-runner-pricing) and [Depot](https://depot.dev/docs/github-actions/runner-types). If they change, override any rate in `[pricing.rates]`.
- **Not counted:** plan-included free minutes (GitHub Team includes 3,000 per month, Depot plans include some) and hardware cost. GitHub announced, then postponed, a $0.002/min charge for self-hosted runners on private repos; set `github_self_hosted_usd_per_minute` if it ever applies.
- **Your cost:** electricity only, calculated as `job_watts` × job time × `electricity_usd_per_kwh`.

## Dashboard

```sh
home-runner ui                                   # this machine's ledger
home-runner ui --ledger mini.jsonl --ledger pc.jsonl --since 90d   # a whole fleet
```

A native window over the same ledger the `jobs` and `savings` commands read. It is read-only, needs no GitHub credentials, and picks up new jobs within a couple of seconds while it is open.

- **Overview:** saved vs GitHub-hosted and vs Depot, job count, success rate, runner time and your cost; cumulative savings and jobs per day; the success / failed / canceled split; job duration and queue wait (average, median, p95, longest).
- **Breakdown:** the same totals grouped by repo, actor, workflow, host/pool, host, day or month.
- **Jobs:** every job, sortable by start, time, queue wait or savings, each linking to its GitHub run.
- The time range and the repo, actor and host filters at the top apply to all three views. Days are UTC.

Headless machines can skip the GUI dependencies with `cargo build --release --no-default-features`; release binaries include it.

## Network (UniFi)

**You don't need a static public IP, port forwarding, or a "host" machine.** Runners make long-polling **outbound** HTTPS connections to GitHub, so every machine works on its own and none of them depends on another.

Recommended UniFi setup:
- **DHCP reservations** (*Client → Settings → Fixed IP Address*) for each runner, so you can SSH or Screen Share in. These are LAN addresses, not public ones.
- A **separate VLAN / network for CI machines** (for example "CI", 10.0.50.0/24). CI jobs run third-party code from dependencies and actions, so treat these machines as semi-trusted.
- **Firewall rules** (*Settings → Security → Firewall*, or Traffic Rules):
  - Allow CI → Internet.
  - Block CI → your main LAN and IoT networks.
  - Allow main LAN → CI if you want to manage the machines from your laptop.
- Optionally, use Ethernet for these machines; macOS VM image pulls are large.

You'd only need inbound access for webhook-driven scale-to-zero, which this design deliberately avoids. Even then, use an outbound tunnel (Cloudflare Tunnel or Tailscale Funnel), not a port forward.

## Operations

| | |
|---|---|
| `home-runner status` | This machine's runners as GitHub sees them (idle/busy), plus lifetime savings |
| `home-runner jobs` / `savings` | Job log and savings report (see above) |
| `home-runner ui` | Dashboard of savings and job stats (see above) |
| `home-runner doctor` | Checks auth, tooling and images, and prints labels |
| Logs | `~/.home-runner/logs/` or `C:\ProgramData\home-runner\logs\`, rotated daily, kept 14 days. Runner output is included. |
| `HOME_RUNNER_LOG=debug` | More detail |
| `home-runner cleanup` | Removes everything this machine registered or created (stop the service first) |
| Upgrade | Replace the binary, then run `home-runner install` again |
| Remove | `home-runner uninstall`, then `home-runner cleanup` |

Self-healing behaviour:
- Failures back off exponentially (5s → 5min).
- A runner that exits without taking a job is treated as a failure.
- Jobs are killed after `job_timeout_minutes` (default 360), counted from when the job starts, so idle runners can wait indefinitely.
- New runner releases are picked up automatically: release info is checked every 6 hours and new images are pulled on start.
- If GitHub is unreachable, the cached runner is used.
- A lock file stops two supervisors running on one machine.

## Sizing guide

| Mac | Suggested pools |
|---|---|
| 8 GB M1 | `linux-arm64` x1 (4 GB), or `macos` x1 alone |
| 16 GB M1/M2 | `linux-arm64` x2 (4 GB each) + `macos` x1 (8 GB) |
| 32 GB+ | `linux-arm64` x2–3, `linux-x64` x1, `macos` x2 |

Idle containers and VMs use little CPU, but a Tart VM reserves its memory while it waits for a job.

## Known limitations and next steps

- **Docker inside Linux jobs:** jobs using `container:`, `services:` or Docker-based actions need a Docker daemon inside the job VM, and the stock `ghcr.io/actions/actions-runner` image doesn't run one. Build your own image `FROM` it with your toolchains, and set `image`.
- **Tart golden images:** for speed and reliability, clone the Cirrus Labs image once, install your toolchains, the runner (in `~/actions-runner`) and an SSH key, then point `image` at that local VM and use `ssh_key`.
- **Intel macOS and Windows jobs** run on the host, not in a VM. Adding a QEMU backend for Windows VMs on Intel Macs would follow the same `Backend` pattern (`src/backend/`).
- **Third-party tools have their own licenses.** home-runner only invokes Tart, Apple `container` and
  Docker; it doesn't bundle them. Check Tart's current license (it has used a source-available license
  with limits for larger organizations), the terms of any VM images you pull, and Apple's macOS
  licence, which allows up to two macOS VMs per Mac.
- **Apple `container` flags:** `--arch` and `list --quiet` were written against the CLI's documented interface. Run `home-runner doctor` and a test job after upgrading `container`.

## License

[MIT](LICENSE). Security reports: see [SECURITY.md](SECURITY.md).
