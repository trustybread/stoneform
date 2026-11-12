#!/bin/bash
# Builds macos/AppIcon.icns from macos/AppIcon.svg, with macos/AppIcon-small.svg
# for the 16 px image. Run it after changing either SVG and commit the result;
# bundle.sh only copies the .icns.
set -euo pipefail
cd "$(dirname "$0")/.."

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
swiftc -O -o "$work/render" scripts/render-icon.swift

iconset="$work/AppIcon.iconset"
mkdir "$iconset"
render() { "$work/render" "$1" "$2" "$iconset/$3"; }
render macos/AppIcon-small.svg 16 icon_16x16.png
for points in 16 32 128 256 512; do
    [[ $points == 16 ]] || render macos/AppIcon.svg "$points" "icon_${points}x${points}.png"
    render macos/AppIcon.svg $((points * 2)) "icon_${points}x${points}@2x.png"
done
iconutil -c icns -o macos/AppIcon.icns "$iconset"
echo "Built macos/AppIcon.icns ($(du -h macos/AppIcon.icns | cut -f1))"
