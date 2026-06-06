import XCTest
@testable import TVOSNetPlayerCacheClient

final class CacheLibraryItemTests: XCTestCase {
    func testPrimaryVariantSkipsUnsupportedProtocolsAndEmptyIDs() {
        let item = CacheLibraryItem.fixture(
            variants: [
                .fixture(id: "", playbackProtocol: "httpFile"),
                .fixture(id: "dash", playbackProtocol: "dash"),
                .fixture(id: "original", playbackProtocol: "HTTP_FILE"),
            ]
        )

        XCTAssertEqual(item.primaryVariantID, "original")
        XCTAssertTrue(item.hasPlayableVariant)
    }

    func testItemWithoutSupportedProtocolIsNotPlayable() {
        let item = CacheLibraryItem.fixture(
            variants: [
                .fixture(id: "unspecified", playbackProtocol: "PLAYBACK_PROTOCOL_UNSPECIFIED"),
                .fixture(id: "dash", playbackProtocol: "dash"),
            ]
        )

        XCTAssertNil(item.primaryVariantID)
        XCTAssertFalse(item.hasPlayableVariant)
    }
}

extension CacheLibraryItem {
    fileprivate static func fixture(variants: [CacheMediaVariant]) -> Self {
        Self(
            id: "item-1",
            title: "Cached video",
            subtitle: "",
            source: "localCache",
            sourceID: "item-1",
            posterURI: "",
            variants: variants
        )
    }
}

extension CacheMediaVariant {
    fileprivate static func fixture(id: String, playbackProtocol: String) -> Self {
        Self(
            id: id,
            label: "Original",
            playbackProtocol: playbackProtocol,
            container: "mp4",
            videoCodec: "",
            audioCodec: "",
            width: 1920,
            height: 1080,
            bitrate: 0,
            sizeBytes: 0
        )
    }
}
