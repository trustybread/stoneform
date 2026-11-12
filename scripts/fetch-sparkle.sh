#!/bin/bash
# Downloads the pinned Sparkle release into target/sparkle/<version> and
# prints that folder. It holds Sparkle.framework and bin/ (generate_keys,
# sign_update, generate_appcast). The download is checked against its
# SHA-256; a cached copy is reused.
set -euo pipefail
cd "$(dirname "$0")/.."

version="2.10.1"
sha256="1a856d2a9edb8e628de88121d7be04cf0e2167f8641650b78fca00b14244cf27"
url="https://github.com/sparkle-project/Sparkle/releases/download/${version}/Sparkle-${version}.tar.xz"
dir="target/sparkle/${version}"

if [[ ! -d "$dir/Sparkle.framework" ]]; then
    mkdir -p "$dir"
    archive="$dir/Sparkle-${version}.tar.xz"
    curl -fsSL --retry 3 -o "$archive" "$url"
    actual="$(shasum -a 256 "$archive" | cut -d' ' -f1)"
    if [[ "$actual" != "$sha256" ]]; then
        echo "Sparkle ${version}: SHA-256 ${actual}, expected ${sha256}" >&2
        rm -f "$archive"
        exit 1
    fi
    tar -xf "$archive" -C "$dir"
    rm "$archive"
fi
echo "$dir"
