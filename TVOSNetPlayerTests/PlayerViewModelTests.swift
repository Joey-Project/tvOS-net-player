import XCTest
@testable import TVOSNetPlayer

final class PlayerViewModelTests: XCTestCase {
    func testNormalizesBareHostAsHTTPURL() {
        let url = PlayerViewModel.normalizedHTTPURL(from: "192.168.1.10:8080/video.mp4")

        XCTAssertEqual(url?.absoluteString, "http://192.168.1.10:8080/video.mp4")
    }

    func testAcceptsHTTPSURL() {
        let url = PlayerViewModel.normalizedHTTPURL(from: " https://example.com/movie.m3u8 ")

        XCTAssertEqual(url?.absoluteString, "https://example.com/movie.m3u8")
    }

    func testRejectsUnsupportedSchemes() {
        XCTAssertNil(PlayerViewModel.normalizedHTTPURL(from: "file:///tmp/movie.mp4"))
    }
}
