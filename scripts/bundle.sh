#!/bin/bash
# Builds StoneForm.app (arm64) with Sparkle inside.
#
#   APP_PATH        where to put the app (default: target/release/bundle/StoneForm.app)
#   VERSION         marketing version (default: latest v* tag, else 0.1.0)
#   BUILD_NUMBER    CFBundleVersion, what Sparkle compares (default: commit count)
#   BUNDLE_ID       bundle identifier (default: io.github.trustybread.stoneform)
#   SIGNING_CERTIFICATE  a base64 .p12 to sign with, as make-signing-cert.sh
#                   makes it; it goes into a throwaway keychain, never the
#                   login keychain. Releases are signed with StoneForm's own
#                   certificate: unlike an ad-hoc signature, it gives every
#                   build the same identity, so macOS keeps StoneForm's
#                   Keychain access across updates instead of asking for the
#                   Mac password.
#   SIGNING_CERTIFICATE_PASSWORD  that .p12's password
#   SIGN_IDENTITY   otherwise, an identity from your keychains (default:
#                   ad-hoc "-"). A "Developer ID Application" identity also
#                   gets the hardened runtime and a secure timestamp, which
#                   notarization requires.
#   SPARKLE_FEED    appcast URL for self-updates. Only release builds set it,
#                   so development copies never replace themselves.
#   SPARKLE_PUBLIC_KEY  EdDSA public key that updates must be signed with
#                   (default: macos/sparkle-public-key).
set -euo pipefail
cd "$(dirname "$0")/.."

latest_tag="$(git describe --tags --abbrev=0 --match 'v*' 2>/dev/null || true)"
latest_version="${latest_tag#v}"
version="${VERSION:-${latest_version:-0.1.0}}"
build_number="${BUILD_NUMBER:-$(git rev-list --count HEAD 2>/dev/null || echo 1)}"
bundle_id="${BUNDLE_ID:-io.github.trustybread.stoneform}"
identity="${SIGN_IDENTITY:--}"
feed="${SPARKLE_FEED:-}"
public_key="${SPARKLE_PUBLIC_KEY:-$(cat macos/sparkle-public-key 2>/dev/null || true)}"
if [[ -n "$feed" && -z "$public_key" ]]; then
    echo "A build with a feed needs the Sparkle public key (macos/sparkle-public-key)" >&2
    exit 1
fi

keychain=""
if [[ -n "${SIGNING_CERTIFICATE:-}" ]]; then
    keychain_dir="$(mktemp -d)"
    keychain="$keychain_dir/signing.keychain-db"
    keychain_password="$(uuidgen)"
    trap 'security delete-keychain "$keychain" 2>/dev/null || true; rm -rf "$keychain_dir"' EXIT
    security create-keychain -p "$keychain_password" "$keychain"
    security set-keychain-settings "$keychain" # no auto-lock while signing
    security unlock-keychain -p "$keychain_password" "$keychain"
    # codesign builds the certificate chain only from keychains on the search
    # list, so this one joins it until the trap deletes it (which also takes
    # it off the list). Without this, GitHub's macOS 15 runners fail with
    # "The specified item could not be found in the keychain".
    keychains=()
    while IFS= read -r line; do
        keychains+=("$(sed -E 's/^[[:space:]]*"(.*)"$/\1/' <<< "$line")")
    done < <(security list-keychains -d user)
    security list-keychains -d user -s "$keychain" ${keychains[@]+"${keychains[@]}"}
    printf '%s' "$SIGNING_CERTIFICATE" | base64 --decode > "$keychain_dir/signing.p12"
    security import "$keychain_dir/signing.p12" -k "$keychain" -T /usr/bin/codesign \
        -P "${SIGNING_CERTIFICATE_PASSWORD:?SIGNING_CERTIFICATE needs SIGNING_CERTIFICATE_PASSWORD}" >/dev/null
    rm "$keychain_dir/signing.p12"
    security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$keychain_password" "$keychain" >/dev/null
    identity="$(security find-identity -p codesigning "$keychain" | awk '$1 == "1)" { print $2; exit }')"
    [[ -n "$identity" ]] || { echo "SIGNING_CERTIFICATE holds no code signing identity" >&2; exit 1; }
fi

sparkle_dir="$(scripts/fetch-sparkle.sh)"
cargo build --release --locked -p stoneform-mac

app="${APP_PATH:-target/release/bundle/StoneForm.app}"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$app/Contents/Frameworks"
cp target/release/StoneForm "$app/Contents/MacOS/StoneForm"
[[ ! -f macos/AppIcon.icns ]] || cp macos/AppIcon.icns "$app/Contents/Resources/AppIcon.icns"

# StoneForm loads Sparkle at run time from here; its XPC services exist for
# sandboxed apps, which StoneForm isn't.
sparkle="$app/Contents/Frameworks/Sparkle.framework"
ditto "$sparkle_dir/Sparkle.framework" "$sparkle"
rm -rf "$sparkle/Versions/B/XPCServices" "$sparkle/XPCServices"

sparkle_keys=""
if [[ -n "$feed" ]]; then
    sparkle_keys="    <key>SUFeedURL</key><string>${feed}</string>
    <key>SUPublicEDKey</key><string>${public_key}</string>
    <key>SUEnableAutomaticChecks</key><true/>
    <key>SUAutomaticallyUpdate</key><true/>"
fi
icon_key=""
[[ ! -f macos/AppIcon.icns ]] || icon_key="    <key>CFBundleIconFile</key><string>AppIcon</string>"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key><string>en</string>
    <key>CFBundleExecutable</key><string>StoneForm</string>
    <key>CFBundleIdentifier</key><string>${bundle_id}</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleName</key><string>StoneForm</string>
    <key>CFBundleDisplayName</key><string>StoneForm</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>${version}</string>
    <key>CFBundleVersion</key><string>${build_number}</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.games</string>
    <key>LSMinimumSystemVersion</key><string>14.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
${icon_key}
${sparkle_keys}
</dict>
</plist>
PLIST
plutil -lint -s "$app/Contents/Info.plist"

# Sign inside out: Sparkle's helpers, the framework, then the app.
sign() {
    local options=(--force --sign "$identity")
    [[ -z "$keychain" ]] || options+=(--keychain "$keychain")
    # Both need an Apple-issued identity.
    [[ "$identity" != "Developer ID Application"* ]] || options+=(--options runtime --timestamp)
    codesign "${options[@]}" "$@"
}
sign "$sparkle/Versions/B/Autoupdate"
sign "$sparkle/Versions/B/Updater.app"
sign "$sparkle"
sign "$app"
codesign --verify --deep --strict "$app"
signer="$(codesign -dvv "$app" 2>&1 | sed -n 's/^Authority=//p' | head -1)"
echo "Built $app $version ($build_number, $(lipo -archs "$app/Contents/MacOS/StoneForm"), signed: ${signer:-ad-hoc}, feed: ${feed:-none})"
