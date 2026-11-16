import AVFoundation
import Foundation

/// Experimental custom-scheme loader kept for comparison. The default live path
/// uses `LowLatencyHLSProxy` instead. URLs are never logged or persisted by this type.
final class LowLatencyResourceLoader: NSObject, AVAssetResourceLoaderDelegate {
    private let session: URLSession
    private let responseQueue = DispatchQueue(
        label: "com.kick.player.low-latency-playlist-response"
    )

    override init() {
        let configuration = URLSessionConfiguration.default
        configuration.urlCache = nil
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        session = URLSession(configuration: configuration)
        super.init()
    }

    deinit {
        session.invalidateAndCancel()
    }

    func cancel() {
        session.invalidateAndCancel()
    }

    func resourceLoader(
        _ resourceLoader: AVAssetResourceLoader,
        shouldWaitForLoadingOfRequestedResource loadingRequest: AVAssetResourceLoadingRequest
    ) -> Bool {
        guard
            let customURL = loadingRequest.request.url,
            customURL.scheme == LowLatencyPlaylistRewriter.customScheme,
            let realURL = LowLatencyPlaylistRewriter.realURL(for: customURL)
        else {
            return false
        }

        #if DEBUG
        print("PLAYLIST_REQUEST")
        fflush(stdout)
        #endif
        fetch(realURL) { [weak self] data, response, error in
            guard let self else { return }
            self.responseQueue.async {
                guard !loadingRequest.isCancelled else { return }
                guard
                    let data,
                    let httpResponse = response as? HTTPURLResponse,
                    (200...299).contains(httpResponse.statusCode),
                    let text = String(data: data, encoding: .utf8)
                else {
                    loadingRequest.finishLoading(with: error ?? self.makeError())
                    return
                }

                let contentType = httpResponse.value(forHTTPHeaderField: "Content-Type")
                    ?? "application/vnd.apple.mpegurl"
                let baseURL = httpResponse.url ?? realURL

                if text.contains("#EXT-X-STREAM-INF") {
                    let rewritten = LowLatencyPlaylistRewriter.rewriteMaster(text, baseURL: baseURL)
                    self.respond(loadingRequest, text: rewritten, contentType: contentType)
                } else {
                    let prefetchURLs = LowLatencyPlaylistRewriter.prefetchURLs(in: text, baseURL: baseURL)
                    self.checkAvailability(of: prefetchURLs) { available in
                        self.responseQueue.async {
                            guard !loadingRequest.isCancelled else { return }
                            let rewritten = LowLatencyPlaylistRewriter.rewriteMedia(
                                text,
                                baseURL: baseURL,
                                availablePrefetchURLs: available
                            )
                            self.respond(loadingRequest, text: rewritten, contentType: contentType)
                        }
                    }
                }
            }
        }
        return true
    }

    private func fetch(
        _ url: URL,
        completion: @escaping (Data?, URLResponse?, Error?) -> Void
    ) {
        var request = URLRequest(url: url)
        request.cachePolicy = .reloadIgnoringLocalCacheData
        request.timeoutInterval = 10
        session.dataTask(with: request, completionHandler: completion).resume()
    }

    private func checkAvailability(
        of urls: [URL],
        completion: @escaping (Set<String>) -> Void
    ) {
        guard !urls.isEmpty else {
            completion([])
            return
        }

        let group = DispatchGroup()
        let lock = NSLock()
        var available = Set<String>()

        for url in urls {
            group.enter()
            var request = URLRequest(url: url)
            request.httpMethod = "GET"
            request.setValue("bytes=0-0", forHTTPHeaderField: "Range")
            request.cachePolicy = .reloadIgnoringLocalCacheData
            request.timeoutInterval = 2
            session.dataTask(with: request) { _, response, error in
                defer { group.leave() }
                guard
                    error == nil,
                    let httpResponse = response as? HTTPURLResponse,
                    (200...299).contains(httpResponse.statusCode)
                else {
                    return
                }
                lock.lock()
                available.insert(url.absoluteString)
                lock.unlock()
            }.resume()
        }

        group.notify(queue: .global()) {
            completion(available)
        }
    }

    private func respond(
        _ loadingRequest: AVAssetResourceLoadingRequest,
        text: String,
        contentType: String
    ) {
        guard let data = text.data(using: .utf8) else {
            loadingRequest.finishLoading(with: makeError())
            return
        }

        #if DEBUG
        print("PLAYLIST_RESPONSE")
        let tags = text.components(separatedBy: .newlines).filter { $0.hasPrefix("#") }
        print("REWRITTEN_TAGS \(tags.joined(separator: " | "))")
        fflush(stdout)
        if let age = Self.playlistAge(text: text) {
            print("PLAYLIST_AGE \(String(format: "%.2f", age))")
            fflush(stdout)
        }
        #endif

        if let contentInformation = loadingRequest.contentInformationRequest {
            contentInformation.contentType = contentType
            contentInformation.contentLength = Int64(data.count)
            contentInformation.isByteRangeAccessSupported = false
        }
        loadingRequest.dataRequest?.respond(with: data)
        loadingRequest.finishLoading()
    }

    private static func playlistAge(text: String) -> TimeInterval? {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        var lastDate: Date?
        var lastDuration: Double?
        for line in text.components(separatedBy: .newlines) {
            if line.hasPrefix("#EXT-X-PROGRAM-DATE-TIME:") {
                let value = String(line.dropFirst("#EXT-X-PROGRAM-DATE-TIME:".count))
                lastDate = formatter.date(from: value)
            } else if line.hasPrefix("#EXTINF:") {
                let value = line.dropFirst("#EXTINF:".count)
                if let duration = value.split(separator: ",").first.flatMap({ Double($0) }) {
                    lastDuration = duration
                }
            }
        }
        guard let lastDate, let lastDuration else { return nil }
        return Date().timeIntervalSince(lastDate.addingTimeInterval(lastDuration))
    }

    private func makeError() -> NSError {
        NSError(
            domain: "KickLowLatencyPlaylist",
            code: -1,
            userInfo: [NSLocalizedDescriptionKey: "Low-latency playlist request failed."]
        )
    }
}
