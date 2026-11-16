import AVFoundation
import XCTest
@testable import KickPlayerCore

final class SeekableTimelineTests: XCTestCase {
    func testMapsPositionsAcrossMultipleSeekableRanges() throws {
        let timeline = try XCTUnwrap(
            SeekableTimeline(timeRanges: [makeRange(start: 10, duration: 10), makeRange(start: 30, duration: 10)])
        )

        XCTAssertEqual(timeline.durationSeconds, 20, accuracy: 0.001)
        XCTAssertEqual(try XCTUnwrap(timeline.fraction(for: time(15))), 0.25, accuracy: 0.001)
        XCTAssertEqual(try XCTUnwrap(timeline.fraction(for: time(35))), 0.75, accuracy: 0.001)
        XCTAssertEqual(try XCTUnwrap(timeline.time(at: 0.75)).seconds, 35, accuracy: 0.001)
    }

    func testMergesOverlappingRangesWithoutCountingTimeTwice() throws {
        let timeline = try XCTUnwrap(
            SeekableTimeline(timeRanges: [
                makeRange(start: 18, duration: 7),
                makeRange(start: 10, duration: 10),
                makeRange(start: 30, duration: 10),
            ])
        )

        XCTAssertEqual(timeline.durationSeconds, 25, accuracy: 0.001)
        XCTAssertEqual(try XCTUnwrap(timeline.fraction(for: time(22.5))), 0.5, accuracy: 0.001)
        XCTAssertEqual(try XCTUnwrap(timeline.time(at: 0.8)).seconds, 35, accuracy: 0.001)
    }

    func testGapPositionMapsToNearestValidTimeAndNeverSeeksIntoGap() throws {
        let timeline = try XCTUnwrap(
            SeekableTimeline(timeRanges: [makeRange(start: 10, duration: 10), makeRange(start: 30, duration: 10)])
        )

        XCTAssertEqual(try XCTUnwrap(timeline.fraction(for: time(25))), 0.5, accuracy: 0.001)

        let boundaryTarget = try XCTUnwrap(timeline.time(at: 0.5))
        XCTAssertTrue(isSeekable(boundaryTarget, ranges: [(10, 20), (30, 40)]))

        let targetPastGap = try XCTUnwrap(timeline.time(at: 0.625))
        XCTAssertEqual(targetPastGap.seconds, 32.5, accuracy: 0.001)
        XCTAssertTrue(isSeekable(targetPastGap, ranges: [(10, 20), (30, 40)]))

        let clampedGapTime = try XCTUnwrap(timeline.clampedTime(time(25)))
        XCTAssertEqual(clampedGapTime.seconds, 30, accuracy: 0.001)
        XCTAssertTrue(isSeekable(clampedGapTime, ranges: [(10, 20), (30, 40)]))
    }

    func testClampsTargetsToTheAvailableRange() throws {
        let timeline = try XCTUnwrap(
            SeekableTimeline(timeRanges: [makeRange(start: 10, duration: 10), makeRange(start: 30, duration: 10)])
        )

        XCTAssertEqual(try XCTUnwrap(timeline.clampedTime(time(5))).seconds, 10, accuracy: 0.001)
        XCTAssertEqual(try XCTUnwrap(timeline.clampedTime(time(50))).seconds, 40 - (1.0 / 600.0), accuracy: 0.001)
        XCTAssertEqual(try XCTUnwrap(timeline.time(at: -1)).seconds, 10, accuracy: 0.001)
        XCTAssertEqual(try XCTUnwrap(timeline.time(at: 2)).seconds, 40 - (1.0 / 600.0), accuracy: 0.001)
    }

    func testRejectsEmptyAndInvalidRanges() {
        XCTAssertNil(SeekableTimeline(timeRanges: []))

        let invalid = NSValue(timeRange: CMTimeRange(start: .invalid, duration: .invalid))
        let indefinite = NSValue(
            timeRange: CMTimeRange(start: .indefinite, duration: CMTime(seconds: 5, preferredTimescale: 600))
        )
        XCTAssertNil(SeekableTimeline(timeRanges: [invalid, indefinite]))
    }

    private func makeRange(start: Double, duration: Double) -> NSValue {
        NSValue(
            timeRange: CMTimeRange(
                start: time(start),
                duration: CMTime(seconds: duration, preferredTimescale: 600)
            )
        )
    }

    func testLiveEndRestartOnlyAppliesToLiveItems() {
        XCTAssertTrue(
            LiveEndRestart.shouldRestart(
                isVodMode: false,
                isLiveSource: true,
                lowLatencyEnabled: true
            )
        )
        XCTAssertFalse(
            LiveEndRestart.shouldRestart(
                isVodMode: true,
                isLiveSource: true,
                lowLatencyEnabled: true
            )
        )
        XCTAssertFalse(
            LiveEndRestart.shouldRestart(
                isVodMode: false,
                isLiveSource: false,
                lowLatencyEnabled: true
            )
        )
        XCTAssertFalse(
            LiveEndRestart.shouldRestart(
                isVodMode: false,
                isLiveSource: true,
                lowLatencyEnabled: false
            )
        )
    }

    func testDvrReviewSuspendsCatchUpUntilLiveEdge() {
        XCTAssertFalse(DvrReview.shouldSuspendCatchUp(reviewActive: false, behindSeconds: 30))
        XCTAssertTrue(DvrReview.shouldSuspendCatchUp(reviewActive: true, behindSeconds: 30))
        XCTAssertFalse(DvrReview.shouldSuspendCatchUp(reviewActive: true, behindSeconds: 0.5))
        XCTAssertFalse(DvrReview.shouldClear(reviewActive: false, behindSeconds: 0.5))
        XCTAssertTrue(DvrReview.shouldClear(reviewActive: true, behindSeconds: 0.5))
        XCTAssertFalse(DvrReview.shouldClear(reviewActive: true, behindSeconds: 30))
    }

    private func time(_ seconds: Double) -> CMTime {
        CMTime(seconds: seconds, preferredTimescale: 600)
    }

    private func isSeekable(_ time: CMTime, ranges: [(start: Double, end: Double)]) -> Bool {
        ranges.contains { time.seconds >= $0.start && time.seconds < $0.end }
    }
}
