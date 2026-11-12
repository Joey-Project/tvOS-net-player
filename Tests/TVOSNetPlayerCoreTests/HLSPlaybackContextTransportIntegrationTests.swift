import Foundation
import GRPCCore
import GRPCNIOTransportHTTP2Posix
import GRPCProtobuf
import SwiftProtobuf
import XCTest
@testable import TVOSNetPlayerCacheClient

/// Exercises generated playback-context routes through the production cache client on loopback.
@MainActor
final class HLSPlaybackContextTransportIntegrationTests: XCTestCase {
    private let contextID = "00000000-0000-4000-8000-000000000001"
    private let challenge1 = "00000000-0000-4000-8000-000000000002"
    private let challenge2 = "00000000-0000-4000-8000-000000000003"

    func testProductionClientUsesGeneratedOpenUpdateAndCloseRoutes() async throws {
        let sessionID = "session-e\u{0301}"
        let probe = HLSPlaybackContextRPCProbe()

        try await withLoopbackServer(probe: probe) { endpoint in
            let client = GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(3))
            let openRequest = HLSPlaybackContextOpenRequest(
                sessionID: sessionID,
                expectedSessionGeneration: 23
            )
            let opened = try await client.openHLSPlaybackContext(openRequest)
            XCTAssertEqual(opened.contextID, self.contextID)
            XCTAssertEqual(opened.challenge, self.challenge1)
            XCTAssertEqual(opened.nextSequence, 1)
            XCTAssertTrue(opened.sessionID.utf8.elementsEqual(sessionID.utf8))

            let updateRequest = HLSPlaybackContextUpdateRequest(
                contextID: opened.contextID,
                challenge: opened.challenge,
                sessionID: sessionID,
                expectedSessionGeneration: 23,
                sequence: opened.nextSequence,
                sample: HLSPlaybackContextTimingSample(
                    playheadSeconds: 5.25,
                    continuousBufferedEndSeconds: 17.5,
                    playbackRate: 1.25
                )
            )
            let updated = try await client.updateHLSPlaybackContext(updateRequest)
            XCTAssertEqual(updated.contextID, opened.contextID)
            XCTAssertEqual(updated.challenge, self.challenge2)
            XCTAssertEqual(updated.nextSequence, 2)

            let closed = try await client.closeHLSPlaybackContext(
                HLSPlaybackContextCloseRequest(contextID: opened.contextID)
            )
            XCTAssertTrue(closed.closed)

            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.openRequests.count, 1)
            XCTAssertTrue(snapshot.openRequests[0].sessionID.utf8.elementsEqual(sessionID.utf8))
            XCTAssertEqual(snapshot.openRequests[0].expectedSessionGeneration, 23)
            XCTAssertEqual(snapshot.updateRequests.count, 1)
            XCTAssertEqual(snapshot.updateRequests[0].contextID, opened.contextID)
            XCTAssertEqual(snapshot.updateRequests[0].challenge, opened.challenge)
            XCTAssertEqual(snapshot.updateRequests[0].sequence, 1)
            XCTAssertEqual(snapshot.updateRequests[0].sample.playheadSeconds, 5.25)
            XCTAssertEqual(snapshot.updateRequests[0].sample.continuousBufferedEndSeconds, 17.5)
            XCTAssertEqual(snapshot.updateRequests[0].sample.playbackRate, 1.25)
            XCTAssertEqual(snapshot.closeRequests.map(\.contextID), [opened.contextID])
        }
    }

    func testProductionClientRejectsMalformedGeneratedTicket() async throws {
        let probe = HLSPlaybackContextRPCProbe(scenario: .malformedOpenTicket)
        let request = HLSPlaybackContextOpenRequest(sessionID: "loopback-session", expectedSessionGeneration: 9)

        try await withLoopbackServer(probe: probe) { endpoint in
            let client = GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(3))
            do {
                _ = try await client.openHLSPlaybackContext(request)
                XCTFail("Expected the malformed generated ticket to be rejected.")
            } catch let error as HLSPlaybackContextValidationError {
                XCTAssertEqual(error, .invalidResponse)
            }
            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.openRequests.count, 1)
        }
    }

    func testProductionClientMapsUnimplementedRouteToTypedUnsupportedOperation() async throws {
        let probe = HLSPlaybackContextRPCProbe(scenario: .unimplementedOpen)

        try await withLoopbackServer(probe: probe) { endpoint in
            let client = GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(3))
            do {
                _ = try await client.openHLSPlaybackContext(
                    HLSPlaybackContextOpenRequest(sessionID: "loopback-session", expectedSessionGeneration: 9))
                XCTFail("Expected the old server route to map to the typed unsupported operation.")
            } catch let error as CacheControlClientUnsupportedOperation {
                XCTAssertEqual(error, .hlsPlaybackContext)
            }
            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.openRequests.count, 1)
        }
    }

    func testCancelledInFlightOpenDoesNotExposeLateTicket() async throws {
        let probe = HLSPlaybackContextRPCProbe(scenario: .heldOpen)

        try await withLoopbackServer(probe: probe) { endpoint in
            let client = GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(3))
            let call = Task {
                try await client.openHLSPlaybackContext(
                    HLSPlaybackContextOpenRequest(sessionID: "loopback-session", expectedSessionGeneration: 9))
            }
            defer { call.cancel() }

            await probe.waitForOpen()
            call.cancel()
            await probe.releaseOpen()

            do {
                _ = try await call.value
                XCTFail("Expected cancellation to take precedence over the late ticket.")
            } catch is CancellationError {
            }
            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.openRequests.count, 1)
        }
    }

    private func withLoopbackServer(
        probe: HLSPlaybackContextRPCProbe,
        operation: (CacheServerEndpoint) async throws -> Void
    ) async throws {
        try await withGRPCServer(
            transport: .http2NIOPosix(
                address: .ipv4(host: "127.0.0.1", port: 0),
                transportSecurity: .plaintext
            ),
            services: [HLSPlaybackContextTestService(probe: probe)]
        ) { server in
            let address = try await server.transport.listeningAddress
            guard let port = address.ipv4?.port else {
                throw HLSPlaybackContextFixtureError.invalidListeningAddress
            }
            try await operation(CacheServerEndpoint(host: "127.0.0.1", port: port))
        }
    }
}

private enum HLSPlaybackContextFixtureError: Error {
    case invalidListeningAddress
}

private enum HLSPlaybackContextScenario: Sendable {
    case normal
    case malformedOpenTicket
    case unimplementedOpen
    case heldOpen
}

private actor HLSPlaybackContextRPCProbe {
    struct Snapshot: Sendable {
        let openRequests: [TvosNetPlayer_V1_OpenHlsPlaybackContextRequest]
        let updateRequests: [TvosNetPlayer_V1_UpdateHlsPlaybackContextRequest]
        let closeRequests: [TvosNetPlayer_V1_CloseHlsPlaybackContextRequest]
    }

    let scenario: HLSPlaybackContextScenario
    private var openRequests: [TvosNetPlayer_V1_OpenHlsPlaybackContextRequest] = []
    private var updateRequests: [TvosNetPlayer_V1_UpdateHlsPlaybackContextRequest] = []
    private var closeRequests: [TvosNetPlayer_V1_CloseHlsPlaybackContextRequest] = []
    private var openWaiters: [CheckedContinuation<Void, Never>] = []
    private var openReleaseWaiters: [CheckedContinuation<Void, Never>] = []
    private var openReached = false
    private var openReleased = false

    init(scenario: HLSPlaybackContextScenario = .normal) {
        self.scenario = scenario
    }

    func open(_ request: TvosNetPlayer_V1_OpenHlsPlaybackContextRequest) async throws
        -> TvosNetPlayer_V1_HlsPlaybackContextTicket
    {
        openRequests.append(request)
        if case .heldOpen = scenario {
            openReached = true
            let waiters = openWaiters
            openWaiters.removeAll()
            waiters.forEach { $0.resume() }
            if !openReleased {
                await withCheckedContinuation { openReleaseWaiters.append($0) }
            }
        }
        if case .unimplementedOpen = scenario {
            throw RPCError(code: .unimplemented, message: "Private route detail.")
        }

        var response = ticket(
            contextID: "00000000-0000-4000-8000-000000000001",
            challenge: "00000000-0000-4000-8000-000000000002",
            sessionID: request.sessionID,
            generation: request.expectedSessionGeneration,
            sequence: 1
        )
        if case .malformedOpenTicket = scenario {
            response.nextSequence = 0
        }
        return response
    }

    func update(_ request: TvosNetPlayer_V1_UpdateHlsPlaybackContextRequest)
        -> TvosNetPlayer_V1_HlsPlaybackContextTicket
    {
        updateRequests.append(request)
        return ticket(
            contextID: request.contextID,
            challenge: "00000000-0000-4000-8000-000000000003",
            sessionID: "session-e\u{0301}",
            generation: 23,
            sequence: request.sequence + 1
        )
    }

    func close(_ request: TvosNetPlayer_V1_CloseHlsPlaybackContextRequest)
        -> TvosNetPlayer_V1_CloseHlsPlaybackContextResponse
    {
        closeRequests.append(request)
        var response = TvosNetPlayer_V1_CloseHlsPlaybackContextResponse()
        response.closed = true
        return response
    }

    func waitForOpen() async {
        guard !openReached else { return }
        await withCheckedContinuation { openWaiters.append($0) }
    }

    func releaseOpen() {
        openReleased = true
        let waiters = openReleaseWaiters
        openReleaseWaiters.removeAll()
        waiters.forEach { $0.resume() }
    }

    func snapshot() -> Snapshot {
        Snapshot(openRequests: openRequests, updateRequests: updateRequests, closeRequests: closeRequests)
    }

    private func ticket(
        contextID: String,
        challenge: String,
        sessionID: String,
        generation: UInt64,
        sequence: UInt64
    ) -> TvosNetPlayer_V1_HlsPlaybackContextTicket {
        var response = TvosNetPlayer_V1_HlsPlaybackContextTicket()
        response.contextID = contextID
        response.challenge = challenge
        response.sessionID = sessionID
        response.sessionGeneration = generation
        response.nextSequence = sequence
        response.validityMillis = 5_000
        return response
    }
}

private struct HLSPlaybackContextTestService: RegistrableRPCService {
    let probe: HLSPlaybackContextRPCProbe

    func registerMethods<Transport: ServerTransport>(with router: inout RPCRouter<Transport>) {
        let probe = self.probe
        Self.registerUnary(
            TvosNetPlayer_V1_CacheService.Method.OpenHlsPlaybackContext.descriptor,
            with: &router
        ) { (request: TvosNetPlayer_V1_OpenHlsPlaybackContextRequest) in
            try await probe.open(request)
        }
        Self.registerUnary(
            TvosNetPlayer_V1_CacheService.Method.UpdateHlsPlaybackContext.descriptor,
            with: &router
        ) { (request: TvosNetPlayer_V1_UpdateHlsPlaybackContextRequest) in
            await probe.update(request)
        }
        Self.registerUnary(
            TvosNetPlayer_V1_CacheService.Method.CloseHlsPlaybackContext.descriptor,
            with: &router
        ) { (request: TvosNetPlayer_V1_CloseHlsPlaybackContextRequest) in
            await probe.close(request)
        }
    }

    private static func registerUnary<Transport, Input, Output>(
        _ descriptor: MethodDescriptor,
        with router: inout RPCRouter<Transport>,
        handler: @escaping @Sendable (Input) async throws -> Output
    )
    where
        Transport: ServerTransport, Input: SwiftProtobuf.Message & Sendable,
        Output: SwiftProtobuf.Message & Sendable
    {
        router.registerHandler(
            forMethod: descriptor,
            deserializer: ProtobufDeserializer<Input>(),
            serializer: ProtobufSerializer<Output>()
        ) { request, _ in
            let request = try await ServerRequest(stream: request)
            let output = try await handler(request.message)
            return StreamingServerResponse(single: ServerResponse(message: output))
        }
    }
}
