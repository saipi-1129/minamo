import AVFoundation
import XCTest
@testable import KickPlayerCore

final class LowLatencyPlaylistRewriterTests: XCTestCase {
    private let mediaBaseURL = URL(string: "https://example.invalid/live/index.m3u8")!

    func testMasterRewritesVariantURLsToCustomScheme() {
        let baseURL = URL(string: "https://example.invalid/live/master.m3u8?token=master")!
        let text = """
        #EXTM3U
        #EXT-X-VERSION:3
        #EXT-X-STREAM-INF:BANDWIDTH=3422999,RESOLUTION=1280x720
        720/index.m3u8?token=variant
        #EXT-X-STREAM-INF:BANDWIDTH=1427999,RESOLUTION=852x480
        480/index.m3u8?token=other
        """

        let output = LowLatencyPlaylistRewriter.rewriteMaster(text, baseURL: baseURL)

        XCTAssertTrue(
            output.contains("kick-lowlatency://example.invalid/live/720/index.m3u8?token=variant")
        )
        XCTAssertTrue(
            output.contains("kick-lowlatency://example.invalid/live/480/index.m3u8?token=other")
        )
        XCTAssertTrue(output.contains("#EXT-X-STREAM-INF:BANDWIDTH=3422999,RESOLUTION=1280x720"))
    }

    func testCustomAndRealURLRoundTripPreservesQuery() throws {
        let realURL = URL(string: "https://example.invalid/live/index.m3u8?token=test")!
        let customURL = LowLatencyPlaylistRewriter.customURL(for: realURL)

        XCTAssertEqual(customURL.scheme, LowLatencyPlaylistRewriter.customScheme)
        XCTAssertEqual(
            LowLatencyPlaylistRewriter.realURL(for: customURL)?.absoluteString,
            realURL.absoluteString
        )
    }

    func testMediaRewriteKeepsLastRegularSegmentsAndAppendsPrefetch() throws {
        let output = LowLatencyPlaylistRewriter.rewriteMedia(
            mediaPlaylist(),
            baseURL: mediaBaseURL,
            availablePrefetchURLs: [
                "https://example.invalid/live/seg104.ts",
                "https://example.invalid/live/seg105.ts",
            ]
        )

        XCTAssertTrue(output.contains("#EXT-X-MEDIA-SEQUENCE:102"))
        XCTAssertTrue(output.contains("https://example.invalid/live/seg102.ts"))
        XCTAssertTrue(output.contains("https://example.invalid/live/seg103.ts"))
        XCTAssertTrue(output.contains("https://example.invalid/live/seg104.ts"))
        XCTAssertTrue(output.contains("https://example.invalid/live/seg105.ts"))
        XCTAssertFalse(output.contains("seg100.ts"))
        XCTAssertFalse(output.contains("seg101.ts"))
        XCTAssertFalse(output.contains("#EXT-X-PREFETCH"))
        XCTAssertFalse(output.contains("#EXT-X-ENDLIST"))
        XCTAssertTrue(output.contains("#EXT-X-PROGRAM-DATE-TIME:2026-11-15T00:00:04.000Z"))
        XCTAssertTrue(output.contains("#EXT-X-PROGRAM-DATE-TIME:2026-11-15T00:00:06.000Z"))
        XCTAssertTrue(output.contains("#EXT-X-PROGRAM-DATE-TIME:2026-11-15T00:00:08.000Z"))
        XCTAssertTrue(output.contains("#EXT-X-PROGRAM-DATE-TIME:2026-11-15T00:00:10.000Z"))
        XCTAssertTrue(output.contains("#EXTINF:2.000,"))
    }

    func testUnavailablePrefetchIsSkippedAndRegularPlaylistIsReturned() {
        let output = LowLatencyPlaylistRewriter.rewriteMedia(
            mediaPlaylist(),
            baseURL: mediaBaseURL,
            availablePrefetchURLs: []
        )

        XCTAssertTrue(output.contains("#EXT-X-MEDIA-SEQUENCE:102"))
        XCTAssertTrue(output.contains("https://example.invalid/live/seg102.ts"))
        XCTAssertTrue(output.contains("https://example.invalid/live/seg103.ts"))
        XCTAssertFalse(output.contains("seg104.ts"))
        XCTAssertFalse(output.contains("seg105.ts"))
        XCTAssertFalse(output.contains("#EXT-X-PREFETCH"))
        XCTAssertFalse(output.contains("#EXT-X-ENDLIST"))
    }

    func testPartiallyAvailablePrefetchOnlyAppendsAvailableSegments() {
        let output = LowLatencyPlaylistRewriter.rewriteMedia(
            mediaPlaylist(),
            baseURL: mediaBaseURL,
            availablePrefetchURLs: ["https://example.invalid/live/seg104.ts"]
        )

        XCTAssertTrue(output.contains("https://example.invalid/live/seg104.ts"))
        XCTAssertFalse(output.contains("seg105.ts"))
        XCTAssertTrue(output.contains("#EXT-X-PROGRAM-DATE-TIME:2026-11-15T00:00:08.000Z"))
        XCTAssertFalse(output.contains("#EXT-X-PROGRAM-DATE-TIME:2026-11-15T00:00:10.000Z"))
    }

    func testPrefetchURLsAreResolvedAgainstTheMediaPlaylist() {
        let urls = LowLatencyPlaylistRewriter.prefetchURLs(
            in: mediaPlaylist(),
            baseURL: mediaBaseURL
        )

        XCTAssertEqual(
            urls.map(\.absoluteString),
            [
                "https://example.invalid/live/seg104.ts",
                "https://example.invalid/live/seg105.ts",
            ]
        )
    }

    func testByteRangeTagsStayWithKeptSegments() {
        let text = """
        #EXTM3U
        #EXT-X-VERSION:3
        #EXT-X-TARGETDURATION:6
        #EXT-X-MEDIA-SEQUENCE:100
        #EXT-X-BYTERANGE:1000@0
        #EXTINF:2.000,
        seg100.ts
        #EXT-X-BYTERANGE:1000@1000
        #EXTINF:2.000,
        seg101.ts
        #EXT-X-BYTERANGE:1000@2000
        #EXTINF:2.000,
        seg102.ts
        #EXT-X-BYTERANGE:1000@3000
        #EXTINF:2.000,
        seg103.ts
        """

        let output = LowLatencyPlaylistRewriter.rewriteMedia(
            text,
            baseURL: mediaBaseURL,
            availablePrefetchURLs: []
        )

        XCTAssertFalse(output.contains("1000@0"))
        XCTAssertFalse(output.contains("1000@1000"))
        XCTAssertTrue(output.contains("1000@2000"))
        XCTAssertTrue(output.contains("1000@3000"))
        XCTAssertTrue(output.contains("#EXT-X-MEDIA-SEQUENCE:102"))
    }

    func testMediaRewriteDisablesBlockingReloads() {
        let text = """
        #EXTM3U
        #EXT-X-VERSION:6
        #EXT-X-TARGETDURATION:2
        #EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=1.0
        #EXT-X-MEDIA-SEQUENCE:100
        #EXTINF:2.000,
        seg100.ts
        #EXTINF:2.000,
        seg101.ts
        """

        let output = LowLatencyPlaylistRewriter.rewriteMedia(
            text,
            baseURL: mediaBaseURL,
            availablePrefetchURLs: []
        )

        XCTAssertTrue(output.contains("CAN-BLOCK-RELOAD=NO"))
        XCTAssertFalse(output.contains("CAN-BLOCK-RELOAD=YES"))
        XCTAssertTrue(output.contains("PART-HOLD-BACK=1.0"))
    }

    func testDiscontinuitySequenceStaysInHeader() {
        let text = """
        #EXTM3U
        #EXT-X-VERSION:3
        #EXT-X-TARGETDURATION:6
        #EXT-X-MEDIA-SEQUENCE:100
        #EXT-X-DISCONTINUITY-SEQUENCE:5
        #EXTINF:2.000,
        seg100.ts
        #EXTINF:2.000,
        seg101.ts
        #EXTINF:2.000,
        seg102.ts
        """

        let output = LowLatencyPlaylistRewriter.rewriteMedia(
            text,
            baseURL: mediaBaseURL,
            availablePrefetchURLs: []
        )

        XCTAssertTrue(output.contains("#EXT-X-DISCONTINUITY-SEQUENCE:5"))
        XCTAssertTrue(output.contains("#EXT-X-MEDIA-SEQUENCE:101"))
    }

    func testMasterRewriteForLocalProxyPointsVariantsAtLoopbackMedia() throws {
        let baseURL = URL(string: "https://example.invalid/live/master.m3u8?token=master")!
        let localMediaURL = URL(string: "http://127.0.0.1:43210/media.m3u8")!
        let text = """
        #EXTM3U
        #EXT-X-VERSION:3
        #EXT-X-STREAM-INF:BANDWIDTH=3422999,RESOLUTION=1280x720
        720/index.m3u8?token=variant
        #EXT-X-STREAM-INF:BANDWIDTH=1427999,RESOLUTION=852x480
        480/index.m3u8?token=other
        """

        let rewrite = LowLatencyPlaylistRewriter.rewriteMasterForLocalProxy(
            text,
            baseURL: baseURL,
            localMediaURL: localMediaURL
        )

        XCTAssertEqual(
            rewrite.variants.map(\.absoluteString),
            [
                "https://example.invalid/live/720/index.m3u8?token=variant",
                "https://example.invalid/live/480/index.m3u8?token=other",
            ]
        )
        XCTAssertTrue(
            rewrite.text.contains("http://127.0.0.1:43210/media.m3u8?variant=0")
        )
        XCTAssertTrue(
            rewrite.text.contains("http://127.0.0.1:43210/media.m3u8?variant=1")
        )
        XCTAssertFalse(rewrite.text.contains("example.invalid/live/720/index.m3u8"))
        XCTAssertFalse(rewrite.text.contains("example.invalid/live/480/index.m3u8"))
        XCTAssertTrue(
            rewrite.text.contains("#EXT-X-STREAM-INF:BANDWIDTH=3422999,RESOLUTION=1280x720")
        )
    }

    private func mediaPlaylist() -> String {
        """
        #EXTM3U
        #EXT-X-VERSION:3
        #EXT-X-TARGETDURATION:6
        #EXT-X-MEDIA-SEQUENCE:100
        #EXT-X-PROGRAM-DATE-TIME:2026-11-15T00:00:00.000Z
        #EXTINF:2.000,
        seg100.ts
        #EXT-X-PROGRAM-DATE-TIME:2026-11-15T00:00:02.000Z
        #EXTINF:2.000,
        seg101.ts
        #EXT-X-PROGRAM-DATE-TIME:2026-11-15T00:00:04.000Z
        #EXTINF:2.000,
        seg102.ts
        #EXT-X-PROGRAM-DATE-TIME:2026-11-15T00:00:06.000Z
        #EXTINF:2.000,
        seg103.ts
        #EXT-X-PREFETCH:https://example.invalid/live/seg104.ts
        #EXT-X-PREFETCH:https://example.invalid/live/seg105.ts
        #EXT-X-ENDLIST
        """
    }
}

final class LowLatencyCatchUpTests: XCTestCase {
    func testUsesCatchUpRateWhenMoreThanFourSecondsBehind() throws {
        let decision = LowLatencyCatchUp.rate(
            lowLatencyEnabled: true,
            isLiveSource: true,
            isPlaying: true,
            currentTime: time(10),
            seekableEnd: time(15)
        )

        XCTAssertEqual(try XCTUnwrap(decision), 1.05, accuracy: 0.001)
    }

    func testReturnsNormalRateWhenWithinOneSecond() throws {
        let decision = LowLatencyCatchUp.rate(
            lowLatencyEnabled: true,
            isLiveSource: true,
            isPlaying: true,
            currentTime: time(14.5),
            seekableEnd: time(15)
        )

        XCTAssertEqual(try XCTUnwrap(decision), 1, accuracy: 0.001)
    }

    func testDoesNothingInsideHysteresisBand() {
        XCTAssertNil(
            LowLatencyCatchUp.rate(
                lowLatencyEnabled: true,
                isLiveSource: true,
                isPlaying: true,
                currentTime: time(13),
                seekableEnd: time(15)
            )
        )
    }

    func testDoesNothingWhenLowLatencyIsOffOrDVR() {
        XCTAssertNil(
            LowLatencyCatchUp.rate(
                lowLatencyEnabled: false,
                isLiveSource: true,
                isPlaying: true,
                currentTime: time(10),
                seekableEnd: time(15)
            )
        )
        XCTAssertNil(
            LowLatencyCatchUp.rate(
                lowLatencyEnabled: true,
                isLiveSource: false,
                isPlaying: true,
                currentTime: time(10),
                seekableEnd: time(15)
            )
        )
    }

    func testDoesNothingWhilePaused() {
        XCTAssertNil(
            LowLatencyCatchUp.rate(
                lowLatencyEnabled: true,
                isLiveSource: true,
                isPlaying: false,
                currentTime: time(10),
                seekableEnd: time(15)
            )
        )
    }

    func testDoesNothingWithInvalidTimes() {
        XCTAssertNil(
            LowLatencyCatchUp.rate(
                lowLatencyEnabled: true,
                isLiveSource: true,
                isPlaying: true,
                currentTime: .invalid,
                seekableEnd: time(15)
            )
        )
        XCTAssertNil(
            LowLatencyCatchUp.rate(
                lowLatencyEnabled: true,
                isLiveSource: true,
                isPlaying: true,
                currentTime: time(10),
                seekableEnd: .invalid
            )
        )
    }

    func testDoesNotResetRateInsideHysteresisBand() {
        XCTAssertFalse(
            LowLatencyCatchUp.shouldResetRate(
                lowLatencyEnabled: true,
                isLiveSource: true,
                isPlaying: true,
                currentTime: time(13),
                seekableEnd: time(15)
            )
        )
    }

    func testResetsRateWhenCatchUpNoLongerApplies() {
        XCTAssertTrue(
            LowLatencyCatchUp.shouldResetRate(
                lowLatencyEnabled: false,
                isLiveSource: true,
                isPlaying: true,
                currentTime: time(13),
                seekableEnd: time(15)
            )
        )
        XCTAssertTrue(
            LowLatencyCatchUp.shouldResetRate(
                lowLatencyEnabled: true,
                isLiveSource: false,
                isPlaying: true,
                currentTime: time(13),
                seekableEnd: time(15)
            )
        )
        XCTAssertTrue(
            LowLatencyCatchUp.shouldResetRate(
                lowLatencyEnabled: true,
                isLiveSource: true,
                isPlaying: false,
                currentTime: time(13),
                seekableEnd: time(15)
            )
        )
        XCTAssertTrue(
            LowLatencyCatchUp.shouldResetRate(
                lowLatencyEnabled: true,
                isLiveSource: true,
                isPlaying: true,
                currentTime: .invalid,
                seekableEnd: time(15)
            )
        )
    }

    func testHardSeekTriggersOnlyBeyondEightSeconds() {
        XCTAssertTrue(
            LowLatencyCatchUp.shouldSeekToLive(
                lowLatencyEnabled: true,
                isLiveSource: true,
                isPlaying: true,
                currentTime: time(91),
                seekableEnd: time(100)
            )
        )
        XCTAssertFalse(
            LowLatencyCatchUp.shouldSeekToLive(
                lowLatencyEnabled: true,
                isLiveSource: true,
                isPlaying: true,
                currentTime: time(93),
                seekableEnd: time(100)
            )
        )
        XCTAssertFalse(
            LowLatencyCatchUp.shouldSeekToLive(
                lowLatencyEnabled: false,
                isLiveSource: true,
                isPlaying: true,
                currentTime: time(91),
                seekableEnd: time(100)
            )
        )
    }

    private func time(_ seconds: Double) -> CMTime {
        CMTime(seconds: seconds, preferredTimescale: 600)
    }
}

final class LowLatencyHLSProxyTests: XCTestCase {
    private let baseURL = URL(string: "http://127.0.0.1:43210/")!

    func testRoutesMasterAndMediaPlaylists() {
        XCTAssertEqual(
            LowLatencyHLSProxy.route(for: URL(string: "/master.m3u8", relativeTo: baseURL)!),
            .master
        )
        XCTAssertEqual(
            LowLatencyHLSProxy.route(
                for: URL(string: "/media.m3u8?variant=0", relativeTo: baseURL)!
            ),
            .media(0)
        )
        XCTAssertEqual(
            LowLatencyHLSProxy.route(
                for: URL(string: "/media.m3u8?variant=7", relativeTo: baseURL)!
            ),
            .media(7)
        )
    }

    func testRejectsUnknownPathsAndInvalidVariants() {
        XCTAssertNil(
            LowLatencyHLSProxy.route(for: URL(string: "/other.m3u8", relativeTo: baseURL)!)
        )
        XCTAssertNil(
            LowLatencyHLSProxy.route(for: URL(string: "/media.m3u8", relativeTo: baseURL)!)
        )
        XCTAssertNil(
            LowLatencyHLSProxy.route(
                for: URL(string: "/media.m3u8?variant=abc", relativeTo: baseURL)!
            )
        )
        XCTAssertNil(
            LowLatencyHLSProxy.route(
                for: URL(string: "/media.m3u8?variant=-1", relativeTo: baseURL)!
            )
        )
        XCTAssertNil(
            LowLatencyHLSProxy.route(for: URL(string: "/master.m3u8/extra", relativeTo: baseURL)!)
        )
    }

    func testProxyStartsOnLoopbackAndStops() throws {
        let proxy = LowLatencyHLSProxy()
        let ready = expectation(description: "proxy ready")
        let box = LockedURLBox()

        proxy.start(
            realLiveURL: URL(string: "https://example.invalid/live/master.m3u8")!
        ) { result in
            if case .success(let url) = result {
                box.set(url)
            }
            ready.fulfill()
        }

        wait(for: [ready], timeout: 5)
        let url = try XCTUnwrap(box.get())
        XCTAssertEqual(url.host, "127.0.0.1")
        XCTAssertEqual(url.path, "/master.m3u8")
        XCTAssertNotNil(url.port)
        proxy.stop()
    }

    func testProxyReturnsNotFoundForUnknownPathAndVariant() throws {
        let proxy = LowLatencyHLSProxy()
        let ready = expectation(description: "proxy ready")
        let box = LockedURLBox()

        proxy.start(
            realLiveURL: URL(string: "https://example.invalid/live/master.m3u8")!
        ) { result in
            if case .success(let url) = result {
                box.set(url)
            }
            ready.fulfill()
        }

        wait(for: [ready], timeout: 5)
        let masterURL = try XCTUnwrap(box.get())

        var missingComponents = URLComponents(url: masterURL, resolvingAgainstBaseURL: false)
        missingComponents?.path = "/missing.m3u8"
        let missingURL = try XCTUnwrap(missingComponents?.url)
        XCTAssertEqual(statusCode(for: missingURL), 404)

        var components = URLComponents(url: masterURL, resolvingAgainstBaseURL: false)
        components?.path = "/media.m3u8"
        components?.queryItems = [URLQueryItem(name: "variant", value: "99")]
        let unknownVariantURL = try XCTUnwrap(components?.url)
        XCTAssertEqual(statusCode(for: unknownVariantURL), 404)

        proxy.stop()
    }

    private func statusCode(for url: URL) -> Int {
        let response = expectation(description: "http response")
        let box = LockedIntBox()
        URLSession.shared.dataTask(with: url) { _, urlResponse, _ in
            box.set((urlResponse as? HTTPURLResponse)?.statusCode ?? -1)
            response.fulfill()
        }.resume()
        wait(for: [response], timeout: 5)
        return box.get()
    }

    func testMinimalRewriteKeepsHeaderAndAppendsPrefetch() {
        let text = """
        #EXTM3U
        #EXT-X-VERSION:3
        #EXT-X-TARGETDURATION:6
        #EXT-X-MEDIA-SEQUENCE:10
        #EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:00.000Z
        #EXTINF:2.000,live
        https://example.invalid/a.ts
        #EXT-X-PREFETCH:https://example.invalid/p.ts
        """
        let output = LowLatencyPlaylistRewriter.rewriteMediaMinimal(
            text,
            baseURL: URL(string: "https://example.invalid/live.m3u8")!,
            availablePrefetchURLs: ["https://example.invalid/p.ts"]
        )
        XCTAssertTrue(output.contains("#EXT-X-MEDIA-SEQUENCE:10"))
        XCTAssertTrue(output.contains("#EXTINF:2.000,live"))
        XCTAssertTrue(output.contains("#EXTINF:2.000,"))
        XCTAssertTrue(output.contains("https://example.invalid/p.ts"))
        XCTAssertFalse(output.contains("#EXT-X-PREFETCH"))
    }
}

private final class LockedURLBox {
    private let lock = NSLock()
    private var value: URL?

    func set(_ url: URL) {
        lock.lock()
        value = url
        lock.unlock()
    }

    func get() -> URL? {
        lock.lock()
        defer { lock.unlock() }
        return value
    }
}

private final class LockedIntBox {
    private let lock = NSLock()
    private var value = -1

    func set(_ newValue: Int) {
        lock.lock()
        value = newValue
        lock.unlock()
    }

    func get() -> Int {
        lock.lock()
        defer { lock.unlock() }
        return value
    }
}
