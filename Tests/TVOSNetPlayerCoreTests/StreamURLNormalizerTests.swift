import XCTest
@testable import TVOSNetPlayerCore

final class StreamURLNormalizerTests: XCTestCase {
    func testNormalizesBareHostAsHTTPURL() {
        let url = StreamURLNormalizer.normalizedHTTPURL(from: "192.168.1.10:8080/video.mp4")

        XCTAssertEqual(url?.absoluteString, "http://192.168.1.10:8080/video.mp4")
    }

    func testAcceptsHTTPSURL() {
        let url = StreamURLNormalizer.normalizedHTTPURL(from: " https://example.com/movie.m3u8 ")

        XCTAssertEqual(url?.absoluteString, "https://example.com/movie.m3u8")
    }

    func testRejectsUnsupportedSchemes() {
        XCTAssertNil(StreamURLNormalizer.normalizedHTTPURL(from: "file:///tmp/movie.mp4"))
    }
}
