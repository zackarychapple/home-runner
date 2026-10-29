# A runner image that behaves like GitHub's `ubuntu-latest` (Ubuntu 24.04) for typical workflows,
# with the toolchains already in place so setup steps find them instead of downloading them:
#
# - Node in the runner tool cache, so `actions/setup-node` resolves it locally, and on PATH for
#   steps that run `node` before any setup action.
# - pnpm installed globally (pnpm switches itself to a repo's `packageManager` version if needed).
# - Rust through rustup with a pinned toolchain preinstalled, plus sccache.
# - A C toolchain, gh, and passwordless sudo (from the base image) for `sudo apt-get` steps.
#
# Package stores and build caches point at /cache. With `cache_size` on the pool that is a
# persistent volume per slot, so the second job onwards starts warm; without it, /cache is just a
# directory in the throwaway container.
#
#   ./images/ubuntu-latest/build.sh arm64
FROM ghcr.io/actions/actions-runner:latest

ARG NODE_VERSION=26
ARG PNPM_VERSION=10.33.1
ARG RUST_TOOLCHAIN=1.99.0
ARG RUST_COMPONENTS="clippy rustfmt"
ARG SCCACHE_VERSION=0.19.0
# Extra apt packages workflows would otherwise install on every run.
ARG APT_PACKAGES="libasound2-dev"

USER root
ENV RUNNER_TOOL_CACHE=/opt/hostedtoolcache \
    AGENT_TOOLSDIRECTORY=/opt/hostedtoolcache
RUN set -eux; \
    export DEBIAN_FRONTEND=noninteractive; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
      ca-certificates curl gnupg wget git jq \
      build-essential pkg-config libssl-dev \
      unzip zip xz-utils zstd \
      python3-pip ${APT_PACKAGES}; \
    install -d -m 0755 /etc/apt/keyrings; \
    curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg -o /etc/apt/keyrings/githubcli.gpg; \
    echo "deb [signed-by=/etc/apt/keyrings/githubcli.gpg] https://cli.github.com/packages stable main" > /etc/apt/sources.list.d/github-cli.list; \
    apt-get update; \
    apt-get install -y --no-install-recommends gh; \
    rm -rf /var/lib/apt/lists/*; \
    case "$(dpkg --print-architecture)" in \
      amd64) node_arch=x64; rust_arch=x86_64 ;; \
      arm64) node_arch=arm64; rust_arch=aarch64 ;; \
    esac; \
    # Node, laid out the way actions/setup-node's tool cache expects (<tool>/<version>/<arch>).
    node="$(curl -fsSL https://nodejs.org/dist/index.json | jq -r --arg m "v${NODE_VERSION}." '[.[] | select(.version | startswith($m))][0].version')"; \
    dir="$RUNNER_TOOL_CACHE/node/${node#v}/$node_arch"; \
    mkdir -p "$dir"; \
    curl -fsSL "https://nodejs.org/dist/$node/node-$node-linux-$node_arch.tar.xz" | tar -xJ -C "$dir" --strip-components=1; \
    touch "$dir.complete"; \
    ln -s "$dir/bin/node" "$dir/bin/npm" "$dir/bin/npx" /usr/local/bin/; \
    npm install -g "pnpm@${PNPM_VERSION}"; \
    ln -s "$dir/bin/pnpm" "$dir/bin/pnpx" /usr/local/bin/; \
    curl -fsSL "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-${rust_arch}-unknown-linux-musl.tar.gz" \
      | tar -xz -C /usr/local/bin --strip-components=1 --wildcards '*/sccache'; \
    mkdir -p /cache; \
    chown -R runner:runner "$RUNNER_TOOL_CACHE" /cache

USER runner
ENV RUSTUP_HOME=/home/runner/.rustup \
    CARGO_HOME=/home/runner/.cargo
RUN set -eux; \
    curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal --default-toolchain "$RUST_TOOLCHAIN"; \
    "$CARGO_HOME/bin/rustup" component add --toolchain "$RUST_TOOLCHAIN" $RUST_COMPONENTS

# rustup's proxies stay in ~/.cargo/bin; CARGO_HOME only decides where the registry and git
# checkouts go, so moving it to /cache keeps downloaded crates between jobs.
ENV PATH=/home/runner/.cargo/bin:$PATH \
    CARGO_HOME=/cache/cargo \
    CARGO_INCREMENTAL=0 \
    RUSTC_WRAPPER=sccache \
    SCCACHE_DIR=/cache/sccache \
    SCCACHE_CACHE_SIZE=10G \
    npm_config_store_dir=/cache/pnpm-store \
    npm_config_cache=/cache/npm \
    TURBO_CACHE_DIR=/cache/turbo \
    ImageOS=ubuntu24 \
    ImageVersion=home-runner
