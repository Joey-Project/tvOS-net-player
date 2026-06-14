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
        XCTAssertEqual(item.primaryVariant?.id, "original")
        XCTAssertTrue(item.hasPlayableVariant)
    }

    func testPrimaryVariantReturnsPlayableVariantForDisplay() {
        let item = CacheLibraryItem.fixture(
            variants: [
                .fixture(id: "dash", label: "DASH", playbackProtocol: "dash"),
                .fixture(id: "hls", label: "HLS", playbackProtocol: "hls"),
            ]
        )

        XCTAssertEqual(item.primaryVariant?.displayLabel, "HLS")
        XCTAssertEqual(item.primaryVariantID, "hls")
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

    func testPlaybackSourceAcceptsExplicitHTTPPlaybackURLs() {
        let cases = [
            (
                "http://mac-mini.local:8080/media/item-1/original",
                "http://mac-mini.local:8080/media/item-1/original"
            ),
            (
                " https://mac-mini.local/cache/item-1/original.m3u8 ",
                "https://mac-mini.local/cache/item-1/original.m3u8"
            ),
        ]

        for (uri, expectedURL) in cases {
            let source = CachePlaybackSource.fixture(uri: uri)

            XCTAssertEqual(source.explicitHTTPURL?.absoluteString, expectedURL)
        }
    }

    func testPlaybackSourceRejectsRelativeAndSchemelessPlaybackURLs() {
        let invalidURIs = [
            "mac-mini.local:8080/media/item-1/original",
            "media/item-1/original",
            "/media/item-1/original",
            "file:///tmp/movie.mp4",
            "http:///media/item-1/original",
        ]

        for uri in invalidURIs {
            let source = CachePlaybackSource.fixture(uri: uri)

            XCTAssertNil(source.explicitHTTPURL, "Expected \(uri) to be rejected.")
        }
    }

    func testProgressivePlaybackTaskExposesPlayableHLSSourceAndMetadata() {
        let task = CacheTask(
            id: "bilibili-playback-1",
            kind: "TASK_KIND_BILIBILI_PROGRESSIVE_PLAYBACK",
            state: "TASK_STATE_PLAYABLE",
            source: "BV1planned",
            title: "Planned video",
            progress: 0,
            message: "Bilibili playback session is playable.",
            libraryItemID: "",
            playbackSource: CachePlaybackSource(
                itemID: "bilibili-playback-1",
                variantID: "h264",
                playbackProtocol: "PLAYBACK_PROTOCOL_HLS",
                uri: "http://mac-mini.local:8080/hls/bilibili-playback-1/master.m3u8"
            ),
            playbackSession: CacheBilibiliPlaybackSession(
                id: "bilibili-playback-1",
                title: "Planned video",
                contentID: "BV1planned-cid1",
                selectedVariantID: "h264",
                selectedVariant: CacheBilibiliPlaybackVariant.fixture(id: "h264"),
                variants: [
                    .fixture(id: "h264"),
                    .fixture(id: "hevc", videoCodec: "hvc1.1.6.L120.90"),
                ]
            )
        )

        XCTAssertTrue(task.isProgressivePlayback)
        XCTAssertTrue(task.playbackSource?.isPlayableByTVOSClient == true)
        XCTAssertEqual(
            task.playbackSource?.explicitHTTPURL?.absoluteString,
            "http://mac-mini.local:8080/hls/bilibili-playback-1/master.m3u8"
        )
        XCTAssertEqual(task.playbackSession?.selectedVariant?.videoCodec, "avc1.640028")
        XCTAssertEqual(task.playbackSession?.variants.map(\.id), ["h264", "hevc"])
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
    fileprivate static func fixture(id: String, label: String = "Original", playbackProtocol: String) -> Self {
        Self(
            id: id,
            label: label,
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
    fileprivate static func fixture(
        playbackProtocol: String = "httpFile",
        uri: String = "http://mac-mini.local:8080/media/item-1/original"
    ) -> Self {
        Self(
            itemID: "item-1",
            variantID: "original",
            playbackProtocol: playbackProtocol,
            uri: uri
        )
    }
}

extension CacheBilibiliPlaybackVariant {
    fileprivate static func fixture(
        id: String,
        videoCodec: String = "avc1.640028"
    ) -> Self {
        Self(
            id: id,
            label: "1920x1080",
            sourceKind: "dash",
            container: "mp4",
            videoCodec: videoCodec,
            audioCodec: "mp4a.40.2",
            width: 1920,
            height: 1080,
            bitrate: 1_000_000,
            sizeBytes: 10_000_000
        )
    }
}
