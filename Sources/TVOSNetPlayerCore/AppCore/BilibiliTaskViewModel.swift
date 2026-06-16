import Combine
import Foundation
import TVOSNetPlayerCacheClient

@MainActor
public final class BilibiliTaskViewModel: ObservableObject {
    @Published public var sourceText: String
    @Published public var qualityPreference: String
    @Published public var encodingPreference: String
    @Published public private(set) var currentTask: CacheTask?
    @Published public private(set) var statusMessage: String = "No Bilibili playback task submitted."
    @Published public private(set) var errorMessage: String?
    @Published public private(set) var isSubmitting = false
    @Published public private(set) var isWatching = false
    @Published public private(set) var isCancelling = false

    private let clientFactory: @Sendable (CacheServerEndpoint) -> any CacheControlClient
    private let operationTimeout: Duration
    private var activeEndpoint: CacheServerEndpoint?
    private var taskWatcher: Task<Void, Never>?
    private var operationSequence = 0
    private var activePlaybackTaskID: String?

    public init(
        sourceText: String = "",
        qualityPreference: String = "",
        encodingPreference: String = "",
        operationTimeout: Duration = .seconds(10),
        clientFactory: @escaping @Sendable (CacheServerEndpoint) -> any CacheControlClient = {
            GRPCCacheControlClient(endpoint: $0)
        }
    ) {
        self.sourceText = sourceText
        self.qualityPreference = qualityPreference
        self.encodingPreference = encodingPreference
        self.operationTimeout = operationTimeout
        self.clientFactory = clientFactory
    }

    deinit {
        taskWatcher?.cancel()
    }

    public var canSubmit: Bool {
        !sourceText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && !isSubmitting
            && !isCancelling
    }

    public var canCancel: Bool {
        guard !isSubmitting else {
            return false
        }

        guard let currentTask else {
            return false
        }

        return !currentTask.isTerminalBilibiliTaskState
            && !currentTask.isCancellationPendingBilibiliTaskState
            && !isCancelling
    }

    public var canRetry: Bool {
        guard !isSubmitting && !isCancelling else {
            return false
        }

        guard let currentTask else {
            return errorMessage != nil
        }

        return currentTask.isRetryableBilibiliTaskState
    }

    public var canPlay: Bool {
        !isSubmitting && !isCancelling && playableURL != nil
    }

    public var progress: Double? {
        guard let currentTask else {
            return nil
        }

        return currentTask.progress > 0 ? min(max(currentTask.progress, 0), 1) : nil
    }

    public var playableURL: URL? {
        currentTask?.playableBilibiliURL
    }

    public var displayTitle: String {
        guard let currentTask else {
            let source = sourceText.trimmingCharacters(in: .whitespacesAndNewlines)
            return source.isEmpty ? "Bilibili video" : source
        }

        return currentTask.bilibiliDisplayTitle
    }

    public func submit(serverAddressText: String) async {
        guard canSubmit else {
            return
        }

        guard let endpoint = CacheServerEndpoint.normalized(from: serverAddressText) else {
            errorMessage = "Use a cache server host and optional port before submitting Bilibili playback."
            statusMessage = "Cache server address is invalid."
            return
        }

        let source = sourceText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !source.isEmpty else {
            errorMessage = "Enter a Bilibili URL, BV, av, season, feed, history, or watch-later input."
            statusMessage = "Bilibili input is required."
            return
        }

        operationSequence += 1
        activePlaybackTaskID = nil
        let sequence = operationSequence

        stopWatching()
        activeEndpoint = endpoint
        isSubmitting = true
        errorMessage = nil
        statusMessage = "Submitting Bilibili playback task..."

        do {
            let client = clientFactory(endpoint)
            let options = BilibiliPlaybackTaskOptions(
                qualityPreference: qualityPreference.trimmingCharacters(in: .whitespacesAndNewlines),
                encodingPreference: encodingPreference.trimmingCharacters(in: .whitespacesAndNewlines)
            )
            let task = try await Self.withOperationTimeout(operationTimeout) {
                try await client.createBilibiliPlaybackTask(urlOrID: source, options: options)
            }

            guard sequence == operationSequence else {
                return
            }

            applyTaskUpdate(task)
            isSubmitting = false
            if !task.isTerminalBilibiliTaskState {
                startWatching(taskID: task.id, endpoint: endpoint, sequence: sequence)
            }
        } catch {
            guard sequence == operationSequence else {
                return
            }

            currentTask = nil
            errorMessage = error.localizedDescription
            statusMessage = "Could not submit Bilibili playback task."
            isSubmitting = false
        }
    }

    public func retry(serverAddressText: String) async {
        if let source = currentTask?.source.trimmingCharacters(in: .whitespacesAndNewlines),
            !source.isEmpty
        {
            sourceText = source
        }

        await submit(serverAddressText: serverAddressText)
    }

    public func cancel(serverAddressText: String) async {
        guard let currentTask else {
            return
        }
        guard canCancel else {
            return
        }

        let endpoint = activeEndpoint ?? CacheServerEndpoint.normalized(from: serverAddressText)
        guard let endpoint else {
            errorMessage = "Use a cache server host and optional port before cancelling."
            statusMessage = "Cache server address is invalid."
            return
        }

        let targetTaskID = currentTask.id
        if activePlaybackTaskID == targetTaskID {
            activePlaybackTaskID = nil
        }
        isCancelling = true
        errorMessage = nil
        statusMessage = "Cancelling \(currentTask.bilibiliDisplayTitle)..."
        let sequence = operationSequence

        do {
            let client = clientFactory(endpoint)
            let task = try await Self.withOperationTimeout(operationTimeout) {
                try await client.cancelTask(id: currentTask.id)
            }

            guard sequence == operationSequence, self.currentTask?.id == targetTaskID else {
                return
            }

            if let currentTask = self.currentTask,
                currentTask.isTerminalBilibiliTaskState
            {
                applyTaskUpdate(currentTask)
                isCancelling = false
                return
            }

            applyTaskUpdate(task)
            isCancelling = false
        } catch {
            guard sequence == operationSequence else {
                return
            }

            if let currentTask = self.currentTask,
                currentTask.id == targetTaskID,
                currentTask.isTerminalBilibiliTaskState
            {
                applyTaskUpdate(currentTask)
                isCancelling = false
                return
            }

            errorMessage = error.localizedDescription
            statusMessage = "Could not cancel \(currentTask.bilibiliDisplayTitle)."
            isCancelling = false
        }
    }

    public func finishPreparedPlayback(didStartPlayback: Bool) {
        guard let currentTask else {
            return
        }

        errorMessage = nil
        if didStartPlayback {
            activePlaybackTaskID = currentTask.id
            statusMessage = "Playing \(currentTask.bilibiliDisplayTitle)."
        } else {
            activePlaybackTaskID = nil
            statusMessage = Self.statusMessage(for: currentTask)
        }
    }

    public func clearPlaybackStatus() {
        guard activePlaybackTaskID != nil else {
            return
        }

        activePlaybackTaskID = nil
        statusMessage = currentTask.map(Self.statusMessage(for:)) ?? "No Bilibili playback task submitted."
    }

    public func isActivePlaybackLibraryItem(id libraryItemID: String) -> Bool {
        guard let currentTask,
            !libraryItemID.isEmpty,
            activePlaybackTaskID == currentTask.id
        else {
            return false
        }

        return currentTask.libraryItemID == libraryItemID
    }

    public func clearTask() {
        operationSequence += 1
        activeEndpoint = nil
        activePlaybackTaskID = nil
        currentTask = nil
        errorMessage = nil
        isSubmitting = false
        isCancelling = false
        stopWatching()
        statusMessage = "No Bilibili playback task submitted."
    }

    @discardableResult
    public func clearTaskIfCachedLibraryItemDeleted(id libraryItemID: String) -> Bool {
        guard let currentTask,
            !libraryItemID.isEmpty,
            currentTask.libraryItemID == libraryItemID
        else {
            return false
        }

        clearTask()
        return true
    }

    private func startWatching(taskID: String, endpoint: CacheServerEndpoint, sequence: Int) {
        stopWatching()
        isWatching = true
        let clientFactory = self.clientFactory
        taskWatcher = Task { [weak self] in
            let client = clientFactory(endpoint)
            let stream = await client.watchTask(id: taskID)
            do {
                for try await task in stream {
                    self?.applyWatchedTask(task, sequence: sequence)
                }
                self?.finishWatching(sequence: sequence, error: nil)
            } catch {
                self?.finishWatching(sequence: sequence, error: error)
            }
        }
    }

    private func stopWatching() {
        taskWatcher?.cancel()
        taskWatcher = nil
        isWatching = false
    }

    private func applyWatchedTask(_ task: CacheTask, sequence: Int) {
        guard sequence == operationSequence else {
            return
        }

        applyTaskUpdate(task)
    }

    private func applyTaskUpdate(_ task: CacheTask) {
        currentTask = task
        if activePlaybackTaskID == task.id, !task.isPlayableBilibiliTaskState {
            activePlaybackTaskID = nil
        }
        if task.isFailedBilibiliTaskState {
            errorMessage = Self.failureMessage(for: task)
        } else {
            errorMessage = nil
        }

        if activePlaybackTaskID == task.id {
            statusMessage = "Playing \(task.bilibiliDisplayTitle)."
        } else {
            statusMessage = Self.statusMessage(for: task)
        }
        if task.isTerminalBilibiliTaskState {
            isCancelling = false
            stopWatching()
        }
    }

    private func finishWatching(sequence: Int, error: Error?) {
        guard sequence == operationSequence else {
            return
        }

        isWatching = false
        if let error, !Task.isCancelled {
            errorMessage = error.localizedDescription
            if let currentTask {
                statusMessage = "Lost task updates for \(currentTask.bilibiliDisplayTitle)."
            } else {
                statusMessage = "Lost Bilibili task updates."
            }
        }
    }

    private static func statusMessage(for task: CacheTask) -> String {
        if task.isCompletedBilibiliTaskState {
            return "\(task.bilibiliDisplayTitle) is cached for LAN playback."
        }

        if task.isPlayableBilibiliTaskState, task.playableBilibiliURL != nil {
            return "\(task.bilibiliDisplayTitle) is ready to play."
        }

        if task.isFailedBilibiliTaskState {
            return failureMessage(for: task)
        }

        if task.isCancelledBilibiliTaskState {
            return "\(task.bilibiliDisplayTitle) was cancelled."
        }

        if !task.message.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            return task.message
        }

        return "Preparing \(task.bilibiliDisplayTitle)..."
    }

    private static func failureMessage(for task: CacheTask) -> String {
        let message = task.message.trimmingCharacters(in: .whitespacesAndNewlines)
        if !message.isEmpty {
            return message
        }

        return "\(task.bilibiliDisplayTitle) failed."
    }

    private static func withOperationTimeout<Value: Sendable>(
        _ timeout: Duration,
        operation: @Sendable @escaping () async throws -> Value
    ) async throws -> Value {
        try await withCheckedThrowingContinuation { continuation in
            let race = BilibiliTaskOperationTimeoutRace(continuation: continuation)
            race.start(timeout: timeout, operation: operation)
        }
    }
}

private extension CacheTask {
    var bilibiliDisplayTitle: String {
        let title = title.trimmingCharacters(in: .whitespacesAndNewlines)
        if !title.isEmpty {
            return title
        }

        let source = source.trimmingCharacters(in: .whitespacesAndNewlines)
        if !source.isEmpty {
            return source
        }

        return id
    }

    var playableBilibiliURL: URL? {
        guard isProgressivePlayback, isPlayableBilibiliTaskState else {
            return nil
        }

        guard let playbackSource, playbackSource.isPlayableByTVOSClient else {
            return nil
        }

        guard let expectedItemID = expectedBilibiliPlaybackSourceItemID,
            playbackSource.itemID == expectedItemID
        else {
            return nil
        }

        return playbackSource.explicitHTTPURL
    }

    var expectedBilibiliPlaybackSourceItemID: String? {
        let itemID = isCompletedBilibiliTaskState ? libraryItemID : id
        let trimmedItemID = itemID.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmedItemID.isEmpty ? nil : trimmedItemID
    }

    var isPlayableBilibiliTaskState: Bool {
        let state = normalizedBilibiliTaskState
        return state.contains("playable") || state.contains("completed")
    }

    var isCompletedBilibiliTaskState: Bool {
        normalizedBilibiliTaskState.contains("completed")
    }

    var isFailedBilibiliTaskState: Bool {
        normalizedBilibiliTaskState.contains("failed")
    }

    var isCancelledBilibiliTaskState: Bool {
        normalizedBilibiliTaskState.contains("cancelled")
    }

    var isCancellationPendingBilibiliTaskState: Bool {
        normalizedBilibiliTaskState.contains("cancelrequested")
    }

    var isRetryableBilibiliTaskState: Bool {
        isFailedBilibiliTaskState || isCancelledBilibiliTaskState
    }

    var isTerminalBilibiliTaskState: Bool {
        let state = normalizedBilibiliTaskState
        return state.contains("succeeded")
            || state.contains("failed")
            || state.contains("cancelled")
            || state.contains("completed")
    }

    var normalizedBilibiliTaskState: String {
        state.lowercased().filter(\.isLetter)
    }
}

private final class BilibiliTaskOperationTimeoutRace<Value: Sendable>: @unchecked Sendable {
    private let lock = NSLock()
    private var continuation: CheckedContinuation<Value, Error>?
    private var operationTask: Task<Void, Never>?
    private var timeoutTask: Task<Void, Never>?

    init(continuation: CheckedContinuation<Value, Error>) {
        self.continuation = continuation
    }

    func start(
        timeout: Duration,
        operation: @Sendable @escaping () async throws -> Value
    ) {
        let operationTask = Task.detached {
            do {
                self.complete(.success(try await operation()))
            } catch {
                self.complete(.failure(error))
            }
        }
        let timeoutTask = Task.detached {
            do {
                try await Task.sleep(for: timeout)
                self.complete(.failure(BilibiliTaskOperationError.timedOut))
            } catch {
                // The timeout task is expected to be cancelled when the operation wins.
            }
        }

        lock.lock()
        if continuation == nil {
            lock.unlock()
            operationTask.cancel()
            timeoutTask.cancel()
            return
        }

        self.operationTask = operationTask
        self.timeoutTask = timeoutTask
        lock.unlock()
    }

    private func complete(_ result: Result<Value, Error>) {
        lock.lock()
        guard let continuation else {
            lock.unlock()
            return
        }

        self.continuation = nil
        let operationTask = operationTask
        let timeoutTask = timeoutTask
        self.operationTask = nil
        self.timeoutTask = nil
        lock.unlock()

        operationTask?.cancel()
        timeoutTask?.cancel()
        continuation.resume(with: result)
    }
}

private enum BilibiliTaskOperationError: LocalizedError {
    case timedOut

    var errorDescription: String? {
        "Cache server request timed out."
    }
}
