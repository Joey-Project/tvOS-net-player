import XCTest
@testable import TVOSNetPlayerCacheClient

final class HLSPlaybackReadinessTests: XCTestCase {
    func testRequestValidationDoesNotTrimIdentityAndAcceptsWindowBoundaries() throws {
        let request = HLSPlaybackReadinessRequest(
            sessionID: " session-id ",
            variantID: "variant-1",
            windowStartSeconds: 0,
            windowDurationSeconds: 60
        )
        // Edge whitespace is rejected by the server; the local check must not normalize it.
        XCTAssertThrowsError(try request.validate())

        let valid = HLSPlaybackReadinessRequest(
            sessionID: "session-id",
            variantID: "variant-1",
            windowStartSeconds: 0,
            windowDurationSeconds: 60
        )
        try valid.validate()
        XCTAssertEqual(valid.sessionID, "session-id")
        let wireRequest = TvosNetPlayer_V1_GetHlsPlaybackReadinessRequest(valid)
        XCTAssertEqual(wireRequest.sessionID, valid.sessionID)
        XCTAssertEqual(wireRequest.variantID, valid.variantID)
        XCTAssertEqual(wireRequest.windowStartSeconds, valid.windowStartSeconds)
        XCTAssertEqual(wireRequest.windowDurationSeconds, valid.windowDurationSeconds)
    }

    func testRequestValidationRejectsMalformedIdentifiersAndWindows() {
        let invalidRequests = [
            HLSPlaybackReadinessRequest(sessionID: "", variantID: "v", windowStartSeconds: 0, windowDurationSeconds: 1),
            HLSPlaybackReadinessRequest(
                sessionID: "s", variantID: "v ", windowStartSeconds: 0, windowDurationSeconds: 1),
            HLSPlaybackReadinessRequest(
                sessionID: "s", variantID: "v-\u{0085}", windowStartSeconds: 0, windowDurationSeconds: 1),
            HLSPlaybackReadinessRequest(
                sessionID: String(repeating: "s", count: 257), variantID: "v", windowStartSeconds: 0,
                windowDurationSeconds: 1),
            HLSPlaybackReadinessRequest(
                sessionID: "s", variantID: "v", windowStartSeconds: -.infinity, windowDurationSeconds: 1),
            HLSPlaybackReadinessRequest(
                sessionID: "s", variantID: "v", windowStartSeconds: 0, windowDurationSeconds: .nan),
            HLSPlaybackReadinessRequest(
                sessionID: "s", variantID: "v", windowStartSeconds: 0, windowDurationSeconds: 0),
            HLSPlaybackReadinessRequest(
                sessionID: "s", variantID: "v", windowStartSeconds: 0, windowDurationSeconds: 60.01),
            HLSPlaybackReadinessRequest(
                sessionID: "s", variantID: "v", windowStartSeconds: Double.greatestFiniteMagnitude,
                windowDurationSeconds: 60),
        ]

        for request in invalidRequests {
            XCTAssertThrowsError(try request.validate())
        }
    }

    func testIdentifierValidationMatchesUTF8ByteAndUnicodeWhitespaceRules() throws {
        let exactByteLimit = HLSPlaybackReadinessRequest(
            sessionID: String(repeating: "\u{00E9}", count: 128),
            variantID: "cafe\u{00E9}",
            windowStartSeconds: 0,
            windowDurationSeconds: 1
        )
        try exactByteLimit.validate()
        XCTAssertEqual(exactByteLimit.sessionID.utf8.count, 256)

        let overByteLimit = HLSPlaybackReadinessRequest(
            sessionID: String(repeating: "\u{00E9}", count: 129),
            variantID: "variant",
            windowStartSeconds: 0,
            windowDurationSeconds: 1
        )
        XCTAssertThrowsError(try overByteLimit.validate())

        for identifier in ["\u{00A0}id", "id\u{2003}", "in\u{0085}ternal"] {
            let request = HLSPlaybackReadinessRequest(
                sessionID: identifier,
                variantID: "variant",
                windowStartSeconds: 0,
                windowDurationSeconds: 1
            )
            XCTAssertThrowsError(try request.validate())
        }
    }

    func testProtoMappingPreservesUnknownEnumsAndOptionalPresence() throws {
        let request = HLSPlaybackReadinessRequest(
            sessionID: "session-id",
            variantID: "variant-id",
            windowStartSeconds: 1.25,
            windowDurationSeconds: 10
        )
        var proto = TvosNetPlayer_V1_HlsPlaybackReadiness()
        proto.sessionID = request.sessionID
        proto.variantID = request.variantID
        proto.windowStartSeconds = request.windowStartSeconds
        proto.windowDurationSeconds = request.windowDurationSeconds
        proto.sessionGeneration = 42
        proto.cacheState = .UNRECOGNIZED(91)
        proto.cacheReason = .UNRECOGNIZED(92)
        proto.inspectionBudgetUsedBytes = 128
        var video = TvosNetPlayer_V1_HlsOriginSupplyEvidence()
        video.ageMillis = 500
        video.throughputBitsPerSecond = 2_000_000
        video.firstByteLatencyMicros = 0
        video.freshnessLimitMillis = 30_000
        proto.videoOrigin = video

        let model = try HLSPlaybackReadiness(proto, matching: request)
        XCTAssertEqual(model.cacheState, .unrecognized(91))
        XCTAssertEqual(model.cacheReason, .unrecognized(92))
        XCTAssertEqual(model.videoOrigin?.firstByteLatencyMicros, 0)
        XCTAssertNil(model.audioOrigin)

        let roundTrip = TvosNetPlayer_V1_HlsPlaybackReadiness(model)
        XCTAssertEqual(roundTrip.cacheState, .UNRECOGNIZED(91))
        XCTAssertEqual(roundTrip.cacheReason, .UNRECOGNIZED(92))
        XCTAssertTrue(roundTrip.hasVideoOrigin)
        XCTAssertTrue(roundTrip.videoOrigin.hasFirstByteLatencyMicros)
        XCTAssertEqual(roundTrip.videoOrigin.firstByteLatencyMicros, 0)
        XCTAssertFalse(roundTrip.hasAudioOrigin)

        proto.cacheState = .unknown
        proto.cacheReason = .unknownGenerationChanged
        let knownFutureSemantics = try HLSPlaybackReadiness(proto, matching: request)
        XCTAssertEqual(knownFutureSemantics.cacheState, .unknown)
        XCTAssertEqual(knownFutureSemantics.cacheReason, .unknownGenerationChanged)
    }

    func testProtoMappingRejectsMismatchedIdentityAndWindow() {
        let request = HLSPlaybackReadinessRequest(
            sessionID: "session-id",
            variantID: "variant-id",
            windowStartSeconds: 1,
            windowDurationSeconds: 10
        )
        var proto = TvosNetPlayer_V1_HlsPlaybackReadiness()
        proto.sessionID = request.sessionID
        proto.variantID = request.variantID
        proto.windowStartSeconds = request.windowStartSeconds
        proto.windowDurationSeconds = request.windowDurationSeconds

        proto.variantID = "other-variant"
        XCTAssertThrowsError(try HLSPlaybackReadiness(proto, matching: request))
        proto.variantID = request.variantID
        proto.windowDurationSeconds = 0
        XCTAssertThrowsError(try HLSPlaybackReadiness(proto, matching: request))
    }

    func testProtoMappingRejectsCanonicallyEquivalentByteDistinctIdentity() {
        let request = HLSPlaybackReadinessRequest(
            sessionID: "\u{00E9}",
            variantID: "variant-id",
            windowStartSeconds: 1,
            windowDurationSeconds: 10
        )
        var proto = TvosNetPlayer_V1_HlsPlaybackReadiness()
        proto.sessionID = "e\u{0301}"
        proto.variantID = request.variantID
        proto.windowStartSeconds = request.windowStartSeconds
        proto.windowDurationSeconds = request.windowDurationSeconds

        XCTAssertEqual(proto.sessionID, request.sessionID)
        XCTAssertFalse(proto.sessionID.utf8.elementsEqual(request.sessionID.utf8))
        XCTAssertThrowsError(try HLSPlaybackReadiness(proto, matching: request))
    }

    func testLegacyClientDefaultReportsUnsupportedFeature() async {
        let client = LegacyReadinessClient()
        let request = HLSPlaybackReadinessRequest(
            sessionID: "session-id",
            variantID: "variant-id",
            windowStartSeconds: 0,
            windowDurationSeconds: 1
        )

        do {
            _ = try await client.getHLSPlaybackReadiness(request)
            XCTFail("Expected unsupported feature.")
        } catch let error as CacheControlClientUnsupportedOperation {
            XCTAssertEqual(error, .hlsPlaybackReadiness)
        } catch {
            XCTFail("Unexpected error: \(error)")
        }
    }

    func testServerSummaryExposesReadinessCapability() {
        XCTAssertEqual(
            String(describing: TvosNetPlayer_V1_ServerCapability.hlsPlaybackReadiness),
            CacheServerCapability.hlsPlaybackReadiness
        )
        let summary = CacheServerSummary(
            id: "server",
            name: "Cache",
            version: "1",
            mediaBaseURIs: [],
            capabilities: [CacheServerCapability.hlsPlaybackReadiness]
        )
        XCTAssertTrue(summary.supportsHLSPlaybackReadiness)
    }

    func testCancelledCallerDoesNotValidateOrConnect() async {
        let task = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            let client = GRPCCacheControlClient(endpoint: CacheServerEndpoint(host: "localhost"))
            do {
                _ = try await client.getHLSPlaybackReadiness(
                    HLSPlaybackReadinessRequest(
                        sessionID: "",
                        variantID: "",
                        windowStartSeconds: .nan,
                        windowDurationSeconds: 0
                    )
                )
                XCTFail("Expected cancellation before validation or transport.")
            } catch is CancellationError {
                return
            } catch {
                XCTFail("Cancellation must take precedence over invalid request or transport.")
            }
        }
        await task.value
    }
}

private struct LegacyReadinessClient: CacheControlClient {
    func getServerInfo() async throws -> CacheServerSummary {
        throw LegacyClientError.notImplemented
    }

    func listCacheRoots() async throws -> [CacheRoot] {
        throw LegacyClientError.notImplemented
    }

    func listLibraryItemsPage(
        pageToken: String,
        pageSize: Int,
        searchText: String?
    ) async throws -> CacheLibraryItemsPage {
        throw LegacyClientError.notImplemented
    }

    func getPlaybackSource(itemID: String, variantID: String) async throws -> CachePlaybackSource {
        throw LegacyClientError.notImplemented
    }

    func deleteLibraryItem(id: String) async throws -> Bool {
        throw LegacyClientError.notImplemented
    }

    func getTask(id: String) async throws -> CacheTask {
        throw LegacyClientError.notImplemented
    }

    func watchTasks(ids: [String]) async -> AsyncThrowingStream<CacheTask, Error> {
        AsyncThrowingStream { $0.finish() }
    }

    func cancelTask(id: String) async throws -> CacheTask {
        throw LegacyClientError.notImplemented
    }

    func createBilibiliPlaybackTask(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask {
        throw LegacyClientError.notImplemented
    }
}

private enum LegacyClientError: Error {
    case notImplemented
}
