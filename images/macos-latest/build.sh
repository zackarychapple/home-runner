#!/bin/sh
# Builds the macos-latest Tart image: Cirrus Labs' macOS + Xcode image with Node, pnpm, Rust (both
# Apple targets) and sccache preinstalled, caches pointed at /cache, and an SSH key authorized.
#   build.sh [NAME=VALUE ...]
# e.g. build.sh RUST_TOOLCHAIN=1.99.0 NODE_VERSION=26 BREW_PACKAGES="jq"
# Settings: BASE (source image), IMAGE (local VM name), SSH_KEY (public key to authorize), plus the
# versions in provision.sh. Building needs one free macOS VM slot (macOS allows two at a time).
set -eu
cd "$(dirname "$0")"
BASE=ghcr.io/cirruslabs/macos-tahoe-xcode:latest
IMAGE=home-runner-macos-latest
SSH_KEY="$HOME/.home-runner/tart_ed25519.pub"
NODE_VERSION=26
PNPM_VERSION=10.33.1
RUST_TOOLCHAIN=1.99.0
RUST_COMPONENTS="clippy rustfmt"
RUST_TARGETS="aarch64-apple-darwin x86_64-apple-darwin"
SCCACHE_VERSION=0.19.0
BREW_PACKAGES="cmake ninja"   # CEF (tauri-runtime-cef) builds its wrapper with CMake + Ninja
for arg in "$@"; do
  case "$arg" in
    *=*) eval "${arg%%=*}=\"\${arg#*=}\"" ;;
    *) echo "usage: $0 [NAME=VALUE ...]" >&2; exit 2 ;;
  esac
done

running=$(tart list --format json | python3 -c 'import json,sys; print(sum(v["Running"] for v in json.load(sys.stdin)))')
if [ "$running" -ge 2 ]; then
  echo "two macOS VMs are already running; stop one (e.g. the home-runner service) first" >&2
  exit 1
fi
[ -f "$SSH_KEY" ] || { echo "no public key at $SSH_KEY (set SSH_KEY=...)" >&2; exit 1; }

vm="$IMAGE-building"
tart delete "$vm" >/dev/null 2>&1 || true
trap 'tart stop "$vm" --timeout 5 >/dev/null 2>&1; tart delete "$vm" >/dev/null 2>&1' EXIT INT TERM
tart clone "$BASE" "$vm"
tart run "$vm" --no-graphics >/dev/null 2>&1 &
ip=$(tart ip "$vm" --wait 180)
ssh_vm() {
  SSHPASS=admin sshpass -e ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    -o LogLevel=ERROR -o ConnectTimeout=5 "admin@$ip" "$@"
}
i=0
until ssh_vm true 2>/dev/null; do
  i=$((i + 1)); [ "$i" -lt 60 ] || { echo "SSH to $ip never came up" >&2; exit 1; }
  sleep 3
done

ssh_vm 'mkdir -p ~/.ssh && chmod 700 ~/.ssh && cat >> ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys' < "$SSH_KEY"
ssh_vm "cat > /tmp/provision.sh" < provision.sh
ssh_vm "NODE_VERSION='$NODE_VERSION' PNPM_VERSION='$PNPM_VERSION' RUST_TOOLCHAIN='$RUST_TOOLCHAIN' \
  RUST_COMPONENTS='$RUST_COMPONENTS' RUST_TARGETS='$RUST_TARGETS' SCCACHE_VERSION='$SCCACHE_VERSION' \
  BREW_PACKAGES='$BREW_PACKAGES' bash /tmp/provision.sh && rm /tmp/provision.sh"
ssh_vm 'sudo shutdown -h now' >/dev/null 2>&1 || true
i=0
while tart list --format json | python3 -c 'import json,sys; sys.exit(0 if any(v["Name"]==sys.argv[1] and v["Running"] for v in json.load(sys.stdin)) else 1)' "$vm"; do
  i=$((i + 1)); [ "$i" -lt 60 ] || { tart stop "$vm" --timeout 10; break; }
  sleep 2
done

trap - EXIT INT TERM
tart delete "$IMAGE" >/dev/null 2>&1 || true
tart rename "$vm" "$IMAGE"
echo "built $IMAGE"
