import Foundation
import XCTest
@testable import KickPlayerCBridge

final class CookieJSONImporterTests: XCTestCase {
    func testImportsOnlyKickCookiesApplicableToTheSessionRoute() throws {
        let fixture = #"""
        [
          {"domain":"kick.com","path":"/","expirationDate":2000,"name":"session_fixture","value":"fixture_a"},
          {"domain":".kick.com","path":"/api","name":"scope_fixture","value":"fixture_b"},
          {"domain":"kick.com","path":"/","name":"region:fixture","value":"safe"},
          {"domain":"elsewhere.invalid","path":"/","name":"other_fixture","value":"other_domain_fixture"},
          {"domain":"kick.com","path":"/api/v2","name":"wrong_path_fixture","value":"wrong_path"},
          {"domain":"kick.com","path":"/","expirationDate":1,"name":"expired_fixture","value":"expired_value"}
        ]
        """#

        let header = try CookieJSONImporter.cookieHeader(
            from: Data(fixture.utf8),
            now: Date(timeIntervalSince1970: 1_000)
        )

        XCTAssertEqual(header, "session_fixture=fixture_a; scope_fixture=fixture_b; region:fixture=safe")
        XCTAssertFalse(header.contains("other_domain_fixture"))
        XCTAssertFalse(header.contains("wrong_path"))
        XCTAssertFalse(header.contains("expired_value"))
    }

    func testRejectsMalformedCookiePairAndJSON() {
        let injected = #"[{"domain":"kick.com","path":"/","name":"cookie","value":"one; injected=two"}]"#
        XCTAssertThrowsError(try CookieJSONImporter.cookieHeader(from: Data(injected.utf8)))

        XCTAssertThrowsError(try CookieJSONImporter.cookieHeader(from: Data("not-json".utf8)))
    }

    func testKeepsBlankValuesAndAppliesCookiePathScope() throws {
        let fixture = #"""
        [
          {"domain":"kick.com","hostOnly":true,"path":"/","name":"blank_fixture","value":""},
          {"domain":".kick.com","hostOnly":false,"path":"/api/v1","expirationDate":2000,"name":"scoped_fixture","value":"safe_value"}
        ]
        """#

        let header = try CookieJSONImporter.cookieHeader(
            from: Data(fixture.utf8),
            now: Date(timeIntervalSince1970: 1_000)
        )

        XCTAssertEqual(header, "blank_fixture=; scoped_fixture=safe_value")
    }

    func testRejectsOversizedJSONAndNoMatchingDomain() {
        let oversized = Data(repeating: 0x20, count: CookieJSONImporter.maximumJSONBytes + 1)
        XCTAssertThrowsError(try CookieJSONImporter.cookieHeader(from: oversized))

        let unrelated = #"[{"domain":"elsewhere.invalid","path":"/","name":"cookie","value":"fixture"}]"#
        XCTAssertThrowsError(try CookieJSONImporter.cookieHeader(from: Data(unrelated.utf8)))
    }
}
