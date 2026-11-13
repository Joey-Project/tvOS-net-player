import XCTest
@testable import TVOSNetPlayerCacheClient

final class HLSPlaybackContextTests: XCTestCase {
    private let contextID = "00000000-0000-4000-8000-000000000001"
    private let challenge = "00000000-0000-4000-8000-000000000002"
    private let readerID = "00000000-0000-4000-8000-0000000000a1"
    private let otherReaderID = "00000000-0000-4000-8000-0000000000a2"

    func testOpenRequestAndTicketPreserveExactUTF8Identity() throws {
        let sessionID = "session-e\u{0301}"
        let request = HLSPlaybackContextOpenRequest(sessionID: sessionID, expectedSessionGeneration: 17)
        try request.validate()

        let ticket = makeTicket(sessionID: sessionID, generation: 17)
        try ticket.validate(matching: request)

        let normalizedTicket = makeTicket(sessionID: "session-\u{00E9}", generation: 17)
        XCTAssertEqual(normalizedTicket.sessionID, request.sessionID)
        XCTAssertFalse(normalizedTicket.sessionID.utf8.elementsEqual(request.sessionID.utf8))
        XCTAssertThrowsError(try normalizedTicket.validate(matching: request)) { error in
            XCTAssertEqual(error as? HLSPlaybackContextValidationError, .invalidResponse)
        }
    }

    func testOpenRequestRejectsInvalidIdentityAndGeneration() {
        let invalidRequests = [
            HLSPlaybackContextOpenRequest(sessionID: "", expectedSessionGeneration: 1),
            HLSPlaybackContextOpenRequest(sessionID: " session", expectedSessionGeneration: 1),
            HLSPlaybackContextOpenRequest(sessionID: "session\u{0085}", expectedSessionGeneration: 1),
            HLSPlaybackContextOpenRequest(
                sessionID: String(repeating: "s", count: 257), expectedSessionGeneration: 1),
            HLSPlaybackContextOpenRequest(sessionID: "session", expectedSessionGeneration: 0),
        ]

        for request in invalidRequests {
            XCTAssertThrowsError(try request.validate()) { error in
                XCTAssertEqual(error as? HLSPlaybackContextValidationError, .invalidRequest)
            }
        }
    }

    func testOpenRequestRequiresCanonicalLowercaseUUIDv4ReaderID() {
        let invalidReaderIDs = [
            "00000000-0000-4000-8000-0000000000A1",
            "00000000-0000-3000-8000-0000000000a1",
            "00000000-0000-4000-7000-0000000000a1",
            "000000000000400080000000000000a1",
            " 00000000-0000-4000-8000-0000000000a1",
            String(repeating: "a", count: 37),
            "",
        ]

        for httpReaderID in invalidReaderIDs {
            let request = HLSPlaybackContextOpenRequest(
                sessionID: "session",
                expectedSessionGeneration: 1,
                httpReaderID: httpReaderID
            )
            XCTAssertThrowsError(try request.validate()) { error in
                XCTAssertEqual(error as? HLSPlaybackContextValidationError, .invalidRequest)
            }
        }

        XCTAssertNoThrow(
            try HLSPlaybackContextOpenRequest(
                sessionID: "session",
                expectedSessionGeneration: 1,
                httpReaderID: readerID
            ).validate()
        )
    }

    func testUpdateRejectsMalformedUUIDSequenceAndTimingValues() {
        let validSample = HLSPlaybackContextTimingSample(
            playheadSeconds: 4,
            continuousBufferedEndSeconds: 12,
            playbackRate: 1.5
        )
        let base = HLSPlaybackContextUpdateRequest(
            contextID: contextID,
            challenge: challenge,
            sessionID: "session",
            expectedSessionGeneration: 3,
            sequence: 1,
            sample: validSample
        )
        XCTAssertNoThrow(try base.validate())

        let invalidRequests = [
            HLSPlaybackContextUpdateRequest(
                contextID: "not-a-uuid", challenge: challenge, sessionID: "session",
                expectedSessionGeneration: 3, sequence: 1, sample: validSample),
            HLSPlaybackContextUpdateRequest(
                contextID: contextID, challenge: "00000000-0000-3000-8000-000000000002", sessionID: "session",
                expectedSessionGeneration: 3, sequence: 1, sample: validSample),
            HLSPlaybackContextUpdateRequest(
                contextID: contextID, challenge: challenge, sessionID: "session",
                expectedSessionGeneration: 3, sequence: 0, sample: validSample),
            HLSPlaybackContextUpdateRequest(
                contextID: contextID, challenge: challenge, sessionID: "session",
                expectedSessionGeneration: 3, sequence: UInt64.max, sample: validSample),
            HLSPlaybackContextUpdateRequest(
                contextID: contextID, challenge: challenge, sessionID: "session",
                expectedSessionGeneration: 0, sequence: 1, sample: validSample),
            HLSPlaybackContextUpdateRequest(
                contextID: contextID, challenge: challenge, sessionID: "session",
                expectedSessionGeneration: 3, sequence: 1, sample: validSample,
                expectedHTTPReaderID: "00000000-0000-4000-8000-0000000000A1"),
            HLSPlaybackContextUpdateRequest(
                contextID: contextID, challenge: challenge, sessionID: "session",
                expectedSessionGeneration: 3, sequence: 1,
                sample: HLSPlaybackContextTimingSample(
                    playheadSeconds: -.infinity, continuousBufferedEndSeconds: 12, playbackRate: 1)),
            HLSPlaybackContextUpdateRequest(
                contextID: contextID, challenge: challenge, sessionID: "session",
                expectedSessionGeneration: 3, sequence: 1,
                sample: HLSPlaybackContextTimingSample(
                    playheadSeconds: 4, continuousBufferedEndSeconds: .nan, playbackRate: 1)),
            HLSPlaybackContextUpdateRequest(
                contextID: contextID, challenge: challenge, sessionID: "session",
                expectedSessionGeneration: 3, sequence: 1,
                sample: HLSPlaybackContextTimingSample(
                    playheadSeconds: 4, continuousBufferedEndSeconds: 3, playbackRate: 1)),
            HLSPlaybackContextUpdateRequest(
                contextID: contextID, challenge: challenge, sessionID: "session",
                expectedSessionGeneration: 3, sequence: 1,
                sample: HLSPlaybackContextTimingSample(
                    playheadSeconds: 4, continuousBufferedEndSeconds: 12, playbackRate: 16.01)),
        ]

        for request in invalidRequests {
            XCTAssertThrowsError(try request.validate()) { error in
                XCTAssertEqual(error as? HLSPlaybackContextValidationError, .invalidRequest)
            }
        }
    }

    func testOpenTicketRejectsWrongIdentityGenerationSequenceAndValidity() throws {
        let request = HLSPlaybackContextOpenRequest(sessionID: "session", expectedSessionGeneration: 7)
        let valid = makeTicket(sessionID: "session", generation: 7)
        try valid.validate(matching: request)

        let invalidTickets = [
            makeTicket(contextID: "bad", sessionID: "session", generation: 7),
            makeTicket(sessionID: "session", generation: 8),
            makeTicket(sessionID: "session", generation: 7, sequence: 0),
            makeTicket(sessionID: "session", generation: 7, validity: 0),
            makeTicket(sessionID: "session", generation: 7, validity: 5_001),
        ]

        for ticket in invalidTickets {
            XCTAssertThrowsError(try ticket.validate(matching: request)) { error in
                XCTAssertEqual(error as? HLSPlaybackContextValidationError, .invalidResponse)
            }
        }
    }

    func testOpenTicketRequiresExactOptionalReaderBinding() throws {
        let request = HLSPlaybackContextOpenRequest(
            sessionID: "session",
            expectedSessionGeneration: 7,
            httpReaderID: readerID
        )
        try makeTicket(sessionID: "session", generation: 7, httpReaderID: readerID)
            .validate(matching: request)

        let invalidTickets = [
            makeTicket(sessionID: "session", generation: 7),
            makeTicket(sessionID: "session", generation: 7, httpReaderID: otherReaderID),
            makeTicket(
                sessionID: "session",
                generation: 7,
                httpReaderID: "00000000-0000-4000-8000-0000000000A1"
            ),
        ]
        for ticket in invalidTickets {
            XCTAssertThrowsError(try ticket.validate(matching: request)) { error in
                XCTAssertEqual(error as? HLSPlaybackContextValidationError, .invalidResponse)
            }
        }

        let unboundRequest = HLSPlaybackContextOpenRequest(sessionID: "session", expectedSessionGeneration: 7)
        let unexpectedBinding = makeTicket(sessionID: "session", generation: 7, httpReaderID: readerID)
        XCTAssertThrowsError(try unexpectedBinding.validate(matching: unboundRequest)) { error in
            XCTAssertEqual(error as? HLSPlaybackContextValidationError, .invalidResponse)
        }
    }

    func testUpdateTicketRequiresExactOwnerNextSequenceAndRotatedChallenge() throws {
        let request = HLSPlaybackContextUpdateRequest(
            contextID: contextID,
            challenge: challenge,
            sessionID: "session",
            expectedSessionGeneration: 4,
            sequence: 12,
            sample: HLSPlaybackContextTimingSample(
                playheadSeconds: 1,
                continuousBufferedEndSeconds: 5,
                playbackRate: 1
            )
        )
        let valid = makeTicket(
            contextID: contextID,
            challenge: "00000000-0000-4000-8000-000000000003",
            sessionID: "session",
            generation: 4,
            sequence: 13
        )
        try valid.validate(matching: request)

        let boundTicket = makeTicket(
            contextID: contextID,
            challenge: "00000000-0000-4000-8000-000000000003",
            sessionID: "session",
            generation: 4,
            sequence: 13,
            httpReaderID: readerID
        )
        let boundRequest = HLSPlaybackContextUpdateRequest(
            contextID: request.contextID,
            challenge: request.challenge,
            sessionID: request.sessionID,
            expectedSessionGeneration: request.expectedSessionGeneration,
            sequence: request.sequence,
            sample: request.sample,
            expectedHTTPReaderID: readerID
        )
        try boundTicket.validate(matching: boundRequest)
        XCTAssertThrowsError(
            try makeTicket(
                contextID: contextID,
                challenge: "00000000-0000-4000-8000-000000000003",
                sessionID: "session",
                generation: 4,
                sequence: 13,
                httpReaderID: otherReaderID
            ).validate(matching: boundRequest)
        ) { error in
            XCTAssertEqual(error as? HLSPlaybackContextValidationError, .invalidResponse)
        }
        XCTAssertThrowsError(try valid.validate(matching: boundRequest)) { error in
            XCTAssertEqual(error as? HLSPlaybackContextValidationError, .invalidResponse)
        }

        let invalidTickets = [
            makeTicket(
                contextID: "00000000-0000-4000-8000-000000000004",
                challenge: "00000000-0000-4000-8000-000000000003",
                sessionID: "session", generation: 4, sequence: 13),
            makeTicket(
                contextID: contextID, challenge: "00000000-0000-4000-8000-000000000003",
                sessionID: "session-e\u{0301}", generation: 4, sequence: 13),
            makeTicket(
                contextID: contextID, challenge: challenge,
                sessionID: "session", generation: 4, sequence: 13),
            makeTicket(
                contextID: contextID, challenge: "00000000-0000-4000-8000-000000000003",
                sessionID: "session", generation: 4, sequence: 12),
        ]

        for ticket in invalidTickets {
            XCTAssertThrowsError(try ticket.validate(matching: request)) { error in
                XCTAssertEqual(error as? HLSPlaybackContextValidationError, .invalidResponse)
            }
        }
    }

    func testCloseAcceptsCanonicalUUIDOfAnyVersionAndRejectsMalformedValue() throws {
        try HLSPlaybackContextCloseRequest(contextID: "00000000-0000-3000-8000-000000000001").validate()
        XCTAssertThrowsError(
            try HLSPlaybackContextCloseRequest(contextID: "00000000000040008000000000000001").validate())
    }

    func testLegacyClientDefaultsReportTypedUnsupportedOperation() async {
        let client = LegacyContextClient()
        let open = HLSPlaybackContextOpenRequest(sessionID: "session", expectedSessionGeneration: 1)
        let update = HLSPlaybackContextUpdateRequest(
            contextID: contextID,
            challenge: challenge,
            sessionID: "session",
            expectedSessionGeneration: 1,
            sequence: 1,
            sample: HLSPlaybackContextTimingSample(
                playheadSeconds: 0,
                continuousBufferedEndSeconds: 1,
                playbackRate: 1
            )
        )
        let close = HLSPlaybackContextCloseRequest(contextID: contextID)

        do {
            _ = try await client.openHLSPlaybackContext(open)
            XCTFail("Expected open to be unsupported.")
        } catch let error as CacheControlClientUnsupportedOperation {
            XCTAssertEqual(error, .hlsPlaybackContext)
        } catch {
            XCTFail("Unexpected error: \(error)")
        }

        do {
            _ = try await client.updateHLSPlaybackContext(update)
            XCTFail("Expected update to be unsupported.")
        } catch let error as CacheControlClientUnsupportedOperation {
            XCTAssertEqual(error, .hlsPlaybackContext)
        } catch {
            XCTFail("Unexpected error: \(error)")
        }

        do {
            _ = try await client.closeHLSPlaybackContext(close)
            XCTFail("Expected close to be unsupported.")
        } catch let error as CacheControlClientUnsupportedOperation {
            XCTAssertEqual(error, .hlsPlaybackContext)
        } catch {
            XCTFail("Unexpected error: \(error)")
        }
    }

    func testServerSummaryExposesPlaybackContextCapability() {
        XCTAssertEqual(
            String(describing: TvosNetPlayer_V1_ServerCapability.hlsPlaybackContext),
            CacheServerCapability.hlsPlaybackContext
        )
        let summary = CacheServerSummary(
            id: "server",
            name: "Cache",
            version: "1",
            mediaBaseURIs: [],
            capabilities: [CacheServerCapability.hlsPlaybackContext]
        )
        XCTAssertTrue(summary.supportsHLSPlaybackContext)

        XCTAssertEqual(
            String(describing: TvosNetPlayer_V1_ServerCapability.hlsPlaybackReaderBinding),
            CacheServerCapability.hlsPlaybackReaderBinding
        )
        let bindingSummary = CacheServerSummary(
            id: "server",
            name: "Cache",
            version: "1",
            mediaBaseURIs: [],
            capabilities: [CacheServerCapability.hlsPlaybackReaderBinding]
        )
        XCTAssertTrue(bindingSummary.supportsHLSPlaybackReaderBinding)
    }

    func testEmptyWireReaderIDMapsToNilAndLegacyInitializersRemainUnbound() throws {
        let legacyRequest = HLSPlaybackContextOpenRequest(sessionID: "session", expectedSessionGeneration: 7)
        XCTAssertNil(legacyRequest.httpReaderID)
        XCTAssertEqual(TvosNetPlayer_V1_OpenHlsPlaybackContextRequest(legacyRequest).httpReaderID, "")

        var proto = TvosNetPlayer_V1_HlsPlaybackContextTicket()
        proto.contextID = contextID
        proto.challenge = challenge
        proto.sessionID = "session"
        proto.sessionGeneration = 7
        proto.nextSequence = 1
        proto.validityMillis = 5_000
        proto.httpReaderID = ""
        let ticket = HLSPlaybackContextTicket(proto)
        XCTAssertNil(ticket.httpReaderID)
        try ticket.validate(matching: legacyRequest)
    }

    private func makeTicket(
        contextID: String? = nil,
        challenge: String? = nil,
        sessionID: String,
        generation: UInt64,
        sequence: UInt64 = 1,
        validity: UInt64 = 5_000,
        httpReaderID: String? = nil
    ) -> HLSPlaybackContextTicket {
        HLSPlaybackContextTicket(
            contextID: contextID ?? self.contextID,
            challenge: challenge ?? self.challenge,
            sessionID: sessionID,
            sessionGeneration: generation,
            nextSequence: sequence,
            validityMillis: validity,
            httpReaderID: httpReaderID
        )
    }
}

private struct LegacyContextClient: CacheControlClient {
    func getServerInfo() async throws -> CacheServerSummary { throw LegacyContextClientError.notImplemented }
    func listCacheRoots() async throws -> [CacheRoot] { throw LegacyContextClientError.notImplemented }
    func listLibraryItemsPage(pageToken: String, pageSize: Int, searchText: String?) async throws
        -> CacheLibraryItemsPage
    {
        throw LegacyContextClientError.notImplemented
    }
    func getPlaybackSource(itemID: String, variantID: String) async throws -> CachePlaybackSource {
        throw LegacyContextClientError.notImplemented
    }
    func deleteLibraryItem(id: String) async throws -> Bool { throw LegacyContextClientError.notImplemented }
    func getTask(id: String) async throws -> CacheTask { throw LegacyContextClientError.notImplemented }
    func watchTasks(ids: [String]) async -> AsyncThrowingStream<CacheTask, Error> {
        AsyncThrowingStream { $0.finish() }
    }
    func cancelTask(id: String) async throws -> CacheTask { throw LegacyContextClientError.notImplemented }
    func createBilibiliPlaybackTask(urlOrID: String, options: BilibiliPlaybackTaskOptions) async throws -> CacheTask {
        throw LegacyContextClientError.notImplemented
    }
}

private enum LegacyContextClientError: Error {
    case notImplemented
}
