import Foundation
import GRPCCore
import GRPCNIOTransportHTTP2Posix
import GRPCProtobuf
import SwiftProtobuf
import XCTest
@testable import TVOSNetPlayerCacheClient

/// Exercises the production cache client against a credential-free loopback HTTP/2 server.
@MainActor
final class HLSRenditionCatalogTransportIntegrationTests: XCTestCase {
    func testProductionClientForwardsExactUTF8AndPreservesCatalogDescriptors() async throws {
        let sessionID = "session-e\u{0301}"
        let request = try HLSRenditionCatalogRequest(sessionID: sessionID)
        let probe = HLSRenditionCatalogRPCProbe(scenario: .valid)

        try await withLoopbackServer(probe: probe) { endpoint in
            let client = GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(3))
            let result = try await client.getHLSRenditionCatalog(request)

            XCTAssertTrue(result.sessionID.utf8.elementsEqual(sessionID.utf8))
            XCTAssertEqual(result.sessionGeneration, 73)
            XCTAssertEqual(result.renditions.count, 2)

            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.requestCount, 1)
            XCTAssertEqual(snapshot.sessionIDs.count, 1)
            XCTAssertTrue(snapshot.sessionIDs[0].utf8.elementsEqual(sessionID.utf8))

            XCTAssertEqual(result.renditions[0].variantID, "video-only")
            XCTAssertEqual(result.renditions[1].variantID, "video-audio")
            let video = try XCTUnwrap(result.rendition(variantID: "video-only"))
            let audio = try XCTUnwrap(result.rendition(variantID: "video-audio"))

            XCTAssertEqual(video.advertisedBandwidthBitsPerSecond, 2_400_000)
            XCTAssertNil(video.videoResourceBandwidthBitsPerSecond)
            XCTAssertNil(video.audioResourceBandwidthBitsPerSecond)
            XCTAssertFalse(video.hasSeparateAudio)
            XCTAssertNil(video.videoResourceCodecs)
            XCTAssertNil(video.audioResourceCodecs)
            XCTAssertNil(video.videoResourceMimeType)
            XCTAssertNil(video.audioResourceMimeType)
            XCTAssertNil(video.advertisedWidth)
            XCTAssertNil(video.advertisedHeight)

            XCTAssertEqual(audio.advertisedBandwidthBitsPerSecond, 5_000_000)
            XCTAssertEqual(audio.videoResourceBandwidthBitsPerSecond, 4_500_000)
            XCTAssertEqual(audio.audioResourceBandwidthBitsPerSecond, 128_000)
            XCTAssertTrue(audio.hasSeparateAudio)
            XCTAssertEqual(audio.videoResourceCodecs, "avc1.640028")
            XCTAssertEqual(audio.audioResourceCodecs, "mp4a.40.2")
            XCTAssertEqual(audio.videoResourceMimeType, "video/mp4")
            XCTAssertEqual(audio.audioResourceMimeType, "audio/mp4")
            XCTAssertEqual(audio.advertisedWidth, 1920)
            XCTAssertEqual(audio.advertisedHeight, 1080)
        }
    }

    func testProductionClientRejectsNFCNFDSessionIDMismatch() async throws {
        let request = try HLSRenditionCatalogRequest(sessionID: "session-e\u{0301}")
        let probe = HLSRenditionCatalogRPCProbe(scenario: .normalizedSessionIDMismatch)

        try await withLoopbackServer(probe: probe) { endpoint in
            let client = GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(3))

            do {
                _ = try await client.getHLSRenditionCatalog(request)
                XCTFail("Expected the canonically equivalent but byte-distinct session ID to be rejected.")
            } catch let error as HLSRenditionCatalogValidationError {
                XCTAssertEqual(error, .invalidResponse)
            }

            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.requestCount, 1)
            XCTAssertTrue(snapshot.sessionIDs[0].utf8.elementsEqual(request.sessionID.utf8))
        }
    }

    func testProductionClientRejectsCatalogsWithoutSessionIDOrGeneration() async throws {
        for scenario in [HLSRenditionCatalogScenario.missingSessionID, .invalidGeneration] {
            let request = try HLSRenditionCatalogRequest(sessionID: "loopback-session")
            let probe = HLSRenditionCatalogRPCProbe(scenario: scenario)

            try await withLoopbackServer(probe: probe) { endpoint in
                let client = GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(3))
                do {
                    _ = try await client.getHLSRenditionCatalog(request)
                    XCTFail("Expected malformed catalog response \(scenario) to be rejected.")
                } catch let error as HLSRenditionCatalogValidationError {
                    XCTAssertEqual(error, .invalidResponse)
                }
                let snapshot = await probe.snapshot()
                XCTAssertEqual(snapshot.requestCount, 1)
            }
        }
    }

    func testProductionClientRejectsDuplicateVariantIDs() async throws {
        let request = try HLSRenditionCatalogRequest(sessionID: "loopback-session")
        let probe = HLSRenditionCatalogRPCProbe(scenario: .duplicateVariantID)

        try await withLoopbackServer(probe: probe) { endpoint in
            let client = GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(3))
            do {
                _ = try await client.getHLSRenditionCatalog(request)
                XCTFail("Expected duplicate variant IDs to be rejected.")
            } catch let error as HLSRenditionCatalogValidationError {
                XCTAssertEqual(error, .invalidResponse)
            }
            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.requestCount, 1)
        }
    }

    func testProductionClientMapsUnimplementedCatalogRPCToStaticUnsupportedOperation() async throws {
        let probe = HLSRenditionCatalogRPCProbe(scenario: .unimplemented)

        try await withLoopbackServer(probe: probe) { endpoint in
            let client = GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(3))
            do {
                _ = try await client.getHLSRenditionCatalog(
                    try HLSRenditionCatalogRequest(sessionID: "loopback-session"))
                XCTFail("Expected the old server route to map to the static unsupported operation.")
            } catch let error as CacheControlClientUnsupportedOperation {
                XCTAssertEqual(error, CacheControlClientUnsupportedOperation.hlsRenditionCatalog)
            }
            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.requestCount, 1)
        }
    }

    func testRequestRejectsInvalidSessionID() {
        XCTAssertThrowsError(try HLSRenditionCatalogRequest(sessionID: "")) { error in
            XCTAssertEqual(error as? HLSRenditionCatalogValidationError, .invalidRequest)
        }
    }

    func testPreCancelledCallDoesNotIssueRPC() async throws {
        let probe = HLSRenditionCatalogRPCProbe(scenario: .valid)
        let request = try HLSRenditionCatalogRequest(sessionID: "loopback-session")

        try await withLoopbackServer(probe: probe) { endpoint in
            let client = GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(3))
            let call = Task { () -> Result<HLSRenditionCatalog, Error> in
                await probe.waitAtPreCall()
                do {
                    return .success(try await client.getHLSRenditionCatalog(request))
                } catch {
                    return .failure(error)
                }
            }
            defer { call.cancel() }

            await probe.waitForPreCall()
            call.cancel()
            await probe.releasePreCall()

            let result = await call.value
            if case .success = result {
                XCTFail("Expected the cancelled call not to start.")
            } else if case .failure(let error) = result, !(error is CancellationError) {
                XCTFail("Expected CancellationError, received \(type(of: error)).")
            }
            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.requestCount, 0)
        }
    }

    func testInFlightCancellationTakesPrecedenceOverMalformedReply() async throws {
        let probe = HLSRenditionCatalogRPCProbe(scenario: .heldMalformedResponse)
        let request = try HLSRenditionCatalogRequest(sessionID: "loopback-session")

        try await withLoopbackServer(probe: probe) { endpoint in
            let client = GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(3))
            let call = Task { () -> Result<HLSRenditionCatalog, Error> in
                let result: Result<HLSRenditionCatalog, Error>
                do {
                    result = .success(try await client.getHLSRenditionCatalog(request))
                } catch {
                    result = .failure(error)
                }
                await probe.callCompleted()
                await probe.releaseHeldResponse()
                return result
            }
            defer { call.cancel() }

            await probe.waitForRequestOrCallCompletion()
            call.cancel()
            await probe.releaseHeldResponse()

            let result = await call.value
            if case .success = result {
                XCTFail("Expected the cancelled in-flight call to fail.")
            } else if case .failure(let error) = result, !(error is CancellationError) {
                XCTFail("Expected CancellationError to take precedence, received \(type(of: error)).")
            }
            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.requestCount, 1)
        }
    }

    private func withLoopbackServer(
        probe: HLSRenditionCatalogRPCProbe,
        operation: (CacheServerEndpoint) async throws -> Void
    ) async throws {
        try await withGRPCServer(
            transport: .http2NIOPosix(
                address: .ipv4(host: "127.0.0.1", port: 0),
                transportSecurity: .plaintext
            ),
            services: [HLSRenditionCatalogTestService(probe: probe)]
        ) { server in
            let address = try await server.transport.listeningAddress
            guard let port = address.ipv4?.port else {
                throw HLSRenditionCatalogFixtureError.invalidListeningAddress
            }
            try await operation(CacheServerEndpoint(host: "127.0.0.1", port: port))
        }
    }
}

private enum HLSRenditionCatalogFixtureError: Error {
    case invalidListeningAddress
}

private enum HLSRenditionCatalogScenario: Equatable, Sendable {
    case valid
    case normalizedSessionIDMismatch
    case missingSessionID
    case invalidGeneration
    case duplicateVariantID
    case unimplemented
    case heldMalformedResponse
}

private actor HLSRenditionCatalogRPCProbe {
    struct Snapshot: Sendable {
        let requestCount: Int
        let sessionIDs: [String]
    }

    let scenario: HLSRenditionCatalogScenario
    private var sessionIDs: [String] = []
    private var requestOrCompletionWaiters: [CheckedContinuation<Void, Never>] = []
    private var responseWaiters: [CheckedContinuation<Void, Never>] = []
    private var preCallWaiters: [CheckedContinuation<Void, Never>] = []
    private var preCallReleaseWaiters: [CheckedContinuation<Void, Never>] = []
    private var responseReleased = false
    private var clientCallCompleted = false
    private var preCallReached = false
    private var preCallReleased = false

    init(scenario: HLSRenditionCatalogScenario) {
        self.scenario = scenario
    }

    func record(_ request: TvosNetPlayer_V1_GetHlsRenditionCatalogRequest) {
        sessionIDs.append(request.sessionID)
        resumeRequestOrCompletionWaiters()
    }

    func waitForRequestOrCallCompletion() async {
        guard sessionIDs.isEmpty, !clientCallCompleted else { return }
        await withCheckedContinuation { requestOrCompletionWaiters.append($0) }
    }

    func callCompleted() {
        clientCallCompleted = true
        resumeRequestOrCompletionWaiters()
    }

    func waitAtPreCall() async {
        preCallReached = true
        let arrivalWaiters = preCallWaiters
        preCallWaiters.removeAll()
        arrivalWaiters.forEach { $0.resume() }
        guard !preCallReleased else { return }
        await withCheckedContinuation { preCallReleaseWaiters.append($0) }
    }

    func waitForPreCall() async {
        guard !preCallReached else { return }
        await withCheckedContinuation { preCallWaiters.append($0) }
    }

    func releasePreCall() {
        preCallReleased = true
        let waiters = preCallReleaseWaiters
        preCallReleaseWaiters.removeAll()
        waiters.forEach { $0.resume() }
    }

    func waitForResponseRelease() async {
        guard !responseReleased else { return }
        await withCheckedContinuation { responseWaiters.append($0) }
    }

    func releaseHeldResponse() {
        responseReleased = true
        let waiters = responseWaiters
        responseWaiters.removeAll()
        waiters.forEach { $0.resume() }
    }

    func snapshot() -> Snapshot {
        Snapshot(requestCount: sessionIDs.count, sessionIDs: sessionIDs)
    }

    private func resumeRequestOrCompletionWaiters() {
        let waiters = requestOrCompletionWaiters
        requestOrCompletionWaiters.removeAll()
        waiters.forEach { $0.resume() }
    }
}

private struct HLSRenditionCatalogTestService: RegistrableRPCService {
    let probe: HLSRenditionCatalogRPCProbe

    func registerMethods<Transport: ServerTransport>(with router: inout RPCRouter<Transport>) {
        let probe = self.probe
        let scenario = probe.scenario
        Self.registerUnary(
            TvosNetPlayer_V1_CacheService.Method.GetHlsRenditionCatalog.descriptor,
            with: &router
        ) { (request: TvosNetPlayer_V1_GetHlsRenditionCatalogRequest) in
            await probe.record(request)
            if scenario == .heldMalformedResponse {
                await probe.waitForResponseRelease()
            }
            if scenario == .unimplemented {
                throw RPCError(code: .unimplemented, message: "Private server implementation detail.")
            }
            return Self.catalog(for: request, scenario: scenario)
        }
    }

    private static func registerUnary<Transport, Input, Output>(
        _ descriptor: MethodDescriptor,
        with router: inout RPCRouter<Transport>,
        handler: @escaping @Sendable (Input) async throws -> Output
    )
    where
        Transport: ServerTransport,
        Input: SwiftProtobuf.Message & Sendable,
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

    private static func catalog(
        for request: TvosNetPlayer_V1_GetHlsRenditionCatalogRequest,
        scenario: HLSRenditionCatalogScenario
    ) -> TvosNetPlayer_V1_HlsRenditionCatalog {
        var response = TvosNetPlayer_V1_HlsRenditionCatalog()
        response.sessionID = request.sessionID
        response.sessionGeneration = 73

        switch scenario {
        case .valid, .unimplemented, .heldMalformedResponse:
            response.renditions = [videoOnlyRendition(), videoAudioRendition()]
        case .normalizedSessionIDMismatch:
            response.sessionID = "session-\u{00E9}"
            response.renditions = [videoOnlyRendition()]
        case .missingSessionID:
            response.sessionID = ""
            response.renditions = [videoOnlyRendition()]
        case .invalidGeneration:
            response.sessionGeneration = 0
            response.renditions = [videoOnlyRendition()]
        case .duplicateVariantID:
            response.renditions = [videoOnlyRendition(), videoOnlyRendition()]
        }

        if scenario == .heldMalformedResponse {
            response.sessionID = "different-session"
            response.sessionGeneration = 0
        }
        return response
    }

    private static func videoOnlyRendition() -> TvosNetPlayer_V1_HlsRenditionDescriptor {
        var rendition = TvosNetPlayer_V1_HlsRenditionDescriptor()
        rendition.variantID = "video-only"
        rendition.advertisedBandwidthBitsPerSecond = 2_400_000
        return rendition
    }

    private static func videoAudioRendition() -> TvosNetPlayer_V1_HlsRenditionDescriptor {
        var rendition = TvosNetPlayer_V1_HlsRenditionDescriptor()
        rendition.variantID = "video-audio"
        rendition.advertisedBandwidthBitsPerSecond = 5_000_000
        rendition.videoResourceBandwidthBitsPerSecond = 4_500_000
        rendition.audioResourceBandwidthBitsPerSecond = 128_000
        rendition.hasSeparateAudio_p = true
        rendition.videoResourceCodecs = "avc1.640028"
        rendition.audioResourceCodecs = "mp4a.40.2"
        rendition.videoResourceMimeType = "video/mp4"
        rendition.audioResourceMimeType = "audio/mp4"
        rendition.advertisedWidth = 1920
        rendition.advertisedHeight = 1080
        return rendition
    }
}
