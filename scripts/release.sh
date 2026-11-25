#!/bin/bash
# Builds a release into dist/: StoneForm-<version>.zip (the app, with the
# update feed) and appcast.xml (the Sparkle feed, signed with the EdDSA key).
#
#   VERSION              marketing version, e.g. 0.2.0 (required)
#   REPOSITORY           GitHub owner/name, e.g. trustybread/stoneform (required)
#   SPARKLE_PRIVATE_KEY  the EdDSA private key, as `generate_keys -x` exports
#                        it; without it the key comes from the login keychain
#                        (account "stoneform"), for releases made on a Mac
#   SIGNING_CERTIFICATE, SIGNING_CERTIFICATE_PASSWORD
#                        StoneForm's signing certificate (see bundle.sh);
#                        or SIGN_IDENTITY, an identity from your keychains.
#                        One is required: SIGN_IDENTITY=- makes an ad-hoc
#                        dry run, which installed copies must never get.
#
# The feed is https://github.com/<REPOSITORY>/releases/latest/download/appcast.xml:
# GitHub redirects it to the newest release's appcast.xml, so it never changes.
set -euo pipefail
cd "$(dirname "$0")/.."

version="${VERSION:?set VERSION, e.g. 0.2.0}"
repository="${REPOSITORY:?set REPOSITORY, e.g. trustybread/stoneform}"
feed="https://github.com/${repository}/releases/latest/download/appcast.xml"
downloads="https://github.com/${repository}/releases/download/v${version}/"
if [[ -z "${SIGNING_CERTIFICATE:-}${SIGN_IDENTITY:-}" ]]; then
    echo "Set SIGNING_CERTIFICATE (or SIGN_IDENTITY, - for an ad-hoc dry run)" >&2
    exit 1
fi

rm -rf dist
mkdir -p dist
APP_PATH=dist/StoneForm.app VERSION="$version" SPARKLE_FEED="$feed" scripts/bundle.sh
zip="dist/StoneForm-${version}.zip"
ditto -c -k --keepParent dist/StoneForm.app "$zip"

# The private key only ever sits in a file only we can read, gone on exit.
key_args=(--account stoneform)
if [[ -n "${SPARKLE_PRIVATE_KEY:-}" ]]; then
    key_file="$(umask 077 && mktemp)"
    trap 'rm -f "$key_file"' EXIT
    printf '%s' "$SPARKLE_PRIVATE_KEY" > "$key_file"
    key_args=(--ed-key-file "$key_file")
fi
feed_dir="$(mktemp -d)"
cp "$zip" "$feed_dir/"
"$(scripts/fetch-sparkle.sh)/bin/generate_appcast" "${key_args[@]}" --maximum-deltas 0 \
    --download-url-prefix "$downloads" --link "https://github.com/${repository}" \
    -o dist/appcast.xml "$feed_dir"
rm -rf "$feed_dir"
grep -q 'sparkle:edSignature' dist/appcast.xml || { echo "appcast.xml is not signed" >&2; exit 1; }
echo "Release ${version}: $(du -h "$zip" | cut -f1) zip, feed ${feed}"
