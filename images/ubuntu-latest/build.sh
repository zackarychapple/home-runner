#!/bin/sh
# Builds the ubuntu-latest runner image.
#   build.sh [arm64|amd64] [--build-arg NAME=VALUE ...]   (default arch: this machine's)
# e.g. build.sh arm64 --build-arg RUST_TOOLCHAIN=1.99.0 --build-arg NODE_VERSION=26
set -eu
cd "$(dirname "$0")"
arch=""
case "${1:-}" in arm64|amd64) arch="$1"; shift ;; esac
tag=home-runner/ubuntu-latest:latest
[ -n "$arch" ] && tag="home-runner/ubuntu-latest:$arch"
if command -v container >/dev/null 2>&1; then
  container build ${arch:+--arch "$arch"} -t "$tag" "$@" .
else
  docker build ${arch:+--platform "linux/$arch"} -t "$tag" "$@" .
fi
echo "built $tag"
