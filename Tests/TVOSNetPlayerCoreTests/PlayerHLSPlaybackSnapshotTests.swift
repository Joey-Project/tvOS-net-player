import AVFoundation
import Combine
import XCTest
@testable import TVOSNetPlayerCore
import TVOSNetPlayerCacheClient

final class PlayerHLSPlaybackSnapshotTests: XCTestCase {
    @MainActor
    func testCapturesActualRateAndInjectedObservationTimeWithoutTransportEffects() throws {
        let (model, controller) = makeFixture(status: .playing)
        controller.defaultRate = 1.5
        controller.actualRate = 0.75
        controller.playbackTime = Self.time(4)
        controller.loadedRanges = [Self.range(0, 12)]
        model.playbackSnapshotNowNanoseconds = { 12_345 }
        let interactionSequence = model.manualInteractionSequence
        let refreshRequestID = model.playbackProgressStatusRefreshRequestID
        let playCalls = controller.playCallCount
        let pauseCalls = controller.pauseCallCount
        let seeks = controller.seekTargets

        let snapshot = try XCTUnwrap(model.captureHLSPlaybackSnapshot())

        XCTAssertTrue(snapshot.isPlaying)
        XCTAssertEqual(snapshot.playheadSeconds, 4)
        XCTAssertEqual(snapshot.contiguousBufferedEndSeconds, 12)
        XCTAssertEqual(snapshot.playbackRate, 0.75)
        XCTAssertEqual(snapshot.observedAtNanoseconds, 12_345)
        XCTAssertEqual(controller.playCallCount, playCalls)
        XCTAssertEqual(controller.pauseCallCount, pauseCalls)
        XCTAssertEqual(controller.seekTargets, seeks)
        XCTAssertEqual(model.manualInteractionSequence, interactionSequence)
        XCTAssertEqual(model.playbackProgressStatusRefreshRequestID, refreshRequestID)
    }

    @MainActor
    func testPausedAndWaitingSnapshotsDoNotClaimPlayback() throws {
        for status in [AVPlayer.TimeControlStatus.paused, .waitingToPlayAtSpecifiedRate] {
            let (model, controller) = makeFixture(status: status)
            controller.actualRate = 0

            let snapshot = try XCTUnwrap(model.captureHLSPlaybackSnapshot())

            XCTAssertFalse(snapshot.isPlaying)
            XCTAssertEqual(snapshot.playbackRate, 0)
        }
    }

    @MainActor
    func testAVPlayerAdapterUsesActualZeroRateInsteadOfDefaultRate() throws {
        let player = AVPlayer()
        player.defaultRate = 2
        player.pause()

        XCTAssertEqual(player.actualPlaybackRate, 0)
        let snapshot = try XCTUnwrap(
            HLSPlaybackSnapshot.capture(
                timeControlStatus: player.timeControlStatus,
                playbackRate: try XCTUnwrap(player.actualPlaybackRate),
                currentTime: Self.time(0),
                loadedTimeRanges: [],
                observedAtNanoseconds: 50
            ))
        XCTAssertFalse(snapshot.isPlaying)

        let model = PlayerViewModel(autoplay: false, playbackProgressReportInterval: nil)
        model.playbackControllerFactory = { _ in (player, player) }
        XCTAssertTrue(model.loadTransient(streamURLText: "http://127.0.0.1:1/media/no-item.m3u8"))
        XCTAssertNil(model.captureHLSPlaybackSnapshot())
    }

    @MainActor
    func testUnavailableRateReadinessItemOrIdentityReturnsNil() {
        let (noRateModel, noRateController) = makeFixture(status: .playing)
        noRateController.actualRate = nil
        XCTAssertNil(noRateModel.captureHLSPlaybackSnapshot())

        let (failedModel, failedController) = makeFixture(status: .playing)
        failedController.readinessStatus = .failed
        XCTAssertNil(failedModel.captureHLSPlaybackSnapshot())

        let (noItemModel, _) = makeFixture(status: .playing, hasItem: false)
        XCTAssertNil(noItemModel.captureHLSPlaybackSnapshot())

        let (mismatchedModel, _) = makeFixture(status: .playing, mismatchedItems: true)
        XCTAssertNil(mismatchedModel.captureHLSPlaybackSnapshot())
    }

    @MainActor
    func testUnresolvedResumeAndPendingReplayReturnNilWithoutAdditionalTransport() throws {
        let (resumeModel, resumeController) = makeFixture(status: .paused)
        let identity = Self.playbackIdentity()
        let checkpoint = ClientPlaybackCheckpoint(identity: identity, positionSeconds: 8)
        let context = PlayerPlaybackProgressContext(
            endpoint: identity.task.endpoint,
            resumptionIdentity: identity
        )
        XCTAssertTrue(
            resumeModel.loadTransient(
                streamURLText: "http://127.0.0.1:1/media/resume.m3u8",
                progressContext: context,
                resumeCheckpoint: checkpoint
            ))
        XCTAssertTrue(resumeModel.isPlaybackPositionUnrestored)
        let resumePauseCalls = resumeController.pauseCallCount
        let resumeSeeks = resumeController.seekTargets
        XCTAssertNil(resumeModel.captureHLSPlaybackSnapshot())
        XCTAssertEqual(resumeController.pauseCallCount, resumePauseCalls)
        XCTAssertEqual(resumeController.seekTargets, resumeSeeks)

        let (replayModel, replayController) = makeFixture(status: .paused)
        replayController.playbackTime = Self.time(10)
        replayController.itemDuration = Self.time(10)
        replayModel.togglePlayback(using: replayController)
        let replayPlayCalls = replayController.playCallCount
        let replayPauseCalls = replayController.pauseCallCount
        let replaySeeks = replayController.seekTargets
        XCTAssertNil(replayModel.captureHLSPlaybackSnapshot())
        XCTAssertEqual(replayController.playCallCount, replayPlayCalls)
        XCTAssertEqual(replayController.pauseCallCount, replayPauseCalls)
        XCTAssertEqual(replayController.seekTargets, replaySeeks)
    }

    @MainActor
    func testFinishedItemReturnsNil() {
        let (model, controller) = makeFixture(status: .paused)
        controller.playbackTime = Self.time(10)
        controller.itemDuration = Self.time(10)

        XCTAssertNil(model.captureHLSPlaybackSnapshot())
    }

    @MainActor
    private func makeFixture(
        status: AVPlayer.TimeControlStatus,
        hasItem: Bool = true,
        mismatchedItems: Bool = false
    ) -> (PlayerViewModel, PlaybackSnapshotControllerSpy) {
        let model = PlayerViewModel(autoplay: false, playbackProgressReportInterval: nil)
        var controller: PlaybackSnapshotControllerSpy!
        model.playbackControllerFactory = { _ in
            let item = AVPlayerItem(asset: AVMutableComposition())
            let player = hasItem ? AVPlayer(playerItem: item) : AVPlayer()
            let controlledItem =
                mismatchedItems
                ? AVPlayerItem(asset: AVMutableComposition())
                : (hasItem ? item : nil)
            controller = PlaybackSnapshotControllerSpy(timeControlStatus: status, currentItem: controlledItem)
            return (player, controller)
        }
        XCTAssertTrue(model.loadTransient(streamURLText: "http://127.0.0.1:1/media/fixture.m3u8"))
        return (model, controller)
    }

    private static func playbackIdentity() -> ClientPlaybackIdentity {
        ClientPlaybackIdentity(
            task: ClientResumptionTaskIdentity(
                endpoint: CacheServerEndpoint(host: "cache.example", port: 50_051),
                serverID: "server-1",
                taskID: "task-1"
            ),
            libraryItemID: "item-1",
            variantID: "variant-1"
        )
    }

    private static func time(_ seconds: Double) -> CMTime {
        CMTime(seconds: seconds, preferredTimescale: 600)
    }

    private static func range(_ start: Double, _ duration: Double) -> CMTimeRange {
        CMTimeRange(start: time(start), duration: time(duration))
    }
}

@MainActor
private final class PlaybackSnapshotControllerSpy: PlayerPlaybackControlling {
    var currentItem: AVPlayerItem?
    var itemDuration: CMTime = .indefinite
    var readinessStatus: AVPlayerItem.Status = .readyToPlay
    var timeControlStatus: AVPlayer.TimeControlStatus
    var defaultRate: Float = 1
    var actualRate: Float? = 1
    var actualPlaybackRate: Float? { actualRate }
    var loadedRanges: [CMTimeRange] = []
    var loadedMediaTimeRanges: [CMTimeRange] { loadedRanges }
    var playbackTime = CMTime.zero
    private(set) var playCallCount = 0
    private(set) var pauseCallCount = 0
    private(set) var seekTargets: [CMTime] = []

    init(timeControlStatus: AVPlayer.TimeControlStatus, currentItem: AVPlayerItem?) {
        self.timeControlStatus = timeControlStatus
        self.currentItem = currentItem
    }

    func currentTime() -> CMTime { playbackTime }

    func play() { playCallCount += 1 }

    func pause() { pauseCallCount += 1 }

    func seek(to time: CMTime, toleranceBefore: CMTime, toleranceAfter: CMTime) {
        seekTargets.append(time)
        playbackTime = time
    }

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
