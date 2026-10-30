#!/bin/sh
# Builds the macos-latest Tart image: Cirrus Labs' macOS + Xcode image with Node, pnpm, Rust (both
# Apple targets) and sccache preinstalled, caches pointed at /cache, and an SSH key authorized.
#   build.sh [NAME=VALUE ...]
# e.g. build.sh RUST_TOOLCHAIN=1.99.0 NODE_VERSION=26 BREW_PACKAGES="jq"
# Settings: BASE (source image), IMAGE (local VM name), SSH_KEY (public key to authorize), plus the
# versions in provision.sh. NATIVE=true also provisions the pinned Tauri CLI and CEF (native/), and
# GUI=true the stable native input helper plus its one-time Accessibility grant (gui/); README:
# "Native GUI workers". PROVISION=false skips the toolchains and only authorizes the key, for a
# pool that wants Cirrus Labs' image as it is but without the default admin/admin password login:
#   build.sh IMAGE=home-runner-macos-keyed PROVISION=false Building needs one free macOS VM slot (macOS allows two at a time).
set -eu
cd "$(dirname "$0")"
BASE=ghcr.io/cirruslabs/macos-tahoe-xcode:latest
IMAGE=home-runner-macos-latest
SSH_KEY="$HOME/.home-runner/tart_ed25519.pub"
PROVISION=true
NODE_VERSION=26
PNPM_VERSION=10.33.1
RUST_TOOLCHAIN=1.99.0
RUST_COMPONENTS="clippy rustfmt"
RUST_TARGETS="aarch64-apple-darwin x86_64-apple-darwin"
SCCACHE_VERSION=0.19.0
BREW_PACKAGES="cmake ninja"   # CEF (tauri-runtime-cef) builds its wrapper with CMake + Ninja
# NATIVE=true: the pins must match the pool's `kind = "tauri-cef"` requirement.
NATIVE=false
NATIVE_ROOT=/opt/home-runner/native
TAURI_REVISION=78f49ba3993fed5a1b921235c547731006ad2a74
TAURI_CLI_VERSION=3.0.0-alpha.5
CEF_ARCHIVE='cef_binary_154.0.34+g14c5a08+chromium-154.0.8037.98_macosarm64_minimal.tar.bz2'
CEF_SHA1=b7081f6609e5edccf07be96b5d7977329fbe1bdf
# GUI=true: the helper is signed with this identity (images/macos-latest/gui/make-signing-identity.sh).
GUI=false
GUI_SIGNING_P12="$HOME/.home-runner/gui-signing.p12"
GUI_HELPER=/usr/local/libexec/home-runner/hr-input
GRANT_POLLS=180   # how many 5-second checks to wait for the operator's Accessibility grant
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
# The GUI grant is given by a person, in the VM's own window.
if [ "$GUI" = true ]; then tart run "$vm" >/dev/null 2>&1 & else tart run "$vm" --no-graphics >/dev/null 2>&1 & fi
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
if [ "$PROVISION" = true ]; then
  ssh_vm "cat > /tmp/provision.sh" < provision.sh
  ssh_vm "NODE_VERSION='$NODE_VERSION' PNPM_VERSION='$PNPM_VERSION' RUST_TOOLCHAIN='$RUST_TOOLCHAIN' \
    RUST_COMPONENTS='$RUST_COMPONENTS' RUST_TARGETS='$RUST_TARGETS' SCCACHE_VERSION='$SCCACHE_VERSION' \
    BREW_PACKAGES='$BREW_PACKAGES' bash /tmp/provision.sh && rm /tmp/provision.sh"
fi
if [ "$NATIVE" = true ]; then
  ssh_vm "cat > /tmp/install-native.sh" < native/install-native.sh
  ssh_vm "TAURI_REVISION='$TAURI_REVISION' TAURI_CLI_VERSION='$TAURI_CLI_VERSION' CEF_ARCHIVE='$CEF_ARCHIVE' \
    CEF_SHA1='$CEF_SHA1' NATIVE_ROOT='$NATIVE_ROOT' bash /tmp/install-native.sh && rm /tmp/install-native.sh"
fi
if [ "$GUI" = true ]; then
  p12=""
  if [ -f "$GUI_SIGNING_P12" ]; then
    p12=/tmp/hr-input-signing.p12
    ssh_vm "cat > $p12" < "$GUI_SIGNING_P12"
  else
    echo "no $GUI_SIGNING_P12 (run gui/make-signing-identity.sh): the helper will be signed ad hoc and every rebuild needs a new grant" >&2
  fi
  ssh_vm "mkdir -p /tmp/hr-gui"
  ssh_vm "cat > /tmp/hr-gui/hr-input.swift" < gui/hr-input.swift
  ssh_vm "cat > /tmp/hr-gui/install-gui.sh" < gui/install-gui.sh
  ssh_vm "HR_INPUT_SRC=/tmp/hr-gui HR_INPUT_P12='$p12' HR_INPUT_DEST='$GUI_HELPER' bash /tmp/hr-gui/install-gui.sh; rc=\$?; rm -rf /tmp/hr-gui $p12; exit \$rc"
  # The grant is per user and survives clones and reboots. Ask once, then wait for the person.
  i=0
  until ssh_vm "'$GUI_HELPER' access" | grep -q '"trusted":true,"post":true'; do
    if [ "$i" -eq 0 ]; then
      ssh_vm "'$GUI_HELPER' access --prompt" >/dev/null 2>&1 || true
      echo "In the VM's window: System Settings > Privacy & Security > Accessibility, add and enable $GUI_HELPER." >&2
    fi
    i=$((i + 1)); [ "$i" -lt "$GRANT_POLLS" ] || { echo "the Accessibility grant was not given in time" >&2; exit 1; }
    sleep 5
  done
  echo "granted; proving a real keyboard and pointer event reaches a disposable app:"
  ssh_vm "'$GUI_HELPER' selftest" || { echo "the input self-test failed; the image is not usable for native GUI jobs" >&2; exit 1; }
fi
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
