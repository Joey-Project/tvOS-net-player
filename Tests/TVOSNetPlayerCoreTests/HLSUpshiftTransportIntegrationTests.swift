import Foundation
import GRPCCore
import GRPCNIOTransportHTTP2Posix
import GRPCProtobuf
import SwiftProtobuf
import XCTest
@testable import TVOSNetPlayerCacheClient
@testable import TVOSNetPlayerCore

/// Exercises HLS readiness through the production client and loopback HTTP/2 serialization.
@MainActor
final class HLSUpshiftTransportIntegrationTests: XCTestCase {
    private let request = HLSPlaybackReadinessRequest(
        sessionID: "loopback-session",
        variantID: "loopback-variant",
        windowStartSeconds: 12.5,
        windowDurationSeconds: 8
    )

    func testProductionClientDispatchesExactRequestAndReturnsGeneration() async throws {
        let probe = HLSReadinessRPCProbe(scenario: .success)
        try await withLoopbackServer(probe: probe) { endpoint in
            let client = GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(3))
            let result = try await client.getHLSPlaybackReadiness(self.request)

            XCTAssertEqual(result.sessionID, self.request.sessionID)
            XCTAssertEqual(result.variantID, self.request.variantID)
            XCTAssertEqual(result.windowStartSeconds, self.request.windowStartSeconds)
            XCTAssertEqual(result.windowDurationSeconds, self.request.windowDurationSeconds)
            XCTAssertEqual(result.sessionGeneration, 73)
            XCTAssertEqual(result.cacheState, .readable)
            XCTAssertEqual(result.cacheReason, .none)
            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.requestCount, 1)
            XCTAssertEqual(snapshot.requests, [HLSReadinessRPCProbe.ObservedRequest(self.request)])
        }
    }

    func testProductionClientPreservesVideoAudioAndOptionalZeroVersusAbsent() async throws {
        let probe = HLSReadinessRPCProbe(scenario: .origins)
        try await withLoopbackServer(probe: probe) { endpoint in
            let client = GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(3))
            let result = try await client.getHLSPlaybackReadiness(self.request)

            XCTAssertEqual(result.videoOrigin?.ageMillis, 120)
            XCTAssertEqual(result.videoOrigin?.throughputBitsPerSecond, 4_000_000)
            XCTAssertEqual(result.videoOrigin?.firstByteLatencyMicros, 0)
            XCTAssertEqual(result.videoOrigin?.freshnessLimitMillis, 5_000)
            XCTAssertEqual(result.audioOrigin?.ageMillis, 240)
            XCTAssertEqual(result.audioOrigin?.throughputBitsPerSecond, 192_000)
            XCTAssertNil(result.audioOrigin?.firstByteLatencyMicros)
            XCTAssertEqual(result.audioOrigin?.freshnessLimitMillis, 8_000)
            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.requestCount, 1)
        }
    }

    func testProductionClientPreservesUnknownWireEnums() async throws {
        let probe = HLSReadinessRPCProbe(scenario: .unknownEnums)
        try await withLoopbackServer(probe: probe) { endpoint in
            let client = GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(3))
            let result = try await client.getHLSPlaybackReadiness(self.request)

            XCTAssertEqual(result.cacheState, .unrecognized(91))
            XCTAssertEqual(result.cacheReason, .unrecognized(92))
            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.requestCount, 1)
        }
    }

    func testProductionClientRejectsMismatchedResponseIdentityAndWindow() async throws {
        let probe = HLSReadinessRPCProbe(scenario: .mismatchedIdentityThenWindow)
        try await withLoopbackServer(probe: probe) { endpoint in
            let client = GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(3))

            do {
                _ = try await client.getHLSPlaybackReadiness(self.request)
                XCTFail("Expected mismatched response identity to be rejected.")
            } catch let error as HLSPlaybackReadinessValidationError {
                XCTAssertEqual(error, .invalidResponse)
            }

            do {
                _ = try await client.getHLSPlaybackReadiness(self.request)
                XCTFail("Expected mismatched response window to be rejected.")
            } catch let error as HLSPlaybackReadinessValidationError {
                XCTAssertEqual(error, .invalidResponse)
            }
            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.requestCount, 2)
            XCTAssertEqual(
                snapshot.requests,
                [
                    HLSReadinessRPCProbe.ObservedRequest(self.request),
                    HLSReadinessRPCProbe.ObservedRequest(self.request),
                ])
        }
    }

    func testProductionClientMapsUnimplementedToExactStaticUnsupportedOperation() async throws {
        let probe = HLSReadinessRPCProbe(scenario: .unimplemented)
        try await withLoopbackServer(probe: probe) { endpoint in
            let client = GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(3))
            do {
                _ = try await client.getHLSPlaybackReadiness(self.request)
                XCTFail("Expected the server's unimplemented response to map to unsupported operation.")
            } catch let error as CacheControlClientUnsupportedOperation {
                XCTAssertEqual(error, CacheControlClientUnsupportedOperation.hlsPlaybackReadiness)
            }
            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.requestCount, 1)
        }
    }

    func testProductionClientMapsBoundedDeadlineToSanitizedTypedError() async throws {
        let probe = HLSReadinessRPCProbe(scenario: .holdResponse)
        try await withLoopbackServer(probe: probe) { endpoint in
            let client = GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(2))
            let call = Task { () -> Result<HLSPlaybackReadiness, Error> in
                let result: Result<HLSPlaybackReadiness, Error>
                do {
                    result = .success(try await client.getHLSPlaybackReadiness(self.request))
                } catch {
                    result = .failure(error)
                }
                await probe.rpcCallCompleted()
                await probe.releaseHeldResponse()
                return result
            }
            await probe.waitForRequestOrCallCompletion()
            let result = await call.value
            switch result {
            case .success:
                XCTFail("Expected the bounded RPC deadline to expire.")
            case .failure(let error as CacheTaskControlError):
                XCTAssertEqual(error, .deadlineExceeded)
                XCTAssertEqual(error.errorDescription, "The cache server did not respond before the deadline.")
            case .failure(let error):
                XCTFail("Expected a sanitized deadline error, received \(type(of: error)).")
            }
            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.requestCount, 1)
        }
    }

    func testAssessmentServiceReturnsCacheBackedOpportunityFromLoopbackReadiness() async throws {
        let probe = HLSReadinessRPCProbe(scenario: .success)
        try await withLoopbackServer(probe: probe) { endpoint in
            let client = GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(3))
            let service = HLSUpshiftAssessmentService(
                client: client,
                serverSummary: Self.supportingServerSummary,
                monotonicNowNanoseconds: { 1_000_000_000 }
            )

            let recommendation = await service.assess(Self.assessmentContext())

            XCTAssertEqual(recommendation.outcome, .cacheBackedOpportunity)
            XCTAssertEqual(recommendation.sessionGeneration, 73)
            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.requestCount, 1)
            XCTAssertEqual(snapshot.requests, [HLSReadinessRPCProbe.ObservedRequest(Self.assessmentRequest)])
        }
    }

    func testAssessmentServiceReturnsOriginPrewarmOpportunityWithFreshVideoAndAudioEvidence() async throws {
        let probe = HLSReadinessRPCProbe(scenario: .missingWithOrigins)
        try await withLoopbackServer(probe: probe) { endpoint in
            let client = GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(3))
            let service = HLSUpshiftAssessmentService(
                client: client,
                serverSummary: Self.supportingServerSummary,
                monotonicNowNanoseconds: { 1_000_000_000 }
            )

            let recommendation = await service.assess(Self.assessmentContext())

            guard case .originPrewarmOpportunity(let estimate) = recommendation.outcome else {
                return XCTFail("Expected a bounded origin prewarm estimate, received \(recommendation.outcome).")
            }
            XCTAssertGreaterThan(estimate.estimatedPreparationSeconds, 0)
            XCTAssertLessThan(estimate.estimatedPreparationSeconds, estimate.availablePreparationSeconds)
            XCTAssertEqual(recommendation.sessionGeneration, 73)
            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.requestCount, 1)
            XCTAssertEqual(snapshot.requests, [HLSReadinessRPCProbe.ObservedRequest(Self.assessmentRequest)])
        }
    }

    func testAssessmentServiceHoldsUnsupportedServerWithoutIssuingRPC() async throws {
        let probe = HLSReadinessRPCProbe(scenario: .success)
        try await withLoopbackServer(probe: probe) { endpoint in
            let client = GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(3))
            let service = HLSUpshiftAssessmentService(
                client: client,
                serverSummary: CacheServerSummary(
                    id: "loopback-server",
                    name: "Loopback",
                    version: "test",
                    mediaBaseURIs: [],
                    capabilities: []
                ),
                monotonicNowNanoseconds: { 1_000_000_000 }
            )

            let recommendation = await service.assess(Self.assessmentContext())

            XCTAssertEqual(recommendation.outcome, .hold(.unsupportedServer))
            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.requestCount, 0)
        }
    }

    private static let assessmentRequest = HLSPlaybackReadinessRequest(
        sessionID: "loopback-session",
        variantID: "loopback-variant",
        windowStartSeconds: 40,
        windowDurationSeconds: 8
    )

    private static let supportingServerSummary = CacheServerSummary(
        id: "loopback-server",
        name: "Loopback",
        version: "test",
        mediaBaseURIs: [],
        capabilities: [CacheServerCapability.hlsPlaybackReadiness]
    )

    private static func assessmentContext() -> HLSUpshiftContext {
        HLSUpshiftContext(
            request: assessmentRequest,
            isPlaying: true,
            playheadSeconds: 0,
            contiguousBufferedEndSeconds: 60,
            playbackRate: 1,
            targetVideoBitrateBitsPerSecond: 256_000,
            targetAudioBitrateBitsPerSecond: 32_000,
            requiresAudio: true,
            targetDecoderSupported: true,
            expectedSessionGeneration: 73,
            contextAgeMillis: 0
        )
    }

    private func withLoopbackServer(
        probe: HLSReadinessRPCProbe,
        operation: (CacheServerEndpoint) async throws -> Void
    ) async throws {
        try await withGRPCServer(
            transport: .http2NIOPosix(
                address: .ipv4(host: "127.0.0.1", port: 0),
                transportSecurity: .plaintext
            ),
            services: [HLSReadinessTestService(probe: probe)]
        ) { server in
            let address = try await server.transport.listeningAddress
            guard let port = address.ipv4?.port else {
                throw HLSReadinessFixtureError.invalidListeningAddress
            }
            try await operation(CacheServerEndpoint(host: "127.0.0.1", port: port))
        }
    }
}

private enum HLSReadinessFixtureError: Error {
    case invalidListeningAddress
}

private enum HLSReadinessScenario: Equatable, Sendable {
    case success
    case origins
    case missingWithOrigins
    case unknownEnums
    case mismatchedIdentityThenWindow
    case unimplemented
    case holdResponse
}

private actor HLSReadinessRPCProbe {
    struct ObservedRequest: Equatable, Sendable {
        let sessionID: String
        let variantID: String
        let windowStartSeconds: Double
        let windowDurationSeconds: Double

        init(_ request: HLSPlaybackReadinessRequest) {
            sessionID = request.sessionID
            variantID = request.variantID
            windowStartSeconds = request.windowStartSeconds
            windowDurationSeconds = request.windowDurationSeconds
        }

        init(_ request: TvosNetPlayer_V1_GetHlsPlaybackReadinessRequest) {
            sessionID = request.sessionID
            variantID = request.variantID
            windowStartSeconds = request.windowStartSeconds
            windowDurationSeconds = request.windowDurationSeconds
        }
    }

    struct Snapshot: Sendable {
        let requestCount: Int
        let requests: [ObservedRequest]
    }

    let scenario: HLSReadinessScenario
    private var requests: [ObservedRequest] = []
    private var requestOrCompletionWaiters: [CheckedContinuation<Void, Never>] = []
    private var responseWaiters: [CheckedContinuation<Void, Never>] = []
    private var responseReleased = false
    private var rpcCallHasCompleted = false

    init(scenario: HLSReadinessScenario) {
        self.scenario = scenario
    }

    func record(_ request: TvosNetPlayer_V1_GetHlsPlaybackReadinessRequest) {
        requests.append(ObservedRequest(request))
        resumeRequestOrCompletionWaiters()
    }

    func waitForRequestOrCallCompletion() async {
        guard requests.isEmpty, !rpcCallHasCompleted else { return }
        await withCheckedContinuation { requestOrCompletionWaiters.append($0) }
    }

    func rpcCallCompleted() {
        rpcCallHasCompleted = true
        resumeRequestOrCompletionWaiters()
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
        Snapshot(requestCount: requests.count, requests: requests)
    }

    private func resumeRequestOrCompletionWaiters() {
        let waiters = requestOrCompletionWaiters
        requestOrCompletionWaiters.removeAll()
        waiters.forEach { $0.resume() }
    }
}

private struct HLSReadinessTestService: RegistrableRPCService {
    let probe: HLSReadinessRPCProbe

    func registerMethods<Transport: ServerTransport>(with router: inout RPCRouter<Transport>) {
        let probe = self.probe
        let scenario = probe.scenario
        Self.registerUnary(
            TvosNetPlayer_V1_CacheService.Method.GetHlsPlaybackReadiness.descriptor,
            with: &router
        ) { (request: TvosNetPlayer_V1_GetHlsPlaybackReadinessRequest) in
            await probe.record(request)
            if scenario == .holdResponse {
                await probe.waitForResponseRelease()
            }
            if scenario == .unimplemented {
                throw RPCError(code: .unimplemented, message: "Private server implementation detail.")
            }
            let snapshot = await probe.snapshot()
            let index = snapshot.requestCount
            return Self.readiness(for: request, scenario: scenario, requestIndex: index)
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

    private static func readiness(
        for request: TvosNetPlayer_V1_GetHlsPlaybackReadinessRequest,
        scenario: HLSReadinessScenario,
        requestIndex: Int
    ) -> TvosNetPlayer_V1_HlsPlaybackReadiness {
        var response = TvosNetPlayer_V1_HlsPlaybackReadiness()
        response.sessionID = request.sessionID
        response.variantID = request.variantID
        response.windowStartSeconds = request.windowStartSeconds
        response.windowDurationSeconds = request.windowDurationSeconds
        response.sessionGeneration = 73
        response.cacheState = .readable
        response.cacheReason = .none
        response.inspectionBudgetUsedBytes = 16_384

        switch scenario {
        case .origins:
            var video = TvosNetPlayer_V1_HlsOriginSupplyEvidence()
            video.ageMillis = 120
            video.throughputBitsPerSecond = 4_000_000
            video.firstByteLatencyMicros = 0
            video.freshnessLimitMillis = 5_000
            response.videoOrigin = video

            var audio = TvosNetPlayer_V1_HlsOriginSupplyEvidence()
            audio.ageMillis = 240
            audio.throughputBitsPerSecond = 192_000
            audio.freshnessLimitMillis = 8_000
            response.audioOrigin = audio
        case .missingWithOrigins:
            response.cacheState = .missing
            response.cacheReason = .notCached

            var video = TvosNetPlayer_V1_HlsOriginSupplyEvidence()
            video.ageMillis = 120
            video.throughputBitsPerSecond = 4_000_000
            video.firstByteLatencyMicros = 10_000
            video.freshnessLimitMillis = 5_000
            response.videoOrigin = video

            var audio = TvosNetPlayer_V1_HlsOriginSupplyEvidence()
            audio.ageMillis = 240
            audio.throughputBitsPerSecond = 1_000_000
            audio.firstByteLatencyMicros = 10_000
            audio.freshnessLimitMillis = 8_000
            response.audioOrigin = audio
        case .unknownEnums:
            response.cacheState = .UNRECOGNIZED(91)
            response.cacheReason = .UNRECOGNIZED(92)
        case .mismatchedIdentityThenWindow:
            if requestIndex == 1 {
                response.variantID = "different-variant"
            } else {
                response.windowDurationSeconds += 1
            }
        case .success, .unimplemented, .holdResponse:
            break
        }
        return response
    }
}
