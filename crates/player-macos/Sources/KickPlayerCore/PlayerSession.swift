import AppKit
import AVFoundation
import AVKit
import CoreMedia

public enum PlayerLanguage: Int32 {
    case japanese = 0
    case english = 1
}

public enum KickPlayerError: Error, LocalizedError {
    case invalidURL
    case sessionClosed
    case noCurrentItem

    public var errorDescription: String? {
        switch self {
        case .invalidURL:
            return "Enter an HTTP or HTTPS HLS playlist URL."
        case .sessionClosed:
            return "This player session has been shut down."
        case .noCurrentItem:
            return "There is no loaded item to resume."
        }
    }
}

public enum KickPlayerItemStatus: Int32 {
    case unknown = 0
    case readyToPlay = 1
    case failed = 2
}

public enum KickPlayerTransportStatus: Int32 {
    case paused = 0
    case waiting = 1
    case playing = 2
    case unknown = 3
}

public struct PlaybackSnapshot {
    public let state: String
    public let itemStatus: String
    public let transportStatus: String
    public let currentTimeSeconds: Double?
    public let rate: Float
    public let volume: Float
    public let isMuted: Bool
    public let hasCurrentItem: Bool
    public let videoReadyForDisplay: Bool

    public var itemStatusCode: Int32 {
        switch itemStatus {
        case "readyToPlay": KickPlayerItemStatus.readyToPlay.rawValue
        case "failed": KickPlayerItemStatus.failed.rawValue
        default: KickPlayerItemStatus.unknown.rawValue
        }
    }

    public var transportStatusCode: Int32 {
        switch transportStatus {
        case "paused": KickPlayerTransportStatus.paused.rawValue
        case "waiting": KickPlayerTransportStatus.waiting.rawValue
        case "playing": KickPlayerTransportStatus.playing.rawValue
        default: KickPlayerTransportStatus.unknown.rawValue
        }
    }
}

/// A compacted view of the time intervals AVPlayerItem currently reports as seekable.
/// Slider coordinates span only those intervals, so a DVR seek can never land in a gap.
struct SeekableTimeline {
    let ranges: [CMTimeRange]
    let durationSeconds: Double

    init?(timeRanges: [NSValue]) {
        let sortedRanges = timeRanges
            .map(\.timeRangeValue)
            .filter { range in
                guard range.start.isValid, range.start.isNumeric,
                      range.duration.isValid, range.duration.isNumeric
                else { return false }

                let start = CMTimeGetSeconds(range.start)
                let end = CMTimeGetSeconds(range.end)
                return start.isFinite && end.isFinite && end > start
            }
            .sorted { CMTimeCompare($0.start, $1.start) < 0 }

        var mergedRanges: [CMTimeRange] = []
        for range in sortedRanges {
            if let last = mergedRanges.last, CMTimeCompare(range.start, last.end) <= 0 {
                let end = CMTimeMaximum(last.end, range.end)
                let duration = CMTimeSubtract(end, last.start)
                mergedRanges[mergedRanges.count - 1] = CMTimeRange(start: last.start, duration: duration)
            } else {
                mergedRanges.append(range)
            }
        }

        let duration = mergedRanges.reduce(0.0) { total, range in
            total + CMTimeGetSeconds(range.duration)
        }
        guard !mergedRanges.isEmpty, duration.isFinite, duration > 0 else { return nil }

        ranges = mergedRanges
        durationSeconds = duration
    }

    func positionSeconds(for time: CMTime) -> Double? {
        guard time.isValid, time.isNumeric else { return nil }
        let targetSeconds = CMTimeGetSeconds(time)
        guard targetSeconds.isFinite else { return nil }

        var elapsed = 0.0
        for range in ranges {
            let start = CMTimeGetSeconds(range.start)
            let end = CMTimeGetSeconds(range.end)
            let duration = end - start
            if targetSeconds < start {
                return elapsed
            }
            if targetSeconds <= end {
                return elapsed + min(max(targetSeconds - start, 0), duration)
            }
            elapsed += duration
        }
        return durationSeconds
    }

    func fraction(for time: CMTime) -> Double? {
        guard let position = positionSeconds(for: time) else { return nil }
        return min(max(position / durationSeconds, 0), 1)
    }

    func time(at fraction: Double) -> CMTime? {
        guard fraction.isFinite else { return nil }
        var remaining = min(max(fraction, 0), 1) * durationSeconds

        for range in ranges {
            let duration = CMTimeGetSeconds(range.duration)
            guard remaining <= duration else {
                remaining -= duration
                continue
            }
            let offset = min(max(remaining, 0), duration)
            let candidate = CMTimeAdd(
                range.start,
                CMTime(seconds: offset, preferredTimescale: 600)
            )
            if let clamped = clampedTime(candidate) {
                return clamped
            }
            remaining -= duration
        }
        return clampedTime(ranges.last?.end ?? .invalid)
    }

    func clampedTime(_ time: CMTime) -> CMTime? {
        guard time.isValid, time.isNumeric else { return nil }
        let targetSeconds = CMTimeGetSeconds(time)
        guard targetSeconds.isFinite else { return nil }

        var nearest: (time: CMTime, distance: Double)?
        for range in ranges {
            let startSeconds = CMTimeGetSeconds(range.start)
            let endSeconds = CMTimeGetSeconds(range.end)
            let duration = endSeconds - startSeconds
            let safeEnd = duration > (1.0 / 600.0)
                ? CMTimeSubtract(range.end, CMTime(value: 1, timescale: 600))
                : range.start

            if targetSeconds >= startSeconds && targetSeconds <= endSeconds {
                return CMTimeCompare(time, safeEnd) > 0 ? safeEnd : maxTime(time, range.start)
            }

            let boundary = targetSeconds < startSeconds ? range.start : safeEnd
            let distance = abs(targetSeconds - CMTimeGetSeconds(boundary))
            if nearest == nil || distance < nearest!.distance {
                nearest = (boundary, distance)
            }
        }
        return nearest?.time
    }

    /// The newest seekable instant, kept just inside the final range.
    var liveEdgeTime: CMTime? {
        guard let last = ranges.last else { return nil }
        return clampedTime(last.end)
    }

    /// Whether the time falls inside one of the seekable ranges.
    func contains(_ time: CMTime) -> Bool {
        guard time.isValid, time.isNumeric else { return false }
        let seconds = CMTimeGetSeconds(time)
        guard seconds.isFinite else { return false }
        return ranges.contains { range in
            let start = CMTimeGetSeconds(range.start)
            let end = CMTimeGetSeconds(range.end)
            return seconds >= start && seconds <= end
        }
    }

    private func maxTime(_ lhs: CMTime, _ rhs: CMTime) -> CMTime {
        CMTimeCompare(lhs, rhs) >= 0 ? lhs : rhs
    }
}

/// User-selectable upper bound for the HLS variant AVPlayer may pick.
/// The options are local UI state and never require a network request.
public enum PlayerQualityCap: Int, CaseIterable {
    case automatic = 0
    case p1080 = 1080
    case p720 = 720
    case p480 = 480

    func title(for language: PlayerLanguage) -> String {
        switch self {
        case .automatic: language == .english ? "Auto" : "自動"
        case .p1080: "1080p"
        case .p720: "720p"
        case .p480: "480p"
        }
    }

    var maximumResolution: CGSize {
        switch self {
        case .automatic: .zero
        case .p1080: CGSize(width: 1920, height: 1080)
        case .p720: CGSize(width: 1280, height: 720)
        case .p480: CGSize(width: 854, height: 480)
        }
    }

    var menuIndex: Int {
        PlayerQualityCap.allCases.firstIndex(of: self) ?? 0
    }

    /// A soft bit-rate ceiling so the cap is honored even when the network
    /// could sustain a higher rendition. Zero means no limit.
    var peakBitRate: Double {
        switch self {
        case .automatic: 0
        case .p1080: 8_000_000
        case .p720: 4_000_000
        case .p480: 2_000_000
        }
    }

    func apply(to item: AVPlayerItem) {
        item.preferredMaximumResolution = maximumResolution
        item.preferredPeakBitRate = peakBitRate
    }
}

/// Resolves the live-edge seek target without touching AVPlayer so it stays testable.
enum LiveEdgeSeek {
    /// Live HLS items report an indefinite (non-numeric) duration; VOD reports a finite one.
    static func isLive(duration: CMTime) -> Bool {
        guard duration.isValid else { return false }
        guard duration.isNumeric else { return true }
        return !CMTimeGetSeconds(duration).isFinite
    }

    /// Prefers the end of the last seekable range, then falls back to AVPlayer's live edge.
    /// `allowNonLiveDuration` lets an explicit DVR/live-edge request work even when
    /// AVPlayerItem reports a finite duration for an EVENT playlist.
    static func target(
        duration: CMTime,
        timeRanges: [NSValue],
        allowNonLiveDuration: Bool = false
    ) -> CMTime? {
        guard allowNonLiveDuration || isLive(duration: duration) else { return nil }
        if let timeline = SeekableTimeline(timeRanges: timeRanges),
           let edge = timeline.liveEdgeTime {
            return edge
        }
        return .positiveInfinity
    }

    /// The startup seek target for a live item when low-latency mode is enabled.
    /// Returns nil for VOD or when the toggle is off so playback starts normally.
    /// `startAtLiveEdge` overrides the low-latency toggle and also works for DVR
    /// EVENT playlists whose duration may be finite.
    static func startTarget(
        duration: CMTime,
        timeRanges: [NSValue],
        lowLatencyEnabled: Bool,
        startAtLiveEdge: Bool = false
    ) -> CMTime? {
        if startAtLiveEdge {
            return target(
                duration: duration,
                timeRanges: timeRanges,
                allowNonLiveDuration: true
            )
        }
        guard lowLatencyEnabled else { return nil }
        return target(duration: duration, timeRanges: timeRanges)
    }
}

/// Decides whether the native live-edge button can act on the current item.
enum LiveEdgeAvailability {
    static func canSeekLiveEdge(
        itemIsReady: Bool,
        allowsLiveEdgeSeek: Bool,
        duration: CMTime
    ) -> Bool {
        itemIsReady && (allowsLiveEdgeSeek || LiveEdgeSeek.isLive(duration: duration))
    }
}

/// Decides whether a completed seek should resume playback.
enum SeekResumePolicy {
    static func shouldResume(
        wasPlaying: Bool,
        isShutdown: Bool,
        requestMatches: Bool,
        currentItemMatches: Bool,
        itemIsReady: Bool,
        playbackRequested: Bool
    ) -> Bool {
        wasPlaying
            && !isShutdown
            && requestMatches
            && currentItemMatches
            && itemIsReady
            && playbackRequested
    }
}

/// Which HLS source should own the current playback item.
enum PlaybackSourceKind: Equatable {
    case live
    case dvr
}

/// Pure source-selection and DVR-position decisions for the live/DVR pair.
enum PlaybackSourceSelection {
    /// Uses the live source when the requested time is inside the live window;
    /// otherwise uses DVR when a DVR timeline is available. Falls back to live
    /// when DVR is unavailable.
    static func sourceForSeek(
        target: CMTime,
        liveTimeline: SeekableTimeline?,
        dvrTimeline: SeekableTimeline?
    ) -> PlaybackSourceKind {
        guard dvrTimeline != nil else { return .live }
        if let liveTimeline, liveTimeline.contains(target) {
            return .live
        }
        return .dvr
    }

    /// Maps a live-item playhead into the DVR timeline by aligning the two
    /// seekable-range ends. DVR playheads are already in the DVR timeline.
    static func dvrPosition(
        currentSource: PlaybackSourceKind,
        currentTime: CMTime,
        liveTimeline: SeekableTimeline?,
        dvrTimeline: SeekableTimeline
    ) -> CMTime {
        switch currentSource {
        case .dvr:
            return currentTime
        case .live:
            guard
                let liveEnd = liveTimeline?.ranges.last?.end,
                let dvrEnd = dvrTimeline.ranges.last?.end
            else {
                return dvrTimeline.ranges.last?.end ?? currentTime
            }
            return CMTimeAdd(currentTime, CMTimeSubtract(dvrEnd, liveEnd))
        }
    }
}

/// Layout decisions shared by the native overlay and its tests.
enum PlayerOverlayLayout {
    static let dvrPositionLabelMinimumWidth: CGFloat = 420

    static func shouldShowDVRPositionLabel(width: CGFloat, hasDVRRange: Bool) -> Bool {
        hasDVRRange && width >= dvrPositionLabelMinimumWidth
    }
}

/// Pure legible-track selection rules shared by the native controls and tests.
enum SubtitleSelection {
    /// Maps a UI index onto the selectable tracks. 0 is "off"; the valid track
    /// range is 1...optionCount. Out-of-range values are rejected rather than
    /// clamped so a stale menu selection cannot silently pick another track.
    static func normalizedIndex(_ index: Int, optionCount: Int) -> Int? {
        guard index >= 0 else { return nil }
        guard index <= optionCount else { return nil }
        return index
    }

    /// A stable identity used to re-apply the user's choice after the live/DVR
    /// item is replaced. Display names alone can collide, so the language tag is
    /// included when the track provides one.
    static func key(for option: AVMediaSelectionOption) -> String {
        let language = option.extendedLanguageTag
            ?? option.locale?.identifier
            ?? ""
        return "\(language)|\(option.displayName)"
    }

    /// Matches an option's language tag or display name against the preferred
    /// language tags (for example `ja` / `japanese`).
    static func languageMatches(language: String, name: String, tags: [String]) -> Bool {
        let language = language.lowercased()
        let name = name.lowercased()
        return tags.contains { tag in
            language == tag || language.hasPrefix("\(tag)-") || name.contains(tag)
        }
    }

    /// A readable menu title, falling back to the track ordinal when the
    /// playlist omits a display name.
    static func title(
        for option: AVMediaSelectionOption,
        fallbackIndex: Int,
        language: PlayerLanguage
    ) -> String {
        let name = option.displayName.trimmingCharacters(in: .whitespacesAndNewlines)
        if !name.isEmpty { return name }
        if let identifier = option.locale?.identifier, !identifier.isEmpty {
            return Locale.current.localizedString(forIdentifier: identifier) ?? identifier
        }
        return language == .english ? "Subtitles \(fallbackIndex)" : "字幕 \(fallbackIndex)"
    }
}

/// AVPlayer buffering policy for live and VOD playback.
///
/// The verified Kick/IVS master playlist exposes ordinary HLS variants only:
/// no `EXT-X-PART`, `EXT-X-PART-INF`, or `EXT-X-SERVER-CONTROL`. Latency is
/// therefore controlled by AVPlayer buffering, not by selecting another URL.
/// Android's shipped IVS bridge calls `setLiveLowLatencyEnabled(true)`; AVPlayer
/// has no equivalent switch, so this policy approximates it conservatively.
struct PlaybackLatencyPolicy: Equatable {
    let automaticallyWaitsToMinimizeStalling: Bool
    let preferredForwardBufferDuration: TimeInterval
    let canUseNetworkResourcesForLiveStreamingWhilePaused: Bool

    static func resolve(isLive: Bool, lowLatencyEnabled: Bool) -> PlaybackLatencyPolicy {
        guard isLive else {
            return PlaybackLatencyPolicy(
                automaticallyWaitsToMinimizeStalling: true,
                preferredForwardBufferDuration: 0,
                canUseNetworkResourcesForLiveStreamingWhilePaused: false
            )
        }
        return PlaybackLatencyPolicy(
            automaticallyWaitsToMinimizeStalling: !lowLatencyEnabled,
            preferredForwardBufferDuration: lowLatencyEnabled ? 1 : 0,
            canUseNetworkResourcesForLiveStreamingWhilePaused: false
        )
    }

    func apply(to item: AVPlayerItem) {
        item.preferredForwardBufferDuration = preferredForwardBufferDuration
        item.canUseNetworkResourcesForLiveStreamingWhilePaused = canUseNetworkResourcesForLiveStreamingWhilePaused
    }
}

/// Bridges AVPictureInPictureController delegate callbacks back to the session
/// without making PlayerSession an NSObject subclass.
@MainActor
private final class PictureInPictureDelegate: NSObject, @preconcurrency AVPictureInPictureControllerDelegate {
    var onStart: (() -> Void)?
    var onStop: (() -> Void)?
    var onFailure: ((String) -> Void)?

    func pictureInPictureControllerDidStartPictureInPicture(
        _ controller: AVPictureInPictureController
    ) {
        onStart?()
    }

    func pictureInPictureControllerDidStopPictureInPicture(
        _ controller: AVPictureInPictureController
    ) {
        onStop?()
    }

    func pictureInPictureController(
        _ controller: AVPictureInPictureController,
        failedToStartPictureInPictureWithError error: Error
    ) {
        onFailure?(error.localizedDescription)
    }
}

/// AppKit surface for custom native playback controls.
@MainActor
public final class PlayerSurface: NSView {
    public var player: AVPlayer? {
        didSet {
            (layer as? AVPlayerLayer)?.player = player
            refreshControlState()
        }
    }

    fileprivate var onPlaybackToggleRequested: (() -> Void)?
    fileprivate var onMuteToggleRequested: (() -> Void)?
    fileprivate var onVolumeChanged: ((Float) -> Void)?
    fileprivate var onSeekRequested: ((CMTime) -> Void)?
    fileprivate var onQualityCapChanged: ((PlayerQualityCap) -> Void)?
    fileprivate var onLiveEdgeRequested: (() -> Void)?
    fileprivate var onLowLatencyChanged: ((Bool) -> Void)?
    fileprivate var onSubtitleChanged: ((Int) -> Void)?
    fileprivate var onSubtitleAutoOn: (() -> Void)?
    fileprivate var onPictureInPictureToggleRequested: (() -> Void)?
    fileprivate var subtitleInfoProvider: (() -> (titles: [String], selected: Int))?
    fileprivate var pictureInPictureStateProvider: (() -> Bool)?

    /// The layer that owns the visible video. Picture in picture needs this
    /// exact layer, not a separate AVPlayerLayer.
    var playerLayer: AVPlayerLayer? { layer as? AVPlayerLayer }

    private var controlsBackground: NSView?
    private var controlsContainer: NSView?
    private var controlsStack: NSStackView?
    private var playPauseButton: NSButton?
    private var muteButton: NSButton?
    private var fullScreenButton: NSButton?
    private var volumeSlider: PlayerOverlaySlider?
    private var volumeValueLabel: NSTextField?
    private var qualityButton: PlayerOverlayPopUpButton?
    private var subtitleButton: PlayerOverlayPopUpButton?
    private var pictureInPictureButton: NSButton?
    private var lowLatencyButton: NSButton?
    private var liveEdgeButton: NSButton?
    private var dvrSlider: PlayerOverlaySlider?
    private var dvrPositionLabel: NSTextField?
    private var transportLabel: NSTextField?
    private var liveBadgeBackground: NSView?
    private var trackingArea: NSTrackingArea?
    private var hideControlsTimer: Timer?
    private var controlsAreVisible = false
    private var pointerInteractionIsActive = false
    private var selectedQualityCap: PlayerQualityCap = .automatic
    fileprivate var language: PlayerLanguage = .japanese
    private var liveBadgeLabel: NSTextField?
    private var lowLatencyEnabled = true
    fileprivate var isVodMode = false
    /// Reads PlayerSession's per-item DVR/live-edge capability without duplicating the flag.
    fileprivate var liveEdgeSeekAvailability: (() -> Bool)?
    fileprivate var allowsLiveEdgeSeek: Bool {
        liveEdgeSeekAvailability?() ?? false
    }
    fileprivate var dvrPositionProvider: (() -> (timeline: SeekableTimeline, currentTime: CMTime)?)?
    private var playPauseWidthConstraint: NSLayoutConstraint?
    private var muteWidthConstraint: NSLayoutConstraint?
    private var qualityWidthConstraint: NSLayoutConstraint?
    private var subtitleWidthConstraint: NSLayoutConstraint?
    private var pictureInPictureWidthConstraint: NSLayoutConstraint?
    private var lowLatencyWidthConstraint: NSLayoutConstraint?
    private var liveEdgeWidthConstraint: NSLayoutConstraint?
    private var volumePreferredWidthConstraint: NSLayoutConstraint?
    private var currentItemHasDVRRange = false
    private var isNarrowLayout = false
    private var isCompactLayout = false
    private var isVeryNarrowLayout = false
    private var isUltraNarrowLayout = false
    private var isDVRRowCompact = false
    private var responsiveLayoutInitialized = false

    private let danmakuOverlay = DanmakuOverlayView(frame: .zero)

    public override var wantsUpdateLayer: Bool { true }

    public override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
        danmakuOverlay.frame = bounds
    }

    func pushDanmaku(_ text: String) {
        danmakuOverlay.push(text)
    }

    func setDanmakuConfig(fontSize: CGFloat, opacity: CGFloat) {
        danmakuOverlay.configure(fontSize: fontSize, opacity: opacity)
    }

    /// Reveals the native controls without a pointer event. Intended for the
    /// demo/verification harness and for accessibility tooling.
    public func showControls() {
        revealControls()
    }

    /// Rebuilds the CC menu from the session's current legible tracks. Called
    /// when an item becomes ready and whenever the selection changes.
    fileprivate func reloadSubtitleControl() {
        updateSubtitleControl()
        refreshControlState()
    }

    fileprivate func setPictureInPictureActive(_ active: Bool) {
        updatePictureInPictureControl(active: active)
    }

    /// Surfaces a PiP failure in the transport label, which is the native
    /// surface's only transient message area.
    fileprivate func showPictureInPictureFailure(_ message: String) {
        transportLabel?.stringValue = message
        transportLabel?.setAccessibilityLabel(message)
        revealControls()
    }

    public override func makeBackingLayer() -> CALayer {
        let playerLayer = AVPlayerLayer()
        playerLayer.videoGravity = .resizeAspect
        playerLayer.backgroundColor = NSColor.black.cgColor
        playerLayer.cornerRadius = 12
        playerLayer.masksToBounds = true
        return playerLayer
    }

    public override func updateLayer() {
        guard let playerLayer = layer as? AVPlayerLayer else { return }
        playerLayer.player = player
        playerLayer.videoGravity = .resizeAspect
        playerLayer.backgroundColor = NSColor.black.cgColor
        playerLayer.cornerRadius = 12
        playerLayer.masksToBounds = true
    }

    public override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        danmakuOverlay.frame = bounds
        danmakuOverlay.autoresizingMask = [.width, .height]
        addSubview(danmakuOverlay)
        wantsLayer = true
        configureControls()
        applyLanguage()
    }

    public required init?(coder: NSCoder) {
        super.init(coder: coder)
        wantsLayer = true
        configureControls()
        applyLanguage()
    }

    deinit {
        hideControlsTimer?.invalidate()
    }

    public override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let trackingArea {
            removeTrackingArea(trackingArea)
        }

        let trackingArea = NSTrackingArea(
            rect: bounds,
            options: [.activeInKeyWindow, .mouseMoved, .mouseEnteredAndExited],
            owner: self,
            userInfo: nil
        )
        addTrackingArea(trackingArea)
        self.trackingArea = trackingArea
    }

    public override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        if window == nil {
            hideControlsTimer?.invalidate()
            hideControlsTimer = nil
            pointerInteractionIsActive = false
            controlsAreVisible = false
            controlsBackground?.isHidden = true
        }
        refreshControlState()
    }

    public override func layout() {
        super.layout()
        updateResponsiveLayout()
    }

    /// Keeps the full-width two-row overlay inside the video bounds by hiding optional
    /// labels and switching the quality/live controls to compact icon-only forms.
    private func updateResponsiveLayout(force: Bool = false) {
        let width = bounds.width
        let narrow = width < 760
        let compact = width < 620
        let veryNarrow = width < 340
        let ultraNarrow = width < 260
        let dvrRowCompact = width < PlayerOverlayLayout.dvrPositionLabelMinimumWidth
        guard force
            || !responsiveLayoutInitialized
            || narrow != isNarrowLayout
            || compact != isCompactLayout
            || veryNarrow != isVeryNarrowLayout
            || ultraNarrow != isUltraNarrowLayout
            || dvrRowCompact != isDVRRowCompact
        else { return }

        responsiveLayoutInitialized = true
        isNarrowLayout = narrow
        isCompactLayout = compact
        isVeryNarrowLayout = veryNarrow
        isUltraNarrowLayout = ultraNarrow
        isDVRRowCompact = dvrRowCompact

        volumeValueLabel?.isHidden = narrow
        dvrPositionLabel?.isHidden = !PlayerOverlayLayout.shouldShowDVRPositionLabel(
            width: width,
            hasDVRRange: currentItemHasDVRRange
        )

        controlsStack?.spacing = veryNarrow ? 2 : (compact ? 3 : 8)
        playPauseWidthConstraint?.constant = ultraNarrow ? 26 : (veryNarrow ? 28 : 34)
        muteWidthConstraint?.constant = ultraNarrow ? 26 : (veryNarrow ? 28 : 34)
        qualityWidthConstraint?.constant = ultraNarrow ? 34 : (veryNarrow ? 40 : (compact ? 40 : 72))
        subtitleWidthConstraint?.constant = ultraNarrow ? 34 : (veryNarrow ? 40 : (compact ? 44 : 64))
        pictureInPictureWidthConstraint?.constant = ultraNarrow ? 26 : (veryNarrow ? 28 : 34)
        lowLatencyWidthConstraint?.constant = ultraNarrow ? 28 : (veryNarrow ? 30 : (compact ? 32 : 96))
        liveEdgeWidthConstraint?.constant = ultraNarrow ? 28 : (veryNarrow ? 30 : (compact ? 32 : 56))
        volumePreferredWidthConstraint?.constant = veryNarrow ? 28 : 80
        // The optional track controls are the first to go when the surface is
        // too narrow to fit every button; the GPUI accessible mirror remains.
        subtitleButton?.isHidden = veryNarrow
        pictureInPictureButton?.isHidden = veryNarrow

        if compact {
            setCompactImage(
                on: qualityButton,
                symbolName: "slider.horizontal.3",
                accessibilityDescription: "画質",
                fallbackTitle: "画質"
            )
            setCompactImage(
                on: lowLatencyButton,
                symbolName: "bolt.fill",
                accessibilityDescription: "低遅延",
                fallbackTitle: "LL"
            )
            setCompactImage(
                on: liveEdgeButton,
                symbolName: "dot.radiowaves.left.and.right",
                accessibilityDescription: "ライブ",
                fallbackTitle: "LIVE"
            )
        } else {
            qualityButton?.image = nil
            qualityButton?.imagePosition = .noImage
            qualityButton?.selectItem(at: selectedQualityCap.menuIndex)
            lowLatencyButton?.image = nil
            lowLatencyButton?.imagePosition = .noImage
            lowLatencyButton?.title = "低遅延"
            liveEdgeButton?.image = nil
            liveEdgeButton?.imagePosition = .noImage
            liveEdgeButton?.title = "ライブ"
        }
        // The compact/non-compact branch above resets the low-latency title;
        // re-apply the on/off label after every responsive change.
        updateLowLatencyControl()
    }

    private func setCompactImage(
        on button: NSButton?,
        symbolName: String,
        accessibilityDescription: String,
        fallbackTitle: String
    ) {
        guard let button else { return }
        if let image = NSImage(
            systemSymbolName: symbolName,
            accessibilityDescription: accessibilityDescription
        ) {
            button.image = image
            button.imagePosition = .imageOnly
            button.title = ""
        } else {
            button.image = nil
            button.imagePosition = .noImage
            button.title = fallbackTitle
        }
    }

    public override func mouseEntered(with event: NSEvent) {
        revealControls()
        super.mouseEntered(with: event)
    }

    public override func mouseMoved(with event: NSEvent) {
        revealControls()
        super.mouseMoved(with: event)
    }

    public override func mouseExited(with event: NSEvent) {
        guard !pointerInteractionIsActive else { return }
        let pointer = convert(event.locationInWindow, from: nil)
        if let controlsBackground, controlsBackground.frame.contains(pointer) {
            return
        }
        scheduleControlsHide(after: 0.35)
        super.mouseExited(with: event)
    }

    /// Let GPUI receive pointer events everywhere except the native controls.
    public override func hitTest(_ point: NSPoint) -> NSView? {
        guard
            controlsAreVisible,
            let superview,
            let controlsContainer
        else {
            return nil
        }

        let localPoint = convert(point, from: superview)
        let containerPoint = controlsContainer.convert(localPoint, from: self)
        guard let hitView = controlsContainer.hitTest(containerPoint) else { return nil }
        guard hitView is NSButton || hitView is PlayerOverlaySlider else { return nil }
        return hitView
    }

    fileprivate func refreshControlState() {
        guard let player, let item = player.currentItem else {
            currentItemHasDVRRange = false
            playPauseButton?.isEnabled = false
            muteButton?.isEnabled = false
            fullScreenButton?.isEnabled = window != nil
            volumeSlider?.isEnabled = false
            qualityButton?.isEnabled = false
            subtitleButton?.isEnabled = false
            pictureInPictureButton?.isEnabled = false
            lowLatencyButton?.isEnabled = false
            liveEdgeButton?.isEnabled = false
            dvrSlider?.isEnabled = false
            dvrSlider?.isHidden = true
            dvrPositionLabel?.isHidden = true
            liveBadgeBackground?.isHidden = true
            controlsAreVisible = false
            controlsBackground?.isHidden = true
            hideControlsTimer?.invalidate()
            hideControlsTimer = nil
            return
        }

        let itemIsReady = item.status == .readyToPlay
        let currentTimeline = itemIsReady ? SeekableTimeline(timeRanges: item.seekableTimeRanges) : nil
        let dvrPosition = dvrPositionProvider?()
        let timeline = dvrPosition?.timeline ?? currentTimeline
        let hasDVRRange = timeline != nil
        currentItemHasDVRRange = hasDVRRange
        liveBadgeBackground?.isHidden = !(itemIsReady && !hasDVRRange)
        liveBadgeBackground?.setAccessibilityLabel(loc("ライブ配信。シーク操作は利用できません", "Live stream. Seeking is unavailable"))
        qualityButton?.isEnabled = true
        let subtitleInfo = subtitleInfoProvider?() ?? (titles: [], selected: 0)
        subtitleButton?.isEnabled = itemIsReady && !subtitleInfo.titles.isEmpty
        pictureInPictureButton?.isEnabled = window != nil
            && AVPictureInPictureController.isPictureInPictureSupported()
        updatePictureInPictureControl()
        let canSeekLiveEdge = LiveEdgeAvailability.canSeekLiveEdge(
            itemIsReady: itemIsReady,
            allowsLiveEdgeSeek: allowsLiveEdgeSeek,
            duration: item.duration
        )
        lowLatencyButton?.isEnabled = canSeekLiveEdge
        liveEdgeButton?.isEnabled = canSeekLiveEdge
        dvrSlider?.isHidden = !hasDVRRange
        dvrPositionLabel?.isHidden = !PlayerOverlayLayout.shouldShowDVRPositionLabel(
            width: bounds.width,
            hasDVRRange: hasDVRRange
        )
        dvrSlider?.isEnabled = false
        let positionLabel = isVodMode ? loc("再生位置", "Playback position") : "DVR"
        if let timeline {
            dvrSlider?.minValue = 0
            dvrSlider?.maxValue = 1
            dvrSlider?.setAccessibilityLabel(loc("DVR位置", "DVR position"))
            dvrSlider?.setAccessibilityHelp(loc("AVPlayer が報告したシーク可能範囲内で再生位置を移動", "Seek within the range reported by AVPlayer"))
            dvrSlider?.toolTip = loc("シーク可能なDVR範囲内で移動", "Seek within the available DVR range")
            if !pointerInteractionIsActive {
                if itemIsReady,
                   let currentTime = dvrPosition?.currentTime ?? player.currentItem?.currentTime(),
                   let fraction = timeline.fraction(for: currentTime) {
                    dvrSlider?.doubleValue = fraction
                    dvrSlider?.isEnabled = true
                    let position = formatTimelineTime(timeline.positionSeconds(for: currentTime) ?? 0)
                    let duration = formatTimelineTime(timeline.durationSeconds)
                    dvrPositionLabel?.stringValue = "\(positionLabel) \(position) / \(duration)"
                    dvrPositionLabel?.setAccessibilityLabel(loc("\(positionLabel) \(position)、使用可能範囲 \(duration)", "\(positionLabel) \(position), available range \(duration)"))
                } else {
                    dvrPositionLabel?.stringValue = "\(positionLabel) --:-- / \(formatTimelineTime(timeline.durationSeconds))"
                    dvrPositionLabel?.setAccessibilityLabel(loc("\(positionLabel)不明、使用可能範囲 \(formatTimelineTime(timeline.durationSeconds))", "\(positionLabel) unknown, available range \(formatTimelineTime(timeline.durationSeconds))"))
                }
            } else {
                dvrSlider?.isEnabled = true
            }
        }

        let canTogglePlayback: Bool
        let actionLabel: String
        let actionSymbol: String
        let transportText: String
        switch player.timeControlStatus {
        case .playing:
            canTogglePlayback = true
            actionLabel = loc("一時停止", "Pause")
            actionSymbol = "pause.fill"
            transportText = loc("再生中", "Playing")
        case .waitingToPlayAtSpecifiedRate:
            canTogglePlayback = true
            actionLabel = loc("一時停止", "Pause")
            actionSymbol = "pause.fill"
            transportText = loc("バッファリング中", "Buffering")
        case .paused:
            canTogglePlayback = true
            actionLabel = loc("再生", "Play")
            actionSymbol = "play.fill"
            transportText = item.status == .unknown ? loc("読み込み中", "Loading") : loc("一時停止", "Paused")
        @unknown default:
            canTogglePlayback = false
            actionLabel = loc("再生状態を確認できません", "Playback state unavailable")
            actionSymbol = "play.fill"
            transportText = loc("状態不明", "Unknown")
        }

        playPauseButton?.isEnabled = canTogglePlayback
        playPauseButton?.image = NSImage(
            systemSymbolName: actionSymbol,
            accessibilityDescription: actionLabel
        )
        playPauseButton?.setAccessibilityLabel(actionLabel)
        playPauseButton?.setAccessibilityHelp(loc("動画の再生または一時停止", "Play or pause the video"))
        muteButton?.isEnabled = true
        muteButton?.image = NSImage(
            systemSymbolName: player.isMuted ? "speaker.slash.fill" : "speaker.wave.2.fill",
            accessibilityDescription: player.isMuted ? loc("ミュート解除", "Unmute") : loc("ミュート", "Mute")
        )
        muteButton?.setAccessibilityLabel(player.isMuted ? loc("ミュート解除", "Unmute") : loc("ミュート", "Mute"))
        muteButton?.setAccessibilityHelp(loc("動画音声のミュートを切り替え", "Toggle video mute"))
        fullScreenButton?.isEnabled = window != nil
        volumeSlider?.isEnabled = true
        volumeSlider?.doubleValue = Double(player.volume)
        let volumePercent = Int((player.volume * 100).rounded())
        volumeValueLabel?.stringValue = "\(volumePercent)%"
        volumeValueLabel?.setAccessibilityLabel(loc("音量 \(volumePercent)%", "Volume \(volumePercent)%"))
        transportLabel?.stringValue = transportText
        transportLabel?.setAccessibilityLabel(loc("再生状態: \(transportText)", "Playback: \(transportText)"))
    }

    private func loc(_ ja: String, _ en: String) -> String {
        language == .english ? en : ja
    }

    public func setLanguage(_ language: PlayerLanguage) {
        guard self.language != language else { return }
        self.language = language
        applyLanguage()
    }

    private func rebuildQualityMenu() {
        guard let qualityButton else { return }
        let selected = qualityButton.indexOfSelectedItem
        qualityButton.removeAllItems()
        qualityButton.addItems(withTitles: PlayerQualityCap.allCases.map { $0.title(for: language) })
        for (index, cap) in PlayerQualityCap.allCases.enumerated() {
            qualityButton.item(at: index)?.tag = cap.rawValue
        }
        if selected >= 0 && selected < qualityButton.numberOfItems {
            qualityButton.selectItem(at: selected)
        }
    }

    private func applyLanguage() {
        setAccessibilityLabel(loc("ライブ動画プレーヤー", "Live video player"))
        playPauseButton?.setAccessibilityLabel(loc("再生", "Play"))
        playPauseButton?.setAccessibilityHelp(loc("動画の再生または一時停止", "Play or pause the video"))
        playPauseButton?.toolTip = loc("再生", "Play")
        muteButton?.setAccessibilityLabel(loc("ミュート", "Mute"))
        muteButton?.setAccessibilityHelp(loc("動画音声のミュートを切り替え", "Toggle video mute"))
        muteButton?.toolTip = loc("ミュート", "Mute")
        if let fullScreenButton {
            if let image = NSImage(
                systemSymbolName: "arrow.up.left.and.arrow.down.right",
                accessibilityDescription: loc("フルスクリーン切り替え", "Toggle fullscreen")
            ) {
                fullScreenButton.image = image
            } else {
                fullScreenButton.title = loc("全画面", "Fullscreen")
            }
            fullScreenButton.setAccessibilityLabel(loc("フルスクリーン切り替え", "Toggle fullscreen"))
            fullScreenButton.setAccessibilityHelp(loc("ウィンドウをフルスクリーン表示に切り替え", "Toggle the window fullscreen"))
            fullScreenButton.toolTip = loc("全画面", "Fullscreen")
        }
        volumeSlider?.setAccessibilityLabel(loc("音量", "Volume"))
        volumeSlider?.setAccessibilityHelp(loc("動画の再生音量を調整", "Adjust the playback volume"))
        volumeSlider?.toolTip = loc("音量", "Volume")
        volumeValueLabel?.setAccessibilityLabel(loc("音量 100%", "Volume 100%"))
        qualityButton?.setAccessibilityLabel(loc("画質: 自動", "Quality: Auto"))
        qualityButton?.setAccessibilityHelp(loc("再生画質の上限を選択。現在は 自動", "Choose the maximum playback quality. Currently Auto"))
        qualityButton?.toolTip = loc("画質: 自動", "Quality: Auto")
        lowLatencyButton?.setAccessibilityLabel(loc("低遅延モード: オン", "Low latency mode: On"))
        lowLatencyButton?.setAccessibilityHelp(loc("ライブ再生のバッファを小さくして遅延を抑えます。不安定な回線ではオフにしてください", "Reduce the live buffer to lower latency. Turn it off on unstable connections"))
        lowLatencyButton?.toolTip = loc("低遅延モード: オン", "Low latency mode: On")
        dvrSlider?.setAccessibilityLabel(loc("DVR位置", "DVR position"))
        dvrSlider?.setAccessibilityHelp(loc("AVPlayer が報告したシーク可能範囲内で再生位置を移動", "Seek within the range reported by AVPlayer"))
        dvrSlider?.toolTip = loc("シーク可能なDVR範囲内で移動", "Seek within the available DVR range")
        dvrPositionLabel?.setAccessibilityLabel(loc("DVR位置不明", "DVR position unknown"))
        liveEdgeButton?.title = loc("ライブ", "Live")
        liveEdgeButton?.setAccessibilityLabel(loc("ライブエッジへ移動", "Go to live edge"))
        liveEdgeButton?.setAccessibilityHelp(loc("最新のライブ位置へシーク", "Seek to the latest live position"))
        liveEdgeButton?.toolTip = loc("ライブエッジへ移動", "Go to live edge")
        subtitleButton?.setAccessibilityLabel(loc("字幕なし", "No subtitles"))
        subtitleButton?.setAccessibilityHelp(loc("この配信には字幕トラックがありません", "This stream has no subtitle tracks"))
        subtitleButton?.toolTip = loc("字幕なし", "No subtitles")
        if let pictureInPictureButton {
            if let image = NSImage(
                systemSymbolName: "pip.enter",
                accessibilityDescription: loc("ピクチャ・イン・ピクチャ", "Picture in Picture")
            ) {
                pictureInPictureButton.image = image
            } else {
                pictureInPictureButton.title = "PiP"
            }
            pictureInPictureButton.setAccessibilityLabel(loc("ピクチャ・イン・ピクチャ", "Picture in Picture"))
            pictureInPictureButton.setAccessibilityHelp(loc("再生中の映像をピクチャ・イン・ピクチャで表示します", "Show the video in Picture in Picture"))
            pictureInPictureButton.toolTip = loc("ピクチャ・イン・ピクチャ", "Picture in Picture")
        }
        transportLabel?.setAccessibilityLabel(loc("再生状態: 一時停止", "Playback: Paused"))
        liveBadgeLabel?.stringValue = loc("ライブ", "LIVE")
        liveBadgeBackground?.setAccessibilityLabel(loc("ライブ配信。シーク操作は利用できません", "Live stream. Seeking is unavailable"))
        rebuildQualityMenu()
        refreshControlState()
        updateQualityControl()
        updateLowLatencyControl()
        updateSubtitleControl()
        updatePictureInPictureControl()
    }

    private func configureControls() {
        setAccessibilityLabel("ライブ動画プレーヤー")

        let playPause = PlayerOverlayButton(title: "", target: self, action: #selector(togglePlayPause(_:)))
        playPause.bezelStyle = .rounded
        playPause.imagePosition = .imageOnly
        playPause.setAccessibilityLabel("再生")
        playPause.setAccessibilityHelp("動画の再生または一時停止")
        playPause.toolTip = "再生"
        let playPauseWidth = playPause.widthAnchor.constraint(equalToConstant: 34)
        playPauseWidth.isActive = true
        playPauseWidthConstraint = playPauseWidth
        playPause.heightAnchor.constraint(equalToConstant: 30).isActive = true
        playPause.onInteractionBegan = { [weak self] in self?.beginPointerInteraction() }
        playPause.onInteractionEnded = { [weak self] in self?.endPointerInteraction() }
        playPauseButton = playPause

        let mute = PlayerOverlayButton(title: "", target: self, action: #selector(toggleMute(_:)))
        mute.bezelStyle = .rounded
        mute.imagePosition = .imageOnly
        mute.setAccessibilityLabel("ミュート")
        mute.setAccessibilityHelp("動画音声のミュートを切り替え")
        mute.toolTip = "ミュート"
        let muteWidth = mute.widthAnchor.constraint(equalToConstant: 34)
        muteWidth.isActive = true
        muteWidthConstraint = muteWidth
        mute.heightAnchor.constraint(equalToConstant: 30).isActive = true
        mute.onInteractionBegan = { [weak self] in self?.beginPointerInteraction() }
        mute.onInteractionEnded = { [weak self] in self?.endPointerInteraction() }
        muteButton = mute

        let fullScreen = PlayerOverlayButton(title: "", target: self, action: #selector(toggleFullScreen(_:)))
        fullScreen.bezelStyle = .rounded
        fullScreen.imagePosition = .imageOnly
        if let image = NSImage(
            systemSymbolName: "arrow.up.left.and.arrow.down.right",
            accessibilityDescription: "フルスクリーン切り替え"
        ) {
            fullScreen.image = image
        } else {
            fullScreen.title = "全画面"
        }
        fullScreen.setAccessibilityLabel("フルスクリーン切り替え")
        fullScreen.setAccessibilityHelp("ウィンドウをフルスクリーン表示に切り替え")
        fullScreen.toolTip = "全画面"
        fullScreen.widthAnchor.constraint(equalToConstant: 34).isActive = true
        fullScreen.heightAnchor.constraint(equalToConstant: 30).isActive = true
        fullScreen.onInteractionBegan = { [weak self] in self?.beginPointerInteraction() }
        fullScreen.onInteractionEnded = { [weak self] in self?.endPointerInteraction() }
        fullScreenButton = fullScreen

        let volume = PlayerOverlaySlider(value: 1, minValue: 0, maxValue: 1)
        volume.onValueChanged = { [weak self] value in self?.changeVolume(value) }
        volume.isContinuous = true
        volume.fillColor = NSColor.white.withAlphaComponent(0.85)
        volume.setAccessibilityLabel("音量")
        volume.setAccessibilityHelp("動画の再生音量を調整")
        volume.toolTip = "音量"
        let volumePreferredWidth = volume.widthAnchor.constraint(equalToConstant: 80)
        volumePreferredWidth.priority = .defaultLow
        volumePreferredWidth.isActive = true
        volumePreferredWidthConstraint = volumePreferredWidth
        volume.widthAnchor.constraint(greaterThanOrEqualToConstant: 28).isActive = true
        volume.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        volume.heightAnchor.constraint(equalToConstant: 20).isActive = true
        volume.onInteractionBegan = { [weak self] in self?.beginPointerInteraction() }
        volume.onInteractionEnded = { [weak self] in self?.endPointerInteraction() }
        volumeSlider = volume

        let volumeValue = NSTextField(labelWithString: "100%")
        volumeValue.font = .monospacedDigitSystemFont(ofSize: 11, weight: .medium)
        volumeValue.alignment = .right
        volumeValue.textColor = NSColor.white.withAlphaComponent(0.85)
        volumeValue.setAccessibilityLabel("音量 100%")
        let volumeValuePreferredWidth = volumeValue.widthAnchor.constraint(equalToConstant: 38)
        volumeValuePreferredWidth.priority = .defaultHigh
        volumeValuePreferredWidth.isActive = true
        volumeValue.widthAnchor.constraint(greaterThanOrEqualToConstant: 22).isActive = true
        volumeValue.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        volumeValueLabel = volumeValue

        let quality = PlayerOverlayPopUpButton(frame: .zero, pullsDown: false)
        quality.translatesAutoresizingMaskIntoConstraints = false
        quality.bezelStyle = .rounded
        quality.controlSize = .small
        quality.font = .systemFont(ofSize: 11, weight: .medium)
        quality.removeAllItems()
        quality.addItems(withTitles: PlayerQualityCap.allCases.map { $0.title(for: language) })
        for (index, cap) in PlayerQualityCap.allCases.enumerated() {
            quality.item(at: index)?.tag = cap.rawValue
        }
        quality.selectItem(at: PlayerQualityCap.automatic.menuIndex)
        quality.target = self
        quality.action = #selector(selectQuality(_:))
        quality.setAccessibilityLabel("画質: 自動")
        quality.setAccessibilityHelp("再生画質の上限を選択。現在は 自動")
        quality.toolTip = "画質: 自動"
        let qualityWidth = quality.widthAnchor.constraint(equalToConstant: 72)
        qualityWidth.isActive = true
        qualityWidthConstraint = qualityWidth
        quality.heightAnchor.constraint(equalToConstant: 30).isActive = true
        quality.onInteractionBegan = { [weak self] in self?.beginPointerInteraction() }
        quality.onInteractionEnded = { [weak self] in self?.endPointerInteraction() }
        qualityButton = quality
        updateQualityControl()

        let lowLatency = PlayerOverlayButton(title: "低遅延 オン", target: self, action: #selector(toggleLowLatency(_:)))
        lowLatency.bezelStyle = .rounded
        lowLatency.controlSize = .small
        lowLatency.setButtonType(.toggle)
        lowLatency.font = .systemFont(ofSize: 11, weight: .medium)
        lowLatency.state = lowLatencyEnabled ? .on : .off
        lowLatency.setAccessibilityLabel("低遅延モード: オン")
        lowLatency.setAccessibilityHelp("ライブ再生のバッファを小さくして遅延を抑えます。不安定な回線ではオフにしてください")
        lowLatency.toolTip = "低遅延モード: オン"
        let lowLatencyWidth = lowLatency.widthAnchor.constraint(equalToConstant: 96)
        lowLatencyWidth.isActive = true
        lowLatencyWidthConstraint = lowLatencyWidth
        lowLatency.heightAnchor.constraint(equalToConstant: 30).isActive = true
        lowLatency.onInteractionBegan = { [weak self] in self?.beginPointerInteraction() }
        lowLatency.onInteractionEnded = { [weak self] in self?.endPointerInteraction() }
        lowLatencyButton = lowLatency
        updateLowLatencyControl()

        let dvr = PlayerOverlaySlider(value: 0, minValue: 0, maxValue: 1)
        dvr.onValueChanged = { [weak self] value in self?.seekDVRPosition(value) }
        dvr.isContinuous = false
        dvr.fillColor = NSColor(
            srgbRed: 83.0 / 255.0,
            green: 252.0 / 255.0,
            blue: 24.0 / 255.0,
            alpha: 1
        )
        dvr.setAccessibilityLabel("DVR位置")
        dvr.setAccessibilityHelp("AVPlayer が報告したシーク可能範囲内で再生位置を移動")
        dvr.toolTip = "シーク可能なDVR範囲内で移動"
        dvr.widthAnchor.constraint(greaterThanOrEqualToConstant: 32).isActive = true
        dvr.setContentHuggingPriority(.defaultLow, for: .horizontal)
        dvr.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        dvr.heightAnchor.constraint(equalToConstant: 20).isActive = true
        dvr.onInteractionBegan = { [weak self] in self?.beginPointerInteraction() }
        dvr.onInteractionEnded = { [weak self] in self?.endPointerInteraction() }
        dvrSlider = dvr

        let dvrPosition = NSTextField(labelWithString: "DVR --:-- / --:--")
        dvrPosition.font = .monospacedDigitSystemFont(ofSize: 10, weight: .medium)
        dvrPosition.alignment = .left
        dvrPosition.textColor = NSColor.white.withAlphaComponent(0.85)
        dvrPosition.setAccessibilityLabel("DVR位置不明")
        let dvrPositionPreferredWidth = dvrPosition.widthAnchor.constraint(equalToConstant: 150)
        dvrPositionPreferredWidth.priority = .defaultHigh
        dvrPositionPreferredWidth.isActive = true
        dvrPosition.widthAnchor.constraint(greaterThanOrEqualToConstant: 80).isActive = true
        dvrPosition.setContentHuggingPriority(.defaultHigh, for: .horizontal)
        dvrPosition.setContentCompressionResistancePriority(.defaultHigh, for: .horizontal)
        dvrPositionLabel = dvrPosition

        let liveEdge = PlayerOverlayButton(title: "ライブ", target: self, action: #selector(seekToLiveEdge(_:)))
        liveEdge.bezelStyle = .rounded
        liveEdge.font = .systemFont(ofSize: 11, weight: .semibold)
        liveEdge.setAccessibilityLabel("ライブエッジへ移動")
        liveEdge.setAccessibilityHelp("最新のライブ位置へシーク")
        liveEdge.toolTip = "ライブエッジへ移動"
        let liveEdgeWidth = liveEdge.widthAnchor.constraint(equalToConstant: 56)
        liveEdgeWidth.isActive = true
        liveEdgeWidthConstraint = liveEdgeWidth
        liveEdge.heightAnchor.constraint(equalToConstant: 30).isActive = true
        liveEdge.onInteractionBegan = { [weak self] in self?.beginPointerInteraction() }
        liveEdge.onInteractionEnded = { [weak self] in self?.endPointerInteraction() }
        liveEdgeButton = liveEdge

        let subtitle = PlayerOverlayPopUpButton(frame: .zero, pullsDown: false)
        subtitle.translatesAutoresizingMaskIntoConstraints = false
        subtitle.bezelStyle = .rounded
        subtitle.controlSize = .small
        subtitle.font = .systemFont(ofSize: 11, weight: .medium)
        subtitle.addItem(withTitle: "字幕なし")
        subtitle.selectItem(at: 0)
        subtitle.isEnabled = false
        subtitle.target = self
        subtitle.action = #selector(selectSubtitle(_:))
        subtitle.setAccessibilityLabel("字幕なし")
        subtitle.setAccessibilityHelp("この配信には字幕トラックがありません")
        subtitle.toolTip = "字幕なし"
        let subtitleWidth = subtitle.widthAnchor.constraint(equalToConstant: 64)
        subtitleWidth.isActive = true
        subtitleWidthConstraint = subtitleWidth
        subtitle.heightAnchor.constraint(equalToConstant: 30).isActive = true
        subtitle.onInteractionBegan = { [weak self] in self?.beginPointerInteraction() }
        subtitle.onInteractionEnded = { [weak self] in self?.endPointerInteraction() }
        subtitleButton = subtitle

        let pictureInPicture = PlayerOverlayButton(
            title: "",
            target: self,
            action: #selector(togglePictureInPicture(_:))
        )
        pictureInPicture.bezelStyle = .rounded
        pictureInPicture.imagePosition = .imageOnly
        if let image = NSImage(
            systemSymbolName: "pip.enter",
            accessibilityDescription: "ピクチャ・イン・ピクチャ"
        ) {
            pictureInPicture.image = image
        } else {
            pictureInPicture.title = "PiP"
        }
        pictureInPicture.setAccessibilityLabel("ピクチャ・イン・ピクチャ")
        pictureInPicture.setAccessibilityHelp("再生中の映像をピクチャ・イン・ピクチャで表示します")
        pictureInPicture.toolTip = "ピクチャ・イン・ピクチャ"
        let pictureInPictureWidth = pictureInPicture.widthAnchor.constraint(equalToConstant: 34)
        pictureInPictureWidth.isActive = true
        pictureInPictureWidthConstraint = pictureInPictureWidth
        pictureInPicture.heightAnchor.constraint(equalToConstant: 30).isActive = true
        pictureInPicture.onInteractionBegan = { [weak self] in self?.beginPointerInteraction() }
        pictureInPicture.onInteractionEnded = { [weak self] in self?.endPointerInteraction() }
        pictureInPictureButton = pictureInPicture

        let transport = NSTextField(labelWithString: "一時停止")
        transport.font = .systemFont(ofSize: 11, weight: .medium)
        transport.textColor = .secondaryLabelColor
        transport.setAccessibilityLabel("再生状態: 一時停止")
        transportLabel = transport

        let stack = NSStackView(views: [
            playPause, mute, volume, volumeValue, quality, lowLatency, liveEdge,
            subtitle, pictureInPicture,
        ])
        stack.orientation = .horizontal
        stack.alignment = .centerY
        stack.spacing = 8
        stack.detachesHiddenViews = true
        stack.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        stack.translatesAutoresizingMaskIntoConstraints = false
        controlsStack = stack

        let dvrRow = NSStackView(views: [dvrPosition, dvr])
        dvrRow.orientation = .horizontal
        dvrRow.alignment = .centerY
        dvrRow.spacing = 8
        dvrRow.distribution = .fill
        dvrRow.detachesHiddenViews = true
        dvrRow.translatesAutoresizingMaskIntoConstraints = false

        let container = NSView()
        container.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(stack)
        container.addSubview(dvrRow)
        controlsContainer = container

        let background = NSView()
        background.wantsLayer = true
        background.layer?.backgroundColor = NSColor.black.withAlphaComponent(0.78).cgColor
        background.layer?.cornerRadius = 0
        background.layer?.masksToBounds = true
        background.translatesAutoresizingMaskIntoConstraints = false
        background.addSubview(container)
        addSubview(background)
        let containerLeading = container.leadingAnchor.constraint(
            equalTo: background.leadingAnchor,
            constant: 12
        )
        containerLeading.priority = .defaultHigh
        let containerTrailing = container.trailingAnchor.constraint(
            equalTo: background.trailingAnchor,
            constant: -12
        )
        containerTrailing.priority = .defaultHigh
        NSLayoutConstraint.activate([
            background.leadingAnchor.constraint(equalTo: leadingAnchor),
            background.trailingAnchor.constraint(equalTo: trailingAnchor),
            background.bottomAnchor.constraint(equalTo: bottomAnchor),
            background.heightAnchor.constraint(equalToConstant: 76),
            containerLeading,
            containerTrailing,
            container.topAnchor.constraint(equalTo: background.topAnchor, constant: 10),
            container.bottomAnchor.constraint(equalTo: background.bottomAnchor, constant: -10),
            stack.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            stack.trailingAnchor.constraint(lessThanOrEqualTo: container.trailingAnchor),
            stack.topAnchor.constraint(equalTo: container.topAnchor),
            stack.heightAnchor.constraint(equalToConstant: 30),
            dvrRow.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            dvrRow.trailingAnchor.constraint(equalTo: container.trailingAnchor),
            dvrRow.bottomAnchor.constraint(equalTo: container.bottomAnchor),
            dvrRow.heightAnchor.constraint(equalToConstant: 20),
            stack.bottomAnchor.constraint(lessThanOrEqualTo: dvrRow.topAnchor, constant: -6),
        ])
        background.isHidden = true
        controlsBackground = background

        let liveBadge = NSTextField(labelWithString: "ライブ")
        liveBadgeLabel = liveBadge
        liveBadge.font = .systemFont(ofSize: 11, weight: .bold)
        liveBadge.textColor = NSColor(
            srgbRed: 83.0 / 255.0,
            green: 252.0 / 255.0,
            blue: 24.0 / 255.0,
            alpha: 1
        )
        liveBadge.setAccessibilityLabel("ライブ配信。シーク操作は利用できません")
        liveBadge.isSelectable = false

        let liveBackground = NSView()
        liveBackground.wantsLayer = true
        liveBackground.layer?.backgroundColor = NSColor.black.withAlphaComponent(0.72).cgColor
        liveBackground.layer?.cornerRadius = 5
        liveBackground.translatesAutoresizingMaskIntoConstraints = false
        liveBadge.translatesAutoresizingMaskIntoConstraints = false
        liveBackground.addSubview(liveBadge)
        addSubview(liveBackground)
        NSLayoutConstraint.activate([
            liveBackground.topAnchor.constraint(equalTo: topAnchor, constant: 12),
            liveBackground.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -12),
            liveBadge.leadingAnchor.constraint(equalTo: liveBackground.leadingAnchor, constant: 7),
            liveBadge.trailingAnchor.constraint(equalTo: liveBackground.trailingAnchor, constant: -7),
            liveBadge.topAnchor.constraint(equalTo: liveBackground.topAnchor, constant: 4),
            liveBadge.bottomAnchor.constraint(equalTo: liveBackground.bottomAnchor, constant: -4),
        ])
        liveBackground.isHidden = true
        liveBadgeBackground = liveBackground

        updateResponsiveLayout(force: true)
        updateLowLatencyControl()
        refreshControlState()
    }

    private func revealControls() {
        guard player?.currentItem != nil else { return }
        controlsAreVisible = true
        controlsBackground?.isHidden = false
        refreshControlState()
        scheduleControlsHide(after: 3)
    }

    private func scheduleControlsHide(after delay: TimeInterval) {
        guard !pointerInteractionIsActive else { return }
        hideControlsTimer?.invalidate()
        hideControlsTimer = Timer.scheduledTimer(withTimeInterval: delay, repeats: false) { [weak self] _ in
            Task { @MainActor [weak self] in
                guard let self else { return }
                guard !self.pointerInteractionIsActive else { return }
                self.controlsAreVisible = false
                self.controlsBackground?.isHidden = true
                self.hideControlsTimer = nil
            }
        }
    }

    @objc private func togglePlayPause(_ sender: NSButton) {
        revealControls()
        onPlaybackToggleRequested?()
    }

    @objc private func toggleMute(_ sender: NSButton) {
        revealControls()
        onMuteToggleRequested?()
    }

    @objc private func toggleFullScreen(_ sender: NSButton) {
        revealControls()
        window?.toggleFullScreen(nil)
    }

    @objc private func selectQuality(_ sender: NSPopUpButton) {
        guard let cap = PlayerQualityCap(rawValue: sender.selectedItem?.tag ?? -1) else {
            updateQualityControl()
            return
        }
        selectedQualityCap = cap
        updateQualityControl()
        onQualityCapChanged?(cap)
        revealControls()
    }

    @objc private func seekToLiveEdge(_ sender: NSButton) {
        revealControls()
        onLiveEdgeRequested?()
    }

    @objc private func selectSubtitle(_ sender: NSPopUpButton) {
        switch sender.indexOfSelectedItem {
        case 0:
            onSubtitleChanged?(0)
        case 1:
            onSubtitleAutoOn?()
        default:
            onSubtitleChanged?(sender.indexOfSelectedItem - 1)
        }
        revealControls()
    }

    @objc private func togglePictureInPicture(_ sender: NSButton) {
        revealControls()
        onPictureInPictureToggleRequested?()
    }

    @objc private func toggleLowLatency(_ sender: NSButton) {
        lowLatencyEnabled = sender.state == .on
        updateLowLatencyControl()
        onLowLatencyChanged?(lowLatencyEnabled)
        revealControls()
    }

    /// Keeps the native popup in sync when the cap is changed
    /// programmatically (the GPUI accessible mirror or a verification harness).
    fileprivate func setQualitySelection(_ cap: PlayerQualityCap) {
        selectedQualityCap = cap
        updateQualityControl()
    }

    private func updateQualityControl() {
        qualityButton?.selectItem(at: selectedQualityCap.menuIndex)
        qualityButton?.setAccessibilityLabel(loc("画質: \(selectedQualityCap.title(for: language))", "Quality: \(selectedQualityCap.title(for: language))"))
        qualityButton?.setAccessibilityHelp(loc("再生画質の上限を選択。現在は \(selectedQualityCap.title(for: language))", "Choose the maximum playback quality. Currently \(selectedQualityCap.title(for: language))"))
        qualityButton?.toolTip = loc("画質: \(selectedQualityCap.title(for: language))", "Quality: \(selectedQualityCap.title(for: language))")
    }

    private func updateLowLatencyControl() {
        let stateText = lowLatencyEnabled ? loc("オン", "On") : loc("オフ", "Off")
        lowLatencyButton?.state = lowLatencyEnabled ? .on : .off
        if isCompactLayout {
            let symbol = lowLatencyEnabled ? "bolt.fill" : "bolt.slash"
            if let image = NSImage(
                systemSymbolName: symbol,
                accessibilityDescription: loc("低遅延 \(stateText)", "Low latency \(stateText)")
            ) {
                lowLatencyButton?.image = image
                lowLatencyButton?.imagePosition = .imageOnly
                lowLatencyButton?.title = ""
            } else {
                lowLatencyButton?.image = nil
                lowLatencyButton?.imagePosition = .noImage
                lowLatencyButton?.title = lowLatencyEnabled ? "LL" : "LL×"
            }
        } else {
            lowLatencyButton?.image = nil
            lowLatencyButton?.imagePosition = .noImage
            lowLatencyButton?.title = loc("低遅延 \(stateText)", "Low latency \(stateText)")
        }
        lowLatencyButton?.contentTintColor = lowLatencyEnabled
            ? NSColor(srgbRed: 83.0 / 255.0, green: 252.0 / 255.0, blue: 24.0 / 255.0, alpha: 1)
            : NSColor.secondaryLabelColor
        lowLatencyButton?.setAccessibilityLabel(loc("低遅延モード: \(stateText)", "Low latency mode: \(stateText)"))
        lowLatencyButton?.setAccessibilityHelp(loc("ライブ再生のバッファを小さくして遅延を抑えます。不安定な回線ではオフにしてください", "Reduce the live buffer to lower latency. Turn it off on unstable connections"))
        lowLatencyButton?.toolTip = loc("低遅延モード: \(stateText)", "Low latency mode: \(stateText)")
    }

    private func updateSubtitleControl() {
        guard let button = subtitleButton else { return }
        let info = subtitleInfoProvider?() ?? (titles: [], selected: 0)
        button.removeAllItems()
        guard !info.titles.isEmpty else {
            button.addItem(withTitle: loc("字幕なし", "No subtitles"))
            button.selectItem(at: 0)
            button.isEnabled = false
            button.setAccessibilityLabel(loc("字幕なし", "No subtitles"))
            button.setAccessibilityHelp(loc("この配信には字幕トラックがありません", "This stream has no subtitle tracks"))
            button.toolTip = loc("字幕なし", "No subtitles")
            return
        }
        button.addItem(withTitle: loc("オフ", "Off"))
        button.addItem(withTitle: loc("オン（自動）", "On (auto)"))
        button.addItems(withTitles: info.titles)
        let selected = min(max(info.selected, 0), info.titles.count)
        button.selectItem(at: selected == 0 ? 0 : selected + 1)
        let label = selected == 0 ? loc("オフ", "Off") : info.titles[selected - 1]
        button.isEnabled = true
        button.setAccessibilityLabel(loc("字幕: \(label)", "Subtitles: \(label)"))
        button.setAccessibilityHelp(loc("字幕トラックを選択。現在は \(label)", "Choose a subtitle track. Currently \(label)"))
        button.toolTip = loc("字幕: \(label)", "Subtitles: \(label)")
    }

    private func updatePictureInPictureControl(active: Bool? = nil) {
        guard let button = pictureInPictureButton else { return }
        let isActive = active ?? (pictureInPictureStateProvider?() ?? false)
        let symbol = isActive ? "pip.exit" : "pip.enter"
        let label = isActive ? loc("ピクチャ・イン・ピクチャを終了", "Exit Picture in Picture") : loc("ピクチャ・イン・ピクチャ", "Picture in Picture")
        if let image = NSImage(
            systemSymbolName: symbol,
            accessibilityDescription: label
        ) {
            button.image = image
            button.imagePosition = .imageOnly
            button.title = ""
        } else {
            button.image = nil
            button.imagePosition = .noImage
            button.title = isActive ? loc("PiP終了", "Exit PiP") : "PiP"
        }
        button.setAccessibilityLabel(label)
        button.setAccessibilityHelp(
            isActive
                ? "ピクチャ・イン・ピクチャを終了してウィンドウ内の再生に戻します"
                : "再生中の映像をピクチャ・イン・ピクチャで表示します"
        )
        button.toolTip = label
    }

    private func changeVolume(_ value: Double) {
        revealControls()
        let percent = Int((value * 100).rounded())
        volumeValueLabel?.stringValue = "\(percent)%"
        volumeValueLabel?.setAccessibilityLabel("音量 \(percent)%")
        onVolumeChanged?(Float(value))
    }

    private func seekDVRPosition(_ value: Double) {
        guard
            let item = player?.currentItem,
            item.status == .readyToPlay
        else {
            refreshControlState()
            return
        }
        let timeline = dvrPositionProvider?()?.timeline
            ?? SeekableTimeline(timeRanges: item.seekableTimeRanges)
        guard let timeline, let target = timeline.time(at: value) else {
            refreshControlState()
            return
        }
        onSeekRequested?(target)
    }

    private func beginPointerInteraction() {
        guard player?.currentItem != nil else { return }
        pointerInteractionIsActive = true
        hideControlsTimer?.invalidate()
        hideControlsTimer = nil
        controlsAreVisible = true
        controlsBackground?.isHidden = false
    }

    private func endPointerInteraction() {
        pointerInteractionIsActive = false
        revealControls()
    }

    private func formatTimelineTime(_ seconds: Double) -> String {
        guard seconds.isFinite, seconds >= 0 else { return "--:--" }
        let totalSeconds = Int(seconds.rounded(.down))
        let hours = totalSeconds / 3_600
        let minutes = (totalSeconds / 60) % 60
        let remainingSeconds = totalSeconds % 60
        if hours > 0 {
            return String(format: "%d:%02d:%02d", hours, minutes, remainingSeconds)
        }
        return String(format: "%d:%02d", totalSeconds / 60, remainingSeconds)
    }
}

@MainActor
private final class PlayerOverlayButton: NSButton {
    var onInteractionBegan: (() -> Void)?
    var onInteractionEnded: (() -> Void)?

    override func mouseDown(with event: NSEvent) {
        onInteractionBegan?()
        defer { onInteractionEnded?() }
        super.mouseDown(with: event)
    }
}

@MainActor
private final class PlayerOverlayPopUpButton: NSPopUpButton {
    var onInteractionBegan: (() -> Void)?
    var onInteractionEnded: (() -> Void)?

    override func mouseDown(with event: NSEvent) {
        onInteractionBegan?()
        defer { onInteractionEnded?() }
        super.mouseDown(with: event)
    }
}

@MainActor
private final class PlayerOverlaySlider: NSControl {
    var onInteractionBegan: (() -> Void)?
    var onInteractionEnded: (() -> Void)?
    var onValueChanged: ((Double) -> Void)?

    var minValue: Double = 0
    var maxValue: Double = 1
    var fillColor: NSColor = .white

    private var storedValue: Double = 0
    private var isDragging = false

    init(value: Double, minValue: Double, maxValue: Double) {
        self.minValue = minValue
        self.maxValue = maxValue
        super.init(frame: .zero)
        translatesAutoresizingMaskIntoConstraints = false
        setAccessibilityElement(true)
        setAccessibilityRole(.slider)
        setAccessibilityMinValue(minValue)
        setAccessibilityMaxValue(maxValue)
        doubleValue = value
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    override var doubleValue: Double {
        get { storedValue }
        set {
            storedValue = Swift.min(Swift.max(newValue, minValue), maxValue)
            setAccessibilityValue(storedValue)
            needsDisplay = true
        }
    }

    override var floatValue: Float {
        get { Float(doubleValue) }
        set { doubleValue = Double(newValue) }
    }

    override var isEnabled: Bool {
        didSet { needsDisplay = true }
    }

    override var intrinsicContentSize: NSSize {
        NSSize(width: NSView.noIntrinsicMetric, height: 20)
    }

    override func draw(_ dirtyRect: NSRect) {
        super.draw(dirtyRect)

        let knobRadius: CGFloat = 4.5
        let trackHeight: CGFloat = 3
        let trackY = (bounds.height - trackHeight) / 2
        let trackRect = NSRect(
            x: knobRadius,
            y: trackY,
            width: Swift.max(bounds.width - knobRadius * 2, 0),
            height: trackHeight
        )
        let fraction: CGFloat = maxValue > minValue
            ? CGFloat((storedValue - minValue) / (maxValue - minValue)).clamped(to: 0...1)
            : 0
        let knobX = trackRect.minX + trackRect.width * fraction
        let alpha: CGFloat = isEnabled ? 1 : 0.35

        NSColor.white.withAlphaComponent(0.22 * alpha).setFill()
        NSBezierPath(
            roundedRect: trackRect,
            xRadius: trackHeight / 2,
            yRadius: trackHeight / 2
        ).fill()

        if knobX > trackRect.minX {
            let fillRect = NSRect(
                x: trackRect.minX,
                y: trackY,
                width: knobX - trackRect.minX,
                height: trackHeight
            )
            fillColor.withAlphaComponent(alpha).setFill()
            NSBezierPath(
                roundedRect: fillRect,
                xRadius: trackHeight / 2,
                yRadius: trackHeight / 2
            ).fill()
        }

        let knobRect = NSRect(
            x: knobX - knobRadius,
            y: bounds.midY - knobRadius,
            width: knobRadius * 2,
            height: knobRadius * 2
        )
        NSColor.white.withAlphaComponent(0.95 * alpha).setFill()
        NSBezierPath(ovalIn: knobRect).fill()
    }

    override func mouseDown(with event: NSEvent) {
        guard isEnabled else { return }
        isDragging = true
        onInteractionBegan?()
        updateValue(with: event)
        if isContinuous {
            onValueChanged?(doubleValue)
        }
    }

    override func mouseDragged(with event: NSEvent) {
        guard isEnabled, isDragging else { return }
        updateValue(with: event)
        if isContinuous {
            onValueChanged?(doubleValue)
        }
    }

    override func mouseUp(with event: NSEvent) {
        guard isEnabled, isDragging else { return }
        updateValue(with: event)
        onValueChanged?(doubleValue)
        isDragging = false
        onInteractionEnded?()
    }

    override func accessibilityPerformIncrement() -> Bool {
        guard isEnabled else { return false }
        doubleValue += (maxValue - minValue) * 0.05
        onValueChanged?(doubleValue)
        return true
    }

    override func accessibilityPerformDecrement() -> Bool {
        guard isEnabled else { return false }
        doubleValue -= (maxValue - minValue) * 0.05
        onValueChanged?(doubleValue)
        return true
    }

    private func updateValue(with event: NSEvent) {
        let point = convert(event.locationInWindow, from: nil)
        let knobRadius: CGFloat = 4.5
        let trackWidth = Swift.max(bounds.width - knobRadius * 2, 1)
        let fraction = ((point.x - knobRadius) / trackWidth).clamped(to: 0...1)
        doubleValue = minValue + Double(fraction) * (maxValue - minValue)
    }
}

private extension Comparable {
    func clamped(to range: ClosedRange<Self>) -> Self {
        Swift.min(Swift.max(self, range.lowerBound), range.upperBound)
    }
}

/// Owns one AVPlayer and its current item. All methods are called on the main actor.
@MainActor
public final class PlayerSession {
    public let surface = PlayerSurface(frame: .zero)
    public var onStatusChanged: ((String) -> Void)?
    public var onPlaybackSnapshotChanged: ((PlaybackSnapshot) -> Void)?

    private let player = AVPlayer()
    private let dvrLoaderPlayer = AVPlayer()
    private let lowLatencyProxy = LowLatencyHLSProxy()
    private enum DVRItemState {
        case unavailable
        case loading
        case ready
        case failed
    }
    private var liveItem: AVPlayerItem?
    private var dvrItem: AVPlayerItem?
    private var liveURL: URL?
    private var dvrURL: URL?
    private var usesLowLatencyPlaylist = false
    private var lowLatencyProxyURL: URL?
    private var lowLatencyProxyItem: AVPlayerItem?
    private var proxyLiveURL: URL?
    private var proxyIsRunning = false
    private var proxyFallbackPerformed = false
    private var proxyGeneration = 0
    private var currentSource: PlaybackSourceKind = .live
    private var dvrItemState: DVRItemState = .unavailable
    private var dvrItemStatusObservation: NSKeyValueObservation?
    private var dvrSeekableRangesObservation: NSKeyValueObservation?
    private var dvrDurationObservation: NSKeyValueObservation?
    private var pendingSeekTarget: CMTime?
    private var itemStatusObservation: NSKeyValueObservation?
    private var seekableRangesObservation: NSKeyValueObservation?
    private var itemDidEndObserver: NSObjectProtocol?
    private var itemFailedObserver: NSObjectProtocol?
    private var transportObservation: NSKeyValueObservation?
    private var timeObserver: Any?
    private var activeRequestID: UUID?
    private var shouldStartWhenReady = false
    private var shouldStartAtLiveEdge = false
    private var currentItemAllowsLiveEdgeSeek = false
    private var isShutdown = false
    private var selectedQualityCap: PlayerQualityCap = .automatic
    private var lowLatencyEnabled = true
    private var isVodMode = false
    private var dvrReviewActive = false
    private var subtitleGroup: AVMediaSelectionGroup?
    private var subtitleOptions: [AVMediaSelectionOption] = []
    private var subtitleTitles: [String] = []
    private var selectedSubtitleIndex = 0
    private var selectedSubtitleKey: String?
    private var lastSubtitleRefresh = Date.distantPast
    private var subtitleLoadGeneration = 0
    private var subtitleLoadInFlight = false
    private var pictureInPictureController: AVPictureInPictureController?
    private var pictureInPictureDelegate: PictureInPictureDelegate?
    private var pictureInPicturePossibleObservation: NSKeyValueObservation?
    private var wantsPictureInPicture = false
    private var lastLiveCatchUpSeek = Date.distantPast
    #if DEBUG
    private var lastLatencyLog = Date.distantPast
    #endif

    public private(set) var stateCode: Int32 = KickPlayerState.idle.rawValue
    public private(set) var statusText = "Enter an HLS playlist URL."

    /// Display titles of the current item's legible tracks, in playlist order.
    public var subtitleTrackTitles: [String] { subtitleTitles }
    /// 0 means subtitles are off; 1...count indexes `subtitleTrackTitles`.
    public var selectedSubtitleTrackIndex: Int { selectedSubtitleIndex }
    public var isPictureInPictureSupported: Bool {
        AVPictureInPictureController.isPictureInPictureSupported()
    }
    public var isPictureInPictureActive: Bool {
        pictureInPictureController?.isPictureInPictureActive ?? false
    }
    /// True after shutdown; the bridge maps this to its own error code.
    public var isClosed: Bool { isShutdown }
    /// The active quality cap, including changes made in the native popup.
    public var qualityCap: PlayerQualityCap { selectedQualityCap }
    /// The current item's presentation size, which reflects the rendition
    /// AVPlayer has selected. Zero before the item reports a video track.
    public var currentVideoSize: CGSize {
        player.currentItem?.presentationSize ?? .zero
    }
    /// The selected variant's video track size. This can differ from
    /// `currentVideoSize` while AVPlayer switches renditions.
    public var currentVideoTrackSize: CGSize {
        player.currentItem?.tracks
            .first { $0.assetTrack?.mediaType == .video }?
            .assetTrack?.naturalSize ?? .zero
    }
    /// The last access-log indicated bitrate in bits per second, or zero when
    /// AVPlayer has not produced an access log yet.
    public var currentIndicatedBitrate: Double {
        player.currentItem?.accessLog()?.events.last?.indicatedBitrate ?? 0
    }
    /// The last access-log observed bitrate in bits per second.
    public var currentObservedBitrate: Double {
        player.currentItem?.accessLog()?.events.last?.observedBitrate ?? 0
    }

    public init() {
        surface.player = player
        dvrLoaderPlayer.isMuted = true
        dvrLoaderPlayer.volume = 0
        dvrLoaderPlayer.pause()
        dvrLoaderPlayer.automaticallyWaitsToMinimizeStalling = false
        surface.liveEdgeSeekAvailability = { [weak self] in
            guard let self, !self.isShutdown else { return false }
            return self.currentItemAllowsLiveEdgeSeek
        }
        surface.dvrPositionProvider = { [weak self] in
            guard let self, !self.isShutdown else { return nil }
            return self.currentDVRPosition()
        }
        surface.onPlaybackToggleRequested = { [weak self] in
            guard let self, !self.isShutdown, self.player.currentItem != nil else { return }
            switch self.player.timeControlStatus {
            case .paused:
                try? self.resume()
            case .waitingToPlayAtSpecifiedRate, .playing:
                self.pause()
            @unknown default:
                break
            }
        }
        surface.onMuteToggleRequested = { [weak self] in
            guard let self, !self.isShutdown, self.player.currentItem != nil else { return }
            self.setMuted(!self.player.isMuted)
        }
        surface.onVolumeChanged = { [weak self] volume in
            guard let self, !self.isShutdown, self.player.currentItem != nil else { return }
            self.setVolume(volume)
        }
        surface.onSeekRequested = { [weak self] time in
            guard let self, !self.isShutdown else { return }
            self.seekDVR(to: time)
        }
        surface.onQualityCapChanged = { [weak self] cap in
            guard let self, !self.isShutdown else { return }
            self.selectedQualityCap = cap
            if let item = self.player.currentItem {
                cap.apply(to: item)
            }
        }
        surface.onLiveEdgeRequested = { [weak self] in
            guard let self, !self.isShutdown else { return }
            self.seekToLiveEdge()
        }
        surface.onLowLatencyChanged = { [weak self] enabled in
            guard let self, !self.isShutdown else { return }
            self.lowLatencyEnabled = enabled
            self.applyLatencyPolicy()
            self.reloadLiveItemForLowLatencyChange()
        }
        surface.subtitleInfoProvider = { [weak self] in
            guard let self, !self.isShutdown else { return (titles: [], selected: 0) }
            return (self.subtitleTitles, self.selectedSubtitleIndex)
        }
        surface.onSubtitleChanged = { [weak self] index in
            guard let self, !self.isShutdown else { return }
            _ = self.setSubtitle(index: index)
        }
        surface.onSubtitleAutoOn = { [weak self] in
            guard let self, !self.isShutdown else { return }
            _ = self.enablePreferredSubtitle()
        }
        surface.pictureInPictureStateProvider = { [weak self] in
            self?.isPictureInPictureActive ?? false
        }
        surface.onPictureInPictureToggleRequested = { [weak self] in
            guard let self, !self.isShutdown else { return }
            self.togglePictureInPicture()
        }
        transportObservation = player.observe(\.timeControlStatus, options: [.initial, .new]) { [weak self] observedPlayer, _ in
            let status = observedPlayer.timeControlStatus
            Task { @MainActor [weak self] in
                self?.handle(transportStatus: status)
            }
        }
        timeObserver = player.addPeriodicTimeObserver(
            forInterval: CMTime(seconds: 0.5, preferredTimescale: 600),
            queue: .main
        ) { [weak self] time in
            Task { @MainActor [weak self] in
                self?.publishSnapshot(time: time)
            }
        }
    }

    /// Replaces the current item and starts it once AVFoundation reports readiness.
    /// `startAtLiveEdge` explicitly seeks to the end of the seekable range before playback,
    /// independent of the low-latency toggle. It is intended for DVR EVENT playlists.
    public func playHLS(_ urlString: String, startAtLiveEdge: Bool = false) throws {
        guard !isShutdown else { throw KickPlayerError.sessionClosed }
        let liveURL = try validatedURL(urlString)
        startPair(
            liveURL: liveURL,
            dvrURL: nil,
            startAtLiveEdge: startAtLiveEdge,
            allowLowLatencyPlaylist: false
        )
    }

    /// Starts a live/DVR pair. The live URL is the low-latency source; the DVR URL
    /// is loaded in the background and used when the user seeks backward.
    public func playHLSPair(
        liveURL: String,
        dvrURL: String,
        startAtLiveEdge: Bool = false
    ) throws {
        guard !isShutdown else { throw KickPlayerError.sessionClosed }
        let liveURL = try validatedURL(liveURL)
        let dvrURL = validatedOptionalURL(dvrURL)
        let allowLowLatencyPlaylist =
            ProcessInfo.processInfo.environment["KICK_LOW_LATENCY_PREFETCH"] == "1"
        startPair(
            liveURL: liveURL,
            dvrURL: dvrURL,
            startAtLiveEdge: startAtLiveEdge,
            allowLowLatencyPlaylist: allowLowLatencyPlaylist
        )
    }

    /// Starts an existing item. Kept module-internal so lifecycle tests can use local assets.
    /// `startAtLiveEdge` is used by DVR EVENT playlists.
    func play(item: AVPlayerItem, startAtLiveEdge: Bool = false) throws {
        guard !isShutdown else { throw KickPlayerError.sessionClosed }
        dvrReviewActive = false
        clearSourcePair()
        liveItem = item
        dvrItem = nil
        dvrItemState = .unavailable
        startCurrentItem(
            item,
            source: .live,
            startAtLiveEdge: startAtLiveEdge,
            seekTarget: nil,
            resumeAfterSeek: true
        )
    }

    private func validatedURL(_ urlString: String) throws -> URL {
        guard
            let components = URLComponents(string: urlString.trimmingCharacters(in: .whitespacesAndNewlines)),
            let scheme = components.scheme?.lowercased(),
            (scheme == "http" || scheme == "https"),
            components.host != nil,
            let url = components.url
        else {
            throw KickPlayerError.invalidURL
        }
        return url
    }

    private func validatedOptionalURL(_ urlString: String) -> URL? {
        let trimmed = urlString.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return nil }
        return try? validatedURL(trimmed)
    }

    private func makeLiveItem(url: URL) -> AVPlayerItem {
        let item = AVPlayerItem(url: url)
        selectedQualityCap.apply(to: item)
        return item
    }

    /// Starts the loopback proxy for the current pair live source. The original
    /// live URL stays active until the proxy reports its local playlist URL.
    private func startLowLatencyProxy(for liveURL: URL) {
        guard lowLatencyEnabled, usesLowLatencyPlaylist, !isShutdown else { return }
        if proxyIsRunning, proxyLiveURL == liveURL, lowLatencyProxyURL != nil {
            return
        }

        proxyIsRunning = false
        proxyLiveURL = nil
        proxyGeneration += 1
        let generation = proxyGeneration
        lowLatencyProxy.start(realLiveURL: liveURL) { [weak self] result in
            Task { @MainActor [weak self] in
                guard let self, !self.isShutdown else { return }
                guard
                    generation == self.proxyGeneration,
                    self.lowLatencyEnabled,
                    self.usesLowLatencyPlaylist,
                    self.liveURL == liveURL,
                    !self.proxyFallbackPerformed
                else { return }

                switch result {
                case .success(let localURL):
                    #if DEBUG
                    print("PROXY_SWAP")
                    fflush(stdout)
                    #endif
                    self.proxyIsRunning = true
                    self.proxyLiveURL = liveURL
                    self.lowLatencyProxyURL = localURL
                    self.installProxyLiveItem(localURL: localURL)
                case .failure:
                    #if DEBUG
                    print("PROXY_FAIL")
                    fflush(stdout)
                    #endif
                    self.proxyIsRunning = false
                    self.proxyLiveURL = nil
                    self.lowLatencyProxyURL = nil
                    self.lowLatencyProxyItem = nil
                }
            }
        }
    }

    private func installProxyLiveItem(localURL: URL) {
        guard lowLatencyEnabled, usesLowLatencyPlaylist, !isShutdown else { return }
        let item = makeLiveItem(url: localURL)
        lowLatencyProxyItem = item
        liveItem = item
        guard currentSource == .live else { return }

        let shouldResume = shouldStartWhenReady
            || player.timeControlStatus == .playing
            || player.rate > 0
        startCurrentItem(
            item,
            source: .live,
            startAtLiveEdge: true,
            seekTarget: nil,
            resumeAfterSeek: shouldResume
        )
    }

    private func stopLowLatencyProxy() {
        proxyGeneration += 1
        lowLatencyProxy.stop()
        proxyIsRunning = false
        proxyLiveURL = nil
        lowLatencyProxyURL = nil
        lowLatencyProxyItem = nil
    }

    private func reloadLiveItemForLowLatencyChange() {
        guard let liveURL else { return }

        if lowLatencyEnabled && usesLowLatencyPlaylist {
            proxyFallbackPerformed = false
            if proxyIsRunning, let lowLatencyProxyURL {
                guard lowLatencyProxyItem == nil else { return }
                let item = makeLiveItem(url: lowLatencyProxyURL)
                lowLatencyProxyItem = item
                liveItem = item
                if currentSource == .live {
                    let shouldResume = shouldStartWhenReady
                        || player.timeControlStatus == .playing
                        || player.rate > 0
                    startCurrentItem(
                        item,
                        source: .live,
                        startAtLiveEdge: true,
                        seekTarget: nil,
                        resumeAfterSeek: shouldResume
                    )
                }
            } else {
                // Keep the original URL playing until the proxy is ready.
                startLowLatencyProxy(for: liveURL)
            }
            return
        }

        let wasUsingProxyItem = lowLatencyProxyItem != nil
        stopLowLatencyProxy()
        let item = makeLiveItem(url: liveURL)
        liveItem = item
        if currentSource == .live, wasUsingProxyItem {
            let shouldResume = shouldStartWhenReady
                || player.timeControlStatus == .playing
                || player.rate > 0
            startCurrentItem(
                item,
                source: .live,
                startAtLiveEdge: true,
                seekTarget: nil,
                resumeAfterSeek: shouldResume
            )
        }
    }

    private func startPair(
        liveURL: URL,
        dvrURL: URL?,
        startAtLiveEdge: Bool,
        allowLowLatencyPlaylist: Bool
    ) {
        dvrReviewActive = false
        clearSourcePair()
        self.liveURL = liveURL
        self.dvrURL = dvrURL
        self.usesLowLatencyPlaylist = allowLowLatencyPlaylist
        self.proxyFallbackPerformed = false
        let liveItem = makeLiveItem(url: liveURL)
        let dvrItem = dvrURL.map { AVPlayerItem(url: $0) }
        self.liveItem = liveItem
        self.dvrItem = dvrItem
        self.dvrItemState = dvrItem == nil ? .unavailable : .loading
        observeDVRItemIfNeeded(dvrItem)
        if let dvrItem {
            selectedQualityCap.apply(to: dvrItem)
            dvrLoaderPlayer.replaceCurrentItem(with: dvrItem)
            dvrLoaderPlayer.isMuted = true
            dvrLoaderPlayer.volume = 0
            dvrLoaderPlayer.pause()
        } else {
            dvrLoaderPlayer.replaceCurrentItem(with: nil)
        }
        startCurrentItem(
            liveItem,
            source: .live,
            startAtLiveEdge: startAtLiveEdge,
            seekTarget: nil,
            resumeAfterSeek: true
        )
        if allowLowLatencyPlaylist && lowLatencyEnabled {
            startLowLatencyProxy(for: liveURL)
        }
    }

    private func startCurrentItem(
        _ item: AVPlayerItem,
        source: PlaybackSourceKind,
        startAtLiveEdge: Bool,
        seekTarget: CMTime?,
        resumeAfterSeek: Bool
    ) {
        removeCurrentItemObservations()
        let requestID = UUID()
        activeRequestID = requestID
        currentSource = source
        pendingSeekTarget = seekTarget
        shouldStartWhenReady = resumeAfterSeek
        shouldStartAtLiveEdge = startAtLiveEdge
        currentItemAllowsLiveEdgeSeek = source == .dvr || startAtLiveEdge
        setState(.loading, text: source == .dvr ? "Loading DVR…" : "Loading stream…")
        resetSubtitleOptionsForNewItem()
        observeCurrentItem(item, requestID: requestID)
        selectedQualityCap.apply(to: item)
        applyLatencyPolicy(to: item)
        player.pause()
        player.replaceCurrentItem(with: item)
        publishSnapshot()
    }

    private func observeCurrentItem(_ item: AVPlayerItem, requestID: UUID) {
        itemStatusObservation = item.observe(\.status, options: [.initial, .new]) { [weak self] observedItem, _ in
            let itemStatus = observedItem.status
            let error = observedItem.error as NSError?
            let errorSummary = error.map { "\($0.domain), code \($0.code)" }

            Task { @MainActor [weak self] in
                self?.handle(itemStatus: itemStatus, errorSummary: errorSummary, requestID: requestID)
            }
        }
        observeTerminalEvents(for: item, requestID: requestID)
        observeSeekableRanges(for: item, requestID: requestID)
    }

    private func clearSourcePair() {
        stopLowLatencyProxy()
        proxyFallbackPerformed = false
        liveItem = nil
        dvrItem = nil
        liveURL = nil
        dvrURL = nil
        usesLowLatencyPlaylist = false
        dvrItemState = .unavailable
        currentSource = .live
        pendingSeekTarget = nil
        dvrItemStatusObservation?.invalidate()
        dvrItemStatusObservation = nil
        dvrSeekableRangesObservation?.invalidate()
        dvrSeekableRangesObservation = nil
        dvrDurationObservation?.invalidate()
        dvrDurationObservation = nil
        dvrLoaderPlayer.pause()
        dvrLoaderPlayer.replaceCurrentItem(with: nil)
    }

    private func observeDVRItemIfNeeded(_ item: AVPlayerItem?) {
        dvrItemStatusObservation?.invalidate()
        dvrItemStatusObservation = nil
        dvrSeekableRangesObservation?.invalidate()
        dvrSeekableRangesObservation = nil
        dvrDurationObservation?.invalidate()
        dvrDurationObservation = nil

        guard let item else {
            dvrItemState = .unavailable
            return
        }
        dvrItemState = .loading
        dvrItemStatusObservation = item.observe(\.status, options: [.initial, .new]) { [weak self] observed, _ in
            Task { @MainActor [weak self] in
                self?.handleDVRItemStatus(observed.status, item: observed)
            }
        }
        dvrSeekableRangesObservation = item.observe(\.seekableTimeRanges, options: [.initial, .new]) { [weak self] _, _ in
            Task { @MainActor [weak self] in
                self?.publishSnapshot()
            }
        }
        dvrDurationObservation = item.observe(\.duration, options: [.initial, .new]) { [weak self] _, _ in
            Task { @MainActor [weak self] in
                self?.publishSnapshot()
            }
        }
    }

    private func handleDVRItemStatus(_ status: AVPlayerItem.Status, item: AVPlayerItem) {
        guard item === dvrItem else { return }
        switch status {
        case .readyToPlay:
            dvrItemState = .ready
            publishSnapshot()
        case .failed:
            dvrItemState = .failed
            if currentSource == .dvr, let requestID = activeRequestID {
                fallbackToLiveAfterDVRFailure(requestID: requestID)
            } else {
                publishSnapshot()
            }
        case .unknown:
            break
        @unknown default:
            break
        }
    }

    private func fallbackToLiveAfterDVRFailure(requestID: UUID) {
        guard activeRequestID == requestID else { return }
        guard let liveItem else {
            finishCurrentItem(
                requestID: requestID,
                expectedItem: player.currentItem,
                state: .failed,
                text: "Playback failed."
            )
            return
        }
        let resume = shouldStartWhenReady
        startCurrentItem(
            liveItem,
            source: .live,
            startAtLiveEdge: true,
            seekTarget: nil,
            resumeAfterSeek: resume
        )
    }

    private func dvrTimeline(for item: AVPlayerItem) -> SeekableTimeline? {
        guard item.status != .failed else { return nil }
        if let timeline = SeekableTimeline(timeRanges: item.seekableTimeRanges) {
            return timeline
        }
        let duration = item.duration
        guard
            duration.isValid,
            duration.isNumeric,
            duration.seconds.isFinite,
            duration.seconds > 0
        else { return nil }
        return SeekableTimeline(timeRanges: [
            NSValue(timeRange: CMTimeRange(start: .zero, duration: duration)),
        ])
    }

    private func currentDVRPosition() -> (timeline: SeekableTimeline, currentTime: CMTime)? {
        guard let dvrItem, let dvrTimeline = dvrTimeline(for: dvrItem) else { return nil }
        let currentTime: CMTime
        if player.currentItem === liveItem {
            let liveTimeline = liveItem.flatMap { SeekableTimeline(timeRanges: $0.seekableTimeRanges) }
            currentTime = PlaybackSourceSelection.dvrPosition(
                currentSource: .live,
                currentTime: player.currentTime(),
                liveTimeline: liveTimeline,
                dvrTimeline: dvrTimeline
            )
        } else {
            currentTime = player.currentTime()
        }
        return (dvrTimeline, currentTime)
    }

    private func seekDVR(to proposedTime: CMTime) {
        guard !isShutdown else { return }
        // A user-requested DVR position must not be pulled back to live by the
        // low-latency catch-up. The flag clears when the playhead returns to
        // the live edge or the user presses the live button.
        dvrReviewActive = true
        let wasPlaying = player.timeControlStatus == .playing || player.rate > 0
        let liveTimeline = liveItem.flatMap { SeekableTimeline(timeRanges: $0.seekableTimeRanges) }
        let dvrTimeline = dvrItem.flatMap { dvrTimeline(for: $0) }
        switch PlaybackSourceSelection.sourceForSeek(
            target: proposedTime,
            liveTimeline: liveTimeline,
            dvrTimeline: dvrTimeline
        ) {
        case .live:
            if currentSource == .live {
                seekWithinSeekableRange(to: proposedTime)
            } else {
                switchToLive(seekTarget: proposedTime, resume: wasPlaying)
            }
        case .dvr:
            if currentSource == .dvr {
                seekWithinSeekableRange(to: proposedTime)
            } else {
                switchToDVR(seekTarget: proposedTime, resume: wasPlaying)
            }
        }
    }

    private func switchToDVR(seekTarget: CMTime, resume: Bool) {
        guard let dvrURL, dvrItemState != .failed else {
            seekWithinSeekableRange(to: seekTarget)
            return
        }
        let visibleDVRItem = AVPlayerItem(url: dvrURL)
        startCurrentItem(
            visibleDVRItem,
            source: .dvr,
            startAtLiveEdge: false,
            seekTarget: seekTarget,
            resumeAfterSeek: resume
        )
    }

    private func switchToLive(seekTarget: CMTime?, resume: Bool) {
        guard let liveItem else { return }
        startCurrentItem(
            liveItem,
            source: .live,
            startAtLiveEdge: seekTarget == nil,
            seekTarget: seekTarget,
            resumeAfterSeek: resume
        )
    }

    private func seekToPendingTarget(item: AVPlayerItem, target: CMTime, requestID: UUID) {
        let wasPlaying = shouldStartWhenReady
        let clampedTarget = SeekableTimeline(timeRanges: item.seekableTimeRanges)?.clampedTime(target) ?? target
        player.seek(to: clampedTarget, toleranceBefore: .zero, toleranceAfter: .zero) { [weak self] _ in
            Task { @MainActor [weak self] in
                guard let self else { return }
                self.publishSnapshot()
                guard
                    self.activeRequestID == requestID,
                    self.player.currentItem === item,
                    item.status == .readyToPlay
                else { return }

                if wasPlaying && self.shouldStartWhenReady {
                    self.player.play()
                    self.setState(.playbackRequested, text: "Playback requested.")
                } else {
                    self.setState(.ready, text: "Ready.")
                }
                self.publishSnapshot()
            }
        }
    }

    public func pause() {
        shouldStartWhenReady = false
        player.pause()
        if player.currentItem == nil {
            setState(.idle, text: "No stream loaded.")
        } else {
            setState(.paused, text: "Paused.")
        }
        publishSnapshot()
    }

    /// Resumes the currently loaded item without replacing it or resolving its URL again.
    public func resume() throws {
        guard !isShutdown else { throw KickPlayerError.sessionClosed }
        guard player.currentItem != nil else { throw KickPlayerError.noCurrentItem }

        shouldStartWhenReady = true
        player.play()
        setState(.playbackRequested, text: "Resume requested.")
        publishSnapshot()
    }

    /// Seeks within the DVR/live timeline using the same source selection as
    /// the native slider. Intended for the demo/verification harness.
    public func seekDVR(toSeconds seconds: Double) {
        guard seconds.isFinite, seconds >= 0 else { return }
        seekDVR(to: CMTime(seconds: seconds, preferredTimescale: 600))
    }

    public func setVolume(_ volume: Float) {
        guard volume.isFinite else { return }
        player.volume = min(max(volume, 0), 1)
        publishSnapshot()
    }

    /// Adds a scrolling comment above the video. Call from the main thread.
    public func pushDanmaku(_ text: String) {
        surface.pushDanmaku(text)
    }

    /// Applies the font size (pt) and opacity used by new comments.
    public func setDanmakuConfig(fontSize: CGFloat, opacity: CGFloat) {
        surface.setDanmakuConfig(fontSize: fontSize, opacity: opacity)
    }

    /// Seeks the current item to an absolute position in seconds.
    public func seek(toSeconds seconds: Double) {
        guard seconds.isFinite, seconds >= 0 else { return }
        player.seek(
            to: CMTime(seconds: seconds, preferredTimescale: 600),
            toleranceBefore: .zero,
            toleranceAfter: .zero
        )
    }

    public func setMuted(_ muted: Bool) {
        player.isMuted = muted
        publishSnapshot()
    }

    /// Marks the current item as a VOD/clip so the native overlay labels the
    /// seek bar as a playback position instead of a live DVR window.
    public func setLanguage(_ language: PlayerLanguage) {
        surface.setLanguage(language)
        subtitleTitles = subtitleOptions.enumerated().map { index, option in
            SubtitleSelection.title(for: option, fallbackIndex: index + 1, language: language)
        }
        surface.reloadSubtitleControl()
    }

    public func setVodMode(_ enabled: Bool) {
        isVodMode = enabled
        surface.isVodMode = enabled
        surface.refreshControlState()
    }

    /// Applies a resolution/bit-rate cap to the current item. AVPlayer treats
    /// both values as preferences; the bit-rate ceiling is what usually wins.
    public func setQualityCap(_ cap: PlayerQualityCap) {
        selectedQualityCap = cap
        surface.setQualitySelection(cap)
        if let item = player.currentItem {
            cap.apply(to: item)
        }
    }

    /// Selects a legible track. 0 turns subtitles off; 1...count selects one.
    /// Returns false when the item has no legible group or the index is invalid.
    @discardableResult
    public func setSubtitle(index: Int) -> Bool {
        guard let item = player.currentItem, let group = subtitleGroup else { return false }
        guard let normalized = SubtitleSelection.normalizedIndex(
            index,
            optionCount: subtitleOptions.count
        ) else { return false }

        if normalized == 0 {
            item.select(nil, in: group)
            selectedSubtitleIndex = 0
            selectedSubtitleKey = nil
        } else {
            let option = subtitleOptions[normalized - 1]
            item.select(option, in: group)
            selectedSubtitleIndex = normalized
            selectedSubtitleKey = SubtitleSelection.key(for: option)
        }
        surface.reloadSubtitleControl()
        return true
    }

    /// Turns subtitles on with the app language's track when the stream offers
    /// it (Japanese for the Japanese UI), falling back to the first track.
    @discardableResult
    public func enablePreferredSubtitle() -> Bool {
        guard !subtitleOptions.isEmpty else { return false }
        return setSubtitle(index: preferredSubtitleOptionIndex() + 1)
    }

    private func preferredSubtitleOptionIndex() -> Int {
        let preferred = surface.language == .english
            ? ["en", "english"]
            : ["ja", "jp", "japanese", "日本語"]
        if let index = subtitleOptions.firstIndex(where: { option in
            subtitleOption(option, matchesAny: preferred)
        }) {
            return index
        }
        return 0
    }

    private func subtitleOption(
        _ option: AVMediaSelectionOption,
        matchesAny tags: [String]
    ) -> Bool {
        SubtitleSelection.languageMatches(
            language: option.extendedLanguageTag ?? option.locale?.identifier ?? "",
            name: option.displayName,
            tags: tags
        )
    }

    /// Loads the legible-track list for a newly ready item and re-applies the
    /// previous choice by language/name when the same track still exists.
    /// The async asset API is used because the synchronous group lookup can
    /// return nil for a remote live item that is still loading its renditions.
    private func refreshSubtitleOptions(for item: AVPlayerItem) {
        subtitleLoadGeneration = subtitleLoadGeneration &+ 1
        let generation = subtitleLoadGeneration
        subtitleLoadInFlight = true
        Task { @MainActor [weak self, weak item] in
            let group = try? await item?.asset.loadMediaSelectionGroup(for: .legible)
            guard let self,
                  self.subtitleLoadGeneration == generation,
                  let item,
                  self.player.currentItem === item
            else { return }
            self.subtitleLoadInFlight = false
            self.applySubtitleGroup(group, to: item)
        }
    }

    private func applySubtitleGroup(_ group: AVMediaSelectionGroup?, to item: AVPlayerItem) {
        subtitleGroup = group
        let options = group?.options ?? []
        subtitleOptions = options
        subtitleTitles = options.enumerated().map { index, option in
            SubtitleSelection.title(for: option, fallbackIndex: index + 1, language: surface.language)
        }
        if let key = selectedSubtitleKey,
           let group,
           let index = options.firstIndex(where: { SubtitleSelection.key(for: $0) == key }) {
            item.select(options[index], in: group)
            selectedSubtitleIndex = index + 1
        } else if group != nil {
            selectedSubtitleIndex = 0
            selectedSubtitleKey = nil
            if let group, !options.isEmpty {
                item.select(nil, in: group)
            }
        } else {
            // The asset can expose the legible group after the item first
            // reports ready. Keep the remembered choice and retry later.
            selectedSubtitleIndex = 0;
        }
        surface.reloadSubtitleControl()
    }

    /// Live HLS assets sometimes populate the legible group after the item
    /// becomes ready. Retry once per second while the list is empty.
    private func refreshSubtitleOptionsIfNeeded() {
        guard subtitleTitles.isEmpty,
              !subtitleLoadInFlight,
              let item = player.currentItem,
              item.status == .readyToPlay,
              Date().timeIntervalSince(lastSubtitleRefresh) > 1
        else { return }
        lastSubtitleRefresh = Date()
        refreshSubtitleOptions(for: item)
    }

    /// Clears per-item track state when the item is replaced. The remembered
    /// selection key survives so the same track can be restored on the new item.
    private func resetSubtitleOptionsForNewItem() {
        subtitleGroup = nil
        subtitleOptions = []
        subtitleTitles = []
        selectedSubtitleIndex = 0
        surface.reloadSubtitleControl()
    }

    private func clearSubtitleOptions() {
        resetSubtitleOptionsForNewItem()
        selectedSubtitleKey = nil
    }

    /// Toggles native picture in picture for the visible AVPlayerLayer.
    public func togglePictureInPicture() {
        setPictureInPicture(enabled: !isPictureInPictureActive)
    }

    /// Starts or stops picture in picture. Starting is asynchronous: when the
    /// system reports the layer is not ready yet, a KVO observation starts it as
    /// soon as it becomes possible.
    public func setPictureInPicture(enabled: Bool) {
        guard !isShutdown else { return }
        guard player.currentItem != nil else {
            surface.showPictureInPictureFailure("再生中の動画がありません")
            return
        }
        guard AVPictureInPictureController.isPictureInPictureSupported() else {
            surface.showPictureInPictureFailure("この環境ではピクチャ・イン・ピクチャを利用できません")
            return
        }
        wantsPictureInPicture = enabled
        guard enabled else {
            pictureInPictureController?.stopPictureInPicture()
            return
        }
        guard let controller = makePictureInPictureControllerIfNeeded() else {
            wantsPictureInPicture = false
            surface.showPictureInPictureFailure("ピクチャ・イン・ピクチャを開始できません")
            return
        }
        if controller.isPictureInPictureActive { return }
        if controller.isPictureInPicturePossible {
            controller.startPictureInPicture()
        }
    }

    private func makePictureInPictureControllerIfNeeded() -> AVPictureInPictureController? {
        if let controller = pictureInPictureController { return controller }
        guard let layer = surface.playerLayer else { return nil }
        guard let controller = AVPictureInPictureController(playerLayer: layer) else { return nil }
        let delegate = PictureInPictureDelegate()
        delegate.onStart = { [weak self] in
            self?.surface.setPictureInPictureActive(true)
        }
        delegate.onStop = { [weak self] in
            self?.wantsPictureInPicture = false
            self?.surface.setPictureInPictureActive(false)
        }
        delegate.onFailure = { [weak self] message in
            self?.wantsPictureInPicture = false
            self?.surface.setPictureInPictureActive(false)
            self?.surface.showPictureInPictureFailure(message)
        }
        controller.delegate = delegate
        pictureInPicturePossibleObservation = controller.observe(
            \.isPictureInPicturePossible,
            options: [.initial, .new]
        ) { [weak self] observed, _ in
            Task { @MainActor [weak self] in
                guard let self,
                      self.wantsPictureInPicture,
                      observed.isPictureInPicturePossible,
                      !observed.isPictureInPictureActive
                else { return }
                observed.startPictureInPicture()
            }
        }
        pictureInPictureController = controller
        pictureInPictureDelegate = delegate
        return controller
    }

    /// Stops playback, removes the item and releases KVO before the host drops the session.
    public func shutdown() {
        guard !isShutdown else { return }
        isShutdown = true
        shouldStartWhenReady = false
        shouldStartAtLiveEdge = false
        currentItemAllowsLiveEdgeSeek = false
        dvrReviewActive = false
        removeCurrentItemObservations()
        clearSourcePair()
        transportObservation?.invalidate()
        transportObservation = nil
        if let timeObserver {
            player.removeTimeObserver(timeObserver)
            self.timeObserver = nil
        }
        activeRequestID = nil
        pictureInPicturePossibleObservation?.invalidate()
        pictureInPicturePossibleObservation = nil
        pictureInPictureController?.delegate = nil
        pictureInPictureController?.stopPictureInPicture()
        pictureInPictureController = nil
        pictureInPictureDelegate = nil
        wantsPictureInPicture = false
        clearSubtitleOptions()
        player.pause()
        player.replaceCurrentItem(with: nil)
        surface.onPlaybackToggleRequested = nil
        surface.onMuteToggleRequested = nil
        surface.onVolumeChanged = nil
        surface.onSeekRequested = nil
        surface.onQualityCapChanged = nil
        surface.onLiveEdgeRequested = nil
        surface.onLowLatencyChanged = nil
        surface.onSubtitleChanged = nil
        surface.onSubtitleAutoOn = nil
        surface.onPictureInPictureToggleRequested = nil
        surface.subtitleInfoProvider = nil
        surface.pictureInPictureStateProvider = nil
        surface.liveEdgeSeekAvailability = nil
        surface.dvrPositionProvider = nil
        lowLatencyProxy.stop()
        surface.player = nil
        setState(.idle, text: "Stopped.")
        publishSnapshot()
        onStatusChanged = nil
        onPlaybackSnapshotChanged = nil
    }

    public var isMuted: Bool { player.isMuted }

    /// Returns a URL-free snapshot of the current AVPlayer state.
    public func currentSnapshot() -> PlaybackSnapshot {
        makeSnapshot()
    }

    private func handle(itemStatus: AVPlayerItem.Status, errorSummary: String?, requestID: UUID) {
        guard activeRequestID == requestID else { return }

        switch itemStatus {
        case .readyToPlay:
            if let item = player.currentItem, LiveEdgeSeek.isLive(duration: item.duration) {
                currentItemAllowsLiveEdgeSeek = true
            }
            if let item = player.currentItem {
                refreshSubtitleOptions(for: item)
            }
            applyLatencyPolicy()
            if let item = player.currentItem, let pendingSeekTarget {
                self.pendingSeekTarget = nil
                seekToPendingTarget(item: item, target: pendingSeekTarget, requestID: requestID)
            } else if shouldStartWhenReady || shouldStartAtLiveEdge {
                startPlaybackWhenReady(item: player.currentItem, requestID: requestID)
            } else {
                setState(.ready, text: "Ready.")
            }
        case .failed:
            if currentSource == .dvr {
                fallbackToLiveAfterDVRFailure(requestID: requestID)
                return
            }
            if
                currentSource == .live,
                let proxyItem = lowLatencyProxyItem,
                player.currentItem === proxyItem,
                !proxyFallbackPerformed,
                lowLatencyEnabled,
                usesLowLatencyPlaylist,
                let liveURL
            {
                proxyFallbackPerformed = true
                let shouldResume = shouldStartWhenReady
                    || player.timeControlStatus == .playing
                    || player.rate > 0
                stopLowLatencyProxy()
                let fallbackItem = makeLiveItem(url: liveURL)
                liveItem = fallbackItem
                startCurrentItem(
                    fallbackItem,
                    source: .live,
                    startAtLiveEdge: true,
                    seekTarget: nil,
                    resumeAfterSeek: shouldResume
                )
                return
            }
            let text: String
            if let errorSummary {
                text = "Playback failed (\(errorSummary))."
            } else {
                text = "AVPlayer could not load this stream."
            }
            finishCurrentItem(
                requestID: requestID,
                expectedItem: player.currentItem,
                state: .failed,
                text: text
            )
            return
        case .unknown:
            break
        @unknown default:
            finishCurrentItem(
                requestID: requestID,
                expectedItem: player.currentItem,
                state: .failed,
                text: "AVPlayer reported an unknown item state."
            )
            return
        }
        publishSnapshot()
    }

    private func observeTerminalEvents(for item: AVPlayerItem, requestID: UUID) {
        let notificationCenter = NotificationCenter.default
        itemDidEndObserver = notificationCenter.addObserver(
            forName: AVPlayerItem.didPlayToEndTimeNotification,
            object: item,
            queue: .main
        ) { [weak self] notification in
            guard let endedItem = notification.object as? AVPlayerItem else { return }
            Task { @MainActor [weak self] in
                guard let self, !self.isShutdown, self.player.currentItem === endedItem else {
                    return
                }
                if LiveEndRestart.shouldRestart(
                    isVodMode: self.isVodMode,
                    isLiveSource: self.currentSource == .live,
                    lowLatencyEnabled: self.lowLatencyEnabled
                ), let liveURL = self.lowLatencyProxyURL ?? self.liveURL {
                    let item = self.makeLiveItem(url: liveURL)
                    self.liveItem = item
                    self.startCurrentItem(
                        item,
                        source: .live,
                        startAtLiveEdge: true,
                        seekTarget: nil,
                        resumeAfterSeek: true
                    )
                    return
                }
                self.finishCurrentItem(
                    requestID: requestID,
                    expectedItem: endedItem,
                    state: .idle,
                    text: "Playback ended."
                )
            }
        }
        itemFailedObserver = notificationCenter.addObserver(
            forName: AVPlayerItem.failedToPlayToEndTimeNotification,
            object: item,
            queue: .main
        ) { [weak self] notification in
            guard let failedItem = notification.object as? AVPlayerItem else { return }
            Task { @MainActor [weak self] in
                // The failure notification's NSError can contain the source URL. Keep it out
                // of status text and logs; KVO reports only the error domain and code.
                self?.finishCurrentItem(
                    requestID: requestID,
                    expectedItem: failedItem,
                    state: .failed,
                    text: "Playback failed."
                )
            }
        }
    }

    private func observeSeekableRanges(for item: AVPlayerItem, requestID: UUID) {
        seekableRangesObservation = item.observe(\.seekableTimeRanges, options: [.initial, .new]) { [weak self] _, _ in
            Task { @MainActor [weak self] in
                guard let self, self.activeRequestID == requestID else { return }
                self.publishSnapshot()
            }
        }
    }

    private func removeCurrentItemObservations() {
        itemStatusObservation?.invalidate()
        itemStatusObservation = nil
        seekableRangesObservation?.invalidate()
        seekableRangesObservation = nil

        let notificationCenter = NotificationCenter.default
        if let itemDidEndObserver {
            notificationCenter.removeObserver(itemDidEndObserver)
            self.itemDidEndObserver = nil
        }
        if let itemFailedObserver {
            notificationCenter.removeObserver(itemFailedObserver)
            self.itemFailedObserver = nil
        }
    }

    private func finishCurrentItem(
        requestID: UUID,
        expectedItem: AVPlayerItem?,
        state: KickPlayerState,
        text: String
    ) {
        guard activeRequestID == requestID else { return }
        if let expectedItem, player.currentItem !== expectedItem { return }

        shouldStartWhenReady = false
        shouldStartAtLiveEdge = false
        currentItemAllowsLiveEdgeSeek = false
        activeRequestID = nil
        removeCurrentItemObservations()
        clearSourcePair()
        player.pause()
        player.replaceCurrentItem(with: nil)
        setState(state, text: text)
        publishSnapshot()
    }

    private func handle(transportStatus: AVPlayer.TimeControlStatus) {
        guard player.currentItem != nil else {
            publishSnapshot()
            return
        }

        switch transportStatus {
        case .playing:
            setState(.playing, text: "Playing.")
        case .paused:
            if stateCode == KickPlayerState.playing.rawValue || stateCode == KickPlayerState.playbackRequested.rawValue {
                setState(.paused, text: "Paused.")
            }
        case .waitingToPlayAtSpecifiedRate:
            if shouldStartWhenReady {
                setState(.playbackRequested, text: "Waiting for media.")
            }
        @unknown default:
            break
        }
        publishSnapshot()
    }

    /// Starts a ready live item at the live edge when low-latency mode is on.
    /// The seek completion revalidates the item, request and pause state before playing.
    private func startPlaybackWhenReady(item: AVPlayerItem?, requestID: UUID) {
        guard let item, item.status == .readyToPlay else { return }
        guard let target = LiveEdgeSeek.startTarget(
            duration: item.duration,
            timeRanges: item.seekableTimeRanges,
            lowLatencyEnabled: lowLatencyEnabled,
            startAtLiveEdge: shouldStartAtLiveEdge
        ) else {
            playIfStillReady(item: item, requestID: requestID)
            return
        }

        if target.isNumeric {
            player.seek(to: target, toleranceBefore: .zero, toleranceAfter: .zero) { [weak self] _ in
                Task { @MainActor [weak self] in
                    self?.playIfStillReady(item: item, requestID: requestID)
                }
            }
        } else {
            player.seek(to: .positiveInfinity) { [weak self] _ in
                Task { @MainActor [weak self] in
                    self?.playIfStillReady(item: item, requestID: requestID)
                }
            }
        }
    }

    private func playIfStillReady(item: AVPlayerItem, requestID: UUID) {
        guard
            !isShutdown,
            activeRequestID == requestID,
            player.currentItem === item,
            item.status == .readyToPlay
        else { return }

        let shouldPlay = shouldStartWhenReady
        shouldStartAtLiveEdge = false
        if shouldPlay {
            player.play()
            setState(.playbackRequested, text: "Playback requested.")
        } else {
            setState(.ready, text: "Ready.")
        }
        publishSnapshot()
    }

    private func applyLatencyPolicy(to item: AVPlayerItem? = nil) {
        guard let target = item ?? player.currentItem else { return }
        let isLive = currentItemAllowsLiveEdgeSeek || LiveEdgeSeek.isLive(duration: target.duration)
        let policy = PlaybackLatencyPolicy.resolve(isLive: isLive, lowLatencyEnabled: lowLatencyEnabled)
        player.automaticallyWaitsToMinimizeStalling = policy.automaticallyWaitsToMinimizeStalling
        policy.apply(to: target)
    }

    private func seekToLiveEdge() {
        guard !isShutdown else { return }
        dvrReviewActive = false
        let wasPlaying = player.timeControlStatus == .playing || player.rate > 0
        if currentSource == .dvr, liveItem != nil {
            switchToLive(seekTarget: nil, resume: wasPlaying)
            return
        }

        guard
            let item = player.currentItem,
            item.status == .readyToPlay,
            let target = LiveEdgeSeek.target(
                duration: item.duration,
                timeRanges: item.seekableTimeRanges,
                allowNonLiveDuration: currentItemAllowsLiveEdgeSeek
            )
        else { return }

        let requestID = activeRequestID
        if target.isNumeric {
            player.seek(to: target, toleranceBefore: .zero, toleranceAfter: .zero) { [weak self] _ in
                Task { @MainActor [weak self] in
                    self?.handleSeekCompletion(item: item, requestID: requestID, wasPlaying: wasPlaying)
                }
            }
        } else {
            player.seek(to: .positiveInfinity) { [weak self] _ in
                Task { @MainActor [weak self] in
                    self?.handleSeekCompletion(item: item, requestID: requestID, wasPlaying: wasPlaying)
                }
            }
        }
    }

    private func seekWithinSeekableRange(to proposedTime: CMTime) {
        guard
            !isShutdown,
            let item = player.currentItem,
            item.status == .readyToPlay
        else { return }

        let target = SeekableTimeline(timeRanges: item.seekableTimeRanges)?.clampedTime(proposedTime)
            ?? proposedTime
        let wasPlaying = player.timeControlStatus == .playing || player.rate > 0
        let requestID = activeRequestID
        player.seek(to: target, toleranceBefore: .zero, toleranceAfter: .zero) { [weak self] _ in
            Task { @MainActor [weak self] in
                self?.handleSeekCompletion(item: item, requestID: requestID, wasPlaying: wasPlaying)
            }
        }
    }

    private func handleSeekCompletion(item: AVPlayerItem, requestID: UUID?, wasPlaying: Bool) {
        publishSnapshot()
        guard SeekResumePolicy.shouldResume(
            wasPlaying: wasPlaying,
            isShutdown: isShutdown,
            requestMatches: activeRequestID == requestID,
            currentItemMatches: player.currentItem === item,
            itemIsReady: item.status == .readyToPlay,
            playbackRequested: shouldStartWhenReady
        ) else { return }

        player.play()
        setState(.playbackRequested, text: "Playback requested.")
        publishSnapshot()
    }

    private func publishSnapshot(time: CMTime? = nil) {
        applyCatchUpRate()
        #if DEBUG
        if Date().timeIntervalSince(lastLatencyLog) >= 1 {
            lastLatencyLog = Date()
            if let item = player.currentItem,
               let end = SeekableTimeline(timeRanges: item.seekableTimeRanges)?.ranges.last?.end,
               end.isValid, end.isNumeric
            {
                let distance = CMTimeGetSeconds(end) - CMTimeGetSeconds(player.currentTime())
                if distance.isFinite {
                    print("LIVE_EDGE_DISTANCE \(String(format: "%.2f", distance))")
                    fflush(stdout)
                }
            }
        }
        #endif
        refreshSubtitleOptionsIfNeeded()
        surface.refreshControlState()
        onPlaybackSnapshotChanged?(makeSnapshot(time: time))
    }

    private func applyCatchUpRate() {
        guard let item = player.currentItem else { return }
        let seekableEnd = SeekableTimeline(timeRanges: item.seekableTimeRanges)?.ranges.last?.end
        let isLiveSource = currentSource == .live
            && (dvrItem != nil || usesLowLatencyPlaylist || LiveEdgeSeek.isLive(duration: item.duration))
        let isPlaying = player.timeControlStatus == .playing
        let currentTime = player.currentTime()
        if dvrReviewActive {
            let behind = seekableEnd.map { CMTimeGetSeconds($0) - CMTimeGetSeconds(currentTime) }
            let behindSeconds = behind?.isFinite == true ? behind! : .infinity
            if DvrReview.shouldClear(reviewActive: true, behindSeconds: behindSeconds) {
                dvrReviewActive = false
            } else if DvrReview.shouldSuspendCatchUp(
                reviewActive: true,
                behindSeconds: behindSeconds
            ) {
                return
            }
        }
        if
            LowLatencyCatchUp.shouldSeekToLive(
                lowLatencyEnabled: lowLatencyEnabled,
                isLiveSource: isLiveSource,
                isPlaying: isPlaying,
                currentTime: currentTime,
                seekableEnd: seekableEnd ?? .invalid
            ),
            Date().timeIntervalSince(lastLiveCatchUpSeek) > 5
        {
            lastLiveCatchUpSeek = Date()
            seekToLiveEdge()
            return
        }
        let decision = LowLatencyCatchUp.rate(
            lowLatencyEnabled: lowLatencyEnabled,
            isLiveSource: isLiveSource,
            isPlaying: isPlaying,
            currentTime: currentTime,
            seekableEnd: seekableEnd ?? .invalid
        )

        if let decision {
            if abs(player.rate - decision) > 0.001 {
                player.rate = decision
            }
        } else if
            LowLatencyCatchUp.shouldResetRate(
                lowLatencyEnabled: lowLatencyEnabled,
                isLiveSource: isLiveSource,
                isPlaying: isPlaying,
                currentTime: currentTime,
                seekableEnd: seekableEnd ?? .invalid
            ),
            abs(player.rate - LowLatencyCatchUp.catchUpRate) < 0.001
        {
            player.rate = LowLatencyCatchUp.normalRate
        }
    }

    private func makeSnapshot(time: CMTime? = nil) -> PlaybackSnapshot {
        let playerLayer = surface.layer as? AVPlayerLayer
        let selectedTime = time ?? player.currentTime()
        let seconds = selectedTime.isValid && selectedTime.isNumeric && selectedTime.seconds.isFinite
            ? selectedTime.seconds
            : nil
        let snapshot = PlaybackSnapshot(
            state: stateName,
            itemStatus: itemStatusName(player.currentItem?.status),
            transportStatus: transportStatusName(player.timeControlStatus),
            currentTimeSeconds: seconds,
            rate: player.rate,
            volume: player.volume,
            isMuted: player.isMuted,
            hasCurrentItem: player.currentItem != nil,
            videoReadyForDisplay: playerLayer?.isReadyForDisplay ?? false
        )
        return snapshot
    }

    private func setState(_ state: KickPlayerState, text: String) {
        stateCode = state.rawValue
        statusText = text
        onStatusChanged?(text)
    }

    private var stateName: String {
        switch KickPlayerState(rawValue: stateCode) ?? .idle {
        case .idle: "idle"
        case .loading: "loading"
        case .ready: "ready"
        case .playbackRequested: "playbackRequested"
        case .paused: "paused"
        case .failed: "failed"
        case .playing: "playing"
        }
    }

    private func itemStatusName(_ status: AVPlayerItem.Status?) -> String {
        switch status {
        case .unknown, nil: "unknown"
        case .readyToPlay: "readyToPlay"
        case .failed: "failed"
        @unknown default: "unknownFutureValue"
        }
    }

    private func transportStatusName(_ status: AVPlayer.TimeControlStatus) -> String {
        switch status {
        case .paused: "paused"
        case .waitingToPlayAtSpecifiedRate: "waiting"
        case .playing: "playing"
        @unknown default: "unknownFutureValue"
        }
    }
}

public enum KickPlayerState: Int32 {
    case idle = 0
    case loading = 1
    case ready = 2
    case playbackRequested = 3
    case paused = 4
    case failed = 5
    case playing = 6
}
