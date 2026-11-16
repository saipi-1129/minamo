import AVFoundation
import XCTest
@testable import KickPlayerCore

@MainActor
final class PlayerSessionLifecycleTests: XCTestCase {
    func testEndNotificationReleasesItemAndSessionAcceptsNextItem() async throws {
        let session = PlayerSession()
        defer { session.shutdown() }

        let ended = expectation(description: "end notification clears the current item")
        session.onStatusChanged = { status in
            if status == "Playback ended." {
                ended.fulfill()
            }
        }

        let endedItem = makeLocalItem()
        try session.play(item: endedItem)
        session.pause()
        XCTAssertTrue(session.currentSnapshot().hasCurrentItem)

        NotificationCenter.default.post(
            name: AVPlayerItem.didPlayToEndTimeNotification,
            object: endedItem
        )

        await fulfillment(of: [ended], timeout: 2)
        XCTAssertFalse(session.currentSnapshot().hasCurrentItem)
        XCTAssertEqual(session.stateCode, KickPlayerState.idle.rawValue)

        try session.play(item: makeLocalItem())
        XCTAssertTrue(session.currentSnapshot().hasCurrentItem)
    }

    func testFailureNotificationReleasesItemAndSetsFailedState() async throws {
        let session = PlayerSession()
        defer { session.shutdown() }

        let failed = expectation(description: "failure notification clears the current item")
        session.onStatusChanged = { status in
            if status == "Playback failed." {
                failed.fulfill()
            }
        }

        let failedItem = makeLocalItem()
        try session.play(item: failedItem)
        session.pause()
        NotificationCenter.default.post(
            name: AVPlayerItem.failedToPlayToEndTimeNotification,
            object: failedItem
        )

        await fulfillment(of: [failed], timeout: 2)
        XCTAssertFalse(session.currentSnapshot().hasCurrentItem)
        XCTAssertEqual(session.stateCode, KickPlayerState.failed.rawValue)
    }

    func testFailedItemStatusReleasesItem() async throws {
        let session = PlayerSession()
        defer { session.shutdown() }

        let failed = expectation(description: "failed item status clears the current item")
        session.onStatusChanged = { status in
            if status.hasPrefix("Playback failed") || status == "AVPlayer could not load this stream." {
                failed.fulfill()
            }
        }

        let missingLocalURL = URL(fileURLWithPath: NSTemporaryDirectory(), isDirectory: true)
            .appendingPathComponent("kick-player-missing-\(UUID().uuidString).mov")
        try session.play(item: AVPlayerItem(url: missingLocalURL))

        await fulfillment(of: [failed], timeout: 5)
        XCTAssertFalse(session.currentSnapshot().hasCurrentItem)
        XCTAssertEqual(session.stateCode, KickPlayerState.failed.rawValue)
    }

    func testSwitchingItemsRemovesPreviousItemNotifications() throws {
        let session = PlayerSession()
        defer { session.shutdown() }

        let oldItem = makeLocalItem()
        let currentItem = makeLocalItem()
        try session.play(item: oldItem)
        session.pause()
        try session.play(item: currentItem)
        session.pause()

        NotificationCenter.default.post(
            name: AVPlayerItem.didPlayToEndTimeNotification,
            object: oldItem
        )

        XCTAssertTrue(session.currentSnapshot().hasCurrentItem)
        XCTAssertEqual(session.statusText, "Paused.")
    }

    private func makeLocalItem() -> AVPlayerItem {
        AVPlayerItem(asset: AVMutableComposition())
    }
}
