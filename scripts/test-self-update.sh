#!/bin/bash
# End-to-end check of StoneForm's self-update: builds an "old" and a "new"
# ad-hoc copy under a throwaway bundle id, signs the new one with a
# throwaway EdDSA key, serves a Sparkle appcast from localhost, runs the old
# copy hidden with the autopilot off, and checks that Sparkle downloads the
# update by itself and installs it when the app quits.
#
# Needs OpenSSL with Ed25519 (Homebrew's). Doesn't touch the real StoneForm,
# its settings, the Keychain, or the game.
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\033[36m==>\033[0m %s\n' "$*"; }
fail() { printf '\033[31mFAIL:\033[0m %s\n' "$*" >&2; exit 1; }

bundle_id="io.github.trustybread.stoneform.updatetest"
port=$(( 20000 + RANDOM % 20000 ))
feed="http://127.0.0.1:$port/appcast.xml"
work="$(mktemp -d)"
installed="$work/Applications/StoneForm.app"
server_pid=""

cleanup() {
    osascript -e "tell application id \"$bundle_id\" to quit" >/dev/null 2>&1 || true
    [[ -z "$server_pid" ]] || { kill "$server_pid" && wait "$server_pid"; } 2>/dev/null || true
    sleep 1 # let the quitting app's last preference writes land before deleting them
    defaults delete "$bundle_id" >/dev/null 2>&1 || true
    rm -rf "$HOME/Library/Caches/$bundle_id" "$HOME/Library/HTTPStorages/$bundle_id" \
        "$HOME/Library/WebKit/$bundle_id" "$HOME/Library/Preferences/$bundle_id.plist" "$work"
}
trap cleanup EXIT

step "Making a throwaway update key"
openssl genpkey -algorithm ed25519 -out "$work/key.pem"
openssl pkey -in "$work/key.pem" -outform DER | tail -c 32 | base64 > "$work/private-key"
public_key="$(openssl pkey -in "$work/key.pem" -pubout -outform DER | tail -c 32 | base64)"

step "Building old (0.0.1) and new (0.0.2) copies"
for pair in "0.0.1 1" "0.0.2 2"; do
    read -r version number <<< "$pair"
    APP_PATH="$work/build-$number/StoneForm.app" VERSION="$version" BUILD_NUMBER="$number" \
        BUNDLE_ID="$bundle_id" SPARKLE_FEED="$feed" SPARKLE_PUBLIC_KEY="$public_key" \
        scripts/bundle.sh | tail -1
done

step "Publishing 0.0.2 on a local feed"
sparkle_dir="$(scripts/fetch-sparkle.sh)"
mkdir -p "$work/feed" "$work/Applications"
ditto -c -k --keepParent "$work/build-2/StoneForm.app" "$work/feed/StoneForm-0.0.2.zip"
"$sparkle_dir/bin/generate_appcast" --ed-key-file "$work/private-key" --maximum-deltas 0 \
    --download-url-prefix "http://127.0.0.1:$port/" -o "$work/feed/appcast.xml" "$work/feed"
(cd "$work/feed" && exec python3 -m http.server "$port" --bind 127.0.0.1 >/dev/null 2>&1) &
server_pid=$!
for _ in $(seq 50); do curl -fs "$feed" >/dev/null && break; sleep 0.1; done
curl -fs "$feed" | grep -q 'sparkle:edSignature' || fail "feed not served"

step "Running 0.0.1 hidden; Sparkle should download the update by itself"
ditto "$work/build-1/StoneForm.app" "$installed"
open -g -j -n --env STONEFORM_SELF_UPDATE_TEST=1 "$installed"
for _ in $(seq 90); do
    [[ -d "$HOME/Library/Caches/$bundle_id/org.sparkle-project.Sparkle/Installation" ]] && break
    sleep 1
done
[[ -d "$HOME/Library/Caches/$bundle_id/org.sparkle-project.Sparkle" ]] || fail "Sparkle never downloaded the update"
sleep 3

step "Quitting; Sparkle installs on quit"
osascript -e "tell application id \"$bundle_id\" to quit"
version=""
for _ in $(seq 60); do
    version="$(/usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' "$installed/Contents/Info.plist" 2>/dev/null || true)"
    [[ "$version" == "0.0.2" ]] && break
    sleep 1
done
[[ "$version" == "0.0.2" ]] || fail "still ${version:-unreadable} after quitting"
codesign --verify --deep --strict "$installed" || fail "the updated app's signature is invalid"
! pgrep -f "$installed/Contents/MacOS/StoneForm" >/dev/null || fail "Sparkle relaunched the app; install-on-quit should stay quiet"
printf '\033[32mPASS:\033[0m 0.0.1 updated itself to 0.0.2 in the background and installed on quit\n'
