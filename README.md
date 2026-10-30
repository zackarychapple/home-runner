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
- To combine machines by hand, copy each machine's ledger and pass them all, e.g. `--ledger mini.jsonl --ledger pc.jsonl`. To do it automatically across houses, see [Fleet](#fleet-several-houses-one-dashboard).
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

## Desktop app

```sh
home-runner gui
```

A window over this machine's supervisor, and the fleet if it has joined one. Switch between **this machine** and **Fleet** at the top; the tabs show the same things for either.

- **Active**: each running job with its repo, workflow, who triggered it and the commit; elapsed time against that job's usual successful run (the bar turns amber when it runs long); live CPU; and money saved so far. Below that, every runner slot (preparing, booting, idle, running, retrying after a failure) with the exact `runs-on` labels. **Stop job** kills a job on this machine and records it as Canceled. In fleet view, a warning appears when a platform has no runner online, so its jobs will queue.
- **History**: totals for the last 24h / 7d / 30d / all, saved per day, and a searchable table filtered by result and pool (or house). Click a row for its trigger, queue wait, what GitHub and Depot would have charged, your power cost and how it compared with its usual run. **Export CSV** saves the filtered rows to Downloads.
- **Fleet**: this month's totals, every house and machine, a coverage grid of runner slots per platform, savings per house, and the state of this machine's reports. Not in a fleet yet? Join one from here.
- **Settings**: host name, keep-awake, electricity price, pool sizes, and fleet sharing. Saving edits `config.toml` in place (comments and other settings are kept) and refuses an invalid result; **Save & restart** restarts the service. **Run doctor** shows `home-runner doctor`'s output.

The app only reads files the supervisor writes (`state.json` every 2 seconds, and the ledger) and asks it to stop a job by dropping a file in `control/`, so you can open and close it at any time. It is built into the release binary. For a headless build without it, use `cargo build --release --no-default-features`.

Live job details come from the job hooks. In VMs and containers, the hooks print the job's details as soon as it starts and a CPU sample every 5 seconds to the runner's output, which the supervisor reads. On the host backend they are read straight from the job's work directory. For Tart and host jobs, CPU is machine-wide.

## Fleet: several houses, one dashboard

Machines at different people's houses can report to one [Cloudflare Worker](fleet-worker/) backed by D1. Each machine sends a heartbeat every 60 seconds (and soon after each job): its pools, slots and running jobs, plus any new ledger records. Everyone in the fleet sees totals, every house and machine, coverage and a fleet-wide job history, in `home-runner gui` or `home-runner fleet status`.

**Deploy the Worker** (once, by whoever runs the fleet):

```sh
cd fleet-worker
npm install
npx wrangler d1 create home-runner-fleet          # put the printed database_id in wrangler.toml
npx wrangler d1 migrations apply home-runner-fleet --remote
npx wrangler secret put ADMIN_TOKEN               # any long random string; only for creating invites
npx wrangler deploy
```

Create the fleet and an invite (`uses` and `days` are optional; the defaults are 10 and 7):

```sh
W=https://home-runner-fleet.<you>.workers.dev
curl -X POST $W/v1/fleets  -H "Authorization: Bearer $ADMIN_TOKEN" -d '{"name":"friends"}'
curl -X POST $W/v1/invites -H "Authorization: Bearer $ADMIN_TOKEN" -d '{"fleet":"friends","uses":5}'
# {"code":"FPDK-3JB9", ...}
```

**Join from each machine**, then restart the supervisor (`home-runner install` again, or **Save & restart** in the app):

```sh
home-runner fleet join --url $W --invite FPDK-3JB9 --house "Priya's place" --share repos
```

This stores the machine's token in `fleet-token` in the data dir (readable only by you) and adds a `[fleet]` section to the config. The Worker keeps only a hash of the token. Joining again from the same host name replaces its token and keeps its history.

**What leaves the machine** is set per machine with `share`, and applied before anything is sent:

| `share` | Sent |
|---|---|
| `totals` | Host, pool, platform, start time, duration, result, CPU, and the cost and savings numbers. No repo, workflow or job names. |
| `repos` (default) | Adds repo, workflow and job names. Private repos (and repos whose visibility couldn't be checked) are sent as a short hash such as `private-c7198fbc19`, unless `hash_private_repos = false`. |
| `full` | Everything in the ledger, including who triggered the job, event, branch, commit and run link. Best when everyone is in the same GitHub org. |

Reports are sent in order from the ledger, and the supervisor remembers how far it got (`fleet-cursor`). If a machine or the Worker is offline, nothing is lost; the Worker ignores records it already has. A machine counts as offline after missing three heartbeats, and immediately when its supervisor stops cleanly. Savings are credited to the house whose machine ran the job.

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

Fleet reporting is outbound HTTPS too. You'd only need inbound access for webhook-driven scale-to-zero, which this design deliberately avoids. Even then, use an outbound tunnel (Cloudflare Tunnel or Tailscale Funnel), not a port forward.

## Operations

| | |
|---|---|
| `home-runner gui` | Desktop app: live jobs, history, savings, fleet, settings |
| `home-runner status` | This machine's runners as GitHub sees them (idle/busy), plus lifetime savings |
| `home-runner fleet status` | Fleet totals, houses and machines |
| `home-runner jobs` / `savings` | Job log and savings report (see above) |
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
