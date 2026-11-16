import AppKit
import KickPlayerCore

@main
@MainActor
struct KickPlayerDemoApp {
    static func main() {
        if ProcessInfo.processInfo.environment["KICK_PLAYER_URL_STDIN"] == "1",
           let line = readLine()?.trimmingCharacters(in: .whitespacesAndNewlines),
           !line.isEmpty
        {
            DemoAppDelegate.pendingURL = line
        }
        let application = NSApplication.shared
        let delegate = DemoAppDelegate()
        application.delegate = delegate
        application.setActivationPolicy(.regular)
        application.run()
    }
}

@MainActor
private final class DemoAppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    static var pendingURL: String?
    private static let appleHLSFixture = "https://devstreaming-cdn.apple.com/videos/streaming/examples/bipbop_16x9/bipbop_16x9_variant.m3u8"

    private enum SmokeStage {
        case waitingForReady
        case waitingForPlayback
        case playing
        case checkingPause
        case checkingAudio
        case tearingDown
        case finished
    }

    private let session = PlayerSession()
    private var window: NSWindow?
    private let urlField = NSTextField()
    private let statusLabel = NSTextField(labelWithString: "Enter an HLS playlist URL.")
    private let telemetryLabel = NSTextField(labelWithString: "")
    private var smokeStage: SmokeStage = .waitingForReady
    private var latestSnapshot: PlaybackSnapshot?
    private var smokeStartTime: Double?
    private var smokePauseTime: Double?
    private var smokeDidReportReady = false
    private var smokeDidReportPlaying = false
    private var smokeDidReportVideoReady = false
    private var smokeDidRequestPause = false
    private var smokeLogHandle: FileHandle?

    private var isSmokeRun: Bool {
        ProcessInfo.processInfo.arguments.contains("--smoke")
            || ProcessInfo.processInfo.environment["KICK_PLAYER_SMOKE"] == "1"
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        let contentView = NSView()
        contentView.translatesAutoresizingMaskIntoConstraints = false

        let playerSurface = session.surface
        playerSurface.translatesAutoresizingMaskIntoConstraints = false

        urlField.placeholderString = "https://…/master.m3u8"
        urlField.translatesAutoresizingMaskIntoConstraints = false
        if let pendingURL = Self.pendingURL {
            urlField.stringValue = pendingURL
        }

        let playButton = NSButton(title: "Load & Play", target: self, action: #selector(playSelectedURL))
        playButton.bezelStyle = .rounded
        let pauseButton = NSButton(title: "Pause", target: self, action: #selector(pausePlayback))
        pauseButton.bezelStyle = .rounded
        let muteButton = NSButton(title: "Mute", target: self, action: #selector(toggleMute(_:)))
        muteButton.bezelStyle = .rounded

        let volumeSlider = NSSlider(value: 1, minValue: 0, maxValue: 1, target: self, action: #selector(volumeChanged(_:)))
        volumeSlider.translatesAutoresizingMaskIntoConstraints = false
        volumeSlider.widthAnchor.constraint(equalToConstant: 130).isActive = true

        let controls = NSStackView(views: [urlField, playButton, pauseButton, muteButton, volumeSlider])
        controls.orientation = .horizontal
        controls.alignment = .centerY
        controls.spacing = 8
        controls.translatesAutoresizingMaskIntoConstraints = false

        statusLabel.translatesAutoresizingMaskIntoConstraints = false
        statusLabel.lineBreakMode = .byTruncatingMiddle
        telemetryLabel.translatesAutoresizingMaskIntoConstraints = false
        telemetryLabel.font = .monospacedSystemFont(ofSize: 11, weight: .regular)
        telemetryLabel.textColor = .secondaryLabelColor

        contentView.addSubview(playerSurface)
        contentView.addSubview(controls)
        contentView.addSubview(statusLabel)
        contentView.addSubview(telemetryLabel)

        NSLayoutConstraint.activate([
            playerSurface.topAnchor.constraint(equalTo: contentView.topAnchor, constant: 12),
            playerSurface.leadingAnchor.constraint(equalTo: contentView.leadingAnchor, constant: 12),
            playerSurface.trailingAnchor.constraint(equalTo: contentView.trailingAnchor, constant: -12),
            playerSurface.heightAnchor.constraint(equalTo: playerSurface.widthAnchor, multiplier: 9.0 / 16.0),

            controls.topAnchor.constraint(equalTo: playerSurface.bottomAnchor, constant: 12),
            controls.leadingAnchor.constraint(equalTo: contentView.leadingAnchor, constant: 12),
            controls.trailingAnchor.constraint(equalTo: contentView.trailingAnchor, constant: -12),

            statusLabel.topAnchor.constraint(equalTo: controls.bottomAnchor, constant: 8),
            statusLabel.leadingAnchor.constraint(equalTo: contentView.leadingAnchor, constant: 12),
            statusLabel.trailingAnchor.constraint(equalTo: contentView.trailingAnchor, constant: -12),
            telemetryLabel.topAnchor.constraint(equalTo: statusLabel.bottomAnchor, constant: 4),
            telemetryLabel.leadingAnchor.constraint(equalTo: contentView.leadingAnchor, constant: 12),
            telemetryLabel.trailingAnchor.constraint(equalTo: contentView.trailingAnchor, constant: -12),
            telemetryLabel.bottomAnchor.constraint(lessThanOrEqualTo: contentView.bottomAnchor, constant: -12),
            urlField.widthAnchor.constraint(greaterThanOrEqualToConstant: 360),
        ])

        if Self.pendingURL != nil {
            DispatchQueue.main.async { [weak self] in
                self?.playSelectedURL()
            }
        }
        session.onStatusChanged = { [weak self] text in
            self?.statusLabel.stringValue = text
            print("STATUS \(text)")
            fflush(stdout)
        }
        session.onPlaybackSnapshotChanged = { [weak self] snapshot in
            guard let self else { return }
            latestSnapshot = snapshot
            print("SNAPSHOT \(snapshot.state) \(snapshot.itemStatus) \(snapshot.transportStatus) \(snapshot.currentTimeSeconds ?? -1)")
            fflush(stdout)
            telemetryLabel.stringValue = Self.format(snapshot)
            if isSmokeRun {
                advanceSmoke(with: snapshot)
            }
        }

        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1100, height: 730),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "Kick AVPlayer PoC"
        window.contentView = contentView
        window.delegate = self
        window.center()
        window.makeKeyAndOrderFront(nil)
        self.window = window

        NSApp.activate(ignoringOtherApps: true)

        if isSmokeRun {
            let logURL = URL(fileURLWithPath: "/tmp/kick-player-smoke.log")
            try? FileManager.default.removeItem(at: logURL)
            FileManager.default.createFile(atPath: logURL.path, contents: nil)
            smokeLogHandle = try? FileHandle(forWritingTo: logURL)
            smokeLog("SMOKE fixture=Apple HLS Bip Bop sample; URL omitted")
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.6) { [weak self] in
                self?.startSmoke()
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 25) { [weak self] in
                guard let self, smokeStage != .finished else { return }
                finishSmoke(success: false, reason: "timed out before playback checks completed")
            }
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        session.shutdown()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }

    func windowWillClose(_ notification: Notification) {
        session.shutdown()
    }

    @objc private func playSelectedURL() {
        do {
            try session.playHLSPair(
                liveURL: urlField.stringValue,
                dvrURL: "",
                startAtLiveEdge: true
            )
        } catch {
            statusLabel.stringValue = error.localizedDescription
        }
    }

    @objc private func pausePlayback() {
        session.pause()
    }

    @objc private func volumeChanged(_ sender: NSSlider) {
        session.setVolume(Float(sender.doubleValue))
    }

    @objc private func toggleMute(_ sender: NSButton) {
        let muted = !session.isMuted
        session.setMuted(muted)
        sender.title = muted ? "Unmute" : "Mute"
    }

    private func startSmoke() {
        do {
            try session.playHLS(Self.appleHLSFixture)
        } catch {
            finishSmoke(success: false, reason: "sample URL rejected")
        }
    }

    private func advanceSmoke(with snapshot: PlaybackSnapshot) {
        if !smokeDidReportReady, snapshot.itemStatus == "readyToPlay" {
            smokeDidReportReady = true
            smokeStage = .waitingForPlayback
            smokeLog("READY PASS item=readyToPlay")
        }

        if !smokeDidReportPlaying, snapshot.transportStatus == "playing", let time = snapshot.currentTimeSeconds {
            smokeDidReportPlaying = true
            smokeStartTime = time
            smokeStage = .playing
            smokeLog("PLAYING PASS transport=playing rate=\(snapshot.rate) time=\(Self.seconds(time))s")
        }

        if !smokeDidReportVideoReady, snapshot.videoReadyForDisplay {
            smokeDidReportVideoReady = true
            smokeLog("VIDEO READY PASS layer=isReadyForDisplay")
        }

        if snapshot.itemStatus == "failed" {
            finishSmoke(success: false, reason: "AVPlayer item failed")
            return
        }

        guard smokeStage == .playing,
              !smokeDidRequestPause,
              let start = smokeStartTime,
              let current = snapshot.currentTimeSeconds,
              current - start >= 3.0 else { return }

        smokeDidRequestPause = true
        smokePauseTime = current
        smokeStage = .checkingPause
        session.pause()
        smokeLog("PAUSE requested at=\(Self.seconds(current))s")
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.25) { [weak self] in
            self?.checkPause()
        }
    }

    private func checkPause() {
        guard let before = smokePauseTime, let paused = latestSnapshot else {
            finishSmoke(success: false, reason: "pause snapshot missing")
            return
        }
        let after = paused.currentTimeSeconds ?? before
        let delta = abs(after - before)
        guard paused.transportStatus == "paused", delta <= 0.2 else {
            finishSmoke(success: false, reason: "pause did not hold")
            return
        }

        guard smokeDidReportVideoReady else {
            finishSmoke(success: false, reason: "AVPlayerLayer did not report its first frame ready")
            return
        }

        smokeLog("PAUSE PASS transport=paused timeDelta=\(Self.seconds(delta))s")
        smokeStage = .checkingAudio
        session.setVolume(0.25)
        session.setMuted(true)
        guard let audio = latestSnapshot,
              abs(audio.volume - 0.25) < 0.001,
              audio.isMuted else {
            finishSmoke(success: false, reason: "volume or mute state did not update")
            return
        }
        smokeLog("AUDIO PASS volume=\(String(format: "%.2f", audio.volume)) muted=true")

        session.setMuted(false)
        session.setVolume(0.6)
        smokeStage = .tearingDown
        session.shutdown()
        guard let stopped = latestSnapshot,
              !stopped.hasCurrentItem,
              stopped.state == "idle",
              stopped.transportStatus == "paused" else {
            finishSmoke(success: false, reason: "player did not detach on shutdown")
            return
        }
        smokeLog("TEARDOWN PASS item=none state=idle transport=paused")
        finishSmoke(success: true, reason: "all runtime checks passed")
    }

    private func finishSmoke(success: Bool, reason: String) {
        guard smokeStage != .finished else { return }
        smokeStage = .finished
        smokeLog("SMOKE \(success ? "PASS" : "FAIL"): \(reason)")
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) {
            NSApp.terminate(nil)
        }
    }

    private static func format(_ snapshot: PlaybackSnapshot) -> String {
        let time = snapshot.currentTimeSeconds.map { "\(seconds($0))s" } ?? "—"
        return "Item: \(snapshot.itemStatus)   Transport: \(snapshot.transportStatus)   Time: \(time)   Volume: \(String(format: "%.2f", snapshot.volume))   Muted: \(snapshot.isMuted ? "yes" : "no")   Video ready: \(snapshot.videoReadyForDisplay ? "yes" : "no")"
    }

    private static func seconds(_ value: Double) -> String {
        String(format: "%.2f", value)
    }

    private func smokeLog(_ message: String) {
        let data = Data((message + "\n").utf8)
        FileHandle.standardOutput.write(data)
        smokeLogHandle?.write(data)
        try? smokeLogHandle?.synchronize()
    }
}
