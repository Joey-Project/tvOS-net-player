import Foundation
import TaskWatchUIFixture
import XCTest

final class FixtureConfigurationTests: XCTestCase {
    func testAcceptsRustOriginalMediaRoute() throws {
        let sample = "http://127.0.0.1:8080/media/local.default.Rml4dHVyZSAwNTUubXA0/original"
        let url = try FixtureConfiguration.validatedMediaURL(sample)
        XCTAssertEqual(url.absoluteString, sample)
    }

    func testAcceptsLoopbackMP4Route() throws {
        let url = try FixtureConfiguration.validatedMediaURL("http://localhost:39123/fixture.mp4")
        XCTAssertEqual(url.path, "/fixture.mp4")
    }

    func testRejectsUnsafeMediaOriginsAndDecorations() {
        let rejected = [
            "http://example.com/original",
            "https://127.0.0.1:39123/original",
            "http://127.0.0.1/original",
            "http://user:secret@127.0.0.1:39123/original",
            "http://127.0.0.1:39123/media/local.default.Rml4dHVyZSAwNTUubXA0/original?token=secret",
            "http://127.0.0.1:39123/media/local.default.bad%2Ftoken/original",
            "http://127.0.0.1:39123/media/local.default.invalid@base64/original",
            "http://127.0.0.1:39123/not-original",
        ]
        for value in rejected {
            XCTAssertThrowsError(try FixtureConfiguration.validatedMediaURL(value), value)
        }
    }

    func testSourceResolverOnlyAcceptsRecognizedBilibiliInputs() throws {
        XCTAssertEqual(try FixtureSourceResolver.scenario(for: "BV1QtjA6BEB8"), .automatic)
        XCTAssertEqual(
            try FixtureSourceResolver.scenario(for: "https://www.bilibili.com/video/BV1uW4y1s7zN/?p=1"),
            .manual
        )
        XCTAssertEqual(try FixtureSourceResolver.scenario(for: FixtureScenario.silent.source), .silent)
        XCTAssertEqual(
            try FixtureSourceResolver.scenario(for: FixtureScenario.resumption.source),
            .resumption
        )
        XCTAssertThrowsError(try FixtureSourceResolver.scenario(for: "BV1unknown"))
        XCTAssertThrowsError(try FixtureSourceResolver.scenario(for: "BV1xK4y1P7mX"))
        XCTAssertThrowsError(try FixtureSourceResolver.scenario(for: "https://example.com/video/BV1QtjA6BEB8"))
    }
}
