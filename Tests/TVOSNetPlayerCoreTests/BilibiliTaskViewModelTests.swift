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

    func testTerminalSubmitResponseDoesNotStartWatching() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(source: "BV1fail", state: "TASK_STATE_FAILED", message: "Planning failed."))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1fail",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_FAILED")
        XCTAssertFalse(model.isWatching)
        XCTAssertEqual(model.statusMessage, "Planning failed.")
        XCTAssertEqual(model.errorMessage, "Planning failed.")
        XCTAssertFalse(model.canCancel)
        XCTAssertTrue(model.canRetry)
    }

    func testDuplicateSubmitWhileSubmittingDoesNotInvalidateInFlightSubmission() async {
        let client = FakeBilibiliCacheControlClient(
            createResponses: [],
            suspendsCreateResponses: true
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1slow",
            clientFactory: { _ in client }
        )

        let submitTask = Task {
            await model.submit(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForCreateRequestCount(1)
        XCTAssertTrue(model.isSubmitting)

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.completeNextCreate(with: .success(.fixture(source: "BV1slow", state: "TASK_STATE_PREPARING")))
        await submitTask.value

        XCTAssertFalse(model.isSubmitting)
        XCTAssertEqual(model.currentTask?.source, "BV1slow")
        let requests = await client.createdRequestsSnapshot()
        XCTAssertEqual(requests.map(\.urlOrID), ["BV1slow"])

        model.clearTask()
    }

    func testSubmittingNewTaskDisablesCancellingPreviousTask() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.playableFixture(id: "old-task", source: "BV1old"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1old",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        XCTAssertEqual(model.currentTask?.id, "old-task")
        XCTAssertTrue(model.canPlay)

        await client.setSuspendsCreateResponses(true)
        model.sourceText = "BV1new"
        let submitTask = Task {
            await model.submit(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForCreateRequestCount(2)
        XCTAssertTrue(model.isSubmitting)
        XCTAssertFalse(model.canCancel)
        XCTAssertFalse(model.canPlay)

        await model.cancel(serverAddressText: "mac-mini.local:50051")
        let cancelledIDs = await client.cancelledIDsSnapshot()
        XCTAssertEqual(cancelledIDs, [])

        await client.completeNextCreate(
            with: .success(.fixture(id: "new-task", source: "BV1new", state: "TASK_STATE_PREPARING")))
        await submitTask.value

        XCTAssertEqual(model.currentTask?.id, "new-task")
        XCTAssertFalse(model.isSubmitting)

        model.clearTask()
    }

    func testCompletedPlayableTaskShowsCachedStatus() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(source: "BV1done", state: "TASK_STATE_PREPARING"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1done",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.yield(
            .playableFixture(
                source: "BV1done",
                state: "TASK_STATE_COMPLETED",
                libraryItemID: "cached-bilibili-playback-1",
                playbackSourceItemID: "cached-bilibili-playback-1"
            )
        )
        await waitUntil(model.currentTask?.state == "TASK_STATE_COMPLETED")

        XCTAssertTrue(model.canPlay)
        XCTAssertEqual(model.statusMessage, "Ready video is cached for LAN playback.")

        model.clearTask()
    }

    func testPlayableTaskRejectsMismatchedPlaybackSourceOwner() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(state: "TASK_STATE_PREPARING"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1wrong",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.yield(.playableFixture(source: "BV1wrong", playbackSourceItemID: "different-task"))
        await waitUntil(model.currentTask?.state == "TASK_STATE_PLAYABLE")

        XCTAssertNil(model.playableURL)
        XCTAssertFalse(model.canPlay)

        model.clearTask()
    }

    func testCompletedTaskRejectsMismatchedPlaybackSourceOwner() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(state: "TASK_STATE_PREPARING"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1cachedWrong",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.yield(
            .playableFixture(
                source: "BV1cachedWrong",
                state: "TASK_STATE_COMPLETED",
                libraryItemID: "cached-bilibili-playback-1",
                playbackSourceItemID: "different-library-item"
            )
        )
        await waitUntil(model.currentTask?.state == "TASK_STATE_COMPLETED")

        XCTAssertNil(model.playableURL)
        XCTAssertFalse(model.canPlay)
        XCTAssertEqual(model.statusMessage, "Ready video is cached for LAN playback.")

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

    func testTerminalWatchUpdateStopsWatching() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(state: "TASK_STATE_PREPARING"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1done",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.yield(.fixture(source: "BV1done", state: "TASK_STATE_COMPLETED"))
        await waitUntil(!model.isWatching)
        await client.waitForWatchTermination()

        XCTAssertFalse(model.isWatching)
        XCTAssertEqual(model.statusMessage, "Ready video is cached for LAN playback.")

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

    func testCancelClearsActivePlaybackStatus() async {
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.playableFixture(source: "BV1ready"))
            ],
            cancelResponsesByID: [
                "bilibili-playback-1": .fixture(source: "BV1ready", state: "TASK_STATE_CANCELLED")
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1ready",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        model.finishPreparedPlayback(didStartPlayback: true)
        XCTAssertEqual(model.statusMessage, "Playing Ready video.")

        await model.cancel(serverAddressText: "mac-mini.local:50051")

        let cancelledIDs = await client.cancelledIDsSnapshot()
        XCTAssertEqual(cancelledIDs, ["bilibili-playback-1"])
        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_CANCELLED")
        XCTAssertEqual(model.statusMessage, "Ready video was cancelled.")

        model.clearTask()
    }

    func testTerminalCancelResponseStopsWatchingAndIgnoresLostWatchError() async {
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1ready", state: "TASK_STATE_PREPARING"))
            ],
            cancelResponsesByID: [
                "bilibili-playback-1": .fixture(source: "BV1ready", state: "TASK_STATE_CANCELLED")
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1ready",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await model.cancel(serverAddressText: "mac-mini.local:50051")
        await waitUntil(!model.isWatching)
        await client.waitForWatchTermination()
        await client.failWatching()
        await Task.yield()

        XCTAssertFalse(model.isWatching)
        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_CANCELLED")
        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(model.statusMessage, "Ready video was cancelled.")

        model.clearTask()
    }

    func testLateCancelResponseDoesNotOverwriteTerminalWatchUpdate() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(source: "BV1race", state: "TASK_STATE_PREPARING"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1race",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.setSuspendsCancelResponses(true)

        let cancelTask = Task {
            await model.cancel(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForCancelRequestCount(1)
        XCTAssertTrue(model.isCancelling)

        await client.yield(.fixture(source: "BV1race", state: "TASK_STATE_CANCELLED"))
        await waitUntil(model.currentTask?.state == "TASK_STATE_CANCELLED")
        XCTAssertFalse(model.isCancelling)
        await client.completeNextCancel(
            with: .success(
                .fixture(source: "BV1race", state: "TASK_STATE_CANCEL_REQUESTED", message: "Cancelling task.")))
        await cancelTask.value

        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_CANCELLED")
        XCTAssertFalse(model.isCancelling)
        XCTAssertEqual(model.statusMessage, "Ready video was cancelled.")

        model.clearTask()
    }

    func testWatchUpdateClearsTransientCancelErrorWhenTaskRecovers() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(source: "BV1recover", state: "TASK_STATE_PREPARING"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1recover",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.setSuspendsCancelResponses(true)

        let cancelTask = Task {
            await model.cancel(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForCancelRequestCount(1)
        await client.completeNextCancel(with: .failure(FakeBilibiliCacheControlClientError.cancelFailed))
        await cancelTask.value

        XCTAssertNotNil(model.errorMessage)

        await client.yield(.playableFixture(source: "BV1recover"))
        await waitUntil(model.currentTask?.state == "TASK_STATE_PLAYABLE")

        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(model.statusMessage, "Ready video is ready to play.")
        XCTAssertTrue(model.canPlay)

        model.clearTask()
    }

    func testCancelErrorDoesNotEnableRetryForActiveTask() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(source: "BV1active", state: "TASK_STATE_PREPARING"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1active",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.setSuspendsCancelResponses(true)
        let cancelTask = Task {
            await model.cancel(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForCancelRequestCount(1)

        await client.completeNextCancel(with: .failure(FakeBilibiliCacheControlClientError.cancelFailed))
        await cancelTask.value

        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_PREPARING")
        XCTAssertNotNil(model.errorMessage)
        XCTAssertFalse(model.canRetry)

        model.clearTask()
    }

    func testLateCancelErrorDoesNotOverwriteTerminalWatchUpdate() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(source: "BV1race", state: "TASK_STATE_PREPARING"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1race",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.setSuspendsCancelResponses(true)

        let cancelTask = Task {
            await model.cancel(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForCancelRequestCount(1)
        XCTAssertTrue(model.isCancelling)

        await client.yield(.fixture(source: "BV1race", state: "TASK_STATE_CANCELLED"))
        await waitUntil(model.currentTask?.state == "TASK_STATE_CANCELLED")
        await client.completeNextCancel(with: .failure(FakeBilibiliCacheControlClientError.cancelFailed))
        await cancelTask.value

        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_CANCELLED")
        XCTAssertNil(model.errorMessage)
        XCTAssertFalse(model.isCancelling)
        XCTAssertEqual(model.statusMessage, "Ready video was cancelled.")

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
        XCTAssertFalse(model.isCancelling)
        XCTAssertFalse(model.canCancel)

        await model.cancel(serverAddressText: "mac-mini.local:50051")
        let repeatedCancelledIDs = await client.cancelledIDsSnapshot()
        XCTAssertEqual(repeatedCancelledIDs, ["bilibili-playback-1"])

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
        XCTAssertEqual(model.statusMessage, "Planning failed.")
        XCTAssertEqual(model.errorMessage, "Planning failed.")
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
    private var suspendsCreateResponses: Bool
    private var suspendsCancelResponses = false
    private var createdRequests: [(urlOrID: String, options: BilibiliPlaybackTaskOptions)] = []
    private var cancelledIDs: [String] = []
    private var pendingCreateContinuations: [CheckedContinuation<CacheTask, Error>] = []
    private var pendingCancelContinuations: [CheckedContinuation<CacheTask, Error>] = []
    private var createRequestWaiters: [(count: Int, continuation: CheckedContinuation<Void, Never>)] = []
    private var cancelRequestWaiters: [(count: Int, continuation: CheckedContinuation<Void, Never>)] = []
    private var watchContinuations: [AsyncThrowingStream<CacheTask, Error>.Continuation] = []
    private var watchWaiters: [CheckedContinuation<Void, Never>] = []
    private var watchTerminationCount = 0
    private var watchTerminationWaiters: [CheckedContinuation<Void, Never>] = []

    init(
        createResponses: [Result<CacheTask, Error>],
        cancelResponsesByID: [String: CacheTask] = [:],
        suspendsCreateResponses: Bool = false
    ) {
        self.createResponses = createResponses
        self.cancelResponsesByID = cancelResponsesByID
        self.suspendsCreateResponses = suspendsCreateResponses
    }

    func getServerInfo() async throws -> CacheServerSummary {
        throw FakeBilibiliCacheControlClientError.notImplemented
    }

    func listCacheRoots() async throws -> [CacheRoot] {
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

    func deleteLibraryItem(id: String) async throws -> Bool {
        throw FakeBilibiliCacheControlClientError.notImplemented
    }

    func getTask(id: String) async throws -> CacheTask {
        throw FakeBilibiliCacheControlClientError.notImplemented
    }

    func watchTasks(ids: [String]) async -> AsyncThrowingStream<CacheTask, Error> {
        AsyncThrowingStream { continuation in
            continuation.onTermination = { _ in
                Task {
                    await self.recordWatchTermination()
                }
            }
            Task {
                self.storeWatchContinuation(continuation)
            }
        }
    }

    func cancelTask(id: String) async throws -> CacheTask {
        cancelledIDs.append(id)
        resumeCancelRequestWaiters()
        if suspendsCancelResponses {
            return try await withCheckedThrowingContinuation { continuation in
                pendingCancelContinuations.append(continuation)
            }
        }

        return cancelResponsesByID[id] ?? .fixture(state: "TASK_STATE_CANCEL_REQUESTED")
    }

    func createBilibiliPlaybackTask(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask {
        createdRequests.append((urlOrID, options))
        resumeCreateRequestWaiters()
        if suspendsCreateResponses {
            return try await withCheckedThrowingContinuation { continuation in
                pendingCreateContinuations.append(continuation)
            }
        }

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

    func setSuspendsCreateResponses(_ suspendsCreateResponses: Bool) {
        self.suspendsCreateResponses = suspendsCreateResponses
    }

    func setSuspendsCancelResponses(_ suspendsCancelResponses: Bool) {
        self.suspendsCancelResponses = suspendsCancelResponses
    }

    func completeNextCreate(with result: Result<CacheTask, Error>) {
        guard !pendingCreateContinuations.isEmpty else {
            return
        }

        pendingCreateContinuations.removeFirst().resume(with: result)
    }

    func completeNextCancel(with result: Result<CacheTask, Error>) {
        guard !pendingCancelContinuations.isEmpty else {
            return
        }

        pendingCancelContinuations.removeFirst().resume(with: result)
    }

    func createdRequestsSnapshot() -> [(urlOrID: String, options: BilibiliPlaybackTaskOptions)] {
        createdRequests
    }

    func cancelledIDsSnapshot() -> [String] {
        cancelledIDs
    }

    func waitForCreateRequestCount(_ count: Int) async {
        guard createdRequests.count < count else {
            return
        }

        await withCheckedContinuation { continuation in
            createRequestWaiters.append((count, continuation))
        }
    }

    func waitForCancelRequestCount(_ count: Int) async {
        guard cancelledIDs.count < count else {
            return
        }

        await withCheckedContinuation { continuation in
            cancelRequestWaiters.append((count, continuation))
        }
    }

    func waitForWatchSubscription() async {
        guard watchContinuations.isEmpty else {
            return
        }

        await withCheckedContinuation { continuation in
            watchWaiters.append(continuation)
        }
    }

    func waitForWatchTermination() async {
        guard watchTerminationCount == 0 else {
            return
        }

        await withCheckedContinuation { continuation in
            watchTerminationWaiters.append(continuation)
        }
    }

    func yield(_ task: CacheTask) {
        watchContinuations.forEach { $0.yield(task) }
    }

    func failWatching() {
        let continuations = watchContinuations
        watchContinuations = []
        continuations.forEach { $0.finish(throwing: FakeBilibiliCacheControlClientError.watchFailed) }
    }

    private func resumeCreateRequestWaiters() {
        let ready = createRequestWaiters.filter { $0.count <= createdRequests.count }
        createRequestWaiters.removeAll { $0.count <= createdRequests.count }
        ready.forEach { $0.continuation.resume() }
    }

    private func resumeCancelRequestWaiters() {
        let ready = cancelRequestWaiters.filter { $0.count <= cancelledIDs.count }
        cancelRequestWaiters.removeAll { $0.count <= cancelledIDs.count }
        ready.forEach { $0.continuation.resume() }
    }

    private func storeWatchContinuation(
        _ continuation: AsyncThrowingStream<CacheTask, Error>.Continuation
    ) {
        watchContinuations.append(continuation)
        let waiters = watchWaiters
        watchWaiters = []
        waiters.forEach { $0.resume() }
    }

    private func recordWatchTermination() {
        watchTerminationCount += 1
        let waiters = watchTerminationWaiters
        watchTerminationWaiters = []
        waiters.forEach { $0.resume() }
    }
}

private enum FakeBilibiliCacheControlClientError: Error {
    case notImplemented
    case noCreateResponse
    case cancelFailed
    case watchFailed
}

private extension CacheTask {
    static func fixture(
        id: String = "bilibili-playback-1",
        source: String = "BV1test",
        title: String = "Ready video",
        state: String,
        progress: Double = 0.25,
        message: String = "Preparing playback.",
        libraryItemID: String = "",
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
            libraryItemID: libraryItemID,
            playbackSource: playbackSource,
            playbackSession: playbackSession
        )
    }

    static func playableFixture(
        id: String = "bilibili-playback-1",
        source: String = "BV1test",
        state: String = "TASK_STATE_PLAYABLE",
        libraryItemID: String = "",
        playbackSourceItemID: String? = nil
    ) -> Self {
        let resolvedPlaybackSourceItemID = playbackSourceItemID ?? id
        return .fixture(
            id: id,
            source: source,
            state: state,
            progress: 1,
            message: "Bilibili playback session is playable.",
            libraryItemID: libraryItemID,
            playbackSource: CachePlaybackSource(
                itemID: resolvedPlaybackSourceItemID,
                variantID: "h264",
                playbackProtocol: "PLAYBACK_PROTOCOL_HLS",
                uri: "http://mac-mini.local:8080/hls/\(resolvedPlaybackSourceItemID)/master.m3u8"
            ),
            playbackSession: CacheBilibiliPlaybackSession(
                id: id,
                title: "Ready video",
                contentID: "BV1ready-cid1",
                selectedVariantID: "h264",
                selectedVariant: nil,
                variants: []
            )
        )
    }
}
