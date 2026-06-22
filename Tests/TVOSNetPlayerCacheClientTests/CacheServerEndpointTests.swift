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
        XCTAssertEqual(endpoint?.displayAddress, "mac-mini.local:50052")
        XCTAssertEqual(endpoint?.usesTLS, false)
    }

    func testNormalizesHTTPURLWithDefaultCachePort() {
        let endpoint = CacheServerEndpoint.normalized(from: "http://mac-mini.local")

        XCTAssertEqual(endpoint, CacheServerEndpoint(host: "mac-mini.local", port: 50_051))
        XCTAssertEqual(endpoint?.displayAddress, "mac-mini.local:50051")
        XCTAssertEqual(endpoint?.usesTLS, false)
    }

    func testNormalizesHTTPSURLWithDefaultTLSPort() {
        let endpoint = CacheServerEndpoint.normalized(from: "https://cache.example.com")

        XCTAssertEqual(
            endpoint,
            CacheServerEndpoint(host: "cache.example.com", port: 443, scheme: .https)
        )
        XCTAssertEqual(endpoint?.displayAddress, "https://cache.example.com")
        XCTAssertEqual(endpoint?.usesTLS, true)
    }

    func testHTTPSInitializerDefaultsToTLSPort() throws {
        let endpoint = try XCTUnwrap(CacheServerEndpoint(host: "cache.example.com", scheme: .https))

        XCTAssertEqual(endpoint.port, 443)
        XCTAssertEqual(endpoint.displayAddress, "https://cache.example.com")
        XCTAssertEqual(endpoint.usesTLS, true)
    }

    func testNormalizesHTTPSURLWithExplicitPort() {
        let endpoint = CacheServerEndpoint.normalized(from: "https://cache.example.com:8443")

        XCTAssertEqual(
            endpoint,
            CacheServerEndpoint(host: "cache.example.com", port: 8_443, scheme: .https)
        )
        XCTAssertEqual(endpoint?.displayAddress, "https://cache.example.com:8443")
        XCTAssertEqual(endpoint?.usesTLS, true)
    }

    func testNormalizesHTTPSURLWithTrailingSlash() {
        let endpoint = CacheServerEndpoint.normalized(from: "https://cache.example.com/")

        XCTAssertEqual(
            endpoint,
            CacheServerEndpoint(host: "cache.example.com", port: 443, scheme: .https)
        )
        XCTAssertEqual(endpoint?.displayAddress, "https://cache.example.com")
    }

    func testRejectsHTTPSBracketedIPv6URL() {
        XCTAssertNil(CacheServerEndpoint.normalized(from: "https://[fd00::1]:8443"))
    }

    func testRejectsHTTPSIPv4URL() {
        XCTAssertNil(CacheServerEndpoint.normalized(from: "https://192.168.1.10"))
    }

    func testPlaintextIPv6EndpointUsesIPv6GRPCTarget() {
        let endpoint = CacheServerEndpoint(host: "fd00::1", port: 50_051)

        XCTAssertEqual(endpoint.grpcTargetKind, .ipv6Literal)
    }

    func testHTTPSInitializerRejectsIPLiteralHosts() {
        XCTAssertNil(CacheServerEndpoint(host: "fd00::1", scheme: .https))
        XCTAssertNil(CacheServerEndpoint(host: "192.168.1.10", scheme: .https))
        XCTAssertNil(CacheServerEndpoint(host: "fd00::1", port: 443, scheme: .https))
    }

    func testDefaultHTTPSUsesHostOnlyDNSTargetPort() throws {
        let endpoint = try XCTUnwrap(CacheServerEndpoint(host: "cache.example.com", scheme: .https))

        XCTAssertEqual(endpoint.grpcTargetKind, .dns)
        XCTAssertNil(endpoint.grpcDNSTargetPort)
    }

    func testExplicitHTTPSPortUsesExplicitDNSTargetPort() throws {
        let endpoint = try XCTUnwrap(CacheServerEndpoint(host: "cache.example.com", port: 8_443, scheme: .https))

        XCTAssertEqual(endpoint.grpcTargetKind, .dns)
        XCTAssertEqual(endpoint.grpcDNSTargetPort, 8_443)
    }

    func testPlaintextDNSHostUsesExplicitDefaultDNSTargetPort() {
        let endpoint = CacheServerEndpoint(host: "mac-mini.local")

        XCTAssertEqual(endpoint.grpcTargetKind, .dns)
        XCTAssertEqual(endpoint.grpcDNSTargetPort, 50_051)
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
        XCTAssertNil(CacheServerEndpoint.normalized(from: "ftp://mac-mini.local:50051"))
    }

    func testRejectsPathScopedURL() {
        XCTAssertNil(CacheServerEndpoint.normalized(from: "https://cache.example.com/grpc"))
    }

    func testRejectsQueryScopedURL() {
        XCTAssertNil(CacheServerEndpoint.normalized(from: "https://cache.example.com?token=ignored"))
    }

    func testRejectsInvalidPort() {
        XCTAssertNil(CacheServerEndpoint.normalized(from: "mac-mini.local:70000"))
        XCTAssertNil(CacheServerEndpoint.normalized(from: "https://cache.example.com:999999999999999999999"))
        XCTAssertNil(CacheServerEndpoint.normalized(from: "https://cache.example.com:not-a-port"))
    }

    func testRejectsExplicitEmptyPort() {
        XCTAssertNil(CacheServerEndpoint.normalized(from: "http://mac-mini.local:"))
        XCTAssertNil(CacheServerEndpoint.normalized(from: "https://cache.example.com:"))
    }
}
