#!/bin/sh
set -eu

fail() {
    printf 'package-macos-app.sh: %s\n' "$1" >&2
    exit 1
}

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
workspace_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)
profile=${KICK_BUILD_PROFILE:-debug}
case "$profile" in debug|release) ;; *) fail "KICK_BUILD_PROFILE must be debug or release" ;; esac
target_dir=${CARGO_TARGET_DIR:-$workspace_root/target}
case "$target_dir" in /*) ;; *) target_dir="$workspace_root/$target_dir" ;; esac
binary="$target_dir/$profile/kick-gpui-poc"
output_app=${1:-/private/tmp/KickNative.app}

case "$output_app" in
    /private/tmp/*.app) ;;
    *) fail "output must be a direct child app bundle under /private/tmp" ;;
esac
[ "$(dirname "$output_app")" = /private/tmp ] \
    || fail "output must be a direct child app bundle under /private/tmp"
[ ! -L "$output_app" ] || fail "refusing to replace an output symlink"
if [ -e "$output_app" ]; then
    [ "$output_app" = /private/tmp/KickNative.app ] \
        || fail "only the fixed development bundle can be replaced"
    [ -d "$output_app" ] || fail "existing output is not an app directory"
    existing_id=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$output_app/Contents/Info.plist")
    [ "$existing_id" = dev.codex.kick-native ] || fail "existing app identity does not match"
fi
[ -x "$binary" ] || fail "missing executable: $binary (build it with cargo build -p kick-gpui-poc)"
xcrun lipo -verify_arch arm64 "$binary" >/dev/null 2>&1 \
    || fail "binary does not contain the arm64 architecture"

staging_directory=$(mktemp -d /private/tmp/kick-gpui-package.XXXXXX)
cleanup() {
    rm -rf "$staging_directory"
}
trap cleanup EXIT HUP INT TERM

bundle_directory="$staging_directory/$(basename "$output_app")"
contents="$bundle_directory/Contents"
mkdir -p "$contents/MacOS" "$contents/Resources"
install -m 755 "$binary" "$contents/MacOS/kick-gpui-poc"

app_icon="$script_dir/assets/AppIcon.icns"
[ -f "$app_icon" ] || fail "missing app icon: $app_icon"
install -m 644 "$app_icon" "$contents/Resources/AppIcon.icns"

# Include the native bridge only when this executable links it.
if otool -L "$contents/MacOS/kick-gpui-poc" | awk '$1 == "@rpath/libKickPlayerCBridge.dylib" { found=1 } END { exit !found }'; then
    library_dir=${KICK_PLAYER_MACOS_LIB_DIR:?set KICK_PLAYER_MACOS_LIB_DIR to the built SwiftPM products}
    library="$library_dir/libKickPlayerCBridge.dylib"
    [ -f "$library" ] || fail "native player dylib is missing"
    xcrun lipo -verify_arch arm64 "$library" >/dev/null 2>&1 || fail "player dylib is not arm64"
    mkdir -p "$contents/Frameworks"
    install -m 755 "$library" "$contents/Frameworks/libKickPlayerCBridge.dylib"
    install_name_tool -id @rpath/libKickPlayerCBridge.dylib "$contents/Frameworks/libKickPlayerCBridge.dylib"
    # SwiftPM/Xcode emit build-machine search paths. This bridge depends only
    # on system frameworks and /usr/lib/swift, so no private build rpath is needed.
    otool -l "$contents/Frameworks/libKickPlayerCBridge.dylib" \
        | awk '$1 == "cmd" { rpath=($2 == "LC_RPATH") } rpath && $1 == "path" { print $2 }' \
        > "$staging_directory/bridge-rpaths"
    while IFS= read -r bridge_rpath; do
        case "$bridge_rpath" in
            /usr/lib/swift) ;;
            *) install_name_tool -delete_rpath "$bridge_rpath" "$contents/Frameworks/libKickPlayerCBridge.dylib" ;;
        esac
    done < "$staging_directory/bridge-rpaths"
    if ! otool -l "$contents/MacOS/kick-gpui-poc" | awk '$1 == "path" && $2 == "@executable_path/../Frameworks" { found=1 } END { exit !found }'; then
        install_name_tool -add_rpath @executable_path/../Frameworks "$contents/MacOS/kick-gpui-poc"
    fi
    if otool -l "$contents/MacOS/kick-gpui-poc" | awk -v dir="$library_dir" '$1 == "path" && $2 == dir { found=1 } END { exit !found }'; then
        install_name_tool -delete_rpath "$library_dir" "$contents/MacOS/kick-gpui-poc"
    fi
fi

cat > "$contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleExecutable</key>
  <string>kick-gpui-poc</string>
  <key>CFBundleIdentifier</key>
  <string>dev.codex.kick-native</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>Kick Native</string>
  <key>CFBundleDisplayName</key>
  <string>Kick Native</string>
  <key>CFBundleIconFile</key>
  <string>AppIcon</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>0.1.0</string>
  <key>CFBundleVersion</key>
  <string>0.1.0</string>
  <key>LSMinimumSystemVersion</key>
  <string>13.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSAppTransportSecurity</key>
  <dict>
    <key>NSAllowsLocalNetworking</key>
    <true/>
  </dict>
  <key>NSPrincipalClass</key>
  <string>NSApplication</string>
</dict>
</plist>
PLIST

printf 'APPL????' > "$contents/PkgInfo"
plutil -lint "$contents/Info.plist" >/dev/null \
    || fail "generated Info.plist is invalid"
plist_executable=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$contents/Info.plist")
[ "$plist_executable" = kick-gpui-poc ] \
    || fail "CFBundleExecutable does not match the packaged executable"
[ -x "$contents/MacOS/kick-gpui-poc" ] \
    || fail "packaged executable is missing or not executable"

# Strip metadata only from this generated bundle. The source binary is untouched.
xattr -cr "$bundle_directory"
if [ -f "$contents/Frameworks/libKickPlayerCBridge.dylib" ]; then
    codesign --force --sign "${KICK_CODESIGN_IDENTITY:--}" --timestamp=none "$contents/Frameworks/libKickPlayerCBridge.dylib"
    codesign --verify --strict "$contents/Frameworks/libKickPlayerCBridge.dylib"
fi
codesign --force --deep --sign "${KICK_CODESIGN_IDENTITY:--}" --timestamp=none "$bundle_directory"
codesign --verify --deep --strict --verbose=2 "$bundle_directory"
if [ -e "$output_app" ]; then
    backup_directory=$(mktemp -d /private/tmp/kick-native-previous.XXXXXX)
    mv "$output_app" "$backup_directory/KickNative.app"
fi
if ! mv "$bundle_directory" "$output_app"; then
    if [ -n "${backup_directory:-}" ]; then
        mv "$backup_directory/KickNative.app" "$output_app"
    fi
    fail "could not publish verified app; previous build restored"
fi
codesign --verify --deep --strict --verbose=2 "$output_app"

printf 'Packaged GPUI app: %s\n' "$output_app"
printf 'Bundle ID: dev.codex.kick-native\nArchitecture: arm64\n'
