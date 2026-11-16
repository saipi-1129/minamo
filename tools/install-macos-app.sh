#!/bin/sh
# Installs the latest packaged Kick Native app into /Applications (or
# KICK_INSTALL_PATH) and relaunches it. The previous bundle is kept under
# /private/tmp until the new one passes signature verification.
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
source_app=${1:-/private/tmp/KickNative.app}
destination=${KICK_INSTALL_PATH:-/Applications/KickNative.app}

if [ ! -d "$source_app" ]; then
    printf 'source app not found: %s\n' "$source_app" >&2
    printf 'run tools/build-macos.sh debug first\n' >&2
    exit 2
fi
case "$destination" in
    /*/KickNative.app) ;;
    *)
        printf 'destination must end with /KickNative.app: %s\n' "$destination" >&2
        exit 2
        ;;
esac

codesign --verify --deep --strict "$source_app"

# Quit a running copy so the bundle can be replaced. The app's quit menu item
# is the graceful path; the pkill fallback only matches this bundle name.
osascript -e 'tell application "Kick Native" to quit' >/dev/null 2>&1 || true
sleep 1
pkill -f '/KickNative.app/Contents/MacOS/kick-gpui-poc' >/dev/null 2>&1 || true
sleep 1

staging="${destination}.staging.$$"
backup=""
cleanup() {
    rm -rf "$staging"
    if [ -n "$backup" ] && [ ! -e "$destination" ] && [ -d "$backup" ]; then
        mv "$backup" "$destination"
    fi
}
trap cleanup EXIT HUP INT TERM

ditto "$source_app" "$staging"
codesign --verify --deep --strict "$staging"

if [ -e "$destination" ]; then
    backup="/private/tmp/KickNative.previous.$(date +%Y%m%d%H%M%S).app"
    mv "$destination" "$backup"
fi
mv "$staging" "$destination"
codesign --verify --deep --strict "$destination"

trap - EXIT HUP INT TERM
printf 'Installed: %s\n' "$destination"
if [ -n "$backup" ]; then
    printf 'Previous bundle: %s\n' "$backup"
fi

# The headless notification helper is installed alongside the app. It is not
# loaded until the user enables background notifications in Settings.
notifier_source=/private/tmp/KickNativeNotifier.app
notifier_destination=/Applications/KickNativeNotifier.app
if [ -d "$notifier_source" ]; then
    codesign --verify --deep --strict "$notifier_source"
    notifier_staging="${notifier_destination}.staging.$$"
    ditto "$notifier_source" "$notifier_staging"
    if [ -e "$notifier_destination" ]; then
        mv "$notifier_destination" "/private/tmp/KickNativeNotifier.previous.$(date +%Y%m%d%H%M%S).app"
    fi
    mv "$notifier_staging" "$notifier_destination"
    codesign --verify --deep --strict "$notifier_destination"
    printf 'Installed notifier: %s\n' "$notifier_destination"
fi

open "$destination"
