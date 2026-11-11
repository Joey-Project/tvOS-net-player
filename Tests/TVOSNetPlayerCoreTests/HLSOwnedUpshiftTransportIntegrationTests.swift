import AVFoundation
import Combine
import Foundation
import GRPCCore
import GRPCNIOTransportHTTP2Posix
import GRPCProtobuf
import SwiftProtobuf
import XCTest
@testable import TVOSNetPlayerCacheClient
@testable import TVOSNetPlayerCore

/// Exercises the owned upshift assessment through the production client and loopback HTTP/2.
@MainActor
final class HLSOwnedUpshiftTransportIntegrationTests: XCTestCase {
    func testCatalogThenReadinessPreservesNFCAndNFDSessionBytesAndTargetRates() async throws {
        let normalizedIDs = ["session-é", "session-e\u{0301}"]
        let probe = HLSOwnedUpshiftRPCProbe(scenario: .valid)

        try await withLoopbackServer(probe: probe) { endpoint in
            for sessionID in normalizedIDs {
                let model = try Self.makePlayingModel(endpoint: endpoint, sessionID: sessionID)
                let result = await model.assessHLSUpshift(
                    targetVariantID: "video-audio",
                    windowStartSeconds: 25,
                    windowDurationSeconds: 4,
                    targetDecoderSupported: true,
                    serverSummary: Self.supportingServerSummary
                )

                guard let assessment = result else {
                    XCTFail("Expected a current assessment for the loopback fixture")
                    continue
                }
                XCTAssertTrue(model.isCurrentHLSUpshiftAssessment(assessment))
                XCTAssertTrue(assessment.snapshot.isPlaying)
                XCTAssertEqual(assessment.snapshot.playheadSeconds, 5)
                XCTAssertEqual(assessment.snapshot.contiguousBufferedEndSeconds, 50)
                XCTAssertEqual(assessment.snapshot.playbackRate, 1)
                XCTAssertEqual(assessment.rendition.variantID, "video-audio")
                XCTAssertEqual(assessment.rendition.advertisedBandwidthBitsPerSecond, 5_000_000)
                XCTAssertEqual(assessment.rendition.videoResourceBandwidthBitsPerSecond, 4_500_000)
                XCTAssertEqual(assessment.rendition.audioResourceBandwidthBitsPerSecond, 128_000)
                XCTAssertTrue(assessment.rendition.hasSeparateAudio)
                XCTAssertNil(assessment.rendition.videoResourceCodecs)
                XCTAssertNil(assessment.rendition.audioResourceCodecs)
                XCTAssertNil(assessment.rendition.videoResourceMimeType)
                XCTAssertNil(assessment.rendition.audioResourceMimeType)
                XCTAssertNil(assessment.rendition.advertisedWidth)
                XCTAssertNil(assessment.rendition.advertisedHeight)
                XCTAssertEqual(assessment.recommendation.outcome, .cacheBackedOpportunity)
                XCTAssertEqual(assessment.recommendation.sessionGeneration, 73)
                XCTAssertTrue(assessment.recommendation.sessionID.utf8.elementsEqual(sessionID.utf8))
                XCTAssertEqual(assessment.recommendation.variantID, "video-audio")
                XCTAssertEqual(assessment.recommendation.windowStartSeconds, 25)
                XCTAssertEqual(assessment.recommendation.windowDurationSeconds, 4)
            }

            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.catalogSessionIDs.count, 2)
            XCTAssertEqual(snapshot.readinessRequests.count, 2)
            let firstCatalogSessionID = try XCTUnwrap(snapshot.catalogSessionIDs.first)
            let secondCatalogSessionID = try XCTUnwrap(snapshot.catalogSessionIDs.dropFirst().first)
            let firstReadinessRequest = try XCTUnwrap(snapshot.readinessRequests.first)
            let secondReadinessRequest = try XCTUnwrap(snapshot.readinessRequests.dropFirst().first)
            XCTAssertTrue(firstCatalogSessionID.utf8.elementsEqual(normalizedIDs[0].utf8))
            XCTAssertTrue(secondCatalogSessionID.utf8.elementsEqual(normalizedIDs[1].utf8))
            XCTAssertFalse(firstCatalogSessionID.utf8.elementsEqual(secondCatalogSessionID.utf8))
            XCTAssertTrue(
                snapshot.readinessRequests.allSatisfy {
                    $0.variantID == "video-audio"
                        && $0.windowStartSeconds == 25
                        && $0.windowDurationSeconds == 4
                })
            XCTAssertTrue(firstReadinessRequest.sessionID.utf8.elementsEqual(normalizedIDs[0].utf8))
            XCTAssertTrue(secondReadinessRequest.sessionID.utf8.elementsEqual(normalizedIDs[1].utf8))
        }
    }

    func testReadinessGenerationMismatchNeverProducesOpportunity() async throws {
        let sessionID = "generation-mismatch-session"
        let probe = HLSOwnedUpshiftRPCProbe(scenario: .generationMismatch)

        try await withLoopbackServer(probe: probe) { endpoint in
            let model = try Self.makePlayingModel(endpoint: endpoint, sessionID: sessionID)
            let result = await model.assessHLSUpshift(
                targetVariantID: "video-audio",
                windowStartSeconds: 25,
                windowDurationSeconds: 4,
                targetDecoderSupported: true,
                serverSummary: Self.supportingServerSummary
            )

            if let assessment = result {
                XCTAssertNotEqual(assessment.recommendation.outcome, .cacheBackedOpportunity)
            }
            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.catalogSessionIDs.count, 1)
            XCTAssertEqual(snapshot.readinessRequests.count, 1)
        }
    }

    func testUnsupportedOrMismatchedCatalogStopsBeforeReadinessRPC() async throws {
        for scenario in [HLSOwnedUpshiftScenario.unimplementedCatalog, .mismatchedCatalogSession] {
            let sessionID = "catalog-rejection-session"
            let probe = HLSOwnedUpshiftRPCProbe(scenario: scenario)

            try await withLoopbackServer(probe: probe) { endpoint in
                let model = try Self.makePlayingModel(endpoint: endpoint, sessionID: sessionID)
                let result = await model.assessHLSUpshift(
                    targetVariantID: "video-audio",
                    windowStartSeconds: 25,
                    windowDurationSeconds: 4,
                    targetDecoderSupported: true,
                    serverSummary: Self.supportingServerSummary
                )

                XCTAssertNil(result)
                let snapshot = await probe.snapshot()
                XCTAssertEqual(snapshot.catalogSessionIDs.count, 1)
                XCTAssertTrue(snapshot.catalogSessionIDs[0].utf8.elementsEqual(sessionID.utf8))
                XCTAssertTrue(snapshot.readinessRequests.isEmpty)
            }
        }
    }

    func testOperationErrorAfterCatalogRPCIsRethrownAfterGracefulServerShutdown() async throws {
        let probe = HLSOwnedUpshiftRPCProbe(scenario: .valid)

        do {
            try await withLoopbackServer(probe: probe) { endpoint in
                let model = try Self.makePlayingModel(endpoint: endpoint, sessionID: "cleanup-error-session")
                let result = await model.assessHLSUpshift(
                    targetVariantID: "video-audio",
                    windowStartSeconds: 25,
                    windowDurationSeconds: 4,
                    targetDecoderSupported: true,
                    serverSummary: Self.supportingServerSummary
                )
                XCTAssertNotNil(result)
                let snapshot = await probe.snapshot()
                XCTAssertEqual(snapshot.catalogSessionIDs.count, 1)
                throw HLSOwnedUpshiftFixtureError.intentionalOperationFailure
            }
            XCTFail("Expected the fixture operation error to be rethrown")
        } catch let error as HLSOwnedUpshiftFixtureError {
            XCTAssertEqual(error, .intentionalOperationFailure)
        }
    }

    private static let supportingServerSummary = CacheServerSummary(
        id: "loopback-owned-upshift-server",
        name: "Loopback",
        version: "test",
        mediaBaseURIs: [],
        capabilities: [
            CacheServerCapability.hlsRenditionCatalog,
            CacheServerCapability.hlsPlaybackReadiness,
        ]
    )

    private static func makePlayingModel(
        endpoint: CacheServerEndpoint,
        sessionID: String
    ) throws -> PlayerViewModel {
        let session = try XCTUnwrap(
            PlayerHLSPlaybackSessionContext(
                serverID: supportingServerSummary.id,
                sessionID: sessionID
            ))
        let playbackURI = "http://127.0.0.1:1/media/owned-upshift.m3u8"
        let model = PlayerViewModel(
            autoplay: false,
            playbackProgressReportInterval: nil,
            cacheClientFactory: { endpoint in
                GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(3))
            }
        )
        model.playbackNotificationCenter = NotificationCenter()
        let item = AVPlayerItem(asset: AVMutableComposition())
        let player = AVPlayer(playerItem: item)
        player.volume = 0
        player.isMuted = true
        let controller = HLSOwnedUpshiftPlaybackController(currentItem: item)
        controller.playbackTime = CMTime(seconds: 5, preferredTimescale: 600)
        controller.loadedRanges = [
            CMTimeRange(
                start: .zero,
                duration: CMTime(seconds: 50, preferredTimescale: 600)
            )
        ]
        model.playbackControllerFactory = { _ in (player, controller) }
        model.playbackSnapshotNowNanoseconds = { 5_000_000_000 }

        let context = PlayerPlaybackProgressContext(
            endpoint: endpoint,
            playbackURI: playbackURI,
            hlsSession: session
        )
        guard model.loadTransient(streamURLText: playbackURI, progressContext: context) else {
            throw HLSOwnedUpshiftFixtureError.invalidPlaybackURI
        }
        return model
    }

    private func withLoopbackServer(
        probe: HLSOwnedUpshiftRPCProbe,
        operation: (CacheServerEndpoint) async throws -> Void
    ) async throws {
        var operationResult: Result<Void, Error>?
        try await withGRPCServer(
            transport: .http2NIOPosix(
                address: .ipv4(host: "127.0.0.1", port: 0),
                transportSecurity: .plaintext
            ),
            services: [HLSOwnedUpshiftTestService(probe: probe)]
        ) { server in
            let address = try await server.transport.listeningAddress
            guard let port = address.ipv4?.port else {
                throw HLSOwnedUpshiftFixtureError.invalidListeningAddress
            }
            do {
                try await operation(CacheServerEndpoint(host: "127.0.0.1", port: port))
                operationResult = .success(())
            } catch {
                operationResult = .failure(error)
            }
        }
        try operationResult?.get()
    }
}

private enum HLSOwnedUpshiftFixtureError: Error, Equatable {
    case invalidListeningAddress
    case invalidPlaybackURI
    case intentionalOperationFailure
}

private enum HLSOwnedUpshiftScenario: Equatable, Sendable {
    case valid
    case generationMismatch
    case unimplementedCatalog
    case mismatchedCatalogSession
}

private actor HLSOwnedUpshiftRPCProbe {
    struct ReadinessRequest: Equatable, Sendable {
        let sessionID: String
        let variantID: String
        let windowStartSeconds: Double
        let windowDurationSeconds: Double
    }

    struct Snapshot: Sendable {
        let catalogSessionIDs: [String]
        let readinessRequests: [ReadinessRequest]
    }

    let scenario: HLSOwnedUpshiftScenario
    private var catalogSessionIDs: [String] = []
    private var readinessRequests: [ReadinessRequest] = []

    init(scenario: HLSOwnedUpshiftScenario) {
        self.scenario = scenario
    }

    func recordCatalog(_ request: TvosNetPlayer_V1_GetHlsRenditionCatalogRequest) {
        catalogSessionIDs.append(request.sessionID)
    }

    func recordReadiness(_ request: TvosNetPlayer_V1_GetHlsPlaybackReadinessRequest) {
        readinessRequests.append(
            ReadinessRequest(
                sessionID: request.sessionID,
                variantID: request.variantID,
                windowStartSeconds: request.windowStartSeconds,
                windowDurationSeconds: request.windowDurationSeconds
            ))
    }

    func snapshot() -> Snapshot {
        Snapshot(catalogSessionIDs: catalogSessionIDs, readinessRequests: readinessRequests)
    }
}

private struct HLSOwnedUpshiftTestService: RegistrableRPCService {
    let probe: HLSOwnedUpshiftRPCProbe

    func registerMethods<Transport: ServerTransport>(with router: inout RPCRouter<Transport>) {
        let probe = self.probe
        let scenario = probe.scenario
        Self.registerUnary(
            TvosNetPlayer_V1_CacheService.Method.GetHlsRenditionCatalog.descriptor,
            with: &router
        ) { (request: TvosNetPlayer_V1_GetHlsRenditionCatalogRequest) in
            await probe.recordCatalog(request)
            if scenario == .unimplementedCatalog {
                throw RPCError(code: .unimplemented, message: "Private fixture detail.")
            }
            return Self.catalog(for: request, scenario: scenario)
        }
        Self.registerUnary(
            TvosNetPlayer_V1_CacheService.Method.GetHlsPlaybackReadiness.descriptor,
            with: &router
        ) { (request: TvosNetPlayer_V1_GetHlsPlaybackReadinessRequest) in
            await probe.recordReadiness(request)
            return Self.readiness(for: request, scenario: scenario)
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
        scenario: HLSOwnedUpshiftScenario
    ) -> TvosNetPlayer_V1_HlsRenditionCatalog {
        var response = TvosNetPlayer_V1_HlsRenditionCatalog()
        response.sessionID = request.sessionID
        response.sessionGeneration = 73
        if scenario == .mismatchedCatalogSession {
            response.sessionID = "different-session"
        }
        response.renditions = [rendition()]
        return response
    }

    private static func rendition() -> TvosNetPlayer_V1_HlsRenditionDescriptor {
        var rendition = TvosNetPlayer_V1_HlsRenditionDescriptor()
        rendition.variantID = "video-audio"
        rendition.advertisedBandwidthBitsPerSecond = 5_000_000
        rendition.videoResourceBandwidthBitsPerSecond = 4_500_000
        rendition.audioResourceBandwidthBitsPerSecond = 128_000
        rendition.hasSeparateAudio_p = true
        return rendition
    }

    private static func readiness(
        for request: TvosNetPlayer_V1_GetHlsPlaybackReadinessRequest,
        scenario: HLSOwnedUpshiftScenario
    ) -> TvosNetPlayer_V1_HlsPlaybackReadiness {
        var response = TvosNetPlayer_V1_HlsPlaybackReadiness()
        response.sessionID = request.sessionID
        response.variantID = request.variantID
        response.windowStartSeconds = request.windowStartSeconds
        response.windowDurationSeconds = request.windowDurationSeconds
        response.sessionGeneration = scenario == .generationMismatch ? 74 : 73
        response.cacheState = .readable
        response.cacheReason = .none
        return response
    }
}

@MainActor
private final class HLSOwnedUpshiftPlaybackController: PlayerPlaybackControlling {
    let currentItem: AVPlayerItem?
    let readinessStatus: AVPlayerItem.Status = .readyToPlay
    let timeControlStatus: AVPlayer.TimeControlStatus = .playing
    var defaultRate: Float = 1
    let actualPlaybackRate: Float? = 1
    var loadedRanges: [CMTimeRange] = []
    var playbackTime = CMTime.zero
    var itemDuration: CMTime = .indefinite

    var loadedMediaTimeRanges: [CMTimeRange] { loadedRanges }

    init(currentItem: AVPlayerItem) {
        self.currentItem = currentItem
    }

    func currentTime() -> CMTime { playbackTime }
    func play() {}
    func pause() {}
    func seek(to time: CMTime, toleranceBefore: CMTime, toleranceAfter: CMTime) {}
    func seekForPlayback(to time: CMTime, completion: @escaping @MainActor @Sendable (Bool) -> Void) {}

    func observeReadiness(
        _ observer: @escaping @MainActor @Sendable (AVPlayerItem.Status) -> Void
    ) -> AnyCancellable {
        observer(readinessStatus)
        return AnyCancellable {}
    }
}
