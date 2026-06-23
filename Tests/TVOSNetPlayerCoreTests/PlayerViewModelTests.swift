import AVFoundation
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
        try await Task.sleep(for: .milliseconds(100))

        let reports = await client.reportsSnapshot()
        XCTAssertEqual(reports.map(\.intent), [.started, .stopped])
        XCTAssertEqual(model.statusMessage, "Playback finished.")
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
}

private actor FakePlaybackProgressClient: CacheControlClient {
    private let delayFirstReport: Bool
    private var reports: [PlaybackProgressReport] = []
    private var reportCallCount = 0
    private var firstReportContinuation: CheckedContinuation<Void, Never>?

    init(delayFirstReport: Bool = false) {
        self.delayFirstReport = delayFirstReport
    }

    func reportPlaybackProgress(_ report: PlaybackProgressReport) async throws -> PlaybackProgressReportResult {
        reportCallCount += 1
        if delayFirstReport, reportCallCount == 1 {
            await withCheckedContinuation { continuation in
                firstReportContinuation = continuation
            }
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
}
