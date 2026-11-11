import XCTest
@testable import TVOSNetPlayerCacheClient

final class HLSRenditionCatalogTests: XCTestCase {
    func testRequestAndDescriptorValidation() throws {
        XCTAssertNoThrow(try HLSRenditionCatalogRequest(sessionID: "session"))
        for sessionID in ["", "\u{00A0}session", "session\u{2003}", "ses\u{0085}sion"] {
            XCTAssertThrowsError(try HLSRenditionCatalogRequest(sessionID: sessionID))
        }
        XCTAssertThrowsError(try HLSRenditionCatalogRequest(sessionID: String(repeating: "s", count: 257)))

        XCTAssertNoThrow(
            try descriptor(
                variantID: "v1",
                videoResourceCodecs: "avc1.640028 , mp4a.40.2",
                videoResourceMimeType: "video/mp4"
            ))
        for codecs in ["", " avc1", "avc1 ", ",avc1", "avc1,,mp4a", "avc1 mp4a", "https://x"] {
            XCTAssertThrowsError(try descriptor(variantID: "v", videoResourceCodecs: codecs))
        }
        for mimeType in ["", " video/mp4", "video/mp4 ", "video", "/mp4", "video/", "video/mp4; codecs=x", "https://x"]
        {
            XCTAssertThrowsError(try descriptor(variantID: "v", videoResourceMimeType: mimeType))
        }

        XCTAssertThrowsError(try descriptor(variantID: "v", advertisedBandwidthBitsPerSecond: 0))
        XCTAssertThrowsError(try descriptor(variantID: "v", advertisedWidth: 1920))
        XCTAssertThrowsError(try descriptor(variantID: "v", advertisedWidth: 0, advertisedHeight: 1080))
        XCTAssertThrowsError(
            try descriptor(
                variantID: "v",
                hasSeparateAudio: false,
                audioResourceBandwidthBitsPerSecond: 1
            ))
        XCTAssertThrowsError(
            try descriptor(
                variantID: "v",
                hasSeparateAudio: true,
                audioResourceBandwidthBitsPerSecond: 0
            ))
        XCTAssertThrowsError(
            try descriptor(
                variantID: "v",
                videoResourceBandwidthBitsPerSecond: 0
            ))
    }

    func testUTF8IdentityForEqualityDeduplicationAndLookup() throws {
        let nfc = "caf\u{00E9}"
        let nfd = "cafe\u{0301}"
        let nfcRendition = try descriptor(variantID: nfc)
        let nfdRendition = try descriptor(variantID: nfd)
        XCTAssertNotEqual(nfcRendition, nfdRendition)

        let request = try HLSRenditionCatalogRequest(sessionID: "session")
        let catalog = try HLSRenditionCatalog(
            sessionID: request.sessionID,
            sessionGeneration: 1,
            renditions: [nfcRendition, nfdRendition]
        )
        XCTAssertEqual(catalog.rendition(variantID: nfc), nfcRendition)
        XCTAssertEqual(catalog.rendition(variantID: nfd), nfdRendition)
        XCTAssertNil(catalog.rendition(variantID: "cafe"))

        let nfcSession = try HLSRenditionCatalog(sessionID: nfc, sessionGeneration: 1, renditions: [nfcRendition])
        let nfdSession = try HLSRenditionCatalog(sessionID: nfd, sessionGeneration: 1, renditions: [nfcRendition])
        XCTAssertNotEqual(nfcSession, nfdSession)

        XCTAssertThrowsError(
            try HLSRenditionCatalog(
                sessionID: "session",
                sessionGeneration: 1,
                renditions: [nfcRendition, nfcRendition]
            ))
    }

    func testProtoMappingPreservesOptionalZeroAndAbsence() throws {
        var descriptorProto = TvosNetPlayer_V1_HlsRenditionDescriptor()
        descriptorProto.variantID = "v1"
        descriptorProto.advertisedBandwidthBitsPerSecond = 1_000
        descriptorProto.videoResourceBandwidthBitsPerSecond = 0
        descriptorProto.hasSeparateAudio_p = true
        XCTAssertTrue(descriptorProto.hasVideoResourceBandwidthBitsPerSecond)
        XCTAssertThrowsError(try HLSRenditionDescriptor(descriptorProto))

        descriptorProto.videoResourceBandwidthBitsPerSecond = 1
        let descriptor = try HLSRenditionDescriptor(descriptorProto)
        XCTAssertEqual(descriptor.videoResourceBandwidthBitsPerSecond, 1)
        XCTAssertNil(descriptor.audioResourceBandwidthBitsPerSecond)
        XCTAssertNil(descriptor.videoResourceCodecs)
        XCTAssertNil(descriptor.advertisedWidth)

        descriptorProto.videoResourceCodecs = ""
        XCTAssertThrowsError(try HLSRenditionDescriptor(descriptorProto))
        descriptorProto.videoResourceCodecs = "avc1"
        descriptorProto.audioResourceBandwidthBitsPerSecond = 0
        XCTAssertTrue(descriptorProto.hasAudioResourceBandwidthBitsPerSecond)
        XCTAssertThrowsError(try HLSRenditionDescriptor(descriptorProto))
    }

    func testCatalogWireIdentityGenerationCountAndEncodedSizeValidation() throws {
        let request = try HLSRenditionCatalogRequest(sessionID: "session")
        var proto = TvosNetPlayer_V1_HlsRenditionCatalog()
        proto.sessionID = request.sessionID
        proto.sessionGeneration = 9
        proto.renditions = [try descriptor(variantID: "v1").proto]

        let decoded = try HLSRenditionCatalog(proto, matching: request)
        XCTAssertEqual(decoded.sessionGeneration, 9)
        XCTAssertEqual(decoded.renditions.count, 1)

        var wrongSession = proto
        wrongSession.sessionID = "other"
        XCTAssertThrowsError(try HLSRenditionCatalog(wrongSession, matching: request))

        proto.sessionGeneration = 0
        XCTAssertThrowsError(try HLSRenditionCatalog(proto, matching: request))
        proto.sessionGeneration = 9
        proto.renditions = []
        XCTAssertThrowsError(try HLSRenditionCatalog(proto, matching: request))
        proto.renditions = try (0..<129).map { try descriptor(variantID: "v\($0)").proto }
        XCTAssertThrowsError(try HLSRenditionCatalog(proto, matching: request))

        var oversized = TvosNetPlayer_V1_HlsRenditionCatalog()
        oversized.sessionID = request.sessionID
        oversized.sessionGeneration = 9
        oversized.renditions = [try descriptor(variantID: "v1").proto]
        var encoded = try oversized.serializedData()
        encoded.append(contentsOf: [0xA2, 0x06])
        appendVarint(65_530, to: &encoded)
        encoded.append(contentsOf: repeatElement(0, count: 65_530))
        let withUnknownField = try TvosNetPlayer_V1_HlsRenditionCatalog(serializedBytes: [UInt8](encoded))
        XCTAssertThrowsError(try HLSRenditionCatalog(withUnknownField, matching: request))
    }

    private func descriptor(
        variantID: String,
        advertisedBandwidthBitsPerSecond: UInt64 = 1_000,
        hasSeparateAudio: Bool = false,
        videoResourceBandwidthBitsPerSecond: UInt64? = nil,
        audioResourceBandwidthBitsPerSecond: UInt64? = nil,
        videoResourceCodecs: String? = nil,
        audioResourceCodecs: String? = nil,
        videoResourceMimeType: String? = nil,
        audioResourceMimeType: String? = nil,
        advertisedWidth: UInt32? = nil,
        advertisedHeight: UInt32? = nil
    ) throws -> HLSRenditionDescriptor {
        try HLSRenditionDescriptor(
            variantID: variantID,
            advertisedBandwidthBitsPerSecond: advertisedBandwidthBitsPerSecond,
            videoResourceBandwidthBitsPerSecond: videoResourceBandwidthBitsPerSecond,
            audioResourceBandwidthBitsPerSecond: audioResourceBandwidthBitsPerSecond,
            hasSeparateAudio: hasSeparateAudio,
            videoResourceCodecs: videoResourceCodecs,
            audioResourceCodecs: audioResourceCodecs,
            videoResourceMimeType: videoResourceMimeType,
            audioResourceMimeType: audioResourceMimeType,
            advertisedWidth: advertisedWidth,
            advertisedHeight: advertisedHeight
        )
    }

    private func appendVarint(_ value: UInt64, to data: inout Data) {
        var remaining = value
        while remaining >= 0x80 {
            data.append(UInt8(remaining & 0x7F) | 0x80)
            remaining >>= 7
        }
        data.append(UInt8(remaining))
    }
}
