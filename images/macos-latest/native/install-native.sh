#!/bin/bash
# Runs inside the VM (as the admin user, with passwordless sudo) while build.sh builds the image
# with NATIVE=true. Provisions the pinned native prerequisites into a root-owned, read-only tree
# that jobs can use but not change, and that `home-runner` verifies (kind = "tauri-cef"):
#
#   $NATIVE_ROOT/tools/bin/cargo-tauri            Cargo Tauri CLI built from one immutable revision
#   $NATIVE_ROOT/tools/tauri-revision             that revision, written only after the build passed
#   $NATIVE_ROOT/cef/<archive>                    the CEF binary distribution, checksum-verified
#   $NATIVE_ROOT/cef/<archive>.tree.sha256        a manifest of every extracted file
#   $NATIVE_ROOT/cef/dist/                        the extracted distribution (WITH_TAP_CEF_PATH)
#
# Idempotent: parts that already verify are left alone. Nothing here runs in a job: CI never builds
# the CLI or downloads CEF. The archive must match BOTH the checksum pinned here and the one the
# CEF builds index publishes; a mismatch aborts before anything is installed.
set -euo pipefail
: "${TAURI_REVISION:?}" "${TAURI_CLI_VERSION:?}" "${CEF_ARCHIVE:?}" "${CEF_SHA1:?}"
NATIVE_ROOT="${NATIVE_ROOT:-/opt/home-runner/native}"
CEF_BASE_URL="${CEF_BASE_URL:-https://cef-builds.spotifycdn.com}"
tools="$NATIVE_ROOT/tools"
cef="$NATIVE_ROOT/cef"
dist="$cef/dist"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

case "$(uname -s)-$(uname -m)" in Darwin-arm64) ;; *) echo "native prerequisites need Darwin/arm64" >&2; exit 1 ;; esac
case "$CEF_ARCHIVE" in *_macosarm64_*) ;; *) echo "CEF_ARCHIVE $CEF_ARCHIVE is not a macosarm64 distribution" >&2; exit 1 ;; esac

writable_for_nobody() { sudo chown -R root:wheel "$1" && sudo chmod -R go-w,u-w "$1"; }

# --- Cargo Tauri CLI ----------------------------------------------------------------------------
if [ "$(cat "$tools/tauri-revision" 2>/dev/null || true)" = "$TAURI_REVISION" ] \
  && "$tools/bin/cargo-tauri" --version 2>&1 | head -n 1 | grep -q "$TAURI_CLI_VERSION"; then
  echo "cargo-tauri $TAURI_CLI_VERSION at $TAURI_REVISION is already installed"
else
  cargo install --git https://github.com/tauri-apps/tauri --rev "$TAURI_REVISION" --locked \
    --root "$work/tools" tauri-cli
  "$work/tools/bin/cargo-tauri" --version 2>&1 | head -n 1 | grep -q "$TAURI_CLI_VERSION" \
    || { echo "the CLI built at $TAURI_REVISION does not identify as $TAURI_CLI_VERSION" >&2; exit 1; }
  printf '%s\n' "$TAURI_REVISION" > "$work/tools/tauri-revision"
  sudo rm -rf "$tools"
  sudo mkdir -p "$NATIVE_ROOT"
  sudo cp -R "$work/tools" "$tools"
  sudo rm -f "$tools/.crates.toml" "$tools/.crates2.json"
  writable_for_nobody "$tools"
fi

# --- CEF ----------------------------------------------------------------------------------------
manifest="$cef/$CEF_ARCHIVE.tree.sha256"
cef_ok() {
  [ -f "$dist/archive.json" ] \
    && grep -q "\"sha1\": *\"$CEF_SHA1\"" "$dist/archive.json" \
    && grep -q "\"name\": *\"$CEF_ARCHIVE\"" "$dist/archive.json" \
    && [ "$(shasum -a 1 "$cef/$CEF_ARCHIVE" 2>/dev/null | awk '{print $1}')" = "$CEF_SHA1" ] \
    && [ -f "$manifest" ] && (cd "$dist" && shasum -a 256 -c "$manifest" >/dev/null 2>&1)
}
if cef_ok; then
  echo "CEF $CEF_ARCHIVE is already installed and verified"
else
  # The published index must agree with the checksum pinned in the operator's config.
  curl -fsSL "$CEF_BASE_URL/index.json" -o "$work/index.json"
  python3 - "$work/index.json" "$CEF_ARCHIVE" "$CEF_SHA1" <<'PY'
import json, sys
index, name, sha1 = json.load(open(sys.argv[1])), sys.argv[2], sys.argv[3]
for version in index.get("macosarm64", {}).get("versions", []):
    for f in version.get("files", []):
        if f.get("name") == name:
            if f.get("sha1") != sha1:
                sys.exit(f"upstream lists sha1 {f.get('sha1')} for {name}, but {sha1} is pinned")
            sys.exit(0)
sys.exit(f"{name} is not in the published CEF builds index for macosarm64")
PY
  encoded="$(python3 -c 'import sys,urllib.parse; print(urllib.parse.quote(sys.argv[1]))' "$CEF_ARCHIVE")"
  curl -fSL "$CEF_BASE_URL/$encoded" -o "$work/$CEF_ARCHIVE"
  [ "$(shasum -a 1 "$work/$CEF_ARCHIVE" | awk '{print $1}')" = "$CEF_SHA1" ] \
    || { echo "downloaded $CEF_ARCHIVE does not match the pinned sha1 $CEF_SHA1" >&2; exit 1; }
  mkdir "$work/dist"
  tar -xjf "$work/$CEF_ARCHIVE" -C "$work/dist" --strip-components=1
  # The layout cef-dll-sys expects of an extracted distribution: the framework beside include/, not
  # under the Release/ folder CEF's archive puts it in.
  if [ -d "$work/dist/Release" ]; then
    mv "$work/dist/Release"/* "$work/dist/" && rmdir "$work/dist/Release"
  fi
  printf '{\n  "type": "minimal",\n  "name": "%s",\n  "sha1": "%s"\n}\n' "$CEF_ARCHIVE" "$CEF_SHA1" > "$work/dist/archive.json"
  (cd "$work/dist" && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 shasum -a 256) > "$work/manifest"
  sudo rm -rf "$cef"
  sudo mkdir -p "$cef"
  sudo cp -R "$work/dist" "$dist"
  sudo cp "$work/$CEF_ARCHIVE" "$cef/$CEF_ARCHIVE"
  sudo cp "$work/manifest" "$manifest"
  writable_for_nobody "$cef"
fi
cef_ok || { echo "CEF did not verify after installing" >&2; exit 1; }
echo "native prerequisites ready in $NATIVE_ROOT"
echo "  profile: tools_dir = \"$tools\", cef_path = \"$dist\", cef_sha1 = \"$CEF_SHA1\""
