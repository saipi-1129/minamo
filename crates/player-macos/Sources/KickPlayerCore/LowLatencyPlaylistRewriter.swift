import CoreMedia
import Foundation

/// Experimental live-playlist rewrite that turns IVS prefetch hints into regular
/// segments so AVPlayer can start closer to the live edge. The rewriter is pure;
/// network fetching and availability checks live in `LowLatencyHLSProxy` and the
/// experimental `LowLatencyResourceLoader`.
enum LowLatencyPlaylistRewriter {
    static let customScheme = "kick-lowlatency"
    static let keptRegularSegmentCount = 2
    static let prefetchSegmentDuration: Double = 2

    private struct MediaSegment {
        let duration: String
        let url: URL
        let programDateTime: String?
        let tags: [String]
    }

    static func customURL(for realURL: URL) -> URL {
        var components = URLComponents(url: realURL, resolvingAgainstBaseURL: false)
        components?.scheme = customScheme
        return components?.url ?? realURL
    }

    static func realURL(for customURL: URL) -> URL? {
        guard customURL.scheme == customScheme else { return nil }
        var components = URLComponents(url: customURL, resolvingAgainstBaseURL: false)
        components?.scheme = "https"
        return components?.url
    }

    static func rewriteMaster(_ text: String, baseURL: URL) -> String {
        var output: [String] = []
        var expectingVariant = false

        for line in text.components(separatedBy: .newlines) {
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            if trimmed.hasPrefix("#EXT-X-STREAM-INF:") {
                output.append(line)
                expectingVariant = true
            } else if expectingVariant, !trimmed.isEmpty, !trimmed.hasPrefix("#") {
                if let url = URL(string: trimmed, relativeTo: baseURL)?.absoluteURL {
                    output.append(customURL(for: url).absoluteString)
                } else {
                    output.append(line)
                }
                expectingVariant = false
            } else {
                output.append(line)
            }
        }

        return output.joined(separator: "\n") + "\n"
    }

    struct MasterRewrite {
        let text: String
        let variants: [URL]
    }

    /// Rewrites master variants to a loopback HTTP `/media.m3u8?variant=N` URL
    /// and returns the real variant URLs in the same order.
    static func rewriteMasterForLocalProxy(
        _ text: String,
        baseURL: URL,
        localMediaURL: URL
    ) -> MasterRewrite {
        var output: [String] = []
        var variants: [URL] = []
        var expectingVariant = false

        for line in text.components(separatedBy: .newlines) {
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            if trimmed.hasPrefix("#EXT-X-STREAM-INF:") {
                output.append(line)
                expectingVariant = true
            } else if expectingVariant, !trimmed.isEmpty, !trimmed.hasPrefix("#") {
                if let url = URL(string: trimmed, relativeTo: baseURL)?.absoluteURL {
                    let index = variants.count
                    variants.append(url)
                    var components = URLComponents(url: localMediaURL, resolvingAgainstBaseURL: false)
                    var queryItems = components?.queryItems ?? []
                    queryItems.append(URLQueryItem(name: "variant", value: String(index)))
                    components?.queryItems = queryItems
                    output.append(components?.url?.absoluteString ?? localMediaURL.absoluteString)
                } else {
                    output.append(line)
                }
                expectingVariant = false
            } else {
                output.append(line)
            }
        }

        return MasterRewrite(
            text: output.joined(separator: "\n") + "\n",
            variants: variants
        )
    }

    static func prefetchURLs(in text: String, baseURL: URL) -> [URL] {
        text.components(separatedBy: .newlines).compactMap { line in
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            guard trimmed.hasPrefix("#EXT-X-PREFETCH:") else { return nil }
            let value = String(trimmed.dropFirst("#EXT-X-PREFETCH:".count))
            return URL(string: value, relativeTo: baseURL)?.absoluteURL
        }
    }

    static func rewriteMedia(
        _ text: String,
        baseURL: URL,
        availablePrefetchURLs: Set<String>
    ) -> String {
        let lines = text.components(separatedBy: .newlines)
        var header: [String] = []
        var segments: [MediaSegment] = []
        var prefetchURLs: [URL] = []
        var mediaSequence: Int?
        var mediaSequenceLineIndex: Int?
        var inSegments = false
        var pendingDuration: String?
        var pendingProgramDateTime: String?
        var pendingTags: [String] = []

        for line in lines {
            let trimmed = line.trimmingCharacters(in: .whitespaces)

            if !inSegments {
                if isSegmentStartTag(trimmed) {
                    inSegments = true
                } else {
                    if trimmed.hasPrefix("#EXT-X-MEDIA-SEQUENCE:") {
                        mediaSequence = Int(trimmed.dropFirst("#EXT-X-MEDIA-SEQUENCE:".count))
                        mediaSequenceLineIndex = header.count
                        header.append(line)
                    } else if trimmed.hasPrefix("#EXT-X-ENDLIST")
                        || trimmed.hasPrefix("#EXT-X-PREFETCH:")
                        || trimmed.hasPrefix("#EXT-X-NET-LIVE-VIDEO-")
                        || trimmed.hasPrefix("#EXT-X-DATERANGE:")
                    {
                        continue
                    } else if trimmed.hasPrefix("#EXT-X-SERVER-CONTROL:") {
                        // The proxy answers every reload immediately with the latest
                        // playlist, so blocking reloads would spin without waiting.
                        header.append(
                            line.replacingOccurrences(
                                of: "CAN-BLOCK-RELOAD=YES",
                                with: "CAN-BLOCK-RELOAD=NO"
                            )
                        )
                    } else {
                        header.append(line)
                    }
                    continue
                }
            }

            if trimmed.hasPrefix("#EXT-X-PREFETCH:") {
                let value = String(trimmed.dropFirst("#EXT-X-PREFETCH:".count))
                if let url = URL(string: value, relativeTo: baseURL)?.absoluteURL {
                    prefetchURLs.append(url)
                }
            } else if trimmed.hasPrefix("#EXT-X-PROGRAM-DATE-TIME:") {
                pendingProgramDateTime = String(trimmed.dropFirst("#EXT-X-PROGRAM-DATE-TIME:".count))
            } else if trimmed.hasPrefix("#EXTINF:") {
                pendingDuration = String(trimmed.dropFirst("#EXTINF:".count))
            } else if trimmed.hasPrefix("#EXT-X-ENDLIST") {
                continue
            } else if trimmed.hasPrefix("#") {
                pendingTags.append(line)
            } else if !trimmed.isEmpty, let duration = pendingDuration {
                if let url = URL(string: trimmed, relativeTo: baseURL)?.absoluteURL {
                    segments.append(MediaSegment(
                        duration: duration,
                        url: url,
                        programDateTime: pendingProgramDateTime,
                        tags: pendingTags
                    ))
                }
                pendingDuration = nil
                pendingProgramDateTime = nil
                pendingTags = []
            }
        }

        if let mediaSequence, let mediaSequenceLineIndex {
            let removedCount = max(0, segments.count - keptRegularSegmentCount)
            header[mediaSequenceLineIndex] = "#EXT-X-MEDIA-SEQUENCE:\(mediaSequence + removedCount)"
        }

        var output = header
        let keptSegments = segments.suffix(keptRegularSegmentCount)
        for segment in keptSegments {
            if let programDateTime = segment.programDateTime {
                output.append("#EXT-X-PROGRAM-DATE-TIME:\(programDateTime)")
            }
            output.append(contentsOf: segment.tags)
            output.append("#EXTINF:\(normalizedDuration(segment.duration))")
            output.append(segment.url.absoluteString)
        }

        let lastProgramDateTime = keptSegments.last?.programDateTime.flatMap(parseProgramDateTime)
        for (index, url) in prefetchURLs.enumerated() {
            guard availablePrefetchURLs.contains(url.absoluteString) else { continue }
            if let lastProgramDateTime {
                let date = lastProgramDateTime.addingTimeInterval(
                    prefetchSegmentDuration * Double(index + 1)
                )
                output.append("#EXT-X-PROGRAM-DATE-TIME:\(formatProgramDateTime(date))")
            }
            output.append("#EXTINF:\(String(format: "%.3f", prefetchSegmentDuration)),")
            output.append(url.absoluteString)
        }

        return output.joined(separator: "\n") + "\n"
    }

    private static func isSegmentStartTag(_ line: String) -> Bool {
        line == "#EXT-X-DISCONTINUITY"
            || line == "#EXT-X-GAP"
            || line.hasPrefix("#EXT-X-PROGRAM-DATE-TIME:")
            || line.hasPrefix("#EXTINF:")
            || line.hasPrefix("#EXT-X-BYTERANGE:")
            || line.hasPrefix("#EXT-X-BITRATE:")
    }

    /// Minimal rewrite that keeps the original header, regular segments and
    /// program-date-time tags, and only turns available prefetch entries into
    /// regular segments. This is the form AVPlayer accepts reliably.
    static func rewriteMediaMinimal(
        _ text: String,
        baseURL: URL,
        availablePrefetchURLs: Set<String>
    ) -> String {
        var lines = text.components(separatedBy: .newlines).filter {
            !$0.hasPrefix("#EXT-X-PREFETCH:")
        }
        for url in prefetchURLs(in: text, baseURL: baseURL)
        where availablePrefetchURLs.contains(url.absoluteString) {
            lines.append("#EXTINF:\(String(format: "%.3f", prefetchSegmentDuration)),")
            lines.append(url.absoluteString)
        }
        return lines.joined(separator: "\n") + "\n"
    }

    private static func normalizedDuration(_ duration: String) -> String {
        duration.contains(",") ? duration : duration + ","
    }

    private static func parseProgramDateTime(_ value: String) -> Date? {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        if let date = formatter.date(from: value) {
            return date
        }
        formatter.formatOptions = [.withInternetDateTime]
        return formatter.date(from: value)
    }

    private static func formatProgramDateTime(_ date: Date) -> String {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        formatter.timeZone = TimeZone(secondsFromGMT: 0)
        return formatter.string(from: date)
    }
}

/// Small bounded catch-up rate for live playback that is behind the live edge.
enum LowLatencyCatchUp {
    static let behindThresholdSeconds: Double = 4
    static let caughtUpThresholdSeconds: Double = 1
    static let catchUpRate: Float = 1.05
    static let normalRate: Float = 1
    static let hardSeekThresholdSeconds: Double = 8

    static func rate(
        lowLatencyEnabled: Bool,
        isLiveSource: Bool,
        isPlaying: Bool,
        currentTime: CMTime,
        seekableEnd: CMTime
    ) -> Float? {
        guard lowLatencyEnabled, isLiveSource, isPlaying else { return nil }
        guard currentTime.isValid, currentTime.isNumeric,
              seekableEnd.isValid, seekableEnd.isNumeric
        else { return nil }

        let behind = CMTimeGetSeconds(seekableEnd) - CMTimeGetSeconds(currentTime)
        guard behind.isFinite else { return nil }

        if behind > behindThresholdSeconds {
            return catchUpRate
        }
        if behind <= caughtUpThresholdSeconds {
            return normalRate
        }
        return nil
    }

    /// Returns true when the playhead has drifted far enough that a one-shot
    /// live-edge seek is cheaper than waiting for the 1.05x rate to close the gap.
    static func shouldSeekToLive(
        lowLatencyEnabled: Bool,
        isLiveSource: Bool,
        isPlaying: Bool,
        currentTime: CMTime,
        seekableEnd: CMTime
    ) -> Bool {
        guard lowLatencyEnabled, isLiveSource, isPlaying else { return false }
        guard currentTime.isValid, currentTime.isNumeric,
              seekableEnd.isValid, seekableEnd.isNumeric
        else { return false }
        let behind = CMTimeGetSeconds(seekableEnd) - CMTimeGetSeconds(currentTime)
        return behind.isFinite && behind > hardSeekThresholdSeconds
    }

    /// Keeps a user-requested DVR position from being pulled back to live by
    /// the low-latency catch-up. The review state clears when the playhead is
    /// within one second of the live edge.
    /// Returns true when catch-up rate control no longer applies and an active
    /// 1.05 rate should be restored to 1.0. The hysteresis band returns false so
    /// an in-progress catch-up is not cancelled before it reaches the threshold.
    static func shouldResetRate(
        lowLatencyEnabled: Bool,
        isLiveSource: Bool,
        isPlaying: Bool,
        currentTime: CMTime,
        seekableEnd: CMTime
    ) -> Bool {
        guard lowLatencyEnabled, isLiveSource, isPlaying else { return true }
        guard currentTime.isValid, currentTime.isNumeric,
              seekableEnd.isValid, seekableEnd.isNumeric
        else { return true }
        return false
    }
}

/// Decides whether a finished item should be restarted at the live edge. A VOD
/// or clip must not restart: it would immediately end again and loop.
enum LiveEndRestart {
    static func shouldRestart(
        isVodMode: Bool,
        isLiveSource: Bool,
        lowLatencyEnabled: Bool
    ) -> Bool {
        !isVodMode && isLiveSource && lowLatencyEnabled
    }
}

/// Keeps a user-requested DVR position from being pulled back to live by the
/// low-latency catch-up. The review state clears when the playhead is within
/// one second of the live edge.
enum DvrReview {
    static let returnToLiveThresholdSeconds: Double = 1.0

    static func shouldSuspendCatchUp(reviewActive: Bool, behindSeconds: Double) -> Bool {
        reviewActive && behindSeconds > returnToLiveThresholdSeconds
    }

    static func shouldClear(reviewActive: Bool, behindSeconds: Double) -> Bool {
        reviewActive && behindSeconds <= returnToLiveThresholdSeconds
    }
}
