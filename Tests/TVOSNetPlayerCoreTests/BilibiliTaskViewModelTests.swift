import TVOSNetPlayerCacheClient
import XCTest
@testable import TVOSNetPlayerCore

@MainActor
final class BilibiliTaskViewModelTests: XCTestCase {
    func testSubmitCreatesPlaybackTaskWithOptionsAndStartsWatching() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(state: "TASK_STATE_PREPARING", message: "Preparing playback."))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1test",
            qualityPreference: "1080p",
            encodingPreference: "h264",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        let requests = await client.createdRequestsSnapshot()
        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(requests.first?.urlOrID, "BV1test")
        XCTAssertEqual(requests.first?.options.qualityPreference, "1080p")
        XCTAssertEqual(requests.first?.options.encodingPreference, "h264")
        XCTAssertEqual(model.currentTask?.id, "bilibili-playback-1")
        XCTAssertTrue(model.isWatching)

        model.clearTask()
    }

    func testWatchUpdateExposesPlayableURL() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(state: "TASK_STATE_PREPARING"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1ready",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.yield(.playableFixture(source: "BV1ready"))
        await waitUntil(model.currentTask?.state == "TASK_STATE_PLAYABLE")

        XCTAssertEqual(
            model.playableURL?.absoluteString,
            "http://mac-mini.local:8080/hls/bilibili-playback-1/master.m3u8"
        )
        XCTAssertTrue(model.canPlay)

        model.finishPreparedPlayback(didStartPlayback: true)
        XCTAssertEqual(model.statusMessage, "Playing Ready video.")

        model.clearTask()
    }

    func testClearPlaybackStatusRestoresTaskStatus() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.playableFixture(source: "BV1ready"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1ready",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        model.finishPreparedPlayback(didStartPlayback: true)
        XCTAssertEqual(model.statusMessage, "Playing Ready video.")

        model.clearPlaybackStatus()

        XCTAssertEqual(model.statusMessage, "Ready video is ready to play.")

        model.clearTask()
    }

    func testCancelUpdatesCurrentTask() async {
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(state: "TASK_STATE_PREPARING"))
            ],
            cancelResponsesByID: [
                "bilibili-playback-1": .fixture(state: "TASK_STATE_CANCEL_REQUESTED", message: "Cancelling task.")
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1cancel",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await model.cancel(serverAddressText: "mac-mini.local:50051")

        let cancelledIDs = await client.cancelledIDsSnapshot()
        XCTAssertEqual(cancelledIDs, ["bilibili-playback-1"])
        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_CANCEL_REQUESTED")
        XCTAssertEqual(model.statusMessage, "Cancelling task.")

        model.clearTask()
    }

    func testRetryUsesFailedTaskSource() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(source: "BV1original", state: "TASK_STATE_PREPARING")),
            .success(.fixture(source: "BV1original", state: "TASK_STATE_PREPARING")),
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1original",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.yield(.fixture(source: "BV1original", state: "TASK_STATE_FAILED", message: "Planning failed."))
        await waitUntil(model.canRetry)
        model.sourceText = "BV1different"

        await model.retry(serverAddressText: "mac-mini.local:50051")

        let requests = await client.createdRequestsSnapshot()
        XCTAssertEqual(requests.map(\.urlOrID), ["BV1original", "BV1original"])

        model.clearTask()
    }

    private func waitUntil(
        _ condition: @autoclosure @escaping @MainActor () -> Bool,
        file: StaticString = #filePath,
        line: UInt = #line
    ) async {
        for _ in 0..<100 {
            if condition() {
                return
            }
            try? await Task.sleep(nanoseconds: 10_000_000)
        }

        XCTAssertTrue(condition(), file: file, line: line)
    }
}

private actor FakeBilibiliCacheControlClient: CacheControlClient {
    private var createResponses: [Result<CacheTask, Error>]
    private let cancelResponsesByID: [String: CacheTask]
    private var createdRequests: [(urlOrID: String, options: BilibiliPlaybackTaskOptions)] = []
    private var cancelledIDs: [String] = []
    private var watchContinuations: [AsyncThrowingStream<CacheTask, Error>.Continuation] = []
    private var watchWaiters: [CheckedContinuation<Void, Never>] = []

    init(
        createResponses: [Result<CacheTask, Error>],
        cancelResponsesByID: [String: CacheTask] = [:]
    ) {
        self.createResponses = createResponses
        self.cancelResponsesByID = cancelResponsesByID
    }

    func getServerInfo() async throws -> CacheServerSummary {
        throw FakeBilibiliCacheControlClientError.notImplemented
    }

    func listLibraryItemsPage(
        pageToken: String,
        pageSize: Int,
        searchText: String?
    ) async throws -> CacheLibraryItemsPage {
        throw FakeBilibiliCacheControlClientError.notImplemented
    }

    func getPlaybackSource(itemID: String, variantID: String) async throws -> CachePlaybackSource {
        throw FakeBilibiliCacheControlClientError.notImplemented
    }

    func getTask(id: String) async throws -> CacheTask {
        throw FakeBilibiliCacheControlClientError.notImplemented
    }

    func watchTasks(ids: [String]) async -> AsyncThrowingStream<CacheTask, Error> {
        AsyncThrowingStream { continuation in
            Task {
                self.storeWatchContinuation(continuation)
            }
        }
    }

    func cancelTask(id: String) async throws -> CacheTask {
        cancelledIDs.append(id)
        return cancelResponsesByID[id] ?? .fixture(state: "TASK_STATE_CANCEL_REQUESTED")
    }

    func createBilibiliPlaybackTask(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask {
        createdRequests.append((urlOrID, options))
        guard !createResponses.isEmpty else {
            throw FakeBilibiliCacheControlClientError.noCreateResponse
        }

        switch createResponses.removeFirst() {
        case let .success(task):
            return task
        case let .failure(error):
            throw error
        }
    }

    func createdRequestsSnapshot() -> [(urlOrID: String, options: BilibiliPlaybackTaskOptions)] {
        createdRequests
    }

    func cancelledIDsSnapshot() -> [String] {
        cancelledIDs
    }

    func waitForWatchSubscription() async {
        guard watchContinuations.isEmpty else {
            return
        }

        await withCheckedContinuation { continuation in
            watchWaiters.append(continuation)
        }
    }

    func yield(_ task: CacheTask) {
        watchContinuations.forEach { $0.yield(task) }
    }

    private func storeWatchContinuation(
        _ continuation: AsyncThrowingStream<CacheTask, Error>.Continuation
    ) {
        watchContinuations.append(continuation)
        let waiters = watchWaiters
        watchWaiters = []
        waiters.forEach { $0.resume() }
    }
}

private enum FakeBilibiliCacheControlClientError: Error {
    case notImplemented
    case noCreateResponse
}

private extension CacheTask {
    static func fixture(
        id: String = "bilibili-playback-1",
        source: String = "BV1test",
        title: String = "Ready video",
        state: String,
        progress: Double = 0.25,
        message: String = "Preparing playback.",
        playbackSource: CachePlaybackSource? = nil,
        playbackSession: CacheBilibiliPlaybackSession? = nil
    ) -> Self {
        Self(
            id: id,
            kind: "TASK_KIND_BILIBILI_PROGRESSIVE_PLAYBACK",
            state: state,
            source: source,
            title: title,
            progress: progress,
            message: message,
            libraryItemID: "",
            playbackSource: playbackSource,
            playbackSession: playbackSession
        )
    }

    static func playableFixture(source: String = "BV1test") -> Self {
        .fixture(
            source: source,
            state: "TASK_STATE_PLAYABLE",
            progress: 1,
            message: "Bilibili playback session is playable.",
            playbackSource: CachePlaybackSource(
                itemID: "bilibili-playback-1",
                variantID: "h264",
                playbackProtocol: "PLAYBACK_PROTOCOL_HLS",
                uri: "http://mac-mini.local:8080/hls/bilibili-playback-1/master.m3u8"
            ),
            playbackSession: CacheBilibiliPlaybackSession(
                id: "bilibili-playback-1",
                title: "Ready video",
                contentID: "BV1ready-cid1",
                selectedVariantID: "h264",
                selectedVariant: nil,
                variants: []
            )
        )
    }
}
