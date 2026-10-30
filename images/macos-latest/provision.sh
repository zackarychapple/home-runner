#!/bin/bash
# Runs inside the VM (as the admin user) while build.sh builds the macos-latest image.
# - Node in the runner tool cache, so `actions/setup-node` resolves it locally, and on PATH.
# - pnpm, and Rust through rustup with a pinned toolchain and both Apple targets, plus sccache.
# - Package stores and build caches point at /cache. With `cache_size` on the pool that is a
#   disk image per slot, so the second job onwards starts warm; without it, /cache is just a
#   directory on the VM's own disk.
set -euo pipefail
: "${NODE_VERSION:?}" "${PNPM_VERSION:?}" "${RUST_TOOLCHAIN:?}" "${SCCACHE_VERSION:?}"
RUST_COMPONENTS="${RUST_COMPONENTS:-clippy rustfmt}"
RUST_TARGETS="${RUST_TARGETS:-aarch64-apple-darwin x86_64-apple-darwin}"
BREW_PACKAGES="${BREW_PACKAGES:-}"

eval "$(/opt/homebrew/bin/brew shellenv)"
export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1

# The base image's Node and pnpm would shadow the pinned ones.
for f in node pnpm; do
  if brew list --formula "$f" >/dev/null 2>&1; then brew uninstall --ignore-dependencies "$f"; fi
done
[ -z "$BREW_PACKAGES" ] || brew install $BREW_PACKAGES

tools="$HOME/hostedtoolcache"
node="$(curl -fsSL https://nodejs.org/dist/index.json \
  | python3 -c 'import json,sys; m=sys.argv[1]; print(next(r["version"] for r in json.load(sys.stdin) if r["version"].startswith(m)))' "v${NODE_VERSION}.")"
dir="$tools/node/${node#v}/arm64"
rm -rf "$tools/node"
mkdir -p "$dir"
curl -fsSL "https://nodejs.org/dist/$node/node-$node-darwin-arm64.tar.xz" | tar -xJ -C "$dir" --strip-components=1
touch "$dir.complete"
PATH="$dir/bin:$PATH" npm install -g "pnpm@${PNPM_VERSION}"

curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal \
  --default-toolchain "$RUST_TOOLCHAIN" -c ${RUST_COMPONENTS// /,} -t ${RUST_TARGETS// /,}
mkdir -p "$HOME/bin"
curl -fsSL "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-aarch64-apple-darwin.tar.gz" \
  | tar -xz -C "$HOME/bin" --strip-components=1 "sccache-v${SCCACHE_VERSION}-aarch64-apple-darwin/sccache"
chmod +x "$HOME/bin/sccache"

# /cache is where a slot's cache disk (volume "HRCache") mounts. The runner script formats a blank
# disk on first use, or makes a plain directory there when the pool has no cache.
grep -q '^cache' /etc/synthetic.conf 2>/dev/null || printf 'cache\tVolumes/HRCache\n' | sudo tee -a /etc/synthetic.conf >/dev/null
# CEF (tauri-runtime-cef) downloads its runtime here on macOS.
mkdir -p "$HOME/Library/Caches"
rm -rf "$HOME/Library/Caches/tauri-cef"
ln -s /cache/tauri-cef "$HOME/Library/Caches/tauri-cef"

# Sourced by home-runner once /cache is ready, right before it starts the runner, so every job
# step sees it. rustup's proxies stay in ~/.cargo/bin; CARGO_HOME only decides where the registry
# and installed tools go. Directories that links point into are created here, because a cache
# that was just formatted or emptied has none.
mkdir -p "$HOME/.home-runner"
cat > "$HOME/.home-runner/env" <<ENV
mkdir -p /cache/tauri-cef
export RUNNER_TOOL_CACHE="$tools" AGENT_TOOLSDIRECTORY="$tools"
export PATH="$HOME/bin:/cache/cargo/bin:$HOME/.cargo/bin:$dir/bin:/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:\$PATH"
export RUSTUP_HOME="$HOME/.rustup" CARGO_HOME=/cache/cargo CARGO_INCREMENTAL=0
export RUSTC_WRAPPER=sccache SCCACHE_DIR=/cache/sccache SCCACHE_CACHE_SIZE=30G
export npm_config_store_dir=/cache/pnpm-store npm_config_cache=/cache/npm
export TURBO_CACHE_DIR=/cache/turbo
export HOMEBREW_NO_AUTO_UPDATE=1
ENV

# Accept the license and install Xcode's extra packages now rather than during the first job.
sudo xcodebuild -license accept >/dev/null 2>&1 || true
sudo xcodebuild -runFirstLaunch >/dev/null 2>&1 || true

# Spotlight indexing a build tree costs CPU for nothing.
sudo mdutil -a -i off >/dev/null 2>&1 || true

. "$HOME/.home-runner/env"
echo "node $(node --version), pnpm $(pnpm --version), $(rustc --version), $(sccache --version)"
rustup target list --installed
