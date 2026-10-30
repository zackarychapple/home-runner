#!/bin/bash
# Runs inside the VM (as the admin user, with passwordless sudo) while build.sh builds the image
# with GUI=true. Builds hr-input from hr-input.swift, signs it with the operator's stable signing
# identity and installs it root-owned and read-only, so the one-time Accessibility grant survives
# rebuilds of the helper. The private key never stays in the image.
#
#   HR_INPUT_SRC   directory holding hr-input.swift (default: this script's directory)
#   HR_INPUT_P12   a PKCS#12 file with the signing identity (optional; without it the helper is
#                  signed ad hoc and every rebuild needs a new grant)
#   HR_INPUT_DEST  install path (default /usr/local/libexec/home-runner/hr-input)
set -euo pipefail
src="${HR_INPUT_SRC:-$(cd "$(dirname "$0")" && pwd)}"
dest="${HR_INPUT_DEST:-/usr/local/libexec/home-runner/hr-input}"
identifier=ai.home-runner.input
work="$(mktemp -d)"
keychain="$work/signing.keychain-db"
cleanup() {
  security delete-keychain "$keychain" >/dev/null 2>&1 || true
  # The default keychain list may still name it.
  security list-keychains -d user -s "$HOME/Library/Keychains/login.keychain-db" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

# The revision is the SHA-256 of the source; home-runner computes the same from its own copy and
# refuses to take jobs on a worker whose helper reports another one.
rev="$(shasum -a 256 "$src/hr-input.swift" | awk '{print $1}')"
printf 'let helperRevision = "%s"\n' "$rev" > "$work/main.swift"
cat "$src/hr-input.swift" >> "$work/main.swift"
swiftc -swift-version 5 -O "$work/main.swift" -o "$work/hr-input"

if [ -n "${HR_INPUT_P12:-}" ]; then
  pw=home-runner   # not a secret: the identity only gives the helper a stable designated requirement
  security create-keychain -p "$pw" "$keychain"
  security set-keychain-settings "$keychain"
  security unlock-keychain -p "$pw" "$keychain"
  security import "$HR_INPUT_P12" -k "$keychain" -P "$pw" -T /usr/bin/codesign >/dev/null
  security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$pw" "$keychain" >/dev/null
  security list-keychains -d user -s "$keychain" "$HOME/Library/Keychains/login.keychain-db"
  codesign --force --sign "home-runner input helper" --identifier "$identifier" "$work/hr-input"
else
  echo "warning: no HR_INPUT_P12; signing ad hoc, so the Accessibility grant is tied to this exact build" >&2
  codesign --force --sign - --identifier "$identifier" "$work/hr-input"
fi
codesign --verify --strict "$work/hr-input"

sudo mkdir -p "$(dirname "$dest")"
sudo install -m 755 -o root -g wheel "$work/hr-input" "$dest"
sudo chmod 755 "$(dirname "$dest")"
sudo chown root:wheel "$(dirname "$dest")"
echo "installed $dest (rev $rev, $(codesign -dr - "$dest" 2>&1 | sed -n 's/^designated => //p'))"
