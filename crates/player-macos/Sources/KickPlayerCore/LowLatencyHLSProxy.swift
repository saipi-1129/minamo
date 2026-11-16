import Foundation
import Network

/// Loopback HTTP HLS proxy used by the live item when low latency is enabled.
///
/// AVPlayer sees a normal `http://127.0.0.1:<port>/master.m3u8` playlist while the
/// proxy fetches the real playlists and injects available prefetch segments.
/// Real URLs stay in memory only; they are never logged or persisted.
final class LowLatencyHLSProxy {
    enum Route: Equatable {
        case master
        case media(Int)
    }

    enum ProxyError: Error {
        case listenerFailed
        case stopped
        case badResponse
    }

    static let maxVariants = 16
    static let maxConnections = 8
    static let maxRequestBytes = 8 * 1024
    static let maxPrefetchChecks = 4
    static let playlistContentType = "application/vnd.apple.mpegurl"

    private struct FetchResult {
        let data: Data
        let responseURL: URL?
    }

    private let queue = DispatchQueue(label: "com.kick.player.low-latency-proxy")
    private let session: URLSession
    private var listener: NWListener?
    private var connections: [ObjectIdentifier: NWConnection] = [:]
    private var variants: [URL] = []
    private var realMasterURL: URL?
    private var port: UInt16?
    private var generation = 0

    init() {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.urlCache = nil
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        configuration.httpShouldSetCookies = false
        configuration.httpCookieAcceptPolicy = .never
        session = URLSession(configuration: configuration)
    }

    deinit {
        listener?.stateUpdateHandler = nil
        listener?.newConnectionHandler = nil
        listener?.cancel()
        let activeConnections = Array(connections.values)
        connections.removeAll()
        for connection in activeConnections {
            connection.cancel()
        }
        session.invalidateAndCancel()
    }

    /// Pure request routing used by the server and by tests.
    static func route(for url: URL) -> Route? {
        switch url.path {
        case "/master.m3u8":
            return .master
        case "/media.m3u8":
            guard
                let value = URLComponents(url: url, resolvingAgainstBaseURL: false)?
                    .queryItems?
                    .first(where: { $0.name == "variant" })?
                    .value,
                let index = Int(value),
                index >= 0
            else {
                return nil
            }
            return .media(index)
        default:
            return nil
        }
    }

    /// Starts a loopback listener and reports the local master playlist URL.
    /// The completion is called on the proxy queue.
    func start(realLiveURL: URL, completion: @escaping (Result<URL, Error>) -> Void) {
        queue.async { [weak self] in
            guard let self else { return }
            self.stopLocked()
            self.generation += 1
            let generation = self.generation
            self.realMasterURL = realLiveURL

            let parameters = NWParameters.tcp
            parameters.allowLocalEndpointReuse = true
            parameters.requiredLocalEndpoint = .hostPort(
                host: .ipv4(.loopback),
                port: .any
            )

            let listener: NWListener
            do {
                listener = try NWListener(using: parameters)
            } catch {
                self.realMasterURL = nil
                completion(.failure(error))
                return
            }

            var didComplete = false
            listener.stateUpdateHandler = { [weak self, weak listener] state in
                guard let self, let listener, generation == self.generation else { return }
                switch state {
                case .ready:
                    guard !didComplete, let port = listener.port else { return }
                    didComplete = true
                    self.port = port.rawValue
                    guard let localURL = self.localURL(path: "/master.m3u8") else {
                        self.stopLocked()
                        completion(.failure(ProxyError.listenerFailed))
                        return
                    }
                    #if DEBUG
                    print("PROXY_STARTED")
                    fflush(stdout)
                    #endif
                    completion(.success(localURL))
                case .failed(let error):
                    guard !didComplete else { return }
                    didComplete = true
                    self.stopLocked()
                    completion(.failure(error))
                case .cancelled:
                    guard !didComplete else { return }
                    didComplete = true
                    completion(.failure(ProxyError.stopped))
                default:
                    break
                }
            }
            listener.newConnectionLimit = Self.maxConnections
            listener.newConnectionHandler = { [weak self] connection in
                guard let self, generation == self.generation else {
                    connection.cancel()
                    return
                }
                self.accept(connection)
            }
            self.listener = listener
            listener.start(queue: self.queue)

            self.queue.asyncAfter(deadline: .now() + 5) { [weak self] in
                guard let self, !didComplete, generation == self.generation else { return }
                didComplete = true
                self.stopLocked()
                completion(.failure(ProxyError.listenerFailed))
            }
        }
    }

    /// Stops the listener, cancels in-flight connections and clears the variant map.
    func stop() {
        queue.async { [weak self] in
            guard let self else { return }
            self.generation += 1
            self.stopLocked()
        }
    }

    private func stopLocked() {
        listener?.stateUpdateHandler = nil
        listener?.newConnectionHandler = nil
        listener?.cancel()
        listener = nil
        let activeConnections = Array(connections.values)
        connections.removeAll()
        for connection in activeConnections {
            connection.cancel()
        }
        variants.removeAll()
        realMasterURL = nil
        port = nil
    }

    private func accept(_ connection: NWConnection) {
        guard connections.count < Self.maxConnections else {
            connection.cancel()
            return
        }

        let id = ObjectIdentifier(connection)
        connections[id] = connection
        connection.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            switch state {
            case .failed, .cancelled:
                if self.connections[id] === connection {
                    self.connections[id] = nil
                }
            default:
                break
            }
        }
        connection.start(queue: queue)
        queue.asyncAfter(deadline: .now() + 10) { [weak self, weak connection] in
            guard
                let self,
                let connection,
                self.connections[id] === connection
            else { return }
            self.connections[id] = nil
            connection.cancel()
        }
        receive(on: connection)
    }

    private func receive(on connection: NWConnection, buffer: Data = Data()) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 4096) {
            [weak self] data, _, isComplete, error in
            guard let self else { return }
            self.queue.async {
                let id = ObjectIdentifier(connection)
                guard self.connections[id] != nil else {
                    connection.cancel()
                    return
                }

                var buffer = buffer
                if let data {
                    buffer.append(data)
                }
                if buffer.count > Self.maxRequestBytes {
                    self.respond(
                        connection,
                        status: "431 Request Header Fields Too Large"
                    )
                    return
                }
                if let headerEnd = buffer.range(of: Data("\r\n\r\n".utf8)) {
                    let header = buffer.subdata(in: 0..<headerEnd.lowerBound)
                    self.handleRequest(header: header, connection: connection)
                } else if isComplete || error != nil {
                    self.connections[id] = nil
                    connection.cancel()
                } else {
                    self.receive(on: connection, buffer: buffer)
                }
            }
        }
    }

    private func handleRequest(header: Data, connection: NWConnection) {
        guard
            let text = String(data: header, encoding: .utf8),
            let requestLine = text.components(separatedBy: "\r\n").first
        else {
            respond(connection, status: "400 Bad Request")
            return
        }

        let parts = requestLine.split(separator: " ", maxSplits: 2, omittingEmptySubsequences: true)
        guard parts.count >= 2 else {
            respond(connection, status: "400 Bad Request")
            return
        }
        guard parts[0] == "GET" else {
            respond(connection, status: "405 Method Not Allowed")
            return
        }
        guard
            let baseURL = localBaseURL,
            let url = URL(string: String(parts[1]), relativeTo: baseURL)?.absoluteURL
        else {
            respond(connection, status: "400 Bad Request")
            return
        }

        switch Self.route(for: url) {
        case .master:
            handleMaster(connection)
        case .media(let index):
            handleMedia(index: index, connection: connection)
        case nil:
            respond(connection, status: "404 Not Found")
        }
    }

    private func handleMaster(_ connection: NWConnection) {
        #if DEBUG
        print("PROXY_MASTER")
        fflush(stdout)
        #endif
        guard let realMasterURL else {
            respond(connection, status: "503 Service Unavailable")
            return
        }
        let generation = self.generation

        fetch(realMasterURL) { [weak self] result in
            guard let self else { return }
            self.queue.async {
                guard generation == self.generation else { return }
                switch result {
                case .failure:
                    self.respond(connection, status: "502 Bad Gateway")
                case .success(let fetched):
                    guard let text = String(data: fetched.data, encoding: .utf8) else {
                        self.respond(connection, status: "502 Bad Gateway")
                        return
                    }
                    let baseURL = fetched.responseURL ?? realMasterURL

                    guard text.contains("#EXT-X-STREAM-INF") else {
                        // The live URL is already a media playlist. Serve it at
                        // /master.m3u8 and expose it as variant 0 as well.
                        self.variants = [baseURL]
                        self.serveMedia(text: text, baseURL: baseURL, connection: connection)
                        return
                    }

                    guard let localMediaURL = self.localURL(path: "/media.m3u8") else {
                        self.respond(connection, status: "500 Internal Server Error")
                        return
                    }
                    let rewrite = LowLatencyPlaylistRewriter.rewriteMasterForLocalProxy(
                        text,
                        baseURL: baseURL,
                        localMediaURL: localMediaURL
                    )
                    guard
                        !rewrite.variants.isEmpty,
                        rewrite.variants.count <= Self.maxVariants
                    else {
                        // Keep the proxy URL usable but let AVPlayer follow the
                        // real variant URLs directly when the master is too large.
                        self.variants = []
                        self.respond(
                            connection,
                            contentType: Self.playlistContentType,
                            body: Data(text.utf8)
                        )
                        return
                    }
                    self.variants = rewrite.variants
                    self.respond(
                        connection,
                        contentType: Self.playlistContentType,
                        body: Data(rewrite.text.utf8)
                    )
                }
            }
        }
    }

    private func handleMedia(index: Int, connection: NWConnection) {
        guard index >= 0, index < variants.count else {
            respond(connection, status: "404 Not Found")
            return
        }
        let variantURL = variants[index]
        let generation = self.generation

        fetch(variantURL) { [weak self] result in
            guard let self else { return }
            self.queue.async {
                guard generation == self.generation else { return }
                switch result {
                case .failure:
                    self.respond(connection, status: "502 Bad Gateway")
                case .success(let fetched):
                    guard let text = String(data: fetched.data, encoding: .utf8) else {
                        self.respond(connection, status: "502 Bad Gateway")
                        return
                    }
                    let baseURL = fetched.responseURL ?? variantURL
                    self.serveMedia(text: text, baseURL: baseURL, connection: connection)
                }
            }
        }
    }

    private func serveMedia(text: String, baseURL: URL, connection: NWConnection) {
        #if DEBUG
        print("PROXY_MEDIA")
        fflush(stdout)
        #endif
        if ProcessInfo.processInfo.environment["KICK_LOW_LATENCY_NO_REWRITE"] == "1" {
            respond(
                connection,
                contentType: Self.playlistContentType,
                body: Data(text.utf8)
            )
            return
        }
        let prefetchURLs = LowLatencyPlaylistRewriter.prefetchURLs(in: text, baseURL: baseURL)
        let generation = self.generation
        availablePrefetchURLs(prefetchURLs) { [weak self] available in
            guard let self else { return }
            self.queue.async {
                guard generation == self.generation else { return }
                let rewritten = LowLatencyPlaylistRewriter.rewriteMediaMinimal(
                    text,
                    baseURL: baseURL,
                    availablePrefetchURLs: available
                )
                self.respond(
                    connection,
                    contentType: Self.playlistContentType,
                    body: Data(rewritten.utf8)
                )
            }
        }
    }

    private static func minimalPlaylistAge(text: String, prefetchCount: Int) -> TimeInterval? {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        var lastDate: Date?
        for line in text.components(separatedBy: .newlines) {
            if line.hasPrefix("#EXT-X-PROGRAM-DATE-TIME:") {
                let value = String(line.dropFirst("#EXT-X-PROGRAM-DATE-TIME:".count))
                lastDate = formatter.date(from: value)
            }
        }
        guard let lastDate else { return nil }
        let liveEdge = lastDate.addingTimeInterval(2 + 2 * Double(prefetchCount))
        return Date().timeIntervalSince(liveEdge)
    }

    private func availablePrefetchURLs(
        _ urls: [URL],
        completion: @escaping (Set<String>) -> Void
    ) {
        let candidates = Array(urls.prefix(Self.maxPrefetchChecks))
        guard !candidates.isEmpty else {
            completion([])
            return
        }

        let group = DispatchGroup()
        let lock = NSLock()
        var available = Set<String>()

        for url in candidates {
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

        group.notify(queue: queue) {
            completion(available)
        }
    }

    private func fetch(
        _ url: URL,
        completion: @escaping (Result<FetchResult, Error>) -> Void
    ) {
        var request = URLRequest(url: url)
        request.httpMethod = "GET"
        request.cachePolicy = .reloadIgnoringLocalCacheData
        request.timeoutInterval = 5
        session.dataTask(with: request) { data, response, error in
            guard
                let data,
                let httpResponse = response as? HTTPURLResponse,
                (200...299).contains(httpResponse.statusCode)
            else {
                completion(.failure(error ?? ProxyError.badResponse))
                return
            }
            completion(.success(FetchResult(data: data, responseURL: httpResponse.url)))
        }.resume()
    }

    private func respond(
        _ connection: NWConnection,
        status: String = "200 OK",
        contentType: String? = nil,
        body: Data = Data()
    ) {
        var headers = "HTTP/1.1 \(status)\r\n"
        if let contentType {
            headers += "Content-Type: \(contentType)\r\n"
        }
        headers += "Content-Length: \(body.count)\r\n"
        headers += "Cache-Control: no-store\r\n"
        headers += "Connection: close\r\n\r\n"

        var data = Data(headers.utf8)
        data.append(body)
        connection.send(
            content: data,
            contentContext: .finalMessage,
            isComplete: true,
            completion: .contentProcessed { [weak self] _ in
                guard let self else {
                    connection.cancel()
                    return
                }
                self.queue.async {
                    let id = ObjectIdentifier(connection)
                    if self.connections[id] === connection {
                        self.connections[id] = nil
                    }
                    connection.cancel()
                }
            }
        )
    }

    private var localBaseURL: URL? {
        guard let port else { return nil }
        return URL(string: "http://127.0.0.1:\(port)/")
    }

    private func localURL(path: String) -> URL? {
        guard let port else { return nil }
        return URL(string: "http://127.0.0.1:\(port)\(path)")
    }
}
