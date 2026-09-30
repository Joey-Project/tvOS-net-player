import XCTest
@testable import TVOSNetPlayerCacheClient

final class GRPCCacheControlClientTests: XCTestCase {
    func testLoginStartDeadlineExceedsServerTicketTimeout() {
        let client = GRPCCacheControlClient(
            endpoint: CacheServerEndpoint(host: "localhost"),
            rpcTimeout: .seconds(10)
        )

        XCTAssertEqual(client.loginStartCallOptions.timeout, .seconds(20))
    }

    func testLoginStartDeadlineDoesNotShortenLongerConfiguredTimeout() {
        let client = GRPCCacheControlClient(
            endpoint: CacheServerEndpoint(host: "localhost"),
            rpcTimeout: .seconds(30)
        )

        XCTAssertEqual(client.loginStartCallOptions.timeout, .seconds(30))
    }
}
