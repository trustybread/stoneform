#!/bin/bash
# Makes StoneForm's release signing certificate: self-signed, for code
# signing only, valid for 20 years. Releases signed with it share one
# identity, so macOS keeps StoneForm's Keychain access across updates.
# Gatekeeper treats it like an ad-hoc signature: only a Developer ID
# avoids "Open Anyway" on the first open.
#
#   scripts/make-signing-cert.sh [out-dir]      (default: the current directory)
#   CERT_NAME   the certificate's name (default: StoneForm)
#
# Writes stoneform-signing.p12 and stoneform-signing-password.txt: keep both
# in a password manager, give them to the release workflow (see README),
# then delete them.
set -euo pipefail

out="${1:-.}"
name="${CERT_NAME:-StoneForm}"
umask 077
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cat > "$work/cert.cnf" <<CNF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = ${name}
[ext]
basicConstraints = critical, CA:false
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
subjectKeyIdentifier = hash
CNF

# macOS's own LibreSSL: its .p12 uses the algorithms `security import` reads,
# where OpenSSL 3's defaults would not.
P12_PASSWORD="$(/usr/bin/openssl rand -hex 24)"
export P12_PASSWORD
/usr/bin/openssl req -x509 -newkey rsa:3072 -nodes -days 7305 -config "$work/cert.cnf" \
    -keyout "$work/key.pem" -out "$work/cert.pem" 2>/dev/null
/usr/bin/openssl pkcs12 -export -inkey "$work/key.pem" -in "$work/cert.pem" -name "$name" \
    -passout env:P12_PASSWORD -out "$out/stoneform-signing.p12"
printf '%s' "$P12_PASSWORD" > "$out/stoneform-signing-password.txt"
echo "Wrote $out/stoneform-signing.p12 and $out/stoneform-signing-password.txt ($name, until $(/usr/bin/openssl x509 -in "$work/cert.pem" -noout -enddate | cut -d= -f2))"
