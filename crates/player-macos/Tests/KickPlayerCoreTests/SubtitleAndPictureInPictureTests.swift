import AVFoundation
import XCTest
@testable import KickPlayerCore

final class SubtitleSelectionTests: XCTestCase {
    func testOffIsAlwaysValid() {
        XCTAssertEqual(SubtitleSelection.normalizedIndex(0, optionCount: 0), 0)
        XCTAssertEqual(SubtitleSelection.normalizedIndex(0, optionCount: 3), 0)
    }

    func testTrackIndicesAreOneBasedAndBounded() {
        XCTAssertEqual(SubtitleSelection.normalizedIndex(1, optionCount: 3), 1)
        XCTAssertEqual(SubtitleSelection.normalizedIndex(3, optionCount: 3), 3)
        XCTAssertNil(SubtitleSelection.normalizedIndex(4, optionCount: 3))
        XCTAssertNil(SubtitleSelection.normalizedIndex(1, optionCount: 0))
        XCTAssertNil(SubtitleSelection.normalizedIndex(-1, optionCount: 3))
    }
}

@MainActor
final class SubtitleAndPictureInPictureSessionTests: XCTestCase {
    func testSessionStartsWithNoTracksAndSubtitlesOff() {
        let session = PlayerSession()
        defer { session.shutdown() }

        XCTAssertTrue(session.subtitleTrackTitles.isEmpty)
        XCTAssertEqual(session.selectedSubtitleTrackIndex, 0)
        XCTAssertFalse(session.setSubtitle(index: 1))
        XCTAssertFalse(session.isPictureInPictureActive)
        XCTAssertFalse(session.isClosed)
    }

    func testPictureInPictureWithoutAnItemIsRejectedWithoutStarting() {
        let session = PlayerSession()
        defer { session.shutdown() }

        session.setPictureInPicture(enabled: true)
        XCTAssertFalse(session.isPictureInPictureActive)
    }

    func testShutdownMarksTheSessionClosed() {
        let session = PlayerSession()
        session.shutdown()

        XCTAssertTrue(session.isClosed)
        XCTAssertFalse(session.setSubtitle(index: 0))
        XCTAssertFalse(session.isPictureInPictureActive)
    }
}
