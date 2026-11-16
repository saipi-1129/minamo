#!/bin/bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
    printf 'Usage: %s <binary> <output.app>\n' "$0" >&2
    exit 2
fi

binary=$1
output=$2
[[ "$binary" == /* && -x "$binary" ]] || { printf 'binary must be an absolute executable path\n' >&2; exit 1; }
[[ "$output" == /private/tmp/*.app && "$(dirname "$output")" == /private/tmp ]] \
    || { printf 'output must be a direct child app bundle under /private/tmp\n' >&2; exit 1; }
[[ ! -e "$output" && ! -L "$output" ]] || { printf 'refusing to replace existing output: %s\n' "$output" >&2; exit 1; }

bundle_name=$(basename "$output" .app)
bundle_id=dev.codex.kick-player-gpui-avplayer-poc
staging=$(mktemp -d /private/tmp/kick-gpui-player-package.XXXXXX)
trap 'rm -rf "$staging"' EXIT
app="$staging/$bundle_name.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
install -m 755 "$binary" "$app/Contents/MacOS/kick-player-gpui-poc"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key><string>kick-player-gpui-poc</string>
    <key>CFBundleIdentifier</key><string>$bundle_id</string>
    <key>CFBundleName</key><string>$bundle_name</string>
    <key>CFBundleDisplayName</key><string>$bundle_name</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleShortVersionString</key><string>0.1</string>
    <key>CFBundleVersion</key><string>1</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>NSPrincipalClass</key><string>NSApplication</string>
</dict>
</plist>
PLIST
printf 'APPL????' > "$app/Contents/PkgInfo"
plutil -lint "$app/Contents/Info.plist" >/dev/null
xcrun lipo -verify_arch arm64 "$app/Contents/MacOS/kick-player-gpui-poc" >/dev/null
xattr -cr "$app"
codesign --force --deep --sign - --timestamp=none "$app"
codesign --verify --deep --strict "$app"
mv "$app" "$output"
codesign --verify --deep --strict "$output"
printf 'Packaged arm64 GPUI player PoC: %s\n' "$output"
