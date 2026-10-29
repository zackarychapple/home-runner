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
| `home-runner status` | This machine's runners as GitHub sees them (idle/busy) |
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
