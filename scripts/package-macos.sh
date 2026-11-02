#!/usr/bin/env bash
set -euo pipefail
lantern_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
lantern_arch="$(uname -m)"
lantern_stage="$lantern_root/work/package-macos"
lantern_release="$lantern_root/releases"
lantern_app="$lantern_root/apps/desktop/src-tauri/target/release/bundle/macos/Lantern Host.app"
cd "$lantern_root/apps/desktop"
pnpm tauri build --bundles app
codesign --force --deep --sign - "$lantern_app"
codesign --verify --deep --strict "$lantern_app"
mkdir -p "$lantern_stage" "$lantern_release"
ditto "$lantern_app" "$lantern_stage/Lantern Host.app"
if [[ ! -e "$lantern_stage/Applications" ]]; then ln -s /Applications "$lantern_stage/Applications"; fi
hdiutil create -volname "Lantern Host" -srcfolder "$lantern_stage" -ov -format UDZO "$lantern_release/Lantern-Host-0.1.0-macos-$lantern_arch.dmg"
ditto -c -k --sequesterRsrc --keepParent "$lantern_app" "$lantern_release/Lantern-Host-0.1.0-macos-$lantern_arch.zip"
cd "$lantern_release"
shasum -a 256 "Lantern-Host-0.1.0-macos-$lantern_arch.dmg" "Lantern-Host-0.1.0-macos-$lantern_arch.zip" > SHA256SUMS
