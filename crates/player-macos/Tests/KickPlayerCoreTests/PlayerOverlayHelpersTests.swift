import AVFoundation
import CoreGraphics
import XCTest
@testable import KickPlayerCore

final class PlayerQualityCapTests: XCTestCase {
    func testMenuTitlesAndOrderMatchTheOverlayMenu() {
        XCTAssertEqual(
            PlayerQualityCap.allCases.map { $0.title(for: .japanese) },
            ["自動", "1080p", "720p", "480p"]
        )
    }

    func testQualityCapsUseExpectedMaximumResolutions() {
        XCTAssertEqual(PlayerQualityCap.automatic.maximumResolution, .zero)
        XCTAssertEqual(PlayerQualityCap.p1080.maximumResolution, CGSize(width: 1920, height: 1080))
        XCTAssertEqual(PlayerQualityCap.p720.maximumResolution, CGSize(width: 1280, height: 720))
        XCTAssertEqual(PlayerQualityCap.p480.maximumResolution, CGSize(width: 854, height: 480))
    }

    func testApplyingQualityCapUpdatesAndResetsTheItem() {
        let item = AVPlayerItem(asset: AVMutableComposition())

        PlayerQualityCap.p720.apply(to: item)
        XCTAssertEqual(item.preferredMaximumResolution, CGSize(width: 1280, height: 720))

        PlayerQualityCap.automatic.apply(to: item)
        XCTAssertEqual(item.preferredMaximumResolution, .zero)
    }

    func testMenuIndexMatchesCaseOrder() {
        for (index, cap) in PlayerQualityCap.allCases.enumerated() {
            XCTAssertEqual(cap.menuIndex, index)
        }
    }
}

final class LiveEdgeSeekTests: XCTestCase {
    func testTargetsTheEndOfTheLastSeekableRange() throws {
        let target = try XCTUnwrap(
            LiveEdgeSeek.target(
                duration: .indefinite,
                timeRanges: [
                    makeRange(start: 10, duration: 10),
                    makeRange(start: 30, duration: 10),
                ]
            )
        )

        XCTAssertEqual(target.seconds, 40 - (1.0 / 600.0), accuracy: 0.001)
    }

    func testFallsBackToPositiveInfinityWithoutSeekableRanges() throws {
        let target = try XCTUnwrap(LiveEdgeSeek.target(duration: .indefinite, timeRanges: []))
        XCTAssertEqual(CMTimeCompare(target, .positiveInfinity), 0)
    }

    func testRejectsFiniteVODDuration() {
        XCTAssertNil(
            LiveEdgeSeek.target(
                duration: CMTime(seconds: 60, preferredTimescale: 600),
                timeRanges: [makeRange(start: 0, duration: 60)]
            )
        )
    }

    func testRejectsInvalidDuration() {
        XCTAssertNil(LiveEdgeSeek.target(duration: .invalid, timeRanges: []))
    }

    func testLiveEdgeTimeStaysInsideTheFinalRange() throws {
        let timeline = try XCTUnwrap(
            SeekableTimeline(timeRanges: [
                makeRange(start: 10, duration: 10),
                makeRange(start: 30, duration: 10),
            ])
        )
        let edge = try XCTUnwrap(timeline.liveEdgeTime)

        XCTAssertEqual(edge.seconds, 40 - (1.0 / 600.0), accuracy: 0.001)
    }

    func testLowLatencyStartTargetsTheEndOfTheLastSeekableRange() throws {
        let target = try XCTUnwrap(
            LiveEdgeSeek.startTarget(
                duration: .indefinite,
                timeRanges: [
                    makeRange(start: 10, duration: 10),
                    makeRange(start: 30, duration: 10),
                ],
                lowLatencyEnabled: true
            )
        )

        XCTAssertEqual(target.seconds, 40 - (1.0 / 600.0), accuracy: 0.001)
    }

    func testLowLatencyStartFallsBackToPositiveInfinityWithoutRanges() throws {
        let target = try XCTUnwrap(
            LiveEdgeSeek.startTarget(
                duration: .indefinite,
                timeRanges: [],
                lowLatencyEnabled: true
            )
        )

        XCTAssertEqual(CMTimeCompare(target, .positiveInfinity), 0)
    }

    func testLowLatencyStartIsSkippedWhenDisabledVODOrInvalid() {
        XCTAssertNil(
            LiveEdgeSeek.startTarget(
                duration: .indefinite,
                timeRanges: [makeRange(start: 0, duration: 10)],
                lowLatencyEnabled: false
            )
        )
        XCTAssertNil(
            LiveEdgeSeek.startTarget(
                duration: CMTime(seconds: 60, preferredTimescale: 600),
                timeRanges: [makeRange(start: 0, duration: 60)],
                lowLatencyEnabled: true
            )
        )
        XCTAssertNil(
            LiveEdgeSeek.startTarget(
                duration: .invalid,
                timeRanges: [],
                lowLatencyEnabled: true
            )
        )
    }

    func testStartAtLiveEdgeSeeksWithFiniteDVREventDurationEvenWhenLowLatencyIsOff() throws {
        let target = try XCTUnwrap(
            LiveEdgeSeek.startTarget(
                duration: CMTime(seconds: 3600, preferredTimescale: 600),
                timeRanges: [makeRange(start: 0, duration: 3600)],
                lowLatencyEnabled: false,
                startAtLiveEdge: true
            )
        )

        XCTAssertEqual(target.seconds, 3600 - (1.0 / 600.0), accuracy: 0.001)
    }

    func testExplicitDVRLiveEdgeTargetWorksWithFiniteDuration() throws {
        let target = try XCTUnwrap(
            LiveEdgeSeek.target(
                duration: CMTime(seconds: 3600, preferredTimescale: 600),
                timeRanges: [makeRange(start: 0, duration: 3600)],
                allowNonLiveDuration: true
            )
        )

        XCTAssertEqual(target.seconds, 3600 - (1.0 / 600.0), accuracy: 0.001)
    }

    func testLiveEdgeTargetForLongDVRSessionUsesSessionEnd() throws {
        let sessionSeconds = 25.0 * 3600 + 40 * 60
        let target = try XCTUnwrap(
            LiveEdgeSeek.target(
                duration: CMTime(seconds: sessionSeconds, preferredTimescale: 600),
                timeRanges: [makeRange(start: 0, duration: sessionSeconds)],
                allowNonLiveDuration: true
            )
        )

        XCTAssertEqual(target.seconds, sessionSeconds - (1.0 / 600.0), accuracy: 0.001)
    }

    func testStartAtLiveEdgeIsSkippedWhenNotRequestedAndLowLatencyIsOff() {
        XCTAssertNil(
            LiveEdgeSeek.startTarget(
                duration: .indefinite,
                timeRanges: [makeRange(start: 0, duration: 3600)],
                lowLatencyEnabled: false,
                startAtLiveEdge: false
            )
        )
    }

    func testLowLatencyStartStillSeeksWhenStartAtLiveEdgeIsFalse() throws {
        let target = try XCTUnwrap(
            LiveEdgeSeek.startTarget(
                duration: .indefinite,
                timeRanges: [makeRange(start: 0, duration: 3600)],
                lowLatencyEnabled: true,
                startAtLiveEdge: false
            )
        )

        XCTAssertEqual(target.seconds, 3600 - (1.0 / 600.0), accuracy: 0.001)
    }

    func testStartAtLiveEdgeFallsBackToPositiveInfinityWithoutRanges() throws {
        let target = try XCTUnwrap(
            LiveEdgeSeek.startTarget(
                duration: CMTime(seconds: 3600, preferredTimescale: 600),
                timeRanges: [],
                lowLatencyEnabled: false,
                startAtLiveEdge: true
            )
        )

        XCTAssertEqual(CMTimeCompare(target, .positiveInfinity), 0)
    }

    func testLongDVRRangeUsesFullSessionDuration() throws {
        let timeline = try XCTUnwrap(
            SeekableTimeline(timeRanges: [makeRange(start: 0, duration: 3600)])
        )

        XCTAssertEqual(timeline.durationSeconds, 3600, accuracy: 0.001)
        XCTAssertEqual(try XCTUnwrap(timeline.time(at: 0.5)).seconds, 1800, accuracy: 0.001)
        XCTAssertEqual(
            try XCTUnwrap(timeline.fraction(for: CMTime(seconds: 1800, preferredTimescale: 600))),
            0.5,
            accuracy: 0.001
        )
    }

    private func makeRange(start: Double, duration: Double) -> NSValue {
        NSValue(
            timeRange: CMTimeRange(
                start: CMTime(seconds: start, preferredTimescale: 600),
                duration: CMTime(seconds: duration, preferredTimescale: 600)
            )
        )
    }
}

final class PlaybackLatencyPolicyTests: XCTestCase {
    func testLiveLowLatencyUsesSmallBufferAndSkipsStallWaiting() {
        let policy = PlaybackLatencyPolicy.resolve(isLive: true, lowLatencyEnabled: true)

        XCTAssertFalse(policy.automaticallyWaitsToMinimizeStalling)
        XCTAssertEqual(policy.preferredForwardBufferDuration, 1.0, accuracy: 0.001)
        XCTAssertFalse(policy.canUseNetworkResourcesForLiveStreamingWhilePaused)
    }

    func testLiveWithoutLowLatencyRestoresAutomaticBuffering() {
        let policy = PlaybackLatencyPolicy.resolve(isLive: true, lowLatencyEnabled: false)

        XCTAssertTrue(policy.automaticallyWaitsToMinimizeStalling)
        XCTAssertEqual(policy.preferredForwardBufferDuration, 0, accuracy: 0.001)
        XCTAssertFalse(policy.canUseNetworkResourcesForLiveStreamingWhilePaused)
    }

    func testVODKeepsReasonableDefaultsEvenWhenLowLatencyIsEnabled() {
        let policy = PlaybackLatencyPolicy.resolve(isLive: false, lowLatencyEnabled: true)

        XCTAssertTrue(policy.automaticallyWaitsToMinimizeStalling)
        XCTAssertEqual(policy.preferredForwardBufferDuration, 0, accuracy: 0.001)
        XCTAssertFalse(policy.canUseNetworkResourcesForLiveStreamingWhilePaused)
    }

    func testPolicyAppliesBufferSettingsToTheItem() {
        let item = AVPlayerItem(asset: AVMutableComposition())
        PlaybackLatencyPolicy.resolve(isLive: true, lowLatencyEnabled: true).apply(to: item)

        XCTAssertEqual(item.preferredForwardBufferDuration, 1.0, accuracy: 0.001)
        XCTAssertFalse(item.canUseNetworkResourcesForLiveStreamingWhilePaused)
    }

    func testLiveDetectionUsesIndefiniteDuration() {
        XCTAssertTrue(LiveEdgeSeek.isLive(duration: .indefinite))
        XCTAssertFalse(LiveEdgeSeek.isLive(duration: CMTime(seconds: 60, preferredTimescale: 600)))
        XCTAssertFalse(LiveEdgeSeek.isLive(duration: .invalid))
    }
}

final class LiveEdgeAvailabilityTests: XCTestCase {
    func testDVRWithFiniteDurationIsEnabledWhenExplicitlyAllowed() {
        XCTAssertTrue(
            LiveEdgeAvailability.canSeekLiveEdge(
                itemIsReady: true,
                allowsLiveEdgeSeek: true,
                duration: CMTime(seconds: 25 * 3600 + 40 * 60, preferredTimescale: 600)
            )
        )
    }

    func testFiniteVODIsDisabledWithoutExplicitDVRFlag() {
        XCTAssertFalse(
            LiveEdgeAvailability.canSeekLiveEdge(
                itemIsReady: true,
                allowsLiveEdgeSeek: false,
                duration: CMTime(seconds: 60, preferredTimescale: 600)
            )
        )
    }

    func testLiveDurationIsEnabledWithoutExplicitDVRFlag() {
        XCTAssertTrue(
            LiveEdgeAvailability.canSeekLiveEdge(
                itemIsReady: true,
                allowsLiveEdgeSeek: false,
                duration: .indefinite
            )
        )
    }

    func testNotReadyIsDisabledEvenWhenExplicitlyAllowed() {
        XCTAssertFalse(
            LiveEdgeAvailability.canSeekLiveEdge(
                itemIsReady: false,
                allowsLiveEdgeSeek: true,
                duration: .indefinite
            )
        )
    }
}

final class SeekResumePolicyTests: XCTestCase {
    func testResumesOnlyWhenPlaybackWasActiveAndItemIsStillValid() {
        XCTAssertTrue(
            SeekResumePolicy.shouldResume(
                wasPlaying: true,
                isShutdown: false,
                requestMatches: true,
                currentItemMatches: true,
                itemIsReady: true,
                playbackRequested: true
            )
        )
    }

    func testManualPauseBeforeSeekIsPreserved() {
        XCTAssertFalse(
            SeekResumePolicy.shouldResume(
                wasPlaying: false,
                isShutdown: false,
                requestMatches: true,
                currentItemMatches: true,
                itemIsReady: true,
                playbackRequested: true
            )
        )
    }

    func testPauseDuringSeekIsPreserved() {
        XCTAssertFalse(
            SeekResumePolicy.shouldResume(
                wasPlaying: true,
                isShutdown: false,
                requestMatches: true,
                currentItemMatches: true,
                itemIsReady: true,
                playbackRequested: false
            )
        )
    }

    func testShutdownOrChangedItemPreventsResume() {
        XCTAssertFalse(
            SeekResumePolicy.shouldResume(
                wasPlaying: true,
                isShutdown: true,
                requestMatches: true,
                currentItemMatches: true,
                itemIsReady: true,
                playbackRequested: true
            )
        )
        XCTAssertFalse(
            SeekResumePolicy.shouldResume(
                wasPlaying: true,
                isShutdown: false,
                requestMatches: false,
                currentItemMatches: true,
                itemIsReady: true,
                playbackRequested: true
            )
        )
        XCTAssertFalse(
            SeekResumePolicy.shouldResume(
                wasPlaying: true,
                isShutdown: false,
                requestMatches: true,
                currentItemMatches: false,
                itemIsReady: true,
                playbackRequested: true
            )
        )
    }

    func testNotReadyItemPreventsResume() {
        XCTAssertFalse(
            SeekResumePolicy.shouldResume(
                wasPlaying: true,
                isShutdown: false,
                requestMatches: true,
                currentItemMatches: true,
                itemIsReady: false,
                playbackRequested: true
            )
        )
    }
}

final class PlayerOverlayLayoutTests: XCTestCase {
    func testDVRPositionLabelIsVisibleAtNormalVideoWidths() {
        XCTAssertTrue(
            PlayerOverlayLayout.shouldShowDVRPositionLabel(width: 735, hasDVRRange: true)
        )
    }

    func testDVRPositionLabelIsVisibleAtTheMinimumWidth() {
        XCTAssertTrue(
            PlayerOverlayLayout.shouldShowDVRPositionLabel(
                width: PlayerOverlayLayout.dvrPositionLabelMinimumWidth,
                hasDVRRange: true
            )
        )
    }

    func testDVRPositionLabelIsHiddenBelowTheMinimumWidth() {
        XCTAssertFalse(
            PlayerOverlayLayout.shouldShowDVRPositionLabel(
                width: PlayerOverlayLayout.dvrPositionLabelMinimumWidth - 1,
                hasDVRRange: true
            )
        )
    }

    func testDVRPositionLabelIsHiddenWithoutADVRRange() {
        XCTAssertFalse(
            PlayerOverlayLayout.shouldShowDVRPositionLabel(width: 735, hasDVRRange: false)
        )
    }
}

final class PlaybackSourceSelectionTests: XCTestCase {
    func testLiveStartUsesLiveWhenTargetIsInsideLiveWindow() throws {
        let live = try XCTUnwrap(
            SeekableTimeline(timeRanges: [makeRange(start: 90, duration: 10)])
        )
        let dvr = try XCTUnwrap(
            SeekableTimeline(timeRanges: [makeRange(start: 0, duration: 100)])
        )

        XCTAssertEqual(
            PlaybackSourceSelection.sourceForSeek(
                target: time(95),
                liveTimeline: live,
                dvrTimeline: dvr
            ),
            .live
        )
    }

    func testBackwardSeekOutsideLiveWindowUsesDVR() throws {
        let live = try XCTUnwrap(
            SeekableTimeline(timeRanges: [makeRange(start: 90, duration: 10)])
        )
        let dvr = try XCTUnwrap(
            SeekableTimeline(timeRanges: [makeRange(start: 0, duration: 100)])
        )

        XCTAssertEqual(
            PlaybackSourceSelection.sourceForSeek(
                target: time(50),
                liveTimeline: live,
                dvrTimeline: dvr
            ),
            .dvr
        )
    }

    func testReturnToLiveUsesLiveWhenTargetIsInsideLiveWindow() throws {
        let live = try XCTUnwrap(
            SeekableTimeline(timeRanges: [makeRange(start: 90, duration: 10)])
        )
        let dvr = try XCTUnwrap(
            SeekableTimeline(timeRanges: [makeRange(start: 0, duration: 100)])
        )

        XCTAssertEqual(
            PlaybackSourceSelection.sourceForSeek(
                target: time(99),
                liveTimeline: live,
                dvrTimeline: dvr
            ),
            .live
        )
    }

    func testDVRAvailabilityFallbackUsesLiveWhenDVRTimelineIsNil() {
        XCTAssertEqual(
            PlaybackSourceSelection.sourceForSeek(
                target: time(50),
                liveTimeline: nil,
                dvrTimeline: nil
            ),
            .live
        )
    }

    func testDVRPositionMapsLivePlayheadByEndOffset() throws {
        let live = try XCTUnwrap(
            SeekableTimeline(timeRanges: [makeRange(start: 90, duration: 10)])
        )
        let dvr = try XCTUnwrap(
            SeekableTimeline(timeRanges: [makeRange(start: 0, duration: 200)])
        )

        let mapped = PlaybackSourceSelection.dvrPosition(
            currentSource: .live,
            currentTime: time(95),
            liveTimeline: live,
            dvrTimeline: dvr
        )

        XCTAssertEqual(mapped.seconds, 195, accuracy: 0.001)
    }

    func testDVRPositionUsesCurrentTimeWhenDVRSource() throws {
        let dvr = try XCTUnwrap(
            SeekableTimeline(timeRanges: [makeRange(start: 0, duration: 200)])
        )

        let mapped = PlaybackSourceSelection.dvrPosition(
            currentSource: .dvr,
            currentTime: time(42),
            liveTimeline: nil,
            dvrTimeline: dvr
        )

        XCTAssertEqual(mapped.seconds, 42, accuracy: 0.001)
    }

    private func makeRange(start: Double, duration: Double) -> NSValue {
        NSValue(
            timeRange: CMTimeRange(
                start: time(start),
                duration: CMTime(seconds: duration, preferredTimescale: 600)
            )
        )
    }

    private func time(_ seconds: Double) -> CMTime {
        CMTime(seconds: seconds, preferredTimescale: 600)
    }
}
