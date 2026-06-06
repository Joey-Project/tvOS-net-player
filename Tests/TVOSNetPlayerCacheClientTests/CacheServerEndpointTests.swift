import XCTest
@testable import TVOSNetPlayerCacheClient

final class CacheServerEndpointTests: XCTestCase {
    func testNormalizesBareHostWithDefaultPort() {
        let endpoint = CacheServerEndpoint.normalized(from: "mac-mini.local")

        XCTAssertEqual(endpoint, CacheServerEndpoint(host: "mac-mini.local", port: 50_051))
        XCTAssertEqual(endpoint?.displayAddress, "mac-mini.local:50051")
    }

    func testNormalizesHostAndPort() {
        let endpoint = CacheServerEndpoint.normalized(from: " 192.168.1.10:6000 ")

        XCTAssertEqual(endpoint, CacheServerEndpoint(host: "192.168.1.10", port: 6000))
        XCTAssertEqual(endpoint?.displayAddress, "192.168.1.10:6000")
    }

    func testNormalizesHTTPURL() {
        let endpoint = CacheServerEndpoint.normalized(from: "http://mac-mini.local:50052")

        XCTAssertEqual(endpoint, CacheServerEndpoint(host: "mac-mini.local", port: 50_052))
    }

    func testNormalizesBracketedIPv6Host() {
        let endpoint = CacheServerEndpoint.normalized(from: "[fd00::1]:6000")

        XCTAssertEqual(endpoint, CacheServerEndpoint(host: "fd00::1", port: 6000))
        XCTAssertEqual(endpoint?.displayAddress, "[fd00::1]:6000")
        XCTAssertEqual(endpoint?.isIPv6Literal, true)
    }

    func testNormalizesBareIPv6HostWithDefaultPort() {
        let endpoint = CacheServerEndpoint.normalized(from: "fd00::1")

        XCTAssertEqual(endpoint, CacheServerEndpoint(host: "fd00::1", port: 50_051))
        XCTAssertEqual(endpoint?.displayAddress, "[fd00::1]:50051")
        XCTAssertEqual(endpoint?.isIPv6Literal, true)
    }

    func testRejectsUnsupportedScheme() {
        XCTAssertNil(CacheServerEndpoint.normalized(from: "https://mac-mini.local:50051"))
    }

    func testRejectsInvalidPort() {
        XCTAssertNil(CacheServerEndpoint.normalized(from: "mac-mini.local:70000"))
    }
}
