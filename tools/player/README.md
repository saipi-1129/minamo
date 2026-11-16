# Direct AVPlayer transport probe

This experimental command-line probe uses a unique bundle identifier, normal AVPlayer URL loading, and its own app cookie store (cookie acceptance disabled). It does not read Safari storage. It prints only URL-free state. Layer readiness without an attached window is not a visual playback test.

Build the probe under `/private/tmp`:

```sh
mkdir -p /private/tmp/KickDirectCheck-20261112.app/Contents/MacOS
cp tools/player/DirectCheck-Info.plist /private/tmp/KickDirectCheck-20261112.app/Contents/Info.plist
swiftc -parse-as-library tools/player/direct-check.swift -o /private/tmp/KickDirectCheck-20261112.app/Contents/MacOS/kick-direct-check
cargo build -p kick-live --features cli --locked
KICK_NATIVE_PROBE=/private/tmp/KickDirectCheck-20261112.app/Contents/MacOS/kick-direct-check target/debug/kick-live native-playback-check
```

The Rust CLI fetches a current live channel and passes its URL over stdin, never argv or logs. Success requires a ready item, numeric CMTime advancing at least three seconds after readiness, positive player rate, playing transport and layer readiness. The app aborts if its own cookie store is not empty. The 20-second deadline bounds the playback observation and the Rust parent terminates the child after 30 seconds; the CLI HTTP client has separate timeouts. No hardware-decoder or process-memory claim is made.

# Player control and VOD resume harnesses

`verify-controls.swift` drives a real `PlayerSession`, prints URL-free state (subtitle titles, selected index, video/track size, access-log bitrate, PiP state), and saves `NSView.cacheDisplay` captures of the native surface. The AVPlayerLayer video is black in those captures because layer-backed video is not part of `cacheDisplay`; the controls and labels are.

Build both harnesses after building the Swift package:

```sh
export CLANG_MODULE_CACHE_PATH=/private/tmp/kick-native-build-hover/module-cache
export SWIFTPM_MODULECACHE_OVERRIDE=$CLANG_MODULE_CACHE_PATH
swift build --disable-sandbox --package-path crates/player-macos \
  --scratch-path /private/tmp/kick-native-build-hover/swift -c debug
BIN=$(swift build --disable-sandbox --package-path crates/player-macos \
  --scratch-path /private/tmp/kick-native-build-hover/swift -c debug --show-bin-path)

swiftc -parse-as-library -I "$BIN" -L "$BIN" -lKickPlayerCore \
  -framework AppKit -framework AVFoundation -framework AVKit \
  tools/player/verify-controls.swift -o /private/tmp/kick-verify-controls
swiftc -parse-as-library -I "$BIN" -L "$BIN" -lKickPlayerCore \
  -framework AppKit -framework AVFoundation -framework AVKit \
  tools/player/verify-vod-resume.swift -o /private/tmp/kick-verify-vod
```

Run the control harness with any HLS URL. The default is Apple's `bipbop_16x9` sample, which has English, French, Spanish, and Japanese legible tracks. The harness never prints the URL.

```sh
KICK_CAPTURE_PREFIX=/private/tmp/kick-player-verify /private/tmp/kick-verify-controls
KICK_VOD_URL=<hls-url> /private/tmp/kick-verify-vod
```

The VOD harness starts the item, seeks to 120 s immediately, waits 12 s, and prints the resulting playhead; it then repeats with a seek issued after readiness. A 12-second wait is intentional: the check is whether AVPlayer honors the early seek, not whether the UI stores the position.

The control harness also clicks the native low-latency toggle after the subtitle capture and saves `-lowlatency-off.png`, so the on/off label can be compared without a live click.

The control harness also seeks 8.5 seconds back with `seekDVR(toSeconds:)`, waits 10 seconds, and prints `dvr_target`, `dvr_after`, and `dvr_after_10s`. This checks that low-latency catch-up does not pull a user-chosen DVR position back to live.

Set `KICK_VOD_MODE=1` to mark the harness item as a VOD and capture the `再生位置` label; the harness also clicks the native `ライブ` button after the DVR review and saves `-live-return.png`.
