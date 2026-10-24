import AVFoundation
import Combine
import XCTest
import TVOSNetPlayerCacheClient
@testable import TVOSNetPlayerCore

final class PlayerViewModelTests: XCTestCase {
    private var defaultsSuiteName: String!
    private var defaults: UserDefaults!

    override func setUpWithError() throws {
        try super.setUpWithError()
        defaultsSuiteName = "PlayerViewModelTests-\(UUID().uuidString)"
        defaults = try XCTUnwrap(UserDefaults(suiteName: defaultsSuiteName))
        defaults.removePersistentDomain(forName: defaultsSuiteName)
    }

    override func tearDown() {
        defaults.removePersistentDomain(forName: defaultsSuiteName)
        defaults = nil
        defaultsSuiteName = nil
        super.tearDown()
    }

    private static func waitForReports(
        from client: FakePlaybackProgressClient,
        minimumCount: Int
    ) async -> [PlaybackProgressReport] {
        for _ in 0..<100 {
            let reports = await client.reportsSnapshot()
            if reports.count >= minimumCount {
                return reports
            }
            try? await Task.sleep(for: .milliseconds(10))
        }
        return await client.reportsSnapshot()
    }

    @MainActor
    private func makeResumableFixture(
        identity: ClientPlaybackIdentity,
        checkpoint: ClientPlaybackCheckpoint? = nil,
        readiness: AVPlayerItem.Status = .readyToPlay,
        duration: CMTime = .indefinite,
        autoplay: Bool = false,
        delayFirstReport: Bool = false,
        failReports: Bool = false,
        readinessTimeout: Duration = .seconds(20),
        seekTimeout: Duration = .seconds(15)
    ) throws -> (PlayerViewModel, PlaybackControlSpy, ClientResumptionStore, FakePlaybackProgressClient) {
        let store = ClientResumptionStore(defaults: defaults)
        store.rememberTask(identity.task)
        if let checkpoint {
            store.beginPlayback(identity, checkpoint: checkpoint)
        }

        let client = FakePlaybackProgressClient(delayFirstReport: delayFirstReport, failReports: failReports)
        let model = PlayerViewModel(
            defaults: defaults,
            autoplay: autoplay,
            playbackProgressReportInterval: nil,
            resumptionStore: store,
            cacheClientFactory: { _ in client }
        )
        model.resumptionReadinessTimeout = readinessTimeout
        model.resumptionSeekTimeout = seekTimeout
        var playbackController: PlaybackControlSpy?
        model.playbackControllerFactory = { url in
            let player = AVPlayer(url: url)
            let controller = PlaybackControlSpy(timeControlStatus: .paused, currentItem: player.currentItem)
            controller.readinessStatus = readiness
            controller.itemDuration = duration
            playbackController = controller
            return (player, controller)
        }
        let context = PlayerPlaybackProgressContext(
            endpoint: identity.task.endpoint,
            libraryItemID: identity.libraryItemID,
            variantID: identity.variantID,
            resumptionIdentity: identity
        )
        XCTAssertTrue(
            model.loadTransient(
                streamURLText: "http://127.0.0.1:1/media/fixture.m3u8",
                progressContext: context,
                resumeCheckpoint: checkpoint
            )
        )
        return (model, try XCTUnwrap(playbackController), store, client)
    }

    @MainActor
    private func assertFailedResumeIsUnloaded(
        _ model: PlayerViewModel,
        controller: PlaybackControlSpy,
        store: ClientResumptionStore,
        client: FakePlaybackProgressClient,
        checkpoint: ClientPlaybackCheckpoint,
        file: StaticString = #filePath,
        line: UInt = #line
    ) async {
        let message = "Could not restore the saved playback position. Try again."
        XCTAssertEqual(model.validationMessage, message, file: file, line: line)
        XCTAssertEqual(model.statusMessage, message, file: file, line: line)
        XCTAssertNil(model.player, file: file, line: line)
        XCTAssertNil(model.loadedURL, file: file, line: line)
        XCTAssertFalse(model.canUsePlaybackControls, file: file, line: line)
        XCTAssertEqual(model.playbackState, .paused, file: file, line: line)
        XCTAssertEqual(model.playbackProgressStatusRefreshRequestID, 0, file: file, line: line)
        XCTAssertEqual(controller.playCallCount, 0, file: file, line: line)
        XCTAssertEqual(controller.timeControlStatus, .paused, file: file, line: line)

        model.checkpointPlayback()
        let report = await model.reportCurrentPlaybackProgress()
        await model.flushPlaybackProgressReports()
        let reports = await client.reportsSnapshot()

        XCTAssertNil(report, file: file, line: line)
        XCTAssertTrue(reports.isEmpty, file: file, line: line)
        XCTAssertEqual(store.record?.playback, checkpoint, file: file, line: line)
        XCTAssertEqual(
            ClientResumptionStore(defaults: defaults).record?.playback,
            checkpoint,
            file: file,
            line: line
        )
    }

    private func makePlaybackIdentity(
        taskID: String = "task-7",
        resultID: String? = "result-2",
        libraryItemID: String = "library-item-4",
        variantID: String = "h264"
    ) -> ClientPlaybackIdentity {
        ClientPlaybackIdentity(
            task: ClientResumptionTaskIdentity(
                endpoint: CacheServerEndpoint(host: "cache.example", port: 50_051),
                serverID: "server-1",
                taskID: taskID
            ),
            resultID: resultID,
            libraryItemID: libraryItemID,
            variantID: variantID
        )
    }

    func testNormalizesBareHostAsHTTPURL() {
        let url = PlayerViewModel.normalizedHTTPURL(from: "192.168.1.10:8080/video.mp4")

        XCTAssertEqual(url?.absoluteString, "http://192.168.1.10:8080/video.mp4")
    }

    func testAcceptsHTTPSURL() {
        let url = PlayerViewModel.normalizedHTTPURL(from: " https://example.com/movie.m3u8 ")

        XCTAssertEqual(url?.absoluteString, "https://example.com/movie.m3u8")
    }

    func testRejectsUnsupportedSchemes() {
        XCTAssertNil(PlayerViewModel.normalizedHTTPURL(from: "file:///tmp/movie.mp4"))
    }

    @MainActor
    func testSavedURLInitializesIdleModel() {
        defaults.set("https://example.com/movie.m3u8", forKey: PlayerViewModel.lastStreamURLDefaultsKey)

        let model = PlayerViewModel(defaults: defaults, autoplay: false)

        XCTAssertEqual(model.streamURLText, "https://example.com/movie.m3u8")
        XCTAssertEqual(model.statusMessage, "Ready to replay https://example.com/movie.m3u8.")
        XCTAssertNil(model.loadedURL)
        XCTAssertNil(model.player)
        XCTAssertTrue(model.canClear)
    }

    @MainActor
    func testClearingSavedURLTextRemovesPersistedURL() {
        defaults.set("https://example.com/movie.m3u8", forKey: PlayerViewModel.lastStreamURLDefaultsKey)
        let model = PlayerViewModel(defaults: defaults, autoplay: false)

        model.streamURLText = ""

        XCTAssertNil(defaults.string(forKey: PlayerViewModel.lastStreamURLDefaultsKey))
        XCTAssertEqual(model.statusMessage, "Ready for an HTTP or HTTPS stream on your network.")
        XCTAssertFalse(model.canClear)
    }

    @MainActor
    func testLoadStoresNormalizedURL() {
        let model = PlayerViewModel(defaults: defaults, autoplay: false)
        model.streamURLText = "example.com/movie.m3u8"

        model.load()

        XCTAssertEqual(model.loadedURL?.absoluteString, "http://example.com/movie.m3u8")
        XCTAssertEqual(model.streamURLText, "http://example.com/movie.m3u8")
        XCTAssertEqual(
            defaults.string(forKey: PlayerViewModel.lastStreamURLDefaultsKey), "http://example.com/movie.m3u8")
        XCTAssertNil(model.validationMessage)
    }

    @MainActor
    func testPlaybackControlsAreDisabledUntilPlayerLoads() {
        let model = PlayerViewModel(defaults: defaults, autoplay: false)

        XCTAssertFalse(model.canUsePlaybackControls)

        model.load(streamURLText: "example.com/movie.m3u8")

        XCTAssertTrue(model.canUsePlaybackControls)
    }

    func testPausedPlaybackStateOffersResume() {
        let state = PlayerPlaybackState(timeControlStatus: .paused)

        XCTAssertEqual(state, .paused)
        XCTAssertEqual(state.displayTitle, "Paused")
        XCTAssertFalse(state.canPause)
    }

    func testWaitingPlaybackStateOffersPauseAndReportsBuffering() {
        let state = PlayerPlaybackState(timeControlStatus: .waitingToPlayAtSpecifiedRate)

        XCTAssertEqual(state, .buffering)
        XCTAssertEqual(state.displayTitle, "Buffering")
        XCTAssertEqual(state.rawValue, "buffering")
        XCTAssertTrue(state.canPause)
    }

    func testPlayingPlaybackStateOffersPause() {
        let state = PlayerPlaybackState(timeControlStatus: .playing)

        XCTAssertEqual(state, .playing)
        XCTAssertEqual(state.displayTitle, "Playing")
        XCTAssertTrue(state.canPause)
    }

    func testFinishedPlaybackStateKeepsTerminalPresentation() {
        let state = PlayerPlaybackState(timeControlStatus: .paused, isFinished: true)

        XCTAssertEqual(state, .finished)
        XCTAssertEqual(state.displayTitle, "Finished")
        XCTAssertFalse(state.canPause)
    }

    @MainActor
    func testToggleDuringWaitingCallsPauseInsteadOfPlay() {
        let player = PlaybackControlSpy(timeControlStatus: .waitingToPlayAtSpecifiedRate)

        PlayerViewModel.togglePlayback(player: player, speed: .double)

        XCTAssertEqual(player.pauseCallCount, 1)
        XCTAssertEqual(player.playCallCount, 0)
        XCTAssertEqual(player.timeControlStatus, .paused)
    }

    @MainActor
    func testToggleDuringPlayingCallsPause() {
        let player = PlaybackControlSpy(timeControlStatus: .playing)

        PlayerViewModel.togglePlayback(player: player, speed: .oneAndHalf)

        XCTAssertEqual(player.pauseCallCount, 1)
        XCTAssertEqual(player.playCallCount, 0)
    }

    @MainActor
    func testToggleAfterBufferingPauseResumesAtSelectedSpeed() {
        let player = PlaybackControlSpy(timeControlStatus: .waitingToPlayAtSpecifiedRate)
        PlayerViewModel.togglePlayback(player: player, speed: .double)

        PlayerViewModel.togglePlayback(player: player, speed: .double)

        XCTAssertEqual(player.pauseCallCount, 1)
        XCTAssertEqual(player.playCallCount, 1)
        XCTAssertEqual(player.defaultRate, PlayerPlaybackSpeed.double.rate)
        XCTAssertTrue(player.seekTargets.isEmpty)
    }

    @MainActor
    func testToggleWithoutPlayerInvalidatesPendingTransientPlayback() {
        let model = PlayerViewModel(defaultStreamURLText: "", defaults: defaults, autoplay: false)
        let sequence = model.manualInteractionSequence

        model.togglePlayback()

        XCTAssertEqual(model.manualInteractionSequence, sequence + 1)
        XCTAssertFalse(
            model.loadTransient(
                streamURLText: "http://127.0.0.1:1/media/fixture.mp4",
                ifManualInteractionSequenceMatches: sequence
            )
        )
        XCTAssertNil(model.player)
        XCTAssertNil(model.loadedURL)
    }

    @MainActor
    private func endedPlaybackFixture() async throws -> (
        PlayerViewModel, PlaybackControlSpy, FakePlaybackProgressClient
    ) {
        let client = FakePlaybackProgressClient()
        let model = PlayerViewModel(
            defaults: defaults, autoplay: false, playbackProgressReportInterval: nil,
            cacheClientFactory: { _ in client }
        )
        model.setPlaybackSpeed(.double)
        _ = model.loadTransient(
            streamURLText: "http://127.0.0.1:1/fixture.mp4",
            progressContext: PlayerPlaybackProgressContext(endpoint: CacheServerEndpoint(host: "127.0.0.1"))
        )
        let item = try XCTUnwrap(model.player?.currentItem)
        NotificationCenter.default.post(name: .AVPlayerItemDidPlayToEndTime, object: item)
        let reports = await Self.waitForReports(from: client, minimumCount: 2)
        XCTAssertEqual(reports.map(\.intent), [.started, .stopped])
        XCTAssertEqual(model.playbackState, .finished)
        return (model, PlaybackControlSpy(timeControlStatus: .paused, currentItem: item), client)
    }

    @MainActor
    private func pausedNearEndPlaybackFixture(playbackProgressReportInterval: Duration? = nil) async throws -> (
        PlayerViewModel, PlaybackControlSpy, FakePlaybackProgressClient
    ) {
        let client = FakePlaybackProgressClient()
        let model = PlayerViewModel(
            defaults: defaults, autoplay: false, playbackProgressReportInterval: playbackProgressReportInterval,
            cacheClientFactory: { _ in client }
        )
        model.setPlaybackSpeed(.double)
        _ = model.loadTransient(
            streamURLText: "http://127.0.0.1:1/fixture.mp4",
            progressContext: PlayerPlaybackProgressContext(endpoint: CacheServerEndpoint(host: "127.0.0.1"))
        )
        let player = PlaybackControlSpy(
            timeControlStatus: .paused, currentItem: try XCTUnwrap(model.player?.currentItem))
        player.itemDuration = CMTime(seconds: 92, preferredTimescale: 600)
        player.positionSeconds = 87
        await model.flushPlaybackProgressReports()
        XCTAssertEqual(model.playbackState, .paused)
        return (model, player, client)
    }

    @MainActor
    private func completedTerminalPlaybackFixture(playbackProgressReportInterval: Duration? = nil) async throws -> (
        PlayerViewModel, PlaybackControlSpy, FakePlaybackProgressClient
    ) {
        let (model, player, client) = try await pausedNearEndPlaybackFixture(
            playbackProgressReportInterval: playbackProgressReportInterval)
        model.seek(by: 10, using: player)
        player.completeNextSeek(finished: true, position: player.itemDuration)
        await model.flushPlaybackProgressReports()
        XCTAssertEqual(model.playbackState, .finished)
        return (model, player, client)
    }

    @MainActor
    func testEndedToggleSeeksBeforePlayingAndReportsReplayAtZero() async throws {
        let (model, player, client) = try await endedPlaybackFixture()
        let sequence = model.manualInteractionSequence

        model.togglePlayback(using: player)

        XCTAssertEqual(model.manualInteractionSequence, sequence + 1)
        XCTAssertEqual(player.seekTargets.map(\.seconds), [0])
        XCTAssertEqual(player.playCallCount, 0)
        XCTAssertEqual(model.playbackState, .buffering)

        player.completeNextSeek(finished: true)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(player.playCallCount, 1)
        XCTAssertEqual(player.defaultRate, PlayerPlaybackSpeed.double.rate)
        XCTAssertNotEqual(model.playbackState, .finished)
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.map(\.intent), [.started, .stopped, .seek, .playing])
        XCTAssertEqual(reports.suffix(2).map(\.positionSeconds), [0, 0])
        XCTAssertEqual(
            reports.suffix(2).map(\.playbackURI), Array(repeating: "http://127.0.0.1:1/fixture.mp4", count: 2))
    }

    @MainActor
    func testReplayUsesSpeedSelectedWhileSeekIsPending() async throws {
        let (model, player, _) = try await endedPlaybackFixture()
        model.togglePlayback(using: player)

        model.setPlaybackSpeed(.oneAndHalf)
        player.completeNextSeek(finished: true)

        XCTAssertEqual(player.playCallCount, 1)
        XCTAssertEqual(player.defaultRate, PlayerPlaybackSpeed.oneAndHalf.rate)
    }

    @MainActor
    func testPauseCancelsPendingEndedReplay() async throws {
        let (model, player, client) = try await endedPlaybackFixture()
        model.togglePlayback(using: player)

        model.togglePlayback(using: player)
        player.completeNextSeek(finished: true)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(player.pauseCallCount, 1)
        XCTAssertEqual(player.playCallCount, 0)
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.map(\.intent), [.started, .stopped])
    }

    @MainActor
    func testReplacingPlaybackRejectsStaleReplaySeekCompletion() async throws {
        let (model, player, client) = try await endedPlaybackFixture()
        model.togglePlayback(using: player)
        _ = model.loadTransient(streamURLText: "http://127.0.0.1:1/replacement.mp4")
        await model.flushPlaybackProgressReports()
        let beforeCompletion = await client.reportsSnapshot()

        player.completeNextSeek(finished: true)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(player.playCallCount, 0)
        XCTAssertEqual(model.loadedURL?.lastPathComponent, "replacement.mp4")
        let afterCompletion = await client.reportsSnapshot()
        XCTAssertEqual(afterCompletion, beforeCompletion)
    }

    @MainActor
    func testStopRejectsPendingReplaySeekCompletion() async throws {
        let (model, player, client) = try await endedPlaybackFixture()
        model.togglePlayback(using: player)
        model.stop()
        await model.flushPlaybackProgressReports()
        let beforeCompletion = await client.reportsSnapshot()

        player.completeNextSeek(finished: true)
        await model.flushPlaybackProgressReports()

        XCTAssertNil(model.player)
        XCTAssertEqual(player.playCallCount, 0)
        let afterCompletion = await client.reportsSnapshot()
        XCTAssertEqual(afterCompletion, beforeCompletion)
    }

    @MainActor
    func testStopReleasesPendingReplayControllerWithoutSeekCallback() async throws {
        var fixture: (PlayerViewModel, PlaybackControlSpy, FakePlaybackProgressClient)? =
            try await endedPlaybackFixture()
        let model = try XCTUnwrap(fixture?.0)
        let controller = { [weak controller = fixture?.1] in controller }
        model.togglePlayback(using: fixture?.1)
        fixture = nil
        XCTAssertNotNil(controller())

        model.stop()

        XCTAssertNil(controller())
    }

    @MainActor
    func testReplacementReleasesPendingReplayControllerWithoutSeekCallback() async throws {
        var fixture: (PlayerViewModel, PlaybackControlSpy, FakePlaybackProgressClient)? =
            try await endedPlaybackFixture()
        let model = try XCTUnwrap(fixture?.0)
        let controller = { [weak controller = fixture?.1] in controller }
        model.togglePlayback(using: fixture?.1)
        fixture = nil
        XCTAssertNotNil(controller())

        _ = model.loadTransient(streamURLText: "http://127.0.0.1:1/replacement.mp4")

        XCTAssertNil(controller())
        XCTAssertEqual(model.loadedURL?.lastPathComponent, "replacement.mp4")
    }

    @MainActor
    func testFailedReplaySeekDoesNotPlayOrReportReplay() async throws {
        let (model, player, client) = try await endedPlaybackFixture()
        model.togglePlayback(using: player)

        player.completeNextSeek(finished: false)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(player.playCallCount, 0)
        XCTAssertEqual(model.playbackState, .finished)
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.map(\.intent), [.started, .stopped])
        model.togglePlayback(using: player)
        XCTAssertEqual(player.seekTargets.count, 2)
    }

    @MainActor
    func testOldReplaySeekCompletionDoesNotResumeNewRequest() async throws {
        let (model, player, _) = try await endedPlaybackFixture()
        model.togglePlayback(using: player)
        model.togglePlayback(using: player)
        model.togglePlayback(using: player)

        player.completeNextSeek(finished: true)

        XCTAssertEqual(player.playCallCount, 0)
        XCTAssertEqual(model.playbackState, .buffering)
        player.completeNextSeek(finished: true)
        XCTAssertEqual(player.playCallCount, 1)
    }

    @MainActor
    func testPausedReplacementItemDoesNotRewindFromOldEndMarker() async throws {
        let (model, player, _) = try await endedPlaybackFixture()
        let replacement = AVPlayerItem(asset: AVMutableComposition())
        model.player?.replaceCurrentItem(with: replacement)
        player.currentItem = replacement

        model.togglePlayback(using: player)

        XCTAssertTrue(player.seekTargets.isEmpty)
        XCTAssertEqual(player.playCallCount, 1)
        XCTAssertNotEqual(model.playbackState, .finished)
    }

    @MainActor
    func testBufferingToggleDoesNotRewindEndedItem() async throws {
        let (model, player, _) = try await endedPlaybackFixture()
        player.timeControlStatus = .waitingToPlayAtSpecifiedRate

        model.togglePlayback(using: player)

        XCTAssertTrue(player.seekTargets.isEmpty)
        XCTAssertEqual(player.pauseCallCount, 1)
        XCTAssertEqual(player.playCallCount, 0)
    }

    @MainActor
    func testForwardAtEndPreservesFinishedStateAndResumeRewinds() async throws {
        let (model, player, client) = try await endedPlaybackFixture()
        let sequence = model.manualInteractionSequence

        model.skipForward()
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(model.manualInteractionSequence, sequence + 1)
        XCTAssertEqual(model.playbackState, .finished)
        XCTAssertEqual(model.statusMessage, "Playback finished.")
        let reportsAfterSkip = await client.reportsSnapshot()
        XCTAssertEqual(reportsAfterSkip.map(\.intent), [.started, .stopped])

        model.togglePlayback(using: player)
        XCTAssertEqual(player.seekTargets.map(\.seconds), [0])
        XCTAssertEqual(player.playCallCount, 0)
        player.completeNextSeek(finished: true)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(player.playCallCount, 1)
        XCTAssertEqual(player.defaultRate, PlayerPlaybackSpeed.double.rate)
        let reportsAfterResume = await client.reportsSnapshot()
        XCTAssertEqual(reportsAfterResume.map(\.intent), [.started, .stopped, .seek, .playing])
        XCTAssertEqual(reportsAfterResume.suffix(2).map(\.positionSeconds), [0, 0])
    }

    @MainActor
    func testZeroAndClampedBackwardAtEndPreserveFinishedState() async throws {
        let (model, _, client) = try await endedPlaybackFixture()

        model.seek(by: 0)
        XCTAssertEqual(model.playbackState, .finished)
        model.skipBackward()
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(model.playbackState, .finished)
        XCTAssertEqual(model.statusMessage, "Playback finished.")
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.map(\.intent), [.started, .stopped])
    }

    @MainActor
    func testBackwardAwayFromEndResumesChosenPositionWithoutReplaySeek() async throws {
        let (model, player, client) = try await endedPlaybackFixture()
        player.positionSeconds = 92

        model.seek(by: -10, using: player)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(player.seekTargets.map(\.seconds), [82])
        XCTAssertEqual(player.positionSeconds, 82)
        XCTAssertEqual(model.playbackState, .paused)
        let reportsAfterSeek = await client.reportsSnapshot()
        XCTAssertEqual(reportsAfterSeek.map(\.intent), [.started, .stopped, .seek])
        XCTAssertEqual(reportsAfterSeek.last?.positionSeconds, 82)
        XCTAssertNil(reportsAfterSeek.last?.durationSeconds)

        model.togglePlayback(using: player)

        XCTAssertEqual(player.playCallCount, 1)
        XCTAssertEqual(player.defaultRate, PlayerPlaybackSpeed.double.rate)
        XCTAssertEqual(player.seekTargets.map(\.seconds), [82])
        XCTAssertEqual(player.positionSeconds, 82)
    }

    @MainActor
    func testForwardAtEndWithUnknownDurationDoesNotSeekPastTerminalPosition() async throws {
        let (model, player, client) = try await endedPlaybackFixture()
        player.positionSeconds = 92
        XCTAssertFalse(try XCTUnwrap(player.currentItem).duration.seconds.isFinite)

        model.seek(by: 10, using: player)
        await model.flushPlaybackProgressReports()

        XCTAssertTrue(player.seekTargets.isEmpty)
        XCTAssertEqual(player.positionSeconds, 92)
        XCTAssertEqual(model.playbackState, .finished)
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.map(\.intent), [.started, .stopped])
    }

    @MainActor
    func testForwardAtEndCancelsPendingReplayWithoutClearingFinishedState() async throws {
        let (model, player, client) = try await endedPlaybackFixture()
        model.togglePlayback(using: player)

        model.skipForward()
        player.completeNextSeek(finished: true)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(player.playCallCount, 0)
        XCTAssertEqual(model.playbackState, .finished)
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.map(\.intent), [.started, .stopped])
        model.togglePlayback(using: player)
        XCTAssertEqual(player.seekTargets.map(\.seconds), [0, 0])
        player.completeNextSeek(finished: true)
        XCTAssertEqual(player.playCallCount, 1)
    }

    @MainActor
    func testSeekTargetsRespectKnownDurationAndLowerBound() {
        XCTAssertEqual(PlayerViewModel.seekTargetSeconds(currentSeconds: 87, durationSeconds: 92, offset: 10), 92)
        XCTAssertEqual(PlayerViewModel.seekTargetSeconds(currentSeconds: 92, durationSeconds: 92, offset: -10), 82)
        XCTAssertEqual(PlayerViewModel.seekTargetSeconds(currentSeconds: 2, durationSeconds: 92, offset: -10), 0)
        XCTAssertEqual(PlayerViewModel.seekTargetSeconds(currentSeconds: 0, durationSeconds: 0, offset: 10), 0)
    }

    @MainActor
    func testSeekWithUnknownDurationReportsActualTargetWithoutInventingDuration() async throws {
        let (model, player, client) = try await endedPlaybackFixture()
        player.positionSeconds = 92
        model.seek(by: -10, using: player)
        model.seek(by: 10, using: player)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(player.seekTargets.map(\.seconds), [82, 92])
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.suffix(2).map(\.positionSeconds), [82, 92])
        XCTAssertTrue(reports.suffix(2).allSatisfy { $0.durationSeconds == nil })
        for duration: Double? in [nil, .nan, .infinity, -1] {
            XCTAssertEqual(
                PlayerViewModel.seekTargetSeconds(currentSeconds: 82, durationSeconds: duration, offset: 10), 92)
        }
    }

    @MainActor
    func testPausedForwardSeekToEndFinishesOnlyAfterCompletionAndResumeRewinds() async throws {
        let (model, player, client) = try await pausedNearEndPlaybackFixture()

        model.seek(by: 10, using: player)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(player.seekTargets.map(\.seconds), [92])
        XCTAssertEqual(player.positionSeconds, 87)
        XCTAssertEqual(model.playbackState, .paused)
        let pendingReports = await client.reportsSnapshot()
        XCTAssertEqual(pendingReports.map(\.intent), [.started])

        player.completeNextSeek(finished: true, position: player.itemDuration)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(model.playbackState, .finished)
        XCTAssertEqual(model.statusMessage, "Playback finished.")
        let terminalReports = await client.reportsSnapshot()
        XCTAssertEqual(terminalReports.map(\.intent), [.started, .seek, .stopped])
        XCTAssertEqual(terminalReports.suffix(2).map(\.positionSeconds), [92, 92])
        XCTAssertEqual(terminalReports.suffix(2).map(\.durationSeconds), [92, 92])

        model.togglePlayback(using: player)
        XCTAssertEqual(player.seekTargets.map(\.seconds), [92, 0])
        XCTAssertEqual(player.playCallCount, 0)
        XCTAssertEqual(model.playbackState, .buffering)
        player.completeNextSeek(finished: true, position: .zero)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(player.positionSeconds, 0)
        XCTAssertEqual(player.playCallCount, 1)
        XCTAssertEqual(player.defaultRate, PlayerPlaybackSpeed.double.rate)
        let replayReports = await client.reportsSnapshot()
        XCTAssertEqual(replayReports.map(\.intent), [.started, .seek, .stopped, .seek, .playing])
        XCTAssertEqual(replayReports.suffix(2).map(\.positionSeconds), [0, 0])
        XCTAssertEqual(replayReports.suffix(2).map(\.durationSeconds), [92, 92])
    }

    @MainActor
    func testFailedTerminalSeekDoesNotFinishOrRewindOnResume() async throws {
        let (model, player, client) = try await pausedNearEndPlaybackFixture()
        model.seek(by: 10, using: player)

        player.completeNextSeek(finished: false)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(model.playbackState, .paused)
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.map(\.intent), [.started])
        model.togglePlayback(using: player)
        XCTAssertEqual(player.seekTargets.map(\.seconds), [92])
        XCTAssertEqual(player.positionSeconds, 87)
        XCTAssertEqual(player.playCallCount, 1)
        XCTAssertEqual(player.defaultRate, PlayerPlaybackSpeed.double.rate)
    }

    @MainActor
    func testCompletedTerminalSeekMustActuallyReachTheEnd() async throws {
        let (model, player, client) = try await pausedNearEndPlaybackFixture()
        model.seek(by: 10, using: player)

        player.completeNextSeek(finished: true, position: CMTime(seconds: 91, preferredTimescale: 600))
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(model.playbackState, .paused)
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.map(\.intent), [.started])
        model.togglePlayback(using: player)
        XCTAssertEqual(player.seekTargets.map(\.seconds), [92])
        XCTAssertEqual(player.playCallCount, 1)
    }

    @MainActor
    func testSupersededTerminalSeekCannotFinishBackwardSeek() async throws {
        let (model, player, client) = try await pausedNearEndPlaybackFixture()
        model.seek(by: 10, using: player)
        model.seek(by: -10, using: player)

        player.completeNextSeek(finished: true)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(player.positionSeconds, 77)
        XCTAssertEqual(model.playbackState, .paused)
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.map(\.intent), [.started, .seek])
        XCTAssertEqual(reports.last?.positionSeconds, 77)
        model.togglePlayback(using: player)
        XCTAssertEqual(player.seekTargets.map(\.seconds), [92, 77])
        XCTAssertEqual(player.playCallCount, 1)
    }

    @MainActor
    func testStoppedTerminalSeekCannotMarkFinished() async throws {
        let (model, player, client) = try await pausedNearEndPlaybackFixture()
        model.seek(by: 10, using: player)
        model.stop()
        await model.flushPlaybackProgressReports()
        let reportsBeforeCompletion = await client.reportsSnapshot()

        player.completeNextSeek(finished: true, position: player.itemDuration)
        await model.flushPlaybackProgressReports()

        XCTAssertNil(model.player)
        XCTAssertEqual(model.statusMessage, "Stopped.")
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports, reportsBeforeCompletion)
    }

    @MainActor
    func testReplacedPlayerRejectsTerminalSeekCompletion() async throws {
        let (model, player, client) = try await pausedNearEndPlaybackFixture()
        model.seek(by: 10, using: player)
        let sequence = model.manualInteractionSequence
        _ = model.loadTransient(streamURLText: "http://127.0.0.1:1/replacement.mp4")
        await model.flushPlaybackProgressReports()
        let reportsBeforeCompletion = await client.reportsSnapshot()

        player.completeNextSeek(finished: true, position: player.itemDuration)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(model.manualInteractionSequence, sequence + 1)
        XCTAssertEqual(model.loadedURL?.lastPathComponent, "replacement.mp4")
        XCTAssertEqual(model.playbackState, .paused)
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports, reportsBeforeCompletion)
    }

    @MainActor
    func testReplacedItemRejectsTerminalSeekCompletion() async throws {
        let (model, player, client) = try await pausedNearEndPlaybackFixture()
        model.seek(by: 10, using: player)
        let sequence = model.manualInteractionSequence
        let replacement = AVPlayerItem(asset: AVMutableComposition())
        model.player?.replaceCurrentItem(with: replacement)
        player.currentItem = replacement

        player.completeNextSeek(finished: true, position: player.itemDuration)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(model.manualInteractionSequence, sequence)
        XCTAssertEqual(model.playbackState, .paused)
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.map(\.intent), [.started])
    }

    @MainActor
    func testDirectTerminalResumeRewindsWithoutAnEndNotification() async throws {
        let (model, player, client) = try await pausedNearEndPlaybackFixture()
        player.positionSeconds = 92

        model.togglePlayback(using: player)

        XCTAssertEqual(player.seekTargets.map(\.seconds), [0])
        XCTAssertEqual(player.playCallCount, 0)
        XCTAssertEqual(model.playbackState, .buffering)
        player.completeNextSeek(finished: true, position: .zero)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(player.playCallCount, 1)
        XCTAssertEqual(player.defaultRate, PlayerPlaybackSpeed.double.rate)
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.map(\.intent), [.started, .stopped, .seek, .playing])
        XCTAssertEqual(reports.dropFirst().map(\.positionSeconds), [92, 0, 0])
    }

    @MainActor
    func testTerminalSeekUsesExactDurationAndRejectsOneTickBeforeEnd() async throws {
        let (model, player, _) = try await pausedNearEndPlaybackFixture()
        let duration = CMTime(value: 4_416_001, timescale: 48_000)
        player.itemDuration = duration

        model.seek(by: 10, using: player)

        XCTAssertEqual(CMTimeCompare(try XCTUnwrap(player.seekTargets.first), duration), 0)
        XCTAssertFalse(
            PlayerViewModel.isAtPlaybackEnd(
                currentTime: CMTime(value: duration.value - 1, timescale: duration.timescale), duration: duration))
        XCTAssertFalse(
            PlayerViewModel.isAtPlaybackEnd(
                currentTime: CMTime(seconds: 92, preferredTimescale: 600), duration: duration))
        XCTAssertTrue(PlayerViewModel.isAtPlaybackEnd(currentTime: duration, duration: duration))
        player.completeNextSeek(finished: true, position: duration)
        XCTAssertEqual(model.playbackState, .finished)
    }

    @MainActor
    func testUnknownAndLiveDurationDoNotInferFinishedFromCurrentPosition() async throws {
        for duration in [CMTime.invalid, .indefinite, .positiveInfinity, .zero] {
            let (model, player, client) = try await pausedNearEndPlaybackFixture()
            player.itemDuration = duration
            player.positionSeconds = 92

            model.togglePlayback(using: player)
            await model.flushPlaybackProgressReports()

            XCTAssertTrue(player.seekTargets.isEmpty)
            XCTAssertEqual(player.playCallCount, 1)
            let reports = await client.reportsSnapshot()
            XCTAssertEqual(reports.map(\.intent), [.started])
        }
    }

    @MainActor
    func testDurationBecomingIndefiniteDuringTerminalSeekDoesNotFinish() async throws {
        let (model, player, client) = try await pausedNearEndPlaybackFixture()
        model.seek(by: 10, using: player)
        player.itemDuration = .indefinite

        player.completeNextSeek(finished: true, position: CMTime(seconds: 92, preferredTimescale: 600))
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(model.playbackState, .paused)
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.map(\.intent), [.started])
    }

    @MainActor
    func testOldTerminalSeekCompletionCannotFinishANewerReplay() async throws {
        let (model, player, client) = try await pausedNearEndPlaybackFixture()
        model.seek(by: 10, using: player)
        player.positionSeconds = 92
        model.togglePlayback(using: player)

        player.completeNextSeek(finished: true)

        XCTAssertEqual(model.playbackState, .buffering)
        XCTAssertEqual(player.playCallCount, 0)
        player.completeNextSeek(finished: true, position: .zero)
        await model.flushPlaybackProgressReports()
        XCTAssertEqual(player.playCallCount, 1)
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.map(\.intent), [.started, .stopped, .seek, .playing])
    }

    @MainActor
    func testExternalBackwardScrubClearsFinishedAndPreservesSelectedPositionAndSpeed() async throws {
        let (model, player, client) = try await completedTerminalPlaybackFixture()
        let sequence = model.manualInteractionSequence
        player.positionSeconds = 42

        model.handlePlaybackTimeJump(using: player)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(model.playbackState, .paused)
        XCTAssertEqual(model.manualInteractionSequence, sequence + 1)
        XCTAssertEqual(player.seekTargets.map(\.seconds), [92])
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.suffix(2).map(\.intent), [.seek, .paused])
        XCTAssertEqual(reports.suffix(2).map(\.positionSeconds), [42, 42])
        XCTAssertEqual(reports.suffix(2).map(\.durationSeconds), [92, 92])

        model.setPlaybackSpeed(.oneAndHalf)
        model.togglePlayback(using: player)

        XCTAssertEqual(player.positionSeconds, 42)
        XCTAssertEqual(player.seekTargets.map(\.seconds), [92])
        XCTAssertEqual(player.playCallCount, 1)
        XCTAssertEqual(player.defaultRate, PlayerPlaybackSpeed.oneAndHalf.rate)
        XCTAssertNotEqual(model.playbackState, .finished)
    }

    @MainActor
    func testCustomResumeReconcilesExternalScrubBeforeTimeJumpNotification() async throws {
        let (model, player, client) = try await completedTerminalPlaybackFixture()
        player.positionSeconds = 37
        model.setPlaybackSpeed(.oneAndQuarter)

        model.togglePlayback(using: player)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(player.seekTargets.map(\.seconds), [92])
        XCTAssertEqual(player.positionSeconds, 37)
        XCTAssertEqual(player.playCallCount, 1)
        XCTAssertEqual(player.defaultRate, PlayerPlaybackSpeed.oneAndQuarter.rate)
        XCTAssertNotEqual(model.playbackState, .finished)
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.suffix(2).map(\.intent), [.seek, .paused])
        XCTAssertEqual(reports.suffix(2).map(\.positionSeconds), [37, 37])
    }

    @MainActor
    func testNativeTimeJumpNotificationSurvivesCompletionAndRestoresPausedAtZero() async throws {
        let (model, player, client) = try await completedTerminalPlaybackFixture()
        let nativePlayer = try XCTUnwrap(model.player)
        let item = try XCTUnwrap(nativePlayer.currentItem)
        XCTAssertEqual(nativePlayer.currentTime().seconds, 0)
        let before = await client.reportsSnapshot()

        NotificationCenter.default.post(name: .AVPlayerItemTimeJumped, object: item)

        let reports = await Self.waitForReports(from: client, minimumCount: before.count + 2)
        XCTAssertEqual(model.playbackState, .paused)
        XCTAssertEqual(reports.suffix(2).map(\.intent), [.seek, .paused])
        XCTAssertEqual(reports.suffix(2).map(\.positionSeconds), [0, 0])
        XCTAssertEqual(player.seekTargets.map(\.seconds), [92])
    }

    @MainActor
    func testExternalScrubRearmsHeartbeatAndNextEndObserver() async throws {
        let (model, player, client) = try await completedTerminalPlaybackFixture(
            playbackProgressReportInterval: .milliseconds(25))
        let before = await client.reportsSnapshot()
        player.positionSeconds = 42

        model.handlePlaybackTimeJump(using: player)

        let restarted = await Self.waitForReports(from: client, minimumCount: before.count + 3)
        let afterEnd = Array(restarted.dropFirst(before.count))
        XCTAssertEqual(afterEnd.prefix(2).map(\.intent), [.seek, .paused])
        XCTAssertEqual(afterEnd.prefix(2).map(\.positionSeconds), [42, 42])
        XCTAssertTrue(afterEnd.dropFirst(2).contains { $0.intent == .paused })

        NotificationCenter.default.post(
            name: .AVPlayerItemDidPlayToEndTime, object: try XCTUnwrap(model.player?.currentItem))

        var stoppedReports: [PlaybackProgressReport] = []
        for _ in 0..<100 {
            stoppedReports = await client.reportsSnapshot().filter { $0.intent == .stopped }
            if stoppedReports.count == 2 { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertEqual(stoppedReports.count, 2)
        XCTAssertEqual(model.playbackState, .finished)
        XCTAssertEqual(model.statusMessage, "Playback finished.")
    }

    @MainActor
    func testTimeJumpDuringPendingRewindDoesNotClearEndedMarkerBeforeCompletion() async throws {
        let (model, player, client) = try await completedTerminalPlaybackFixture()
        model.togglePlayback(using: player)
        player.positionSeconds = 0

        model.handlePlaybackTimeJump(using: player)
        NotificationCenter.default.post(name: .AVPlayerItemTimeJumped, object: try XCTUnwrap(model.player?.currentItem))
        await model.flushPlaybackProgressReports()
        model.handlePlaybackTimeJump(using: player)

        XCTAssertEqual(model.playbackState, .buffering)
        XCTAssertEqual(player.playCallCount, 0)
        let beforeCompletion = await client.reportsSnapshot()
        XCTAssertEqual(beforeCompletion.map(\.intent), [.started, .seek, .stopped])

        player.completeNextSeek(finished: true, position: .zero)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(player.playCallCount, 1)
        XCTAssertEqual(player.defaultRate, PlayerPlaybackSpeed.double.rate)
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.map(\.intent), [.started, .seek, .stopped, .seek, .playing])
    }

    @MainActor
    func testExternalNonzeroScrubCancelsPendingReplayAndRejectsItsCompletion() async throws {
        let (model, player, client) = try await completedTerminalPlaybackFixture()
        model.togglePlayback(using: player)
        player.positionSeconds = 45

        model.handlePlaybackTimeJump(using: player)
        player.completeNextSeek(finished: true)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(model.playbackState, .paused)
        XCTAssertEqual(player.playCallCount, 0)
        XCTAssertEqual(player.positionSeconds, 45)
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.suffix(2).map(\.intent), [.seek, .paused])
        XCTAssertEqual(reports.suffix(2).map(\.positionSeconds), [45, 45])
        model.togglePlayback(using: player)
        XCTAssertEqual(player.playCallCount, 1)
        XCTAssertEqual(player.seekTargets.map(\.seconds), [92, 0])
    }

    @MainActor
    func testReplayCompletionDetectsNativeScrubBeforeItsNotification() async throws {
        let (model, player, client) = try await completedTerminalPlaybackFixture()
        model.togglePlayback(using: player)
        player.positionSeconds = 45

        player.completeNextSeek(finished: true)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(model.playbackState, .paused)
        XCTAssertEqual(player.playCallCount, 0)
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.suffix(2).map(\.intent), [.seek, .paused])
        XCTAssertEqual(reports.suffix(2).map(\.positionSeconds), [45, 45])
    }

    @MainActor
    func testExternalScrubToZeroWithoutPendingReplayRestoresPausedState() async throws {
        let (model, player, client) = try await completedTerminalPlaybackFixture()
        player.positionSeconds = 0

        model.handlePlaybackTimeJump(using: player)
        model.togglePlayback(using: player)
        await model.flushPlaybackProgressReports()

        XCTAssertNotEqual(model.playbackState, .finished)
        XCTAssertEqual(player.playCallCount, 1)
        XCTAssertEqual(player.seekTargets.map(\.seconds), [92])
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.suffix(2).map(\.positionSeconds), [0, 0])
    }

    @MainActor
    func testExternalScrubWithUnknownDurationUsesObservedEndPositionWithoutInventingDuration() async throws {
        let (model, player, client) = try await completedTerminalPlaybackFixture()
        player.itemDuration = .indefinite
        player.positionSeconds = 30

        model.handlePlaybackTimeJump(using: player)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(model.playbackState, .paused)
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.suffix(2).map(\.positionSeconds), [30, 30])
        XCTAssertTrue(reports.suffix(2).allSatisfy { $0.durationSeconds == nil })
    }

    @MainActor
    func testUnknownUnobservedEndPositionKeepsAuthoritativeNaturalEndMarker() async throws {
        let (model, player, client) = try await endedPlaybackFixture()
        player.positionSeconds = 30

        model.handlePlaybackTimeJump(using: player)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(model.playbackState, .finished)
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.map(\.intent), [.started, .stopped])
        model.togglePlayback(using: player)
        XCTAssertEqual(player.seekTargets.map(\.seconds), [0])
    }

    @MainActor
    func testInvalidOrTerminalTimeJumpDoesNotClearFinishedState() async throws {
        let (model, player, client) = try await completedTerminalPlaybackFixture()
        let sequence = model.manualInteractionSequence
        for time in [
            CMTime.invalid, .indefinite, .positiveInfinity,
            CMTime(seconds: -1, preferredTimescale: 600),
            CMTime(seconds: 92, preferredTimescale: 600), CMTime(seconds: 93, preferredTimescale: 600),
        ] {
            player.playbackTime = time
            model.handlePlaybackTimeJump(using: player)
            XCTAssertEqual(model.playbackState, .finished)
        }
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(model.manualInteractionSequence, sequence)
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.map(\.intent), [.started, .seek, .stopped])
    }

    @MainActor
    func testStaleReplacedItemTimeJumpCannotReactivateOldProgress() async throws {
        let (model, player, client) = try await completedTerminalPlaybackFixture()
        let oldItem = try XCTUnwrap(player.currentItem)
        player.positionSeconds = 30
        _ = model.loadTransient(streamURLText: "http://127.0.0.1:1/replacement.mp4")
        await model.flushPlaybackProgressReports()
        let before = await client.reportsSnapshot()

        model.handlePlaybackTimeJump(using: player)
        NotificationCenter.default.post(name: .AVPlayerItemTimeJumped, object: oldItem)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(model.loadedURL?.lastPathComponent, "replacement.mp4")
        XCTAssertEqual(model.playbackState, .paused)
        let after = await client.reportsSnapshot()
        XCTAssertEqual(after, before)
    }

    @MainActor
    func testModelBackwardSeekDoesNotDoubleReportAsExternalScrub() async throws {
        let (model, player, client) = try await completedTerminalPlaybackFixture()
        let sequence = model.manualInteractionSequence

        model.seek(by: -10, using: player)
        model.handlePlaybackTimeJump(using: player)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(model.manualInteractionSequence, sequence + 1)
        XCTAssertEqual(player.positionSeconds, 82)
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.map(\.intent), [.started, .seek, .stopped, .seek])
        XCTAssertEqual(reports.last?.positionSeconds, 82)
    }

    @MainActor
    func testQueuedOldEndCannotFinishAfterNativeScrubRearmsObserver() async throws {
        let (model, player, client) = try await pausedNearEndPlaybackFixture()
        let item = try XCTUnwrap(player.currentItem)
        NotificationCenter.default.post(name: .AVPlayerItemDidPlayToEndTime, object: item)
        model.seek(by: 10, using: player)
        player.completeNextSeek(finished: true, position: player.itemDuration)
        player.positionSeconds = 42
        model.handlePlaybackTimeJump(using: player)
        await model.flushPlaybackProgressReports()
        try await Task.sleep(for: .milliseconds(25))

        XCTAssertEqual(model.playbackState, .paused)
        let beforeNewEnd = await client.reportsSnapshot()
        XCTAssertEqual(beforeNewEnd.filter { $0.intent == .stopped }.count, 1)
        XCTAssertEqual(beforeNewEnd.suffix(2).map(\.intent), [.seek, .paused])
        XCTAssertEqual(beforeNewEnd.suffix(2).map(\.positionSeconds), [42, 42])

        NotificationCenter.default.post(name: .AVPlayerItemDidPlayToEndTime, object: item)

        let afterNewEnd = await Self.waitForReports(from: client, minimumCount: beforeNewEnd.count + 1)
        XCTAssertEqual(afterNewEnd.filter { $0.intent == .stopped }.count, 2)
        XCTAssertEqual(model.playbackState, .finished)
    }

    @MainActor
    func testQueuedOldEndCannotFinishAfterReplayRestoresObservation() async throws {
        let (model, player, client) = try await pausedNearEndPlaybackFixture()
        NotificationCenter.default.post(name: .AVPlayerItemDidPlayToEndTime, object: try XCTUnwrap(player.currentItem))
        model.seek(by: 10, using: player)
        player.completeNextSeek(finished: true, position: player.itemDuration)
        model.togglePlayback(using: player)
        player.completeNextSeek(finished: true, position: .zero)
        await model.flushPlaybackProgressReports()
        try await Task.sleep(for: .milliseconds(25))

        XCTAssertNotEqual(model.playbackState, .finished)
        XCTAssertEqual(player.playCallCount, 1)
        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.map(\.intent), [.started, .seek, .stopped, .seek, .playing])
    }

    @MainActor
    func testObservedEndPositionComparisonRequiresPositiveBackwardEvidence() {
        let end = CMTime(seconds: 92, preferredTimescale: 600)
        let earlier = CMTime(seconds: 42, preferredTimescale: 600)
        XCTAssertTrue(PlayerViewModel.hasMovedAwayFromEnd(currentTime: earlier, endTime: end, duration: end))
        XCTAssertTrue(PlayerViewModel.hasMovedAwayFromEnd(currentTime: .zero, endTime: end, duration: .indefinite))
        XCTAssertFalse(PlayerViewModel.hasMovedAwayFromEnd(currentTime: end, endTime: end, duration: end))
        XCTAssertFalse(PlayerViewModel.hasMovedAwayFromEnd(currentTime: .invalid, endTime: end, duration: end))
        XCTAssertFalse(PlayerViewModel.hasMovedAwayFromEnd(currentTime: earlier, endTime: .invalid, duration: end))
        XCTAssertFalse(PlayerViewModel.hasMovedAwayFromEnd(currentTime: earlier, endTime: .zero, duration: .indefinite))
        XCTAssertFalse(
            PlayerViewModel.hasMovedAwayFromEnd(
                currentTime: end, endTime: CMTime(seconds: 93, preferredTimescale: 600), duration: end))
    }

    @MainActor
    func testPlaybackSpeedCanBeSelectedBeforePlayback() {
        let model = PlayerViewModel(defaults: defaults, autoplay: false)
        let sequence = model.manualInteractionSequence

        model.setPlaybackSpeed(.oneAndHalf)

        XCTAssertEqual(model.manualInteractionSequence, sequence)
        XCTAssertEqual(model.playbackSpeed, .oneAndHalf)
        XCTAssertEqual(model.statusMessage, "Playback speed 1.5x selected.")
    }

    @MainActor
    func testPlaybackSpeedSelectionDoesNotCancelPendingTransientPlayback() {
        let model = PlayerViewModel(defaults: defaults, autoplay: false)
        let sequence = model.manualInteractionSequence

        model.setPlaybackSpeed(.oneAndHalf)
        let didLoad = model.loadTransient(
            streamURLText: "mac-mini.local:8080/media/item-a/original",
            ifManualInteractionSequenceMatches: sequence
        )

        XCTAssertTrue(didLoad)
        XCTAssertEqual(model.loadedURL?.absoluteString, "http://mac-mini.local:8080/media/item-a/original")
        XCTAssertEqual(model.player?.defaultRate, PlayerPlaybackSpeed.oneAndHalf.rate)
    }

    @MainActor
    func testPlaybackSpeedAppliesToLoadedPlayerDefaultRate() {
        let model = PlayerViewModel(defaults: defaults, autoplay: false)
        model.setPlaybackSpeed(.oneAndQuarter)

        model.load(streamURLText: "example.com/movie.m3u8")

        XCTAssertEqual(model.player?.defaultRate, PlayerPlaybackSpeed.oneAndQuarter.rate)
        XCTAssertEqual(model.playbackSpeed, .oneAndQuarter)
    }

    @MainActor
    func testSeekWithoutLoadedPlayerReportsNoStream() {
        let model = PlayerViewModel(defaults: defaults, autoplay: false)

        model.skipForward()

        XCTAssertEqual(model.statusMessage, "No stream loaded.")
        XCTAssertFalse(model.canUsePlaybackControls)
    }

    @MainActor
    func testSkipForwardAndBackwardUpdateStatus() {
        let model = PlayerViewModel(defaults: defaults, autoplay: false)
        model.load(streamURLText: "example.com/movie.m3u8")

        model.skipForward()
        XCTAssertEqual(model.statusMessage, "Skipped forward 10 seconds.")

        model.skipBackward()
        XCTAssertEqual(model.statusMessage, "Skipped back 10 seconds.")
    }

    @MainActor
    func testLoadWithStreamURLTextStoresNormalizedURL() {
        let model = PlayerViewModel(defaults: defaults, autoplay: false)

        model.load(streamURLText: "example.com/movie.m3u8")

        XCTAssertEqual(model.loadedURL?.absoluteString, "http://example.com/movie.m3u8")
        XCTAssertEqual(model.streamURLText, "http://example.com/movie.m3u8")
    }

    @MainActor
    func testTransientLoadDoesNotPersistOrReplaceManualURLText() {
        let model = PlayerViewModel(defaults: defaults, autoplay: false)
        model.streamURLText = "example.com/manual.m3u8"
        let sequenceBeforeTransientLoad = model.manualInteractionSequence

        let didLoad = model.loadTransient(streamURLText: "mac-mini.local:8080/media/item-a/original")

        XCTAssertTrue(didLoad)
        XCTAssertGreaterThan(model.manualInteractionSequence, sequenceBeforeTransientLoad)
        XCTAssertEqual(model.loadedURL?.absoluteString, "http://mac-mini.local:8080/media/item-a/original")
        XCTAssertEqual(model.streamURLText, "example.com/manual.m3u8")
        XCTAssertNil(defaults.string(forKey: PlayerViewModel.lastStreamURLDefaultsKey))
        XCTAssertNil(model.validationMessage)
    }

    @MainActor
    func testTransientLoadWithMatchingManualSequenceStartsPlayback() {
        let model = PlayerViewModel(defaults: defaults, autoplay: false)
        let sequence = model.manualInteractionSequence

        let didLoad = model.loadTransient(
            streamURLText: "mac-mini.local:8080/media/item-a/original",
            ifManualInteractionSequenceMatches: sequence
        )

        XCTAssertTrue(didLoad)
        XCTAssertGreaterThan(model.manualInteractionSequence, sequence)
        XCTAssertEqual(model.loadedURL?.absoluteString, "http://mac-mini.local:8080/media/item-a/original")
        XCTAssertNil(model.validationMessage)
    }

    @MainActor
    func testCheckpointPlaybackPersistsBeforeSerializedReportCompletes() async throws {
        let identity = makePlaybackIdentity()
        let (model, controller, store, client) = try makeResumableFixture(
            identity: identity,
            delayFirstReport: true
        )
        controller.positionSeconds = 42.5

        model.checkpointPlayback()

        XCTAssertEqual(store.record?.playback?.positionSeconds, 42.5)
        for _ in 0..<100 {
            if await client.reportCallCountSnapshot() == 1 { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        let reportCallCount = await client.reportCallCountSnapshot()
        XCTAssertEqual(reportCallCount, 1)
        await client.resumeFirstReport()
        await model.flushPlaybackProgressReports()
    }

    @MainActor
    func testNaturalEndRemainsCompletedAfterStopAndCompletedResumeStartsAtZero() async throws {
        let identity = makePlaybackIdentity()
        let (model, controller, store, _) = try makeResumableFixture(identity: identity)
        controller.itemDuration = CMTime(seconds: 120, preferredTimescale: 600)
        controller.positionSeconds = 120
        let item = try XCTUnwrap(model.player?.currentItem)

        NotificationCenter.default.post(name: .AVPlayerItemDidPlayToEndTime, object: item)
        for _ in 0..<100 {
            if model.statusMessage == "Playback finished." { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        await model.flushPlaybackProgressReports()
        XCTAssertEqual(store.record?.playback?.completed, true)

        model.stop()
        await model.flushPlaybackProgressReports()
        model.checkpointPlayback()
        let savedCheckpoint = try XCTUnwrap(store.record?.playback)
        XCTAssertTrue(savedCheckpoint.completed)

        let (resumedModel, resumedController, recreatedStore, _) = try makeResumableFixture(
            identity: identity,
            checkpoint: savedCheckpoint,
            autoplay: true
        )
        XCTAssertEqual(resumedController.seekTargets.map(\.seconds), [0])
        XCTAssertEqual(recreatedStore.record?.playback, savedCheckpoint)

        resumedController.completeNextSeek(finished: true, position: .zero)
        await resumedModel.flushPlaybackProgressReports()

        XCTAssertEqual(resumedController.playCallCount, 1)
        XCTAssertEqual(recreatedStore.record?.playback?.positionSeconds, 0)
        XCTAssertEqual(recreatedStore.record?.playback?.completed, false)
    }

    @MainActor
    func testReplayAndManualSeekAwayFromNaturalEndClearCompletion() async throws {
        let replayIdentity = makePlaybackIdentity(taskID: "task-replay")
        let (replayModel, replayController, replayStore, _) = try makeResumableFixture(identity: replayIdentity)
        replayController.itemDuration = CMTime(seconds: 120, preferredTimescale: 600)
        replayController.positionSeconds = 120
        let replayItem = try XCTUnwrap(replayModel.player?.currentItem)

        NotificationCenter.default.post(name: .AVPlayerItemDidPlayToEndTime, object: replayItem)
        for _ in 0..<100 {
            if replayModel.statusMessage == "Playback finished." { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        await replayModel.flushPlaybackProgressReports()
        XCTAssertEqual(replayStore.record?.playback?.completed, true)

        replayModel.togglePlayback(using: replayController)
        replayController.completeNextSeek(finished: true, position: .zero)
        await replayModel.flushPlaybackProgressReports()

        XCTAssertEqual(replayStore.record?.playback?.positionSeconds, 0)
        XCTAssertEqual(replayStore.record?.playback?.completed, false)

        let seekIdentity = makePlaybackIdentity(taskID: "task-seek")
        let (seekModel, seekController, seekStore, _) = try makeResumableFixture(identity: seekIdentity)
        seekController.itemDuration = CMTime(seconds: 120, preferredTimescale: 600)
        seekController.positionSeconds = 120
        let seekItem = try XCTUnwrap(seekModel.player?.currentItem)

        NotificationCenter.default.post(name: .AVPlayerItemDidPlayToEndTime, object: seekItem)
        for _ in 0..<100 {
            if seekModel.statusMessage == "Playback finished." { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        await seekModel.flushPlaybackProgressReports()
        XCTAssertEqual(seekStore.record?.playback?.completed, true)

        seekModel.seek(by: -10, using: seekController)
        await seekModel.flushPlaybackProgressReports()

        XCTAssertEqual(seekStore.record?.playback?.positionSeconds, 110)
        XCTAssertEqual(seekStore.record?.playback?.completed, false)
    }

    @MainActor
    func testNormalSelectionOfSameMediaResetsSavedCheckpoint() async throws {
        let identity = makePlaybackIdentity()
        let checkpoint = ClientPlaybackCheckpoint(
            identity: identity,
            positionSeconds: 42,
            durationSeconds: 100,
            completed: true
        )
        let store = ClientResumptionStore(defaults: defaults)
        store.rememberTask(identity.task)
        store.beginPlayback(identity, checkpoint: checkpoint)
        let client = FakePlaybackProgressClient()
        let model = PlayerViewModel(
            defaults: defaults,
            autoplay: false,
            playbackProgressReportInterval: nil,
            resumptionStore: store,
            cacheClientFactory: { _ in client }
        )
        var playbackController: PlaybackControlSpy?
        model.playbackControllerFactory = { url in
            let player = AVPlayer(url: url)
            let controller = PlaybackControlSpy(timeControlStatus: .paused, currentItem: player.currentItem)
            playbackController = controller
            return (player, controller)
        }
        let context = PlayerPlaybackProgressContext(
            endpoint: identity.task.endpoint,
            libraryItemID: identity.libraryItemID,
            variantID: identity.variantID,
            resumptionIdentity: identity
        )

        XCTAssertTrue(
            model.loadTransient(
                streamURLText: "http://127.0.0.1:1/media/fixture.m3u8",
                progressContext: context
            )
        )
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(try XCTUnwrap(playbackController).playCallCount, 0)
        XCTAssertEqual(store.record?.playback?.positionSeconds, 0)
        XCTAssertEqual(store.record?.playback?.completed, false)
    }

    @MainActor
    func testNearEndCheckpointRemainsUnfinished() throws {
        let identity = makePlaybackIdentity()
        let (model, controller, store, _) = try makeResumableFixture(identity: identity)
        controller.itemDuration = CMTime(seconds: 100, preferredTimescale: 600)
        controller.positionSeconds = 99.5

        model.checkpointPlayback()

        XCTAssertEqual(store.record?.playback?.positionSeconds, 99.5)
        XCTAssertEqual(store.record?.playback?.completed, false)
    }

    @MainActor
    func testExplicitResumeWaitsForReadinessAndClampsToFreshDuration() async throws {
        let identity = makePlaybackIdentity()
        let checkpoint = ClientPlaybackCheckpoint(
            identity: identity,
            positionSeconds: 150,
            durationSeconds: 180,
            completed: false
        )
        let (model, controller, store, _) = try makeResumableFixture(
            identity: identity,
            checkpoint: checkpoint,
            readiness: .unknown,
            autoplay: true
        )
        controller.itemDuration = CMTime(seconds: 100, preferredTimescale: 600)

        XCTAssertEqual(model.playbackState, .buffering)
        XCTAssertTrue(controller.seekTargets.isEmpty)
        XCTAssertEqual(store.record?.playback, checkpoint)

        controller.setReadiness(.readyToPlay)

        XCTAssertEqual(controller.seekTargets.map(\.seconds), [99])
        XCTAssertEqual(controller.playCallCount, 0)
        XCTAssertEqual(store.record?.playback, checkpoint)

        controller.completeNextSeek(
            finished: true,
            position: CMTime(seconds: 99, preferredTimescale: 600)
        )
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(controller.playCallCount, 1)
        XCTAssertEqual(store.record?.playback?.positionSeconds, 99)
        XCTAssertEqual(store.record?.playback?.completed, false)
    }

    @MainActor
    func testIndefiniteFreshDurationResumesAtSavedPosition() async throws {
        let identity = makePlaybackIdentity()
        let checkpoint = ClientPlaybackCheckpoint(
            identity: identity,
            positionSeconds: 37,
            completed: false
        )
        let (model, controller, store, _) = try makeResumableFixture(
            identity: identity,
            checkpoint: checkpoint,
            duration: .indefinite,
            autoplay: true
        )

        XCTAssertEqual(controller.seekTargets.map(\.seconds), [37])
        controller.completeNextSeek(finished: true, position: CMTime(seconds: 37, preferredTimescale: 600))
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(controller.playCallCount, 1)
        XCTAssertEqual(store.record?.playback?.positionSeconds, 37)
        XCTAssertEqual(store.record?.playback?.completed, false)
    }

    @MainActor
    func testFinishedSeekAtWrongPositionFailsWithoutAutoplayOrCheckpointOverwrite() async throws {
        let identity = makePlaybackIdentity()
        let checkpoint = ClientPlaybackCheckpoint(
            identity: identity,
            positionSeconds: 52,
            durationSeconds: 100,
            completed: false
        )
        let (model, controller, store, _) = try makeResumableFixture(
            identity: identity,
            checkpoint: checkpoint,
            autoplay: true
        )

        controller.completeNextSeek(finished: true, position: .zero)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(model.validationMessage, "Could not restore the saved playback position. Try again.")
        XCTAssertNil(model.player)
        XCTAssertNil(model.loadedURL)
        XCTAssertFalse(model.canUsePlaybackControls)
        XCTAssertEqual(controller.playCallCount, 0)
        XCTAssertEqual(store.record?.playback, checkpoint)
    }

    @MainActor
    func testCompletedResumeRejectsFinishedSeekAwayFromZeroTarget() async throws {
        let identity = makePlaybackIdentity()
        let checkpoint = ClientPlaybackCheckpoint(
            identity: identity,
            positionSeconds: 120,
            durationSeconds: 120,
            completed: true
        )
        let (model, controller, store, _) = try makeResumableFixture(
            identity: identity,
            checkpoint: checkpoint,
            duration: CMTime(seconds: 120, preferredTimescale: 600),
            autoplay: true
        )

        XCTAssertEqual(controller.seekTargets.map(\.seconds), [0])
        controller.completeNextSeek(finished: true, position: CMTime(seconds: 10, preferredTimescale: 600))
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(model.validationMessage, "Could not restore the saved playback position. Try again.")
        XCTAssertNil(model.player)
        XCTAssertNil(model.loadedURL)
        XCTAssertFalse(model.canUsePlaybackControls)
        XCTAssertEqual(controller.playCallCount, 0)
        XCTAssertEqual(store.record?.playback, checkpoint)
    }

    @MainActor
    func testResumeReadinessFailureUnloadsPlayerAndPreservesCheckpoint() async throws {
        for initialReadiness in [AVPlayerItem.Status.failed, .unknown] {
            let identity = makePlaybackIdentity(taskID: "task-readiness-\(initialReadiness.rawValue)")
            let checkpoint = ClientPlaybackCheckpoint(
                identity: identity,
                positionSeconds: 47.5,
                durationSeconds: 100
            )
            let (model, controller, store, client) = try makeResumableFixture(
                identity: identity,
                checkpoint: checkpoint,
                readiness: initialReadiness,
                autoplay: true
            )

            if initialReadiness == .unknown {
                let staleReadiness = controller.lastReadinessObserver
                controller.setReadiness(.failed)
                staleReadiness?(.readyToPlay)
            }

            XCTAssertTrue(controller.seekTargets.isEmpty)
            await assertFailedResumeIsUnloaded(
                model, controller: controller, store: store, client: client, checkpoint: checkpoint
            )
        }
    }

    @MainActor
    func testResumeReadinessTimeoutUnloadsPlayerAndPreservesCheckpoint() async throws {
        let identity = makePlaybackIdentity()
        let checkpoint = ClientPlaybackCheckpoint(identity: identity, positionSeconds: 47.5, durationSeconds: 100)
        let (model, controller, store, client) = try makeResumableFixture(
            identity: identity,
            checkpoint: checkpoint,
            readiness: .unknown,
            autoplay: true,
            readinessTimeout: .milliseconds(20)
        )
        let staleReadiness = controller.lastReadinessObserver

        for _ in 0..<100 {
            if model.validationMessage != nil { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        staleReadiness?(.readyToPlay)

        XCTAssertTrue(controller.seekTargets.isEmpty)
        await assertFailedResumeIsUnloaded(
            model, controller: controller, store: store, client: client, checkpoint: checkpoint
        )
    }

    @MainActor
    func testFailedResumeSeekUnloadsPlayerAndPreservesCheckpoint() async throws {
        let identity = makePlaybackIdentity()
        let checkpoint = ClientPlaybackCheckpoint(identity: identity, positionSeconds: 52, durationSeconds: 100)
        let (model, controller, store, client) = try makeResumableFixture(
            identity: identity,
            checkpoint: checkpoint,
            autoplay: true
        )

        controller.completeNextSeek(finished: false, position: .zero)

        await assertFailedResumeIsUnloaded(
            model, controller: controller, store: store, client: client, checkpoint: checkpoint
        )
    }

    @MainActor
    func testNearEndResumeClampsBelowFreshDurationAndStaysUnfinished() async throws {
        let identity = makePlaybackIdentity()
        let checkpoint = ClientPlaybackCheckpoint(
            identity: identity,
            positionSeconds: 99.5,
            durationSeconds: 100,
            completed: false
        )
        let (model, controller, store, _) = try makeResumableFixture(
            identity: identity,
            checkpoint: checkpoint,
            duration: CMTime(seconds: 100, preferredTimescale: 600)
        )

        XCTAssertEqual(controller.seekTargets.map(\.seconds), [99])
        controller.completeNextSeek(finished: true, position: CMTime(seconds: 99, preferredTimescale: 600))
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(store.record?.playback?.positionSeconds, 99)
        XCTAssertEqual(store.record?.playback?.completed, false)
    }

    @MainActor
    func testPendingResumeCheckpointAndStaleReadinessSurviveStop() async throws {
        let identity = makePlaybackIdentity()
        let checkpoint = ClientPlaybackCheckpoint(
            identity: identity,
            positionSeconds: 47.5,
            durationSeconds: 100,
            completed: false
        )
        let (model, controller, store, _) = try makeResumableFixture(
            identity: identity,
            checkpoint: checkpoint,
            readiness: .unknown,
            autoplay: true
        )

        model.checkpointPlayback()
        XCTAssertEqual(store.record?.playback, checkpoint)

        model.stop()
        controller.lastReadinessObserver?(.readyToPlay)
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(store.record?.playback, checkpoint)
        XCTAssertTrue(controller.seekTargets.isEmpty)
        XCTAssertEqual(controller.playCallCount, 0)
    }

    @MainActor
    func testManualSeekInvalidatesPendingResumeWithoutStaleAutoplayOrOverwrite() async throws {
        let identity = makePlaybackIdentity()
        let checkpoint = ClientPlaybackCheckpoint(
            identity: identity,
            positionSeconds: 42,
            durationSeconds: 100,
            completed: false
        )
        let (model, controller, store, _) = try makeResumableFixture(
            identity: identity,
            checkpoint: checkpoint,
            autoplay: true
        )
        controller.itemDuration = CMTime(seconds: 100, preferredTimescale: 600)

        model.seek(by: 5, using: controller)
        controller.completeNextSeek(finished: true, position: CMTime(seconds: 42, preferredTimescale: 600))
        await model.flushPlaybackProgressReports()

        XCTAssertEqual(controller.playCallCount, 0)
        XCTAssertEqual(store.record?.playback?.positionSeconds, 5)
    }

    @MainActor
    func testResumeSeekTimeoutRetainsCheckpointAndRejectsLateCompletion() async throws {
        let identity = makePlaybackIdentity()
        let checkpoint = ClientPlaybackCheckpoint(
            identity: identity,
            positionSeconds: 52,
            durationSeconds: 100,
            completed: false
        )
        let (model, controller, store, client) = try makeResumableFixture(
            identity: identity,
            checkpoint: checkpoint,
            autoplay: true,
            seekTimeout: .milliseconds(20)
        )

        for _ in 0..<100 {
            if model.validationMessage != nil { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertEqual(model.validationMessage, "Could not restore the saved playback position. Try again.")

        controller.completeNextSeek(finished: true, position: CMTime(seconds: 52, preferredTimescale: 600))

        await assertFailedResumeIsUnloaded(
            model, controller: controller, store: store, client: client, checkpoint: checkpoint
        )
    }

    @MainActor
    func testFailedProgressReportDoesNotDiscardLocalCheckpoint() async throws {
        let identity = makePlaybackIdentity()
        let (model, controller, store, _) = try makeResumableFixture(
            identity: identity,
            failReports: true
        )
        controller.positionSeconds = 18

        let result = await model.reportCurrentPlaybackProgress(intent: .playing)

        XCTAssertNil(result)
        XCTAssertEqual(model.playbackProgressReportingMessage, "Could not report playback position.")
        XCTAssertEqual(store.record?.playback?.positionSeconds, 18)
    }

    @MainActor
    func testTransientLoadCanReportPlaybackProgressWithContext() async throws {
        let client = FakePlaybackProgressClient()
        let endpoint = CacheServerEndpoint(host: "mac-mini.local")
        let model = PlayerViewModel(
            defaults: defaults,
            autoplay: false,
            playbackProgressReportInterval: nil,
            cacheClientFactory: { _ in client }
        )
        let context = PlayerPlaybackProgressContext(
            endpoint: endpoint,
            libraryItemID: "bilibili.hls.session-1",
            variantID: "h264"
        )

        let didLoad = model.loadTransient(
            streamURLText: "mac-mini.local:8080/hls/session-1/master.m3u8",
            progressContext: context
        )
        let result = await model.reportCurrentPlaybackProgress(intent: .playing)

        XCTAssertTrue(didLoad)
        XCTAssertEqual(result?.accepted, true)
        let reports = await client.reportsSnapshot()
        XCTAssertTrue(
            reports.contains {
                $0.intent == .playing
                    && $0.playbackURI == "http://mac-mini.local:8080/hls/session-1/master.m3u8"
                    && $0.libraryItemID == "bilibili.hls.session-1"
                    && $0.variantID == "h264"
            }
        )
        XCTAssertNil(model.playbackProgressReportingMessage)
    }

    @MainActor
    func testPlaybackProgressReportsAreSerialized() async throws {
        let client = FakePlaybackProgressClient(delayFirstReport: true)
        let endpoint = CacheServerEndpoint(host: "mac-mini.local")
        let model = PlayerViewModel(
            defaults: defaults,
            autoplay: false,
            playbackProgressReportInterval: nil,
            cacheClientFactory: { _ in client }
        )
        let context = PlayerPlaybackProgressContext(
            endpoint: endpoint,
            libraryItemID: "bilibili.hls.session-1",
            variantID: "h264"
        )

        _ = model.loadTransient(
            streamURLText: "mac-mini.local:8080/hls/session-1/master.m3u8",
            progressContext: context
        )
        model.stop()
        try await Task.sleep(for: .milliseconds(50))

        let callCountBeforeResume = await client.reportCallCountSnapshot()
        let reportsBeforeResume = await client.reportsSnapshot()
        XCTAssertEqual(callCountBeforeResume, 1)
        XCTAssertTrue(reportsBeforeResume.isEmpty)

        await client.resumeFirstReport()
        try await Task.sleep(for: .milliseconds(50))

        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.map(\.intent), [.started, .stopped])
    }

    @MainActor
    func testFlushPlaybackProgressReportsWaitsForQueuedStartedReport() async throws {
        let client = FakePlaybackProgressClient(delayFirstReport: true)
        let endpoint = CacheServerEndpoint(host: "mac-mini.local")
        let model = PlayerViewModel(
            defaults: defaults,
            autoplay: false,
            playbackProgressReportInterval: nil,
            cacheClientFactory: { _ in client }
        )
        let context = PlayerPlaybackProgressContext(
            endpoint: endpoint,
            libraryItemID: "bilibili.hls.session-1",
            variantID: "h264"
        )

        _ = model.loadTransient(
            streamURLText: "mac-mini.local:8080/hls/session-1/master.m3u8",
            progressContext: context
        )
        let flushTask = Task {
            await model.flushPlaybackProgressReports()
        }
        try await Task.sleep(for: .milliseconds(50))

        let callCountBeforeResume = await client.reportCallCountSnapshot()
        let reportsBeforeResume = await client.reportsSnapshot()
        XCTAssertEqual(callCountBeforeResume, 1)
        XCTAssertTrue(reportsBeforeResume.isEmpty)

        await client.resumeFirstReport()
        await flushTask.value

        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.map(\.intent), [.started])
    }

    @MainActor
    func testPlaybackEndReportsStoppedAndStopsPeriodicProgress() async throws {
        let client = FakePlaybackProgressClient()
        let endpoint = CacheServerEndpoint(host: "mac-mini.local")
        let model = PlayerViewModel(
            defaults: defaults,
            autoplay: false,
            playbackProgressReportInterval: .milliseconds(25),
            cacheClientFactory: { _ in client }
        )
        let context = PlayerPlaybackProgressContext(
            endpoint: endpoint,
            libraryItemID: "bilibili.hls.session-1",
            variantID: "h264"
        )

        _ = model.loadTransient(
            streamURLText: "mac-mini.local:8080/hls/session-1/master.m3u8",
            progressContext: context
        )
        let item = try XCTUnwrap(model.player?.currentItem)

        NotificationCenter.default.post(name: .AVPlayerItemDidPlayToEndTime, object: item)

        let reports = await Self.waitForReports(from: client, minimumCount: 2)
        XCTAssertEqual(reports.map(\.intent), [.started, .stopped])
        XCTAssertEqual(model.playbackProgressStatusRefreshRequestID, 1)
        XCTAssertEqual(model.statusMessage, "Playback finished.")

        try await Task.sleep(for: .milliseconds(100))
        let reportsAfterStopWindow = await client.reportsSnapshot()
        XCTAssertEqual(reportsAfterStopWindow.map(\.intent), [.started, .stopped])
    }

    @MainActor
    func testBackwardSeekAwayFromPlaybackEndRestartsPeriodicProgress() async throws {
        let client = FakePlaybackProgressClient()
        let endpoint = CacheServerEndpoint(host: "mac-mini.local")
        let model = PlayerViewModel(
            defaults: defaults,
            autoplay: false,
            playbackProgressReportInterval: .milliseconds(25),
            cacheClientFactory: { _ in client }
        )
        let context = PlayerPlaybackProgressContext(
            endpoint: endpoint,
            libraryItemID: "bilibili.hls.session-1",
            variantID: "h264"
        )

        _ = model.loadTransient(
            streamURLText: "mac-mini.local:8080/hls/session-1/master.m3u8",
            progressContext: context
        )
        let item = try XCTUnwrap(model.player?.currentItem)
        NotificationCenter.default.post(name: .AVPlayerItemDidPlayToEndTime, object: item)

        let reportsAfterEnd = await Self.waitForReports(from: client, minimumCount: 2)
        XCTAssertEqual(reportsAfterEnd.map(\.intent), [.started, .stopped])

        let controller = PlaybackControlSpy(timeControlStatus: .paused, currentItem: item)
        controller.positionSeconds = 92
        model.seek(by: -10, using: controller)

        let replayReports = await Self.waitForReports(
            from: client,
            minimumCount: reportsAfterEnd.count + 2
        ).dropFirst(reportsAfterEnd.count)
        XCTAssertTrue(replayReports.contains { $0.intent == .seek })
        XCTAssertTrue(replayReports.contains { $0.intent == .paused })
    }

    @MainActor
    func testPausedPlaybackProgressKeepsLeaseWarm() async throws {
        let client = FakePlaybackProgressClient()
        let endpoint = CacheServerEndpoint(host: "mac-mini.local")
        let model = PlayerViewModel(
            defaults: defaults,
            autoplay: false,
            playbackProgressReportInterval: .milliseconds(25),
            cacheClientFactory: { _ in client }
        )
        let context = PlayerPlaybackProgressContext(
            endpoint: endpoint,
            libraryItemID: "bilibili.hls.session-1",
            variantID: "h264"
        )

        _ = model.loadTransient(
            streamURLText: "mac-mini.local:8080/hls/session-1/master.m3u8",
            progressContext: context
        )

        let reports = await Self.waitForReports(from: client, minimumCount: 2)
        XCTAssertTrue(
            reports.contains {
                $0.intent == .paused
                    && $0.playbackURI == "http://mac-mini.local:8080/hls/session-1/master.m3u8"
            }
        )
    }

    @MainActor
    func testStalePlaybackEndNotificationDoesNotStopNewPlayback() async throws {
        let client = FakePlaybackProgressClient()
        let endpoint = CacheServerEndpoint(host: "mac-mini.local")
        let model = PlayerViewModel(
            defaults: defaults,
            autoplay: false,
            playbackProgressReportInterval: nil,
            cacheClientFactory: { _ in client }
        )
        let firstContext = PlayerPlaybackProgressContext(
            endpoint: endpoint,
            libraryItemID: "bilibili.hls.session-1",
            variantID: "h264"
        )
        let secondContext = PlayerPlaybackProgressContext(
            endpoint: endpoint,
            libraryItemID: "bilibili.hls.session-2",
            variantID: "h264"
        )

        _ = model.loadTransient(
            streamURLText: "mac-mini.local:8080/hls/session-1/master.m3u8",
            progressContext: firstContext
        )
        let firstItem = try XCTUnwrap(model.player?.currentItem)

        NotificationCenter.default.post(name: .AVPlayerItemDidPlayToEndTime, object: firstItem)
        _ = model.loadTransient(
            streamURLText: "mac-mini.local:8080/hls/session-2/master.m3u8",
            progressContext: secondContext
        )
        try await Task.sleep(for: .milliseconds(100))

        XCTAssertEqual(model.loadedURL?.absoluteString, "http://mac-mini.local:8080/hls/session-2/master.m3u8")
        XCTAssertEqual(model.statusMessage, "Playing http://mac-mini.local:8080/hls/session-2/master.m3u8")
        let reports = await client.reportsSnapshot()
        XCTAssertFalse(
            reports.contains {
                $0.intent == .stopped
                    && $0.playbackURI == "http://mac-mini.local:8080/hls/session-2/master.m3u8"
            }
        )
    }

    @MainActor
    func testTransientLoadWithStaleManualSequenceDoesNotReplaceManualPlayback() {
        let model = PlayerViewModel(defaults: defaults, autoplay: false)
        let staleSequence = model.manualInteractionSequence

        model.load(streamURLText: "example.com/manual.m3u8")
        let didLoad = model.loadTransient(
            streamURLText: "mac-mini.local:8080/media/item-a/original",
            ifManualInteractionSequenceMatches: staleSequence
        )

        XCTAssertFalse(didLoad)
        XCTAssertEqual(model.loadedURL?.absoluteString, "http://example.com/manual.m3u8")
        XCTAssertEqual(model.streamURLText, "http://example.com/manual.m3u8")
    }

    @MainActor
    func testTransientLoadWithStaleManualSequenceDoesNotRestartAfterClear() {
        let model = PlayerViewModel(defaults: defaults, autoplay: false)
        model.load(streamURLText: "example.com/manual.m3u8")
        let staleSequence = model.manualInteractionSequence

        model.clear()
        let didLoad = model.loadTransient(
            streamURLText: "mac-mini.local:8080/media/item-a/original",
            ifManualInteractionSequenceMatches: staleSequence
        )

        XCTAssertFalse(didLoad)
        XCTAssertNil(model.loadedURL)
        XCTAssertNil(model.player)
        XCTAssertEqual(model.statusMessage, "Ready for an HTTP or HTTPS stream on your network.")
    }

    @MainActor
    func testInvalidURLKeepsCurrentPlayer() {
        let model = PlayerViewModel(defaults: defaults, autoplay: false)
        model.streamURLText = "https://example.com/movie.m3u8"
        model.load()
        let loadedURL = model.loadedURL

        model.streamURLText = "file:///tmp/movie.mp4"
        model.load()

        XCTAssertEqual(model.loadedURL, loadedURL)
        XCTAssertNotNil(model.player)
        XCTAssertEqual(model.validationMessage, "Use an HTTP or HTTPS URL.")
    }

    @MainActor
    func testCorrectingInvalidURLClearsValidationMessage() {
        let model = PlayerViewModel(defaults: defaults, autoplay: false)
        model.streamURLText = "file:///tmp/movie.mp4"
        model.load()

        model.streamURLText = "https://example.com/movie.m3u8"

        XCTAssertNil(model.validationMessage)
        XCTAssertEqual(model.statusMessage, "Ready to play https://example.com/movie.m3u8.")
    }

    @MainActor
    func testClearCanResetEmptyValidationState() {
        let model = PlayerViewModel(defaults: defaults, autoplay: false)

        model.load()

        XCTAssertTrue(model.canClear)
        XCTAssertEqual(model.validationMessage, "Use an HTTP or HTTPS URL.")

        model.clear()

        XCTAssertFalse(model.canClear)
        XCTAssertEqual(model.statusMessage, "Ready for an HTTP or HTTPS stream on your network.")
        XCTAssertNil(model.validationMessage)
    }

    @MainActor
    func testClearRemovesSavedURLAndStopsPlayer() {
        let model = PlayerViewModel(defaults: defaults, autoplay: false)
        model.streamURLText = "https://example.com/movie.m3u8"
        model.load()

        model.clear()

        XCTAssertEqual(model.streamURLText, "")
        XCTAssertNil(model.loadedURL)
        XCTAssertNil(model.player)
        XCTAssertNil(defaults.string(forKey: PlayerViewModel.lastStreamURLDefaultsKey))
    }

    @MainActor
    func testClearRequestsPlaybackProgressStatusRefreshAfterQueuedStop() async {
        let client = FakePlaybackProgressClient()
        let endpoint = CacheServerEndpoint(host: "mac-mini.local")
        let model = PlayerViewModel(
            defaults: defaults,
            autoplay: false,
            playbackProgressReportInterval: nil,
            cacheClientFactory: { _ in client }
        )
        let context = PlayerPlaybackProgressContext(
            endpoint: endpoint,
            libraryItemID: "bilibili.hls.session-1",
            variantID: "h264"
        )

        _ = model.loadTransient(
            streamURLText: "mac-mini.local:8080/hls/session-1/master.m3u8",
            progressContext: context
        )

        XCTAssertEqual(model.playbackProgressStatusRefreshRequestID, 0)

        model.clear()
        await model.flushPlaybackProgressReports()

        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.map(\.intent), [.started, .stopped])
        XCTAssertEqual(model.playbackProgressStatusRefreshRequestID, 1)
        XCTAssertEqual(model.streamURLText, "")
        XCTAssertNil(model.loadedURL)
        XCTAssertNil(model.player)
    }
}

@MainActor
private final class PlaybackControlSpy: PlayerPlaybackControlling {
    var currentItem: AVPlayerItem?
    var itemDuration: CMTime = .indefinite
    var readinessStatus: AVPlayerItem.Status = .readyToPlay
    var timeControlStatus: AVPlayer.TimeControlStatus
    var defaultRate: Float = 1
    var playbackTime: CMTime = .zero
    var positionSeconds: Double {
        get { playbackTime.seconds }
        set { playbackTime = CMTime(seconds: newValue, preferredTimescale: 600) }
    }
    private(set) var playCallCount = 0
    private(set) var pauseCallCount = 0
    private(set) var seekTargets: [CMTime] = []
    private var seekCompletions: [@MainActor @Sendable (Bool) -> Void] = []
    private var readinessObservers: [UUID: @MainActor @Sendable (AVPlayerItem.Status) -> Void] = [:]
    private(set) var lastReadinessObserver: (@MainActor @Sendable (AVPlayerItem.Status) -> Void)?

    init(timeControlStatus: AVPlayer.TimeControlStatus, currentItem: AVPlayerItem? = nil) {
        self.timeControlStatus = timeControlStatus
        self.currentItem = currentItem
    }

    func play() {
        playCallCount += 1
        timeControlStatus = .waitingToPlayAtSpecifiedRate
    }

    func pause() {
        pauseCallCount += 1
        timeControlStatus = .paused
    }

    func currentTime() -> CMTime {
        playbackTime
    }

    func seek(to time: CMTime, toleranceBefore: CMTime, toleranceAfter: CMTime) {
        seekTargets.append(time)
        playbackTime = time
    }

    func seekForPlayback(to time: CMTime, completion: @escaping @MainActor @Sendable (Bool) -> Void) {
        seekTargets.append(time)
        seekCompletions.append(completion)
    }

    func observeReadiness(
        _ observer: @escaping @MainActor @Sendable (AVPlayerItem.Status) -> Void
    ) -> AnyCancellable {
        let id = UUID()
        readinessObservers[id] = observer
        lastReadinessObserver = observer
        return AnyCancellable { [weak self] in
            Task { @MainActor in
                self?.readinessObservers[id] = nil
            }
        }
    }

    func setReadiness(_ status: AVPlayerItem.Status) {
        readinessStatus = status
        for observer in readinessObservers.values {
            observer(status)
        }
    }

    func completeNextSeek(finished: Bool, position: CMTime? = nil) {
        if let position { playbackTime = position }
        seekCompletions.removeFirst()(finished)
    }
}

private actor FakePlaybackProgressClient: CacheControlClient {
    private let delayFirstReport: Bool
    private let failReports: Bool
    private var reports: [PlaybackProgressReport] = []
    private var reportCallCount = 0
    private var firstReportContinuation: CheckedContinuation<Void, Never>?

    init(delayFirstReport: Bool = false, failReports: Bool = false) {
        self.delayFirstReport = delayFirstReport
        self.failReports = failReports
    }

    func reportPlaybackProgress(_ report: PlaybackProgressReport) async throws -> PlaybackProgressReportResult {
        reportCallCount += 1
        if delayFirstReport, reportCallCount == 1 {
            await withCheckedContinuation { continuation in
                firstReportContinuation = continuation
            }
        }
        if failReports {
            throw FakePlaybackProgressClientError.reportFailed
        }
        reports.append(report)
        return PlaybackProgressReportResult(
            accepted: true,
            sessionID: "session-1",
            message: "Playback progress recorded."
        )
    }

    func reportsSnapshot() -> [PlaybackProgressReport] {
        reports
    }

    func reportCallCountSnapshot() -> Int {
        reportCallCount
    }

    func resumeFirstReport() {
        firstReportContinuation?.resume()
        firstReportContinuation = nil
    }

    func getServerInfo() async throws -> CacheServerSummary {
        throw FakePlaybackProgressClientError.notImplemented
    }

    func listCacheRoots() async throws -> [CacheRoot] {
        throw FakePlaybackProgressClientError.notImplemented
    }

    func listLibraryItemsPage(
        pageToken: String,
        pageSize: Int,
        searchText: String?
    ) async throws -> CacheLibraryItemsPage {
        throw FakePlaybackProgressClientError.notImplemented
    }

    func getPlaybackSource(itemID: String, variantID: String) async throws -> CachePlaybackSource {
        throw FakePlaybackProgressClientError.notImplemented
    }

    func deleteLibraryItem(id: String) async throws -> Bool {
        throw FakePlaybackProgressClientError.notImplemented
    }

    func getTask(id: String) async throws -> CacheTask {
        throw FakePlaybackProgressClientError.notImplemented
    }

    func watchTasks(ids: [String]) async -> AsyncThrowingStream<CacheTask, Error> {
        AsyncThrowingStream { continuation in
            continuation.finish(throwing: FakePlaybackProgressClientError.notImplemented)
        }
    }

    func cancelTask(id: String) async throws -> CacheTask {
        throw FakePlaybackProgressClientError.notImplemented
    }

    func createBilibiliPlaybackTask(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask {
        throw FakePlaybackProgressClientError.notImplemented
    }
}

private enum FakePlaybackProgressClientError: Error {
    case notImplemented
    case reportFailed
}
