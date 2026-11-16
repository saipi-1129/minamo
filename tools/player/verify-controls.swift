// Verification harness for the native player surface.
//
// It plays an HLS playlist, prints the legible-track and rendition state, and
// saves PNG captures of the AppKit surface with the controls visible. The
// captures use NSView.cacheDisplay, so they do not need Screen Recording
// permission. The AVPlayerLayer video itself is not part of the capture; the
// control overlay and its labels are.
//
// Build (after `swift build` for the package):
//   swiftc -I <bin-path> -L <bin-path> -lKickPlayerCore \
//     -framework AppKit -framework AVFoundation -framework AVKit \
//     tools/player/verify-controls.swift -o /private/tmp/kick-verify-controls
//
// Run:
//   KICK_CAPTURE_PREFIX=/private/tmp/kick-player-verify \
//     /private/tmp/kick-verify-controls
//
// KICK_PLAYER_URL overrides the default Apple HLS sample, which carries
// English, French, Spanish, and Japanese legible tracks.

import AppKit
import AVFoundation
import KickPlayerCore

@main
@MainActor
struct VerifyControls {
    static let appleHLSFixture =
        "https://devstreaming-cdn.apple.com/videos/streaming/examples/bipbop_16x9/bipbop_16x9_variant.m3u8"

    static func main() {
        let app = NSApplication.shared
        app.setActivationPolicy(.regular)

        let session = PlayerSession()
        let window = NSWindow(
            contentRect: NSRect(x: 80, y: 80, width: 960, height: 600),
            styleMask: [.titled, .closable],
            backing: .buffered,
            defer: false
        )
        window.title = "Kick Player Verify"
        window.contentView = session.surface
        window.makeKeyAndOrderFront(nil)
        app.activate(ignoringOtherApps: true)

        let environment = ProcessInfo.processInfo.environment
        let url = environment["KICK_PLAYER_URL"] ?? appleHLSFixture
        let capturePrefix = environment["KICK_CAPTURE_PREFIX"] ?? "/private/tmp/kick-player-verify"

        do {
            try session.playHLS(url)
            if environment["KICK_VOD_MODE"] == "1" {
                session.setVodMode(true)
            }
        } catch {
            print("play_error=\(error.localizedDescription)")
            exit(1)
        }

        wait(seconds: 12)
        print("state=\(session.stateCode)")
        print("titles=\(session.subtitleTrackTitles)")
        print("selected=\(session.selectedSubtitleTrackIndex)")
        print(
            "video=\(sizeDescription(session.currentVideoSize)) "
                + "track=\(sizeDescription(session.currentVideoTrackSize)) "
                + "indicated=\(Int(session.currentIndicatedBitrate)) "
                + "observed=\(Int(session.currentObservedBitrate))"
        )

        if let index = session.subtitleTrackTitles.firstIndex(where: {
            $0.contains("Japanese") && !$0.contains("Forced")
        }) {
            let accepted = session.setSubtitle(index: index + 1)
            wait(seconds: 1)
            print("subtitle_select=\(accepted) selected=\(session.selectedSubtitleTrackIndex)")
        } else {
            print("subtitle_select=no_japanese_track")
        }
        session.surface.showControls()
        wait(seconds: 1)
        capture(session.surface, path: "\(capturePrefix)-subtitles.png")

        if let current = session.currentSnapshot().currentTimeSeconds {
            let target = max(0, current - 8.5)
            session.seekDVR(toSeconds: target)
            wait(seconds: 2)
            let after = session.currentSnapshot().currentTimeSeconds ?? -1
            print("dvr_target=\(String(format: "%.1f", target)) dvr_after=\(String(format: "%.1f", after))")
            wait(seconds: 8)
            let later = session.currentSnapshot().currentTimeSeconds ?? -1
            print("dvr_after_10s=\(String(format: "%.1f", later))")
            session.surface.showControls()
            wait(seconds: 1)
            capture(session.surface, path: "\(capturePrefix)-dvr-review.png")
        }

        if let liveButton = findButton(in: session.surface, titlePrefix: "ライブ") {
            liveButton.performClick(nil)
            wait(seconds: 2)
            let live = session.currentSnapshot().currentTimeSeconds ?? -1
            print("live_after=\(String(format: "%.1f", live))")
            session.surface.showControls()
            capture(session.surface, path: "\(capturePrefix)-live-return.png")
        }

        if let lowLatency = findButton(in: session.surface, titlePrefix: "低遅延") {
            lowLatency.performClick(nil)
            wait(seconds: 1)
            session.surface.showControls()
            capture(session.surface, path: "\(capturePrefix)-lowlatency-off.png")
        } else {
            print("lowlatency_button=not_found")
        }

        print("quality_begin state=\(session.stateCode) has_item=\(session.currentSnapshot().hasCurrentItem)")
        for cap in [PlayerQualityCap.automatic, .p1080, .p720, .p480] {
            if session.currentSnapshot().transportStatus != "playing" {
                try? session.resume()
            }
            session.setQualityCap(cap)
            wait(seconds: 8)
            let snapshot = session.currentSnapshot()
            print(
                "quality=\(cap.rawValue) state=\(session.stateCode) "
                    + "item=\(snapshot.itemStatusCode) has_item=\(snapshot.hasCurrentItem) "
                    + "video=\(sizeDescription(session.currentVideoSize)) "
                    + "track=\(sizeDescription(session.currentVideoTrackSize)) "
                    + "indicated=\(Int(session.currentIndicatedBitrate)) "
                    + "observed=\(Int(session.currentObservedBitrate))"
            )
            session.surface.showControls()
            wait(seconds: 1)
            capture(
                session.surface,
                path: "\(capturePrefix)-quality-\(cap.rawValue).png"
            )
        }

        print("pip_supported=\(session.isPictureInPictureSupported)")
        session.setPictureInPicture(enabled: true)
        wait(seconds: 5)
        print("pip_active=\(session.isPictureInPictureActive)")
        session.surface.showControls()
        wait(seconds: 1)
        capture(session.surface, path: "\(capturePrefix)-pip.png")
        session.setPictureInPicture(enabled: false)
        wait(seconds: 2)
        print("pip_after_stop=\(session.isPictureInPictureActive)")

        session.shutdown()
        window.orderOut(nil)
        print("done")
    }

    static func findButton(in view: NSView, titlePrefix: String) -> NSButton? {
        if let button = view as? NSButton, button.title.hasPrefix(titlePrefix) {
            return button
        }
        for subview in view.subviews {
            if let found = findButton(in: subview, titlePrefix: titlePrefix) {
                return found
            }
        }
        return nil
    }

    static func wait(seconds: TimeInterval) {
        let deadline = Date().addingTimeInterval(seconds)
        while Date() < deadline {
            RunLoop.current.run(
                mode: .default,
                before: min(deadline, Date().addingTimeInterval(0.1))
            )
        }
    }

    static func capture(_ view: NSView, path: String) {
        guard let representation = view.bitmapImageRepForCachingDisplay(in: view.bounds) else {
            print("capture_no_rep=\(path)")
            return
        }
        view.cacheDisplay(in: view.bounds, to: representation)
        guard let data = representation.representation(using: .png, properties: [:]) else {
            print("capture_no_png=\(path)")
            return
        }
        do {
            try data.write(to: URL(fileURLWithPath: path))
            print(
                "capture=\(path) bytes=\(data.count) "
                    + "size=\(representation.pixelsWide)x\(representation.pixelsHigh)"
            )
        } catch {
            print("capture_error=\(path) \(error.localizedDescription)")
        }
    }

    static func sizeDescription(_ size: CGSize) -> String {
        "\(Int(size.width.rounded()))x\(Int(size.height.rounded()))"
    }
}
