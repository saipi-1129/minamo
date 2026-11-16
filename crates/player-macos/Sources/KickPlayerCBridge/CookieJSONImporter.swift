import Foundation

enum CookieJSONImportError: Error, Equatable {
    case tooLarge
    case tooManyCookies
    case malformed
    case noMatchingKickCookies
    case invalidCookie
}

/// Converts a browser cookie-export array into one Cookie header for kick.com.
/// Values from other domains, unrelated paths, and expired rows are not decoded into Strings.
enum CookieJSONImporter {
    static let maximumJSONBytes = 1 * 1024 * 1024
    private static let maximumCookieCount = 512
    private static let maximumHeaderBytes = 8 * 1024
    private static let nowKey = CodingUserInfoKey(rawValue: "cookieJSONImportNow")!

    static func cookieHeader(from data: Data, now: Date = Date()) throws -> String {
        guard data.count <= maximumJSONBytes else { throw CookieJSONImportError.tooLarge }

        let decoder = JSONDecoder()
        decoder.userInfo[nowKey] = now
        var records: [ExportedCookie]
        do {
            records = try decoder.decode([ExportedCookie].self, from: data)
        } catch {
            throw CookieJSONImportError.malformed
        }
        guard records.count <= maximumCookieCount else {
            throw CookieJSONImportError.tooManyCookies
        }

        var header = ""
        var importedAnyCookie = false
        var succeeded = false
        defer {
            for index in records.indices {
                records[index].clearValue()
            }
            if !succeeded {
                header.removeAll(keepingCapacity: false)
            }
        }

        for index in records.indices {
            guard var value = records[index].value else { continue }
            let name = records[index].name ?? ""
            guard isValidCookieName(name), isValidCookieValue(value) else {
                value.removeAll(keepingCapacity: false)
                throw CookieJSONImportError.invalidCookie
            }

            let separatorBytes = header.isEmpty ? 0 : 2
            let pairBytes = name.utf8.count + 1 + value.utf8.count
            guard header.utf8.count + separatorBytes + pairBytes <= maximumHeaderBytes else {
                value.removeAll(keepingCapacity: false)
                throw CookieJSONImportError.tooLarge
            }

            if !header.isEmpty { header.append("; ") }
            header.append(name)
            header.append("=")
            header.append(value)
            value.removeAll(keepingCapacity: false)
            records[index].clearValue()
            importedAnyCookie = true
        }

        guard importedAnyCookie else {
            throw CookieJSONImportError.noMatchingKickCookies
        }
        succeeded = true
        return header
    }

    private static func isEligible(domain: String, path: String, expirationDate: Double?, now: Date) -> Bool {
        let normalizedDomain = domain.lowercased()
        guard normalizedDomain == "kick.com" || normalizedDomain == ".kick.com" else {
            return false
        }
        guard pathMatchesKickUserRoute(path) else { return false }
        if let expirationDate, expirationDate <= now.timeIntervalSince1970 {
            return false
        }
        return true
    }

    private static func pathMatchesKickUserRoute(_ cookiePath: String) -> Bool {
        guard cookiePath.hasPrefix("/") else { return false }
        let requestPath = "/api/v1/user"
        if cookiePath == requestPath { return true }
        guard requestPath.hasPrefix(cookiePath) else { return false }
        return cookiePath.hasSuffix("/") || requestPath.dropFirst(cookiePath.count).first == "/"
    }

    private static func isValidCookieName(_ name: String) -> Bool {
        !name.isEmpty && name.utf8.allSatisfy { byte in
            (0x30...0x39).contains(byte)
                || (0x41...0x5a).contains(byte)
                || (0x61...0x7a).contains(byte)
                || isCookieNameSymbol(byte)
        }
    }

    private static func isCookieNameSymbol(_ byte: UInt8) -> Bool {
        switch byte {
        case 0x21, 0x23...0x27, 0x2a, 0x2b, 0x2d, 0x2e, 0x3a, 0x5e, 0x5f, 0x60, 0x7c, 0x7e:
            true
        default:
            false
        }
    }

    private static func isValidCookieValue(_ value: String) -> Bool {
        let bytes = Array(value.utf8)
        let cookieOctets: ArraySlice<UInt8>
        if bytes.count >= 2, bytes.first == 0x22 {
            guard bytes.last == 0x22 else { return false }
            cookieOctets = bytes.dropFirst().dropLast()
        } else {
            cookieOctets = bytes[...]
        }
        return cookieOctets.allSatisfy(isCookieOctet)
    }

    private static func isCookieOctet(_ byte: UInt8) -> Bool {
        byte == 0x21
            || (0x23...0x2b).contains(byte)
            || (0x2d...0x3a).contains(byte)
            || (0x3c...0x5b).contains(byte)
            || (0x5d...0x7e).contains(byte)
    }

    private struct ExportedCookie: Decodable {
        private enum CodingKeys: String, CodingKey {
            case domain
            case expirationDate
            case name
            case path
            case value
        }

        let domain: String
        let path: String
        let expirationDate: Double?
        private(set) var name: String?
        private(set) var value: String?

        init(from decoder: Decoder) throws {
            let container = try decoder.container(keyedBy: CodingKeys.self)
            domain = try container.decode(String.self, forKey: .domain)
            path = try container.decodeIfPresent(String.self, forKey: .path) ?? "/"
            expirationDate = try container.decodeIfPresent(Double.self, forKey: .expirationDate)

            let now = decoder.userInfo[CookieJSONImporter.nowKey] as? Date ?? Date()
            guard CookieJSONImporter.isEligible(
                domain: domain,
                path: path,
                expirationDate: expirationDate,
                now: now
            ) else {
                name = nil
                value = nil
                return
            }

            name = try container.decode(String.self, forKey: .name)
            value = try container.decode(String.self, forKey: .value)
        }

        mutating func clearValue() {
            value?.removeAll(keepingCapacity: false)
            value = nil
        }
    }
}
