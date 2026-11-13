import Foundation
import XCTest
import TVOSNetPlayerCacheClient
@testable import TVOSNetPlayerCore

final class HLSPlaybackReaderURLBindingTests: XCTestCase {
    private let readerID = "00000000-0000-4000-8000-0000000000a1"

    func testAddsBindingToMatchingMasterAndPreservesEncodedQueryItems() throws {
        let originalText =
            "https://cache.local/proxy/api/hls/session-1/master.m3u8?token=a%2Fb&flag&empty=&q%20name=value%2Bplus"
        let original = try XCTUnwrap(URL(string: originalText))
        let bound = try XCTUnwrap(
            HLSPlaybackReaderURLBinding.applying(
                to: original,
                owner: try owner(),
                server: server(supportsReaderBinding: true)
            )
        )

        XCTAssertEqual(
            URLComponents(url: bound, resolvingAgainstBaseURL: false)?.percentEncodedQuery,
            "token=a%2Fb&flag&empty=&q%20name=value%2Bplus&playback_reader=\(readerID)"
        )
        XCTAssertEqual(original.absoluteString, originalText)
        XCTAssertEqual(
            URLComponents(url: bound, resolvingAgainstBaseURL: false)?.percentEncodedPath,
            "/proxy/api/hls/session-1/master.m3u8")
    }

    func testEncodedExactSessionSegmentIsRecognizedWithoutDecodingSeparators() throws {
        let encodedSession = try XCTUnwrap(URL(string: "https://cache.local/hls/session%2D1/master.m3u8"))
        let bound = try XCTUnwrap(
            HLSPlaybackReaderURLBinding.applying(
                to: encodedSession,
                owner: try owner(),
                server: server(supportsReaderBinding: true)
            )
        )
        XCTAssertEqual(URLComponents(url: bound, resolvingAgainstBaseURL: false)?.queryItems?.last?.value, readerID)

        let encodedSeparator = try XCTUnwrap(
            URL(string: "https://cache.local/hls/session-1%2F..%2Fsession-1/master.m3u8")
        )
        XCTAssertNil(
            HLSPlaybackReaderURLBinding.applying(
                to: encodedSeparator,
                owner: try owner(),
                server: server(supportsReaderBinding: true)
            )
        )
    }

    func testSameReaderIsIdempotentAndConflictingOrDuplicateReadersFailClosed() throws {
        let alreadyBound = try XCTUnwrap(
            URL(string: "https://cache.local/hls/session-1/master.m3u8?keep=yes&playback_reader=\(readerID)")
        )
        XCTAssertEqual(
            HLSPlaybackReaderURLBinding.applying(
                to: alreadyBound,
                owner: try owner(),
                server: server(supportsReaderBinding: true)
            ),
            alreadyBound
        )

        let conflictingURLs = [
            "https://cache.local/hls/session-1/master.m3u8?playback_reader=00000000-0000-4000-8000-0000000000a2",
            "https://cache.local/hls/session-1/master.m3u8?playback_reader=not-a-uuid",
            "https://cache.local/hls/session-1/master.m3u8?playback_reader=\(readerID)&playback_reader=\(readerID)",
            "https://cache.local/hls/session-1/master.m3u8?playback_reader",
        ]
        for text in conflictingURLs {
            let url = try XCTUnwrap(URL(string: text))
            XCTAssertNil(
                HLSPlaybackReaderURLBinding.applying(
                    to: url,
                    owner: try owner(),
                    server: server(supportsReaderBinding: true)
                ),
                text
            )
        }
    }

    func testUnsupportedServerReturnsOriginalURLWithoutBinding() throws {
        let original = try XCTUnwrap(
            URL(string: "https://cache.local/hls/session-1/master.m3u8?playback_reader=foreign")
        )
        XCTAssertEqual(
            HLSPlaybackReaderURLBinding.applying(
                to: original,
                owner: try owner(),
                server: server(supportsReaderBinding: false)
            ),
            original
        )
    }

    func testCapability22RejectsWrongOwnerOrUnrecognizedURLs() throws {
        let inputs = [
            "https://cache.local/hls/other-session/master.m3u8",
            "https://cache.local/api/videos/session-1/master.m3u8",
            "https://cache.local/hls/session-1/segment.m3u8",
            "ftp://cache.local/hls/session-1/master.m3u8",
            "https://user:password@cache.local/hls/session-1/master.m3u8",
            "https://cache.local/hls/session-1/master.m3u8#fragment",
            "https://cache.local/hls/session-1%2F..%2Fother/master.m3u8",
        ]
        for text in inputs {
            let url = try XCTUnwrap(URL(string: text))
            XCTAssertNil(
                HLSPlaybackReaderURLBinding.applying(
                    to: url,
                    owner: try owner(),
                    server: server(supportsReaderBinding: true)
                ),
                text
            )
        }

        let masterURL = try XCTUnwrap(URL(string: "https://cache.local/hls/session-1/master.m3u8"))
        XCTAssertNil(
            HLSPlaybackReaderURLBinding.applying(
                to: masterURL,
                owner: try owner(),
                server: server(id: "other", supportsReaderBinding: true)
            )
        )

        let nonV4Owner = try XCTUnwrap(
            HLSPlaybackContextOwner(
                ownerToken: XCTUnwrap(UUID(uuidString: "00000000-0000-3000-8000-0000000000a1")),
                serverID: "server",
                sessionID: "session-1",
                sessionGeneration: 1
            )
        )
        XCTAssertNil(
            HLSPlaybackReaderURLBinding.applying(
                to: try XCTUnwrap(URL(string: "https://cache.local/hls/session-1/master.m3u8")),
                owner: nonV4Owner,
                server: server(supportsReaderBinding: true)
            )
        )
    }

    private func owner() throws -> HLSPlaybackContextOwner {
        try XCTUnwrap(
            HLSPlaybackContextOwner(
                ownerToken: XCTUnwrap(UUID(uuidString: readerID)),
                serverID: "server",
                sessionID: "session-1",
                sessionGeneration: 1
            )
        )
    }

    private func server(id: String = "server", supportsReaderBinding: Bool) -> CacheServerSummary {
        CacheServerSummary(
            id: id,
            name: "Cache",
            version: "test",
            mediaBaseURIs: [],
            capabilities: supportsReaderBinding ? [CacheServerCapability.hlsPlaybackReaderBinding] : []
        )
    }
}
