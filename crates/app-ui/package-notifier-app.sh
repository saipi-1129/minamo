#!/bin/sh
# Builds the headless notification helper app from the packaged main app.
# The helper has its own bundle identity so macOS can grant it notification
# permission independently, and LSUIElement keeps it out of the Dock/menu bar.
set -eu

fail() {
    printf 'package-notifier-app.sh: %s\n' "$1" >&2
    exit 1
}

source_app=${1:-/private/tmp/Minamo.app}
output_app=${2:-/private/tmp/MinamoNotifier.app}

[ -d "$source_app" ] || fail "source app not found: $source_app"
[ "$output_app" = /private/tmp/MinamoNotifier.app ] \
    || fail "output must be /private/tmp/MinamoNotifier.app"
[ ! -L "$output_app" ] || fail "refusing to replace an output symlink"

staging_directory=$(mktemp -d /private/tmp/kick-notifier-package.XXXXXX)
cleanup() {
    rm -rf "$staging_directory"
}
trap cleanup EXIT HUP INT TERM

bundle_directory="$staging_directory/MinamoNotifier.app"
ditto "$source_app" "$bundle_directory"
plist="$bundle_directory/Contents/Info.plist"

/usr/libexec/PlistBuddy -c 'Set :CFBundleIdentifier dev.codex.minamo.notifier' "$plist"
/usr/libexec/PlistBuddy -c 'Set :CFBundleName Minamo Notifier' "$plist"
/usr/libexec/PlistBuddy -c 'Set :CFBundleDisplayName Minamo Notifier' "$plist"
if /usr/libexec/PlistBuddy -c 'Print :LSUIElement' "$plist" >/dev/null 2>&1; then
    /usr/libexec/PlistBuddy -c 'Set :LSUIElement true' "$plist"
else
    /usr/libexec/PlistBuddy -c 'Add :LSUIElement bool true' "$plist"
fi
plutil -lint "$plist" >/dev/null || fail "generated helper Info.plist is invalid"

xattr -cr "$bundle_directory"
codesign --force --deep --sign "${KICK_CODESIGN_IDENTITY:--}" --timestamp=none "$bundle_directory"
codesign --verify --deep --strict "$bundle_directory"

if [ -e "$output_app" ]; then
    backup_directory=$(mktemp -d /private/tmp/kick-notifier-previous.XXXXXX)
    mv "$output_app" "$backup_directory/MinamoNotifier.app"
fi
if ! mv "$bundle_directory" "$output_app"; then
    if [ -n "${backup_directory:-}" ]; then
        mv "$backup_directory/MinamoNotifier.app" "$output_app"
    fi
    fail "could not publish verified helper; previous build restored"
fi
codesign --verify --deep --strict "$output_app"

printf 'Packaged notifier app: %s\n' "$output_app"
printf 'Bundle ID: dev.codex.minamo.notifier\n'
