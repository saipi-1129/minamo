#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
profile=${1:-debug}
case "$profile" in debug|check|install) swift_profile=debug ;; release) swift_profile=release ;; *) printf 'usage: %s [debug|release|check|install]\n' "$0" >&2; exit 2 ;; esac
build_root=${KICK_NATIVE_BUILD_ROOT:-/private/tmp/kick-native-build}
case "$build_root" in /private/tmp/*) ;; *) printf 'build root must be under /private/tmp\n' >&2; exit 2 ;; esac
mkdir -p "$build_root/module-cache" "$build_root/bin"
cat > "$build_root/bin/xcrun" <<'WRAPPER'
#!/bin/sh
if [ "$3" = metal ]; then
    exec /usr/bin/xcrun "$@" "-fmodules-cache-path=$KICK_NATIVE_METAL_CACHE"
fi
exec /usr/bin/xcrun "$@"
WRAPPER
chmod 755 "$build_root/bin/xcrun"
export KICK_NATIVE_METAL_CACHE="$build_root/module-cache"
export CLANG_MODULE_CACHE_PATH="$build_root/module-cache"
export SWIFTPM_MODULECACHE_OVERRIDE="$build_root/module-cache"
export PATH="$build_root/bin:$PATH"

# Keep the code signature stable across local rebuilds so macOS can retain the
# Keychain ACL for the saved session cookie. Ad-hoc signatures change on every
# build and cause the login-keychain password prompt to reappear.
if [ -z "${KICK_CODESIGN_IDENTITY:-}" ]; then
    stable_identity=$(security find-identity -v -p codesigning 2>/dev/null \
        | sed -n 's/.*"\(Apple Development: [^"]*\)"/\1/p' \
        | head -n 1)
    if [ -n "$stable_identity" ]; then
        KICK_CODESIGN_IDENTITY=$stable_identity
        export KICK_CODESIGN_IDENTITY
        printf 'Code signing identity: %s\n' "$stable_identity" >&2
    fi
fi

swift build --disable-sandbox --package-path "$root/crates/player-macos" --scratch-path "$build_root/swift" -c "$swift_profile" --product KickPlayerCBridge
KICK_PLAYER_MACOS_LIB_DIR=$(swift build --disable-sandbox --package-path "$root/crates/player-macos" --scratch-path "$build_root/swift" -c "$swift_profile" --show-bin-path)
export KICK_PLAYER_MACOS_LIB_DIR
cd "$root"
if [ "$profile" = check ]; then
    cargo fmt --all -- --check
    cargo check --workspace --all-features --locked --offline
    cargo test --workspace --all-features --locked --offline
    python3 -m unittest discover -s tools/bench -p 'test_*.py' -q
    exit 0
fi
if [ "$profile" = release ]; then
    cargo build -p kick-gpui-poc --locked --release
else
    cargo build -p kick-gpui-poc --locked
fi
if [ "$profile" = install ]; then
    KICK_BUILD_PROFILE=debug "$root/crates/app-ui/package-macos-app.sh"
    "$root/crates/app-ui/package-notifier-app.sh"
    exec "$root/tools/install-macos-app.sh"
fi
export KICK_BUILD_PROFILE="$profile"
exec "$root/crates/app-ui/package-macos-app.sh"
