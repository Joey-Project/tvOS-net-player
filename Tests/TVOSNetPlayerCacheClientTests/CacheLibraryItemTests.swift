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

    func testHLSVariantProtocolsArePlayable() {
        let protocolNames = ["hls", "HLS", "PLAYBACK_PROTOCOL_HLS"]

        for protocolName in protocolNames {
            let item = CacheLibraryItem.fixture(
                variants: [
                    .fixture(id: "hls-variant", playbackProtocol: protocolName)
                ]
            )

            XCTAssertEqual(item.primaryVariantID, "hls-variant", "Expected \(protocolName) to be playable.")
            XCTAssertTrue(item.hasPlayableVariant, "Expected \(protocolName) to be playable.")
        }
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

    func testPlaybackSourceSupportsHTTPFileAndHLSProtocols() {
        let protocolNames = ["httpFile", "PLAYBACK_PROTOCOL_HTTP_FILE", "hls", "PLAYBACK_PROTOCOL_HLS"]

        for protocolName in protocolNames {
            let source = CachePlaybackSource.fixture(playbackProtocol: protocolName)
            XCTAssertTrue(source.isPlayableByTVOSClient, "Expected \(protocolName) to be playable.")
        }
    }

    func testPlaybackSourceRejectsUnsupportedProtocol() {
        let source = CachePlaybackSource.fixture(playbackProtocol: "PLAYBACK_PROTOCOL_UNSPECIFIED")

        XCTAssertFalse(source.isPlayableByTVOSClient)
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

extension CachePlaybackSource {
    fileprivate static func fixture(playbackProtocol: String) -> Self {
        Self(
            itemID: "item-1",
            variantID: "original",
            playbackProtocol: playbackProtocol,
            uri: "http://mac-mini.local:8080/media/item-1/original"
        )
    }
}
