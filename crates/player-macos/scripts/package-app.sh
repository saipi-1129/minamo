#!/bin/bash
set -euo pipefail

usage() {
    cat >&2 <<'USAGE'
Usage: package-app.sh [--smoke] <kick-player-demo-binary> [output.app]

Builds and ad-hoc signs a minimal macOS app bundle under /private/tmp.
The --smoke bundle sets KICK_PLAYER_SMOKE=1 through LaunchServices.
USAGE
}

fail() {
    printf 'package-app.sh: %s\n' "$1" >&2
    exit 1
}

mode=demo
if [[ "${1:-}" == "--smoke" ]]; then
    mode=smoke
    shift
fi

if [[ $# -lt 1 || $# -gt 2 ]]; then
    usage
    exit 2
fi

binary_argument=$1
if [[ "$binary_argument" == /* ]]; then
    binary_path=$binary_argument
else
    binary_path="$(pwd -P)/$binary_argument"
fi
[[ -f "$binary_path" ]] || fail "binary not found: $binary_path"
[[ -x "$binary_path" ]] || fail "binary is not executable: $binary_path"
xcrun lipo -verify_arch arm64 "$binary_path" >/dev/null 2>&1 \
    || fail "binary does not contain the arm64 architecture"

executable_name=kick-player-demo
if [[ "$mode" == smoke ]]; then
    bundle_identifier=dev.codex.kick-player-macos-smoke-ready
    bundle_name=KickPlayerSmokeReady
    default_output=/private/tmp/KickPlayerSmokeReady.app
else
    bundle_identifier=dev.codex.kick-player-macos-poc-ready
    bundle_name=KickPlayerReady
    default_output=/private/tmp/KickPlayerReady.app
fi

output_app=${2:-$default_output}
[[ "$output_app" == /private/tmp/*.app ]] \
    || fail "output must be a direct child app bundle under /private/tmp"
[[ "$(dirname "$output_app")" == /private/tmp ]] \
    || fail "output must be a direct child app bundle under /private/tmp"
[[ ! -L "$output_app" ]] || fail "refusing to replace a symlink output"

[[ ! -e "$output_app" ]] \
    || fail "output already exists; choose a new path or remove it after closing the app"

staging_directory=$(mktemp -d /private/tmp/kick-player-package.XXXXXX)
cleanup() {
    rm -rf "$staging_directory"
}
trap cleanup EXIT

bundle_directory="$staging_directory/$bundle_name.app"
contents_directory="$bundle_directory/Contents"
binary_directory="$contents_directory/MacOS"
mkdir -p "$binary_directory" "$contents_directory/Resources"
install -m 755 "$binary_path" "$binary_directory/$executable_name"

info_plist="$contents_directory/Info.plist"
cat > "$info_plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleExecutable</key>
    <string>$executable_name</string>
    <key>CFBundleIdentifier</key>
    <string>$bundle_identifier</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>$bundle_name</string>
    <key>CFBundleDisplayName</key>
    <string>$bundle_name</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>LSMinimumSystemVersion</key>
    <string>13.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSPrincipalClass</key>
    <string>NSApplication</string>
</dict>
</plist>
PLIST

if [[ "$mode" == smoke ]]; then
    /usr/libexec/PlistBuddy -c 'Add :LSEnvironment dict' "$info_plist"
    /usr/libexec/PlistBuddy -c 'Add :LSEnvironment:KICK_PLAYER_SMOKE string 1' "$info_plist"
fi

printf 'APPL????' > "$contents_directory/PkgInfo"
plutil -lint "$info_plist" >/dev/null || fail "generated Info.plist is invalid"

plist_executable=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$info_plist")
principal_class=$(/usr/libexec/PlistBuddy -c 'Print :NSPrincipalClass' "$info_plist")
[[ "$plist_executable" == "$executable_name" ]] \
    || fail "CFBundleExecutable does not match the executable filename"
[[ "$principal_class" == NSApplication ]] \
    || fail "NSPrincipalClass must be NSApplication"
bundle_executable="$contents_directory/MacOS/$plist_executable"
[[ -f "$bundle_executable" && -x "$bundle_executable" ]] \
    || fail "CFBundleExecutable path is missing or not executable"
xcrun lipo -verify_arch arm64 "$bundle_executable" >/dev/null 2>&1 \
    || fail "packaged executable does not contain the arm64 architecture"

if [[ "$mode" == smoke ]]; then
    smoke_value=$(/usr/libexec/PlistBuddy -c 'Print :LSEnvironment:KICK_PLAYER_SMOKE' "$info_plist")
    [[ "$smoke_value" == 1 ]] || fail "smoke bundle environment is missing"
fi

# Strip metadata only from this newly generated app bundle, never from the source binary.
xattr -cr "$bundle_directory"
codesign --force --deep --sign - --timestamp=none "$bundle_directory"
codesign --verify --deep --strict --verbose=2 "$bundle_directory"

mv "$bundle_directory" "$output_app"
codesign --verify --deep --strict --verbose=2 "$output_app"

printf 'Packaged %s bundle: %s\n' "$mode" "$output_app"
printf 'Architecture: arm64\nPrincipal class: %s\n' "$principal_class"
if [[ "$mode" == smoke ]]; then
    printf 'LaunchServices smoke flag: KICK_PLAYER_SMOKE=1\nSmoke log: /tmp/kick-player-smoke.log\n'
fi
