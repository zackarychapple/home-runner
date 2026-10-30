#!/bin/sh
# Run once on the host. Creates the stable code-signing identity install-gui.sh signs hr-input
# with, so rebuilding the helper keeps the Accessibility grant already given in the golden image.
# Keep the file private to the operator: it is not part of any image. It is a local, self-signed
# identity, trusted by nothing except the grant you give it.
set -eu
out="${1:-$HOME/.home-runner/gui-signing.p12}"
[ ! -e "$out" ] || { echo "$out already exists; keep using it so grants stay valid" >&2; exit 1; }
dir="$(mktemp -d)"
trap 'rm -rf "$dir"' EXIT
cat > "$dir/openssl.cnf" <<CNF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = home-runner input helper
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
CNF
openssl req -x509 -newkey rsa:2048 -nodes -days 7300 -config "$dir/openssl.cnf" \
  -keyout "$dir/key.pem" -out "$dir/cert.pem" 2>/dev/null
mkdir -p "$(dirname "$out")"
openssl pkcs12 -export -inkey "$dir/key.pem" -in "$dir/cert.pem" -passout pass:home-runner \
  -name "home-runner input helper" -out "$out"
chmod 600 "$out"
echo "wrote $out; build the image with: ./images/macos-latest/build.sh GUI=true"
