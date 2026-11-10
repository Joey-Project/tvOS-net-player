import Foundation
import XCTest
import TVOSNetPlayerCacheClient
@testable import TVOSNetPlayerCore

final class HLSUpshiftAssessmentServiceTests: XCTestCase {
    func testUnsupportedCapabilityFallsBackWithoutCallingClient() async {
        let client = ReadinessTestClient(readiness: makeReadiness(), behavior: .success)
        let service = makeService(client: client, supportsReadiness: false)

        let result = await service.assess(makeContext())

        XCTAssertEqual(result.outcome, .hold(.unsupportedServer))
        let calls = await client.callCount()
        XCTAssertEqual(calls, 0)
    }

    func testLegacyUnsupportedOperationMapsToSanitizedHold() async {
        let client = ReadinessTestClient(readiness: makeReadiness(), behavior: .unsupported)
        let service = makeService(client: client)

        let result = await service.assess(makeContext())

        XCTAssertEqual(result.outcome, .hold(.unsupportedServer))
        let calls = await client.callCount()
        XCTAssertEqual(calls, 1)
    }

    func testTransportFailureDoesNotExposeUnderlyingError() async {
        let client = ReadinessTestClient(readiness: makeReadiness(), behavior: .transportFailure)
        let service = makeService(client: client)

        let result = await service.assess(makeContext())

        XCTAssertEqual(result.outcome, .hold(.requestFailed))
    }

    func testPausedContextAndPreCancelledTaskDoNotCallClient() async {
        let client = ReadinessTestClient(readiness: makeReadiness(), behavior: .success)
        let service = makeService(client: client)
        let paused = await service.assess(makeContext(isPlaying: false))
        let context = makeContext()
        let cancelledTask = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            return await service.assess(context)
        }
        let cancelled = await cancelledTask.value

        XCTAssertEqual(paused.outcome, .hold(.notPlaying))
        XCTAssertEqual(cancelled.outcome, .hold(.cancelled))
        let calls = await client.callCount()
        XCTAssertEqual(calls, 0)
    }

    func testCancellationAfterAwaitWinsOverLateReadableResponse() async {
        let readiness = makeReadiness(state: .readable, reason: .none, video: nil, audio: nil)
        let client = ReadinessTestClient(readiness: readiness, behavior: .pending)
        let service = makeService(client: client)
        let context = makeContext(videoBitrate: nil, audioBitrate: nil)
        let task = Task { await service.assess(context) }
        await client.waitForCall()
        task.cancel()
        await client.completePendingResponse()

        let result = await task.value

        XCTAssertEqual(result.outcome, .hold(.cancelled))
        XCTAssertEqual(result.sessionGeneration, readiness.sessionGeneration)
    }

    func testCancellationTagRequiresByteExactIdentity() async {
        let request = HLSPlaybackReadinessRequest(
            sessionID: "\u{00E9}",
            variantID: "variant-720p",
            windowStartSeconds: 40,
            windowDurationSeconds: 8
        )
        let readiness = HLSPlaybackReadiness(
            sessionID: "e\u{0301}",
            variantID: request.variantID,
            windowStartSeconds: request.windowStartSeconds,
            windowDurationSeconds: request.windowDurationSeconds,
            sessionGeneration: 12,
            cacheState: .readable,
            cacheReason: .none,
            inspectionBudgetUsedBytes: 0,
            videoOrigin: nil,
            audioOrigin: nil
        )
        let client = ReadinessTestClient(readiness: readiness, behavior: .pending)
        let service = makeService(client: client)
        let context = makeContext(request: request, videoBitrate: nil, audioBitrate: nil)
        let task = Task { await service.assess(context) }
        await client.waitForCall()
        task.cancel()
        await client.completePendingResponse()

        let result = await task.value

        XCTAssertEqual(result.outcome, .hold(.cancelled))
        XCTAssertNil(result.sessionGeneration)
    }

    func testCancellationWinsOverUnsupportedAndMalformedFailures() async {
        let context = makeContext()
        for failure in [ReadinessTestClient.PendingFailure.unsupported, .malformed] {
            let client = ReadinessTestClient(readiness: makeReadiness(), behavior: .pending)
            let service = makeService(client: client)
            let task = Task { await service.assess(context) }
            await client.waitForCall()
            task.cancel()
            await client.failPendingResponse(failure)

            let result = await task.value
            XCTAssertEqual(result.outcome, .hold(.cancelled))
        }
    }

    func testLateResponseIsTaggedButStaleSnapshotCannotAct() async {
        let clock = MutableMonotonicClock(value: 1_000_000_000)
        let readiness = makeReadiness(state: .readable, reason: .none, video: nil, audio: nil)
        let client = ReadinessTestClient(readiness: readiness, behavior: .pending)
        let service = makeService(client: client, clock: clock)
        let context = makeContext(videoBitrate: nil, audioBitrate: nil, contextAge: 4_999)
        let task = Task { await service.assess(context) }
        await client.waitForCall()
        clock.set(1_002_000_000)
        await client.completePendingResponse()

        let result = await task.value

        XCTAssertEqual(result.sessionID, context.request.sessionID)
        XCTAssertEqual(result.variantID, context.request.variantID)
        XCTAssertEqual(result.windowStartSeconds, context.request.windowStartSeconds)
        XCTAssertEqual(result.windowDurationSeconds, context.request.windowDurationSeconds)
        XCTAssertEqual(result.sessionGeneration, readiness.sessionGeneration)
        XCTAssertEqual(result.outcome, HLSUpshiftOutcome.hold(.staleContext))
    }

    func testClockReversalAfterRPCFailsClosed() async {
        let clock = MutableMonotonicClock(value: 2_000_000_000)
        let client = ReadinessTestClient(readiness: makeReadiness(), behavior: .pending)
        let service = makeService(client: client, clock: clock)
        let context = makeContext()
        let task = Task { await service.assess(context) }
        await client.waitForCall()
        clock.set(1_000_000_000)
        await client.completePendingResponse()

        let result = await task.value

        XCTAssertEqual(result.outcome, .hold(.clockAnomaly))
    }

    func testQueryTimeCountsAgainstOriginEvidenceFreshness() async {
        let clock = MutableMonotonicClock(value: 10_000_000_000)
        let readiness = makeReadiness(video: supply(age: 30_000, freshness: 30_000))
        let client = ReadinessTestClient(readiness: readiness, behavior: .pending)
        let service = makeService(client: client, clock: clock)
        let context = makeContext()
        let task = Task { await service.assess(context) }
        await client.waitForCall()
        clock.set(10_001_000_000)
        await client.completePendingResponse()

        let result = await task.value

        XCTAssertEqual(result.outcome, .hold(.insufficientOriginEvidence))
    }

    func testQueryTimeReducesOriginPreparationBudgetAtPlaybackRate() async {
        let clock = MutableMonotonicClock(value: 10_000_000_000)
        let request = makeRequest(start: 21.4, duration: 1)
        let readiness = makeReadiness(
            request: request,
            video: supply(throughput: 20_000_000, latency: 600_000),
            audio: nil
        )
        let client = ReadinessTestClient(readiness: readiness, behavior: .pending)
        let service = makeService(client: client, clock: clock)
        let context = makeContext(
            request: request,
            playbackRate: 0.75,
            videoBitrate: 1_000_000,
            audioBitrate: nil,
            requiresAudio: false
        )
        let task = Task { await service.assess(context) }
        await client.waitForCall()
        clock.set(11_000_000_000)
        await client.completePendingResponse()

        let result = await task.value

        XCTAssertEqual(result.outcome, .hold(.insufficientPreparationTime))
    }

    private func makeService(
        client: any CacheControlClient,
        supportsReadiness: Bool = true,
        clock: MutableMonotonicClock = MutableMonotonicClock(value: 10_000_000_000)
    ) -> HLSUpshiftAssessmentService {
        let capabilities = supportsReadiness ? [CacheServerCapability.hlsPlaybackReadiness] : []
        let summary = CacheServerSummary(
            id: "test-server",
            name: "Test server",
            version: "test",
            mediaBaseURIs: [],
            capabilities: capabilities
        )
        return HLSUpshiftAssessmentService(
            client: client,
            serverSummary: summary,
            monotonicNowNanoseconds: { clock.now() }
        )
    }

    private func makeContext(
        request: HLSPlaybackReadinessRequest? = nil,
        isPlaying: Bool = true,
        playhead: Double = 0,
        bufferedEnd: Double = 100,
        playbackRate: Double = 1,
        videoBitrate: UInt64? = 6_000_000,
        audioBitrate: UInt64? = 192_000,
        requiresAudio: Bool = true,
        decoderSupported: Bool? = true,
        expectedGeneration: UInt64? = 7,
        contextAge: UInt64 = 0
    ) -> HLSUpshiftContext {
        HLSUpshiftContext(
            request: request ?? makeRequest(),
            isPlaying: isPlaying,
            playheadSeconds: playhead,
            contiguousBufferedEndSeconds: bufferedEnd,
            playbackRate: playbackRate,
            targetVideoBitrateBitsPerSecond: videoBitrate,
            targetAudioBitrateBitsPerSecond: audioBitrate,
            requiresAudio: requiresAudio,
            targetDecoderSupported: decoderSupported,
            expectedSessionGeneration: expectedGeneration,
            contextAgeMillis: contextAge
        )
    }

    private func makeRequest(start: Double = 40, duration: Double = 8) -> HLSPlaybackReadinessRequest {
        HLSPlaybackReadinessRequest(
            sessionID: "session-1",
            variantID: "variant-720p",
            windowStartSeconds: start,
            windowDurationSeconds: duration
        )
    }

    private func makeReadiness(
        request: HLSPlaybackReadinessRequest? = nil,
        state: HLSPlaybackCacheState = .missing,
        reason: HLSPlaybackCacheReason = .notCached,
        generation: UInt64 = 7,
        video: HLSOriginSupplyEvidence? = HLSOriginSupplyEvidence(
            ageMillis: 1_000,
            throughputBitsPerSecond: 30_000_000,
            firstByteLatencyMicros: 100_000,
            freshnessLimitMillis: 30_000
        ),
        audio: HLSOriginSupplyEvidence? = HLSOriginSupplyEvidence(
            ageMillis: 1_000,
            throughputBitsPerSecond: 20_000_000,
            firstByteLatencyMicros: 50_000,
            freshnessLimitMillis: 30_000
        )
    ) -> HLSPlaybackReadiness {
        let exactRequest = request ?? makeRequest()
        return HLSPlaybackReadiness(
            sessionID: exactRequest.sessionID,
            variantID: exactRequest.variantID,
            windowStartSeconds: exactRequest.windowStartSeconds,
            windowDurationSeconds: exactRequest.windowDurationSeconds,
            sessionGeneration: generation,
            cacheState: state,
            cacheReason: reason,
            inspectionBudgetUsedBytes: .max,
            videoOrigin: video,
            audioOrigin: audio
        )
    }

    private func supply(
        age: UInt64 = 1_000,
        throughput: UInt64 = 30_000_000,
        latency: UInt64? = 100_000,
        freshness: UInt64 = 30_000
    ) -> HLSOriginSupplyEvidence {
        HLSOriginSupplyEvidence(
            ageMillis: age,
            throughputBitsPerSecond: throughput,
            firstByteLatencyMicros: latency,
            freshnessLimitMillis: freshness
        )
    }
}

private final class MutableMonotonicClock: @unchecked Sendable {
    private let lock = NSLock()
    private var value: UInt64

    init(value: UInt64) {
        self.value = value
    }

    func now() -> UInt64 {
        lock.lock()
        defer { lock.unlock() }
        return value
    }

    func set(_ newValue: UInt64) {
        lock.lock()
        defer { lock.unlock() }
        value = newValue
    }
}

private actor ReadinessTestClient: CacheControlClient {
    enum Behavior: Sendable {
        case success
        case unsupported
        case transportFailure
        case pending
    }

    enum PendingFailure: Sendable {
        case unsupported
        case malformed
    }

    private let readiness: HLSPlaybackReadiness
    private let behavior: Behavior
    private var calls = 0
    private var pendingResponse: CheckedContinuation<HLSPlaybackReadiness, Error>?
    private var callWaiters: [CheckedContinuation<Void, Never>] = []

    init(readiness: HLSPlaybackReadiness, behavior: Behavior) {
        self.readiness = readiness
        self.behavior = behavior
    }

    func getServerInfo() async throws -> CacheServerSummary {
        throw URLError(.unsupportedURL)
    }

    func listCacheRoots() async throws -> [CacheRoot] {
        throw URLError(.unsupportedURL)
    }

    func listLibraryItemsPage(
        pageToken: String,
        pageSize: Int,
        searchText: String?
    ) async throws -> CacheLibraryItemsPage {
        throw URLError(.unsupportedURL)
    }

    func getPlaybackSource(itemID: String, variantID: String) async throws -> CachePlaybackSource {
        throw URLError(.unsupportedURL)
    }

    func deleteLibraryItem(id: String) async throws -> Bool {
        throw URLError(.unsupportedURL)
    }

    func getTask(id: String) async throws -> CacheTask {
        throw URLError(.unsupportedURL)
    }

    func watchTasks(ids: [String]) async -> AsyncThrowingStream<CacheTask, Error> {
        AsyncThrowingStream { $0.finish() }
    }

    func cancelTask(id: String) async throws -> CacheTask {
        throw URLError(.unsupportedURL)
    }

    func createBilibiliPlaybackTask(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask {
        throw URLError(.unsupportedURL)
    }

    func getHLSPlaybackReadiness(_ request: HLSPlaybackReadinessRequest) async throws -> HLSPlaybackReadiness {
        calls += 1
        let waiters = callWaiters
        callWaiters.removeAll()
        waiters.forEach { $0.resume() }

        switch behavior {
        case .success:
            return readiness
        case .unsupported:
            throw CacheControlClientUnsupportedOperation.hlsPlaybackReadiness
        case .transportFailure:
            throw URLError(.timedOut)
        case .pending:
            return try await withCheckedThrowingContinuation { pendingResponse = $0 }
        }
    }

    func waitForCall() async {
        guard calls == 0 else {
            return
        }
        await withCheckedContinuation { callWaiters.append($0) }
    }

    func completePendingResponse() {
        pendingResponse?.resume(returning: readiness)
        pendingResponse = nil
    }

    func failPendingResponse(_ failure: PendingFailure) {
        switch failure {
        case .unsupported:
            pendingResponse?.resume(throwing: CacheControlClientUnsupportedOperation.hlsPlaybackReadiness)
        case .malformed:
            pendingResponse?.resume(throwing: HLSPlaybackReadinessValidationError.invalidResponse)
        }
        pendingResponse = nil
    }

    func callCount() -> Int {
        calls
    }
}
