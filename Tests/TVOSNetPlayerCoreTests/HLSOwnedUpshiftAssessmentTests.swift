import AVFoundation
import Combine
import XCTest
@testable import TVOSNetPlayerCore
import TVOSNetPlayerCacheClient

final class HLSOwnedUpshiftAssessmentTests: XCTestCase {
    func testReadableCachedAssessmentIsCurrentAndDoesNotChangePlayback() async throws {
        let client = GatedUpshiftClient()
        let clock = TestMonotonicClock(10_000_000_000)
        let harness = await PlaybackHarness(client: client, clock: clock)
        let interactionSequence = await harness.model.manualInteractionSequence
        let optionalController = await harness.controller
        let controller = try XCTUnwrap(optionalController)
        let callsBefore = await controller.transportCallCounts

        let task = Task { @MainActor in await harness.assess() }
        try await client.waitForEntry(.catalog, count: 1)
        await client.completeCatalog(1)
        try await client.waitForEntry(.readiness, count: 1)
        await client.completeReadiness(1, state: .readable)
        let optionalResult = await task.value
        let result = try XCTUnwrap(optionalResult)

        XCTAssertEqual(result.recommendation.outcome, .cacheBackedOpportunity)
        let isCurrent = await harness.model.isCurrentHLSUpshiftAssessment(result)
        XCTAssertTrue(isCurrent)
        XCTAssertEqual(result.snapshot.observedAtNanoseconds, 10_000_000_000)
        let finalSequence = await harness.model.manualInteractionSequence
        let callsAfter = await controller.transportCallCounts
        XCTAssertEqual(finalSequence, interactionSequence)
        XCTAssertEqual(callsAfter, callsBefore)
        await client.releaseAll()
    }

    func testCurrentPolicyHoldRemainsOwnedAndUnknownInputsAreNotInferred() async throws {
        let client = GatedUpshiftClient(rendition: Self.rendition(videoBandwidth: nil, audioBandwidth: nil))
        let harness = await PlaybackHarness(client: client)
        let task = Task { @MainActor in
            await harness.assess(targetDecoderSupported: nil)
        }
        try await client.waitForEntry(.catalog, count: 1)
        await client.completeCatalog(1)
        try await client.waitForEntry(.readiness, count: 1)
        await client.completeReadiness(1, state: .readable, video: nil, audio: nil)
        let optionalResult = await task.value
        let result = try XCTUnwrap(optionalResult)

        XCTAssertEqual(result.recommendation.outcome, .hold(.decoderUnknown))
        let isCurrent = await harness.model.isCurrentHLSUpshiftAssessment(result)
        XCTAssertTrue(isCurrent)
        XCTAssertNil(result.rendition.videoResourceBandwidthBitsPerSecond)
        XCTAssertNil(result.rendition.audioResourceBandwidthBitsPerSecond)
        await client.releaseAll()
    }

    func testGenerationMismatchNeverProducesOpportunity() async throws {
        let client = GatedUpshiftClient(generation: 7, readinessGeneration: 8)
        let harness = await PlaybackHarness(client: client)
        let task = Task { @MainActor in await harness.assess() }
        try await client.waitForEntry(.catalog, count: 1)
        await client.completeCatalog(1)
        try await client.waitForEntry(.readiness, count: 1)
        await client.completeReadiness(1, state: .readable)
        let optionalResult = await task.value
        let result = try XCTUnwrap(optionalResult)

        XCTAssertEqual(result.recommendation.outcome, .hold(.generationMismatch))
        let isCurrent = await harness.model.isCurrentHLSUpshiftAssessment(result)
        XCTAssertTrue(isCurrent)
        await client.releaseAll()
    }

    func testMissingBindingWrongServerAndUnsupportedServerAvoidRPC() async throws {
        let client = GatedUpshiftClient()
        let unbound = await PlaybackHarness(client: client, includeSession: false)
        let unboundResult = await unbound.assess()
        XCTAssertNil(unboundResult)

        let bound = await PlaybackHarness(client: client)
        let wrongServer = await bound.serverSummary(id: "another-server")
        let wrongServerResult = await bound.assess(serverSummary: wrongServer)
        XCTAssertNil(wrongServerResult)

        let unsupported = await bound.serverSummary(capabilities: [])
        let unsupportedResult = await bound.assess(serverSummary: unsupported)
        XCTAssertNil(unsupportedResult)
        let catalogCalls = await client.entryCount(.catalog)
        let readinessCalls = await client.entryCount(.readiness)
        XCTAssertEqual(catalogCalls, 0)
        XCTAssertEqual(readinessCalls, 0)
        await client.releaseAll()
    }

    func testInvalidRequestAndAbsentTargetDoNotCallReadiness() async throws {
        let invalidClient = GatedUpshiftClient()
        let invalidHarness = await PlaybackHarness(client: invalidClient)
        let invalidResult = await invalidHarness.assess(targetVariantID: "bad\nvariant")
        XCTAssertNil(invalidResult)
        let invalidCatalogCalls = await invalidClient.entryCount(.catalog)
        XCTAssertEqual(invalidCatalogCalls, 0)

        let sourceClient = GatedUpshiftClient()
        let mismatchedSourceHarness = await PlaybackHarness(
            client: sourceClient,
            playbackURI: "http://127.0.0.1:1/media/other.m3u8"
        )
        let mismatchedSourceResult = await mismatchedSourceHarness.assess()
        XCTAssertNil(mismatchedSourceResult)
        let sourceCatalogCalls = await sourceClient.entryCount(.catalog)
        XCTAssertEqual(sourceCatalogCalls, 0)

        let absentClient = GatedUpshiftClient(renditions: [Self.rendition(variantID: "other")])
        let absentHarness = await PlaybackHarness(client: absentClient)
        let task = Task { @MainActor in await absentHarness.assess() }
        try await absentClient.waitForEntry(.catalog, count: 1)
        await absentClient.completeCatalog(1)
        let absentResult = await task.value
        XCTAssertNil(absentResult)
        let absentReadinessCalls = await absentClient.entryCount(.readiness)
        XCTAssertEqual(absentReadinessCalls, 0)
        await invalidClient.releaseAll()
        await sourceClient.releaseAll()
        await absentClient.releaseAll()
    }

    func testCatalogSessionComparisonIsByteExactForNFCAndNFD() async throws {
        let nfc = "\u{00E9}"
        let nfd = "e\u{0301}"
        XCTAssertNotEqual(
            PlayerHLSPlaybackSessionContext(serverID: "server", sessionID: nfc),
            PlayerHLSPlaybackSessionContext(serverID: "server", sessionID: nfd)
        )

        let client = GatedUpshiftClient(catalogSessionID: nfd)
        let harness = await PlaybackHarness(client: client, sessionID: nfc)
        let task = Task { @MainActor in await harness.assess() }
        try await client.waitForEntry(.catalog, count: 1)
        await client.completeCatalog(1)
        let result = await task.value
        XCTAssertNil(result)
        let readinessCalls = await client.entryCount(.readiness)
        XCTAssertEqual(readinessCalls, 0)
        await client.releaseAll()
    }

    func testAssessmentSnapshotDoesNotAdvanceManualInteractionSequence() async throws {
        let client = GatedUpshiftClient()
        let harness = await PlaybackHarness(client: client)
        let initialSequence = await harness.model.manualInteractionSequence
        let task = Task { @MainActor in await harness.assess() }
        try await client.waitForEntry(.catalog, count: 1)
        let afterStart = await harness.model.manualInteractionSequence
        XCTAssertEqual(afterStart, initialSequence)
        await client.completeCatalog(1)
        try await client.waitForEntry(.readiness, count: 1)
        let afterCatalog = await harness.model.manualInteractionSequence
        XCTAssertEqual(afterCatalog, initialSequence)
        await client.completeReadiness(1, state: .readable)
        _ = await task.value
        let afterReadiness = await harness.model.manualInteractionSequence
        XCTAssertEqual(afterReadiness, initialSequence)
        await client.releaseAll()
    }

    func testPlaybackMutationMakesLateAssessmentObsolete() async throws {
        let client = GatedUpshiftClient()
        let harness = await PlaybackHarness(client: client)
        let task = Task { @MainActor in await harness.assess() }
        try await client.waitForEntry(.catalog, count: 1)
        await client.completeCatalog(1)
        try await client.waitForEntry(.readiness, count: 1)
        await harness.pause()
        await client.completeReadiness(1, state: .readable)
        let result = await task.value
        XCTAssertNil(result)
        await client.releaseAll()
    }

    func testCurrentItemTimeJumpInvalidatesPendingAssessment() async throws {
        let client = GatedUpshiftClient()
        let harness = await PlaybackHarness(client: client)
        let task = Task { @MainActor in await harness.assess() }
        try await client.waitForEntry(.catalog, count: 1)

        await harness.postTimeJump()
        try await client.waitForEntry(.catalogCancelled, count: 1)
        await client.completeCatalog(1)

        let result = await task.value
        XCTAssertNil(result)
        await client.releaseAll()
    }

    func testCurrentItemPlaybackEndInvalidatesPendingAssessment() async throws {
        let client = GatedUpshiftClient()
        let harness = await PlaybackHarness(client: client)
        let task = Task { @MainActor in await harness.assess() }
        try await client.waitForEntry(.catalog, count: 1)

        await harness.postPlaybackEnd()
        try await client.waitForEntry(.catalogCancelled, count: 1)
        let state = await harness.model.playbackState
        if case .finished = state {
        } else {
            XCTFail("Expected current-item end notification to finish playback")
        }
        await client.completeCatalog(1)

        let result = await task.value
        XCTAssertNil(result)
        await client.releaseAll()
    }

    func testObsoleteItemNotificationsDoNotInvalidateCurrentAssessment() async throws {
        let client = GatedUpshiftClient()
        let harness = await PlaybackHarness(client: client)
        let possibleOldItem = await harness.currentItem
        let oldItem = try XCTUnwrap(possibleOldItem)
        await harness.perform(.reloadSameURL)

        let task = Task { @MainActor in await harness.assess() }
        try await client.waitForEntry(.catalog, count: 1)
        await harness.postTimeJump(for: oldItem)
        await harness.postPlaybackEnd(for: oldItem)
        await client.completeCatalog(1)
        try await client.waitForEntry(.readiness, count: 1)
        await client.completeReadiness(1, state: .readable)

        let result = await task.value
        let assessment = try XCTUnwrap(result)
        let isCurrent = await harness.model.isCurrentHLSUpshiftAssessment(assessment)
        XCTAssertTrue(isCurrent)
        await client.releaseAll()
    }

    func testReadinessFailureAndControllerItemMismatchRejectAssessmentWithoutTransportEffects() async throws {
        let failedClient = GatedUpshiftClient()
        let failedHarness = await PlaybackHarness(client: failedClient)
        await failedHarness.setReadiness(.failed)
        let failedResult = await failedHarness.assess()
        XCTAssertNil(failedResult)
        let failedCatalogCalls = await failedClient.entryCount(.catalog)
        XCTAssertEqual(failedCatalogCalls, 0)

        let mismatchClient = GatedUpshiftClient()
        let mismatchHarness = await PlaybackHarness(client: mismatchClient, mismatchedItems: true)
        let mismatchResult = await mismatchHarness.assess()
        XCTAssertNil(mismatchResult)
        let mismatchCatalogCalls = await mismatchClient.entryCount(.catalog)
        XCTAssertEqual(mismatchCatalogCalls, 0)
        await failedClient.releaseAll()
        await mismatchClient.releaseAll()
    }

    func testSourceReplacementCancelsOldOwnerAndReleasesItsControllerBeforeRPCReturns() async throws {
        let client = GatedUpshiftClient()
        let harness = await PlaybackHarness(client: client)
        weak let oldController = await harness.controller
        let task = Task { @MainActor in await harness.assess() }
        try await client.waitForEntry(.catalog, count: 1)

        await harness.replaceSource()

        XCTAssertNil(oldController, "The fixture must not retain the replaced controller")
        await client.completeCatalog(1)
        let result = await task.value
        XCTAssertNil(result)
        await client.releaseAll()
    }

    func testSameURLReloadStopAndClearInvalidatePendingOwners() async throws {
        for action in [PlaybackHarness.PendingAction.reloadSameURL, .stop, .clear] {
            let client = GatedUpshiftClient()
            let harness = await PlaybackHarness(client: client)
            let task = Task { @MainActor in await harness.assess() }
            try await client.waitForEntry(.catalog, count: 1)
            await harness.perform(action)
            await client.completeCatalog(1)
            let result = await task.value
            XCTAssertNil(result)
            await client.releaseAll()
        }
    }

    func testLatestRequestWinsWhenResponsesCompleteInReverseOrder() async throws {
        let client = GatedUpshiftClient()
        let harness = await PlaybackHarness(client: client)
        let first = Task { @MainActor in await harness.assess(targetVariantID: "variant-720p") }
        try await client.waitForEntry(.catalog, count: 1)
        await client.completeCatalog(1)
        try await client.waitForEntry(.readiness, count: 1)

        let second = Task { @MainActor in await harness.assess(targetVariantID: "variant-1080p") }
        try await client.waitForEntry(.catalog, count: 2)
        await client.completeCatalog(2)
        try await client.waitForEntry(.readiness, count: 2)

        await client.completeReadiness(2, state: .readable)
        let optionalLatest = await second.value
        let latest = try XCTUnwrap(optionalLatest)
        await client.completeReadiness(1, state: .readable)
        let firstResult = await first.value
        XCTAssertNil(firstResult)
        XCTAssertEqual(latest.rendition.variantID, "variant-1080p")
        let isCurrent = await harness.model.isCurrentHLSUpshiftAssessment(latest)
        XCTAssertTrue(isCurrent)
        await client.releaseAll()
    }

    func testExplicitInvalidationAndPreCancelledCallerDoNotStartRPC() async throws {
        let cancelledClient = GatedUpshiftClient()
        let cancelledHarness = await PlaybackHarness(client: cancelledClient)
        let cancelled = Task { @MainActor in
            withUnsafeCurrentTask { $0?.cancel() }
            return await cancelledHarness.assess()
        }
        let cancelledResult = await cancelled.value
        XCTAssertNil(cancelledResult)
        let cancelledCatalogCalls = await cancelledClient.entryCount(.catalog)
        XCTAssertEqual(cancelledCatalogCalls, 0)

        let invalidatedClient = GatedUpshiftClient()
        let invalidatedHarness = await PlaybackHarness(client: invalidatedClient)
        let pending = Task { @MainActor in await invalidatedHarness.assess() }
        try await invalidatedClient.waitForEntry(.catalog, count: 1)
        await invalidatedHarness.invalidate()
        await invalidatedClient.completeCatalog(1)
        let pendingResult = await pending.value
        XCTAssertNil(pendingResult)
        await cancelledClient.releaseAll()
        await invalidatedClient.releaseAll()
    }

    func testCancellationIgnoredByBackendCannotPublishLateResult() async throws {
        let client = GatedUpshiftClient()
        let harness = await PlaybackHarness(client: client)
        let task = Task { @MainActor in await harness.assess() }
        try await client.waitForEntry(.catalog, count: 1)
        await harness.invalidate()
        await client.completeCatalog(1)
        let result = await task.value
        XCTAssertNil(result)
        let readinessCalls = await client.entryCount(.readiness)
        XCTAssertEqual(readinessCalls, 0)
        await client.releaseAll()
    }

    func testOldCancellationDoesNotInvalidateNewAssessment() async throws {
        let client = GatedUpshiftClient()
        let harness = await PlaybackHarness(client: client)
        let old = Task { @MainActor in await harness.assess(targetVariantID: "variant-720p") }
        try await client.waitForEntry(.catalog, count: 1)
        await client.completeCatalog(1)
        try await client.waitForEntry(.readiness, count: 1)

        let new = Task { @MainActor in await harness.assess(targetVariantID: "variant-1080p") }
        try await client.waitForEntry(.catalog, count: 2)
        await client.completeCatalog(2)
        try await client.waitForEntry(.readiness, count: 2)
        await client.completeReadiness(2, state: .readable)
        let optionalCurrent = await new.value
        let current = try XCTUnwrap(optionalCurrent)
        await client.completeReadiness(1, state: .readable)

        let oldResult = await old.value
        XCTAssertNil(oldResult)
        let isCurrent = await harness.model.isCurrentHLSUpshiftAssessment(current)
        XCTAssertTrue(isCurrent)
        await client.releaseAll()
    }

    func testSnapshotAgeIncludesBothRPCsAndBackwardClockRejects() async throws {
        let expiryClient = GatedUpshiftClient()
        let expiryClock = TestMonotonicClock(5_000_000_000)
        let expiryHarness = await PlaybackHarness(client: expiryClient, clock: expiryClock)
        let expired = Task { @MainActor in await expiryHarness.assess() }
        try await expiryClient.waitForEntry(.catalog, count: 1)
        expiryClock.set(7_500_000_000)
        await expiryClient.completeCatalog(1)
        try await expiryClient.waitForEntry(.readiness, count: 1)
        expiryClock.set(10_001_000_000)
        await expiryClient.completeReadiness(1, state: .readable)
        let expiredResult = await expired.value
        XCTAssertNil(expiredResult)

        let reversedClient = GatedUpshiftClient()
        let reversedClock = TestMonotonicClock(5_000_000_000)
        let reversedHarness = await PlaybackHarness(client: reversedClient, clock: reversedClock)
        let reversed = Task { @MainActor in await reversedHarness.assess() }
        try await reversedClient.waitForEntry(.catalog, count: 1)
        await reversedClient.completeCatalog(1)
        try await reversedClient.waitForEntry(.readiness, count: 1)
        reversedClock.set(4_000_000_000)
        await reversedClient.completeReadiness(1, state: .readable)
        let reversedResult = await reversed.value
        XCTAssertNil(reversedResult)
        await expiryClient.releaseAll()
        await reversedClient.releaseAll()
    }

    private static func rendition(
        variantID: String = "variant-720p",
        videoBandwidth: UInt64? = 5_000_000,
        audioBandwidth: UInt64? = 192_000
    ) -> HLSRenditionDescriptor {
        try! HLSRenditionDescriptor(
            variantID: variantID,
            advertisedBandwidthBitsPerSecond: 5_200_000,
            videoResourceBandwidthBitsPerSecond: videoBandwidth,
            audioResourceBandwidthBitsPerSecond: audioBandwidth,
            hasSeparateAudio: true
        )
    }
}

@MainActor
private final class PlaybackHarness {
    enum PendingAction {
        case reloadSameURL
        case stop
        case clear
    }

    let model: PlayerViewModel
    private(set) var controller: OwnedAssessmentControllerSpy!
    private let notificationCenter = NotificationCenter()
    private let sessionID: String
    private let playbackURI: String
    private let endpoint = CacheServerEndpoint(host: "127.0.0.1", port: 50_051)
    private let clock: TestMonotonicClock
    private let mismatchedItems: Bool

    init(
        client: any CacheControlClient,
        clock: TestMonotonicClock = TestMonotonicClock(10_000_000_000),
        sessionID: String = "session-1",
        includeSession: Bool = true,
        playbackURI: String = "",
        mismatchedItems: Bool = false
    ) {
        self.clock = clock
        self.sessionID = sessionID
        self.playbackURI =
            playbackURI.isEmpty
            ? "http://127.0.0.1:1/media/fixture.m3u8" : playbackURI
        self.mismatchedItems = mismatchedItems
        model = PlayerViewModel(
            autoplay: false,
            playbackProgressReportInterval: nil,
            cacheClientFactory: { _ in client }
        )
        model.playbackNotificationCenter = notificationCenter
        model.playbackSnapshotNowNanoseconds = { clock.now() }
        model.playbackControllerFactory = { [weak self] _ in
            let playerItem = AVPlayerItem(asset: AVMutableComposition())
            let player = AVPlayer(playerItem: playerItem)
            player.isMuted = true
            player.volume = 0
            let controllerItem =
                self?.mismatchedItems == true
                ? AVPlayerItem(asset: AVMutableComposition()) : playerItem
            let spy = OwnedAssessmentControllerSpy(item: controllerItem)
            self?.controller = spy
            return (player, spy)
        }
        _ = model.loadTransient(
            streamURLText: "http://127.0.0.1:1/media/fixture.m3u8",
            progressContext: progressContext(includeSession: includeSession)
        )
    }

    func assess(
        targetVariantID: String = "variant-720p",
        targetDecoderSupported: Bool? = true,
        serverSummary: CacheServerSummary? = nil
    ) async -> HLSOwnedUpshiftAssessment? {
        await model.assessHLSUpshift(
            targetVariantID: targetVariantID,
            windowStartSeconds: 40,
            windowDurationSeconds: 8,
            targetDecoderSupported: targetDecoderSupported,
            serverSummary: serverSummary ?? self.serverSummary()
        )
    }

    func serverSummary(
        id: String = "server-1",
        capabilities: [String] = [
            CacheServerCapability.hlsPlaybackReadiness, CacheServerCapability.hlsRenditionCatalog,
        ]
    ) -> CacheServerSummary {
        CacheServerSummary(id: id, name: "Test", version: "1", mediaBaseURIs: [], capabilities: capabilities)
    }

    func pause() {
        model.togglePlayback(using: controller)
    }

    func setReadiness(_ status: AVPlayerItem.Status) {
        controller.readinessStatus = status
    }

    func invalidate() {
        model.invalidateHLSUpshiftAssessment()
    }

    func replaceSource() {
        _ = model.loadTransient(
            streamURLText: "http://127.0.0.1:1/media/replaced.m3u8",
            progressContext: progressContext()
        )
    }

    var currentItem: AVPlayerItem? { model.player?.currentItem }

    func postTimeJump(for item: AVPlayerItem? = nil) {
        notificationCenter.post(name: .AVPlayerItemTimeJumped, object: item ?? model.player?.currentItem)
    }

    func postPlaybackEnd(for item: AVPlayerItem? = nil) {
        notificationCenter.post(name: .AVPlayerItemDidPlayToEndTime, object: item ?? model.player?.currentItem)
    }

    func perform(_ action: PendingAction) {
        switch action {
        case .reloadSameURL:
            _ = model.loadTransient(
                streamURLText: "http://127.0.0.1:1/media/fixture.m3u8",
                progressContext: progressContext()
            )
        case .stop:
            model.stop()
        case .clear:
            model.clear()
        }
    }

    private func progressContext(includeSession: Bool = true) -> PlayerPlaybackProgressContext {
        PlayerPlaybackProgressContext(
            endpoint: endpoint,
            playbackURI: playbackURI,
            hlsSession: includeSession
                ? PlayerHLSPlaybackSessionContext(serverID: "server-1", sessionID: sessionID) : nil
        )
    }
}

@MainActor
private final class OwnedAssessmentControllerSpy: PlayerPlaybackControlling {
    let currentItem: AVPlayerItem?
    var itemDuration: CMTime = .indefinite
    var readinessStatus: AVPlayerItem.Status = .readyToPlay
    var timeControlStatus: AVPlayer.TimeControlStatus = .playing
    var defaultRate: Float = 1
    var actualPlaybackRate: Float? = 1
    var loadedMediaTimeRanges: [CMTimeRange] = [
        CMTimeRange(
            start: CMTime(seconds: 0, preferredTimescale: 600),
            duration: CMTime(seconds: 120, preferredTimescale: 600)
        )
    ]
    private(set) var playCallCount = 0
    private(set) var pauseCallCount = 0
    private(set) var seekTargets: [CMTime] = []

    var transportCallCounts: [Int] { [playCallCount, pauseCallCount, seekTargets.count] }

    init(item: AVPlayerItem?) { currentItem = item }
    func currentTime() -> CMTime { CMTime(seconds: 40, preferredTimescale: 600) }
    func play() {
        playCallCount += 1
        timeControlStatus = .playing
        actualPlaybackRate = defaultRate
    }
    func pause() {
        pauseCallCount += 1
        timeControlStatus = .paused
        actualPlaybackRate = 0
    }
    func seek(to time: CMTime, toleranceBefore: CMTime, toleranceAfter: CMTime) { seekTargets.append(time) }
    func seekForPlayback(to time: CMTime, completion: @escaping @MainActor @Sendable (Bool) -> Void) {
        seekTargets.append(time)
    }
    func observeReadiness(
        _ observer: @escaping @MainActor @Sendable (AVPlayerItem.Status) -> Void
    ) -> AnyCancellable {
        observer(readinessStatus)
        return AnyCancellable {}
    }
}

private actor GatedUpshiftClient: CacheControlClient {
    enum Kind: Hashable, Sendable { case catalog, catalogCancelled, readiness }

    private let renditions: [HLSRenditionDescriptor]
    private let catalogSessionID: String
    private let generation: UInt64
    private let readinessGeneration: UInt64
    private var entered: [Kind: Int] = [:]
    private struct EntryWaiter {
        let id: UUID
        let target: Int
        let continuation: CheckedContinuation<Void, Error>
        let timeoutTask: Task<Void, Never>
    }
    private var entryWaiters: [Kind: [EntryWaiter]] = [:]
    private var catalogWaiters: [Int: CheckedContinuation<HLSRenditionCatalog, Error>] = [:]
    private var readinessWaiters: [Int: CheckedContinuation<HLSPlaybackReadiness, Error>] = [:]
    private var readinessRequests: [Int: HLSPlaybackReadinessRequest] = [:]

    init(
        renditions: [HLSRenditionDescriptor] = [
            try! HLSRenditionDescriptor(
                variantID: "variant-720p", advertisedBandwidthBitsPerSecond: 5_200_000,
                videoResourceBandwidthBitsPerSecond: 5_000_000,
                audioResourceBandwidthBitsPerSecond: 192_000, hasSeparateAudio: true
            ),
            try! HLSRenditionDescriptor(
                variantID: "variant-1080p", advertisedBandwidthBitsPerSecond: 8_200_000,
                videoResourceBandwidthBitsPerSecond: 8_000_000,
                audioResourceBandwidthBitsPerSecond: 192_000, hasSeparateAudio: true
            ),
        ],
        rendition: HLSRenditionDescriptor? = nil,
        catalogSessionID: String = "session-1",
        generation: UInt64 = 7,
        readinessGeneration: UInt64? = nil
    ) {
        self.renditions = rendition.map { [$0] } ?? renditions
        self.catalogSessionID = catalogSessionID
        self.generation = generation
        self.readinessGeneration = readinessGeneration ?? generation
    }

    func getHLSRenditionCatalog(_ request: HLSRenditionCatalogRequest) async throws -> HLSRenditionCatalog {
        let index = record(.catalog)
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { catalogWaiters[index] = $0 }
        } onCancel: {
            Task { await self.record(.catalogCancelled) }
        }
    }

    func getHLSPlaybackReadiness(_ request: HLSPlaybackReadinessRequest) async throws -> HLSPlaybackReadiness {
        let index = record(.readiness)
        readinessRequests[index] = request
        return try await withCheckedThrowingContinuation { readinessWaiters[index] = $0 }
    }

    func waitForEntry(_ kind: Kind, count: Int) async throws {
        if entered[kind, default: 0] >= count { return }
        let id = UUID()
        try await withCheckedThrowingContinuation { continuation in
            let timeoutTask = Task {
                do { try await Task.sleep(nanoseconds: 5_000_000_000) } catch { return }
                self.expireEntryWaiter(kind, id: id)
            }
            entryWaiters[kind, default: []].append(
                EntryWaiter(id: id, target: count, continuation: continuation, timeoutTask: timeoutTask))
        }
    }

    func entryCount(_ kind: Kind) -> Int { entered[kind, default: 0] }

    func completeCatalog(_ index: Int) {
        guard let continuation = catalogWaiters.removeValue(forKey: index) else { return }
        continuation.resume(
            returning: try! HLSRenditionCatalog(
                sessionID: catalogSessionID,
                sessionGeneration: generation,
                renditions: renditions
            ))
    }

    func completeReadiness(
        _ index: Int,
        state: HLSPlaybackCacheState,
        video: HLSOriginSupplyEvidence? = nil,
        audio: HLSOriginSupplyEvidence? = nil
    ) {
        guard let continuation = readinessWaiters.removeValue(forKey: index),
            let request = readinessRequests[index]
        else { return }
        continuation.resume(
            returning: HLSPlaybackReadiness(
                sessionID: request.sessionID,
                variantID: request.variantID,
                windowStartSeconds: request.windowStartSeconds,
                windowDurationSeconds: request.windowDurationSeconds,
                sessionGeneration: readinessGeneration,
                cacheState: state,
                cacheReason: state == .readable ? .none : .notCached,
                inspectionBudgetUsedBytes: 0,
                videoOrigin: video,
                audioOrigin: audio
            ))
    }

    func releaseAll() {
        let pendingCatalog = Array(catalogWaiters.values)
        catalogWaiters.removeAll()
        for continuation in pendingCatalog {
            continuation.resume(throwing: CancellationError())
        }
        let pendingReadiness = Array(readinessWaiters.values)
        readinessWaiters.removeAll()
        for continuation in pendingReadiness {
            continuation.resume(throwing: CancellationError())
        }
        let pendingEntries = entryWaiters.values.flatMap { $0 }
        entryWaiters.removeAll()
        for waiter in pendingEntries {
            waiter.timeoutTask.cancel()
            waiter.continuation.resume(throwing: CancellationError())
        }
    }

    func getServerInfo() async throws -> CacheServerSummary { throw URLError(.unsupportedURL) }
    func listCacheRoots() async throws -> [CacheRoot] { throw URLError(.unsupportedURL) }
    func listLibraryItemsPage(pageToken: String, pageSize: Int, searchText: String?) async throws
        -> CacheLibraryItemsPage
    {
        throw URLError(.unsupportedURL)
    }
    func getPlaybackSource(itemID: String, variantID: String) async throws -> CachePlaybackSource {
        throw URLError(.unsupportedURL)
    }
    func deleteLibraryItem(id: String) async throws -> Bool { throw URLError(.unsupportedURL) }
    func getTask(id: String) async throws -> CacheTask { throw URLError(.unsupportedURL) }
    func watchTasks(ids: [String]) async -> AsyncThrowingStream<CacheTask, Error> {
        AsyncThrowingStream { $0.finish() }
    }
    func cancelTask(id: String) async throws -> CacheTask { throw URLError(.unsupportedURL) }
    func createBilibiliPlaybackTask(urlOrID: String, options: BilibiliPlaybackTaskOptions) async throws -> CacheTask {
        throw URLError(.unsupportedURL)
    }

    private func record(_ kind: Kind) -> Int {
        let next = entered[kind, default: 0] + 1
        entered[kind] = next
        var pending: [EntryWaiter] = []
        for waiter in entryWaiters[kind] ?? [] {
            if next >= waiter.target {
                waiter.timeoutTask.cancel()
                waiter.continuation.resume()
            } else {
                pending.append(waiter)
            }
        }
        entryWaiters[kind] = pending
        return next
    }

    private func expireEntryWaiter(_ kind: Kind, id: UUID) {
        guard let index = entryWaiters[kind]?.firstIndex(where: { $0.id == id }) else { return }
        let waiter = entryWaiters[kind]!.remove(at: index)
        waiter.continuation.resume(throwing: EntryWaitTimeout())
    }
}

private struct EntryWaitTimeout: Error {}

private final class TestMonotonicClock: @unchecked Sendable {
    private let lock = NSLock()
    private var value: UInt64
    init(_ value: UInt64) { self.value = value }
    func now() -> UInt64 { lock.lock(); defer { lock.unlock() }; return value }
    func set(_ value: UInt64) { lock.lock(); defer { lock.unlock() }; self.value = value }
}
