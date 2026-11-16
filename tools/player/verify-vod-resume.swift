// Verification harness for VOD resume seeking.
//
// The app calls `PlayerSession.seek(toSeconds:)` immediately after starting a
// VOD item. This harness checks whether AVPlayer honors that early seek, and
// compares it with a seek issued after the item reports ready.
//
// Build:
//   swiftc -parse-as-library -I <bin-path> -L <bin-path> -lKickPlayerCore \
//     -framework AppKit -framework AVFoundation -framework AVKit \
//     tools/player/verify-vod-resume.swift -o /private/tmp/kick-verify-vod
//
// Run:
//   KICK_VOD_URL=<hls-url> /private/tmp/kick-verify-vod

import AppKit
import AVFoundation
import KickPlayerCore

@main
@MainActor
struct VerifyVODResume {
    static let appleHLSFixture =
        "https://devstreaming-cdn.apple.com/videos/streaming/examples/bipbop_16x9/bipbop_16x9_variant.m3u8"

    static func main() {
        let app = NSApplication.shared
        app.setActivationPolicy(.regular)
        let url = ProcessInfo.processInfo.environment["KICK_VOD_URL"] ?? appleHLSFixture
        let target = 120.0

        let early = run(url: url, seekImmediately: true, target: target)
        print("early_seek=\(early)")

        let ready = run(url: url, seekImmediately: false, target: target)
        print("ready_seek=\(ready)")

        print("done")
    }

    static func run(url: String, seekImmediately: Bool, target: Double) -> String {
        let session = PlayerSession()
        let window = NSWindow(
            contentRect: NSRect(x: 80, y: 80, width: 640, height: 400),
            styleMask: [.titled, .closable],
            backing: .buffered,
            defer: false
        )
        window.contentView = session.surface
        window.makeKeyAndOrderFront(nil)
        NSApplication.shared.activate(ignoringOtherApps: true)

        do {
            try session.playHLS(url)
        } catch {
            return "play_error=\(error.localizedDescription)"
        }
        if seekImmediately {
            session.seek(toSeconds: target)
        }
        wait(seconds: 12)
        if !seekImmediately {
            session.seek(toSeconds: target)
            wait(seconds: 4)
        }
        let snapshot = session.currentSnapshot()
        let result =
            "state=\(session.stateCode) item=\(snapshot.itemStatusCode) "
            + "time=\(secondsDescription(snapshot.currentTimeSeconds)) "
            + "video=\(sizeDescription(session.currentVideoSize))"
        session.shutdown()
        window.orderOut(nil)
        return result
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

    static func secondsDescription(_ seconds: Double?) -> String {
        guard let seconds, seconds.isFinite else { return "none" }
        return String(format: "%.1f", seconds)
    }

    static func sizeDescription(_ size: CGSize) -> String {
        "\(Int(size.width.rounded()))x\(Int(size.height.rounded()))"
    }
}
