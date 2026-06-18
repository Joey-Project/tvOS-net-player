import Combine
import Foundation
import TVOSNetPlayerCacheClient

public struct ProgressiveCacheStatusBadge: Equatable, Sendable {
    public let label: String
    public let systemImage: String
}

private struct BilibiliResolvedInputContext: Equatable {
    let source: String
    let endpoint: CacheServerEndpoint
    let options: BilibiliPlaybackTaskOptions
}

@MainActor
public final class BilibiliTaskViewModel: ObservableObject {
    @Published public var sourceText: String
    @Published public var qualityPreference: String
    @Published public var encodingPreference: String
    @Published public private(set) var currentTask: CacheTask?
    @Published public private(set) var statusMessage: String = "No Bilibili playback task submitted."
    @Published public private(set) var errorMessage: String?
    @Published public private(set) var isSubmitting = false
    @Published public private(set) var isResolving = false
    @Published public private(set) var isWatching = false
    @Published public private(set) var isCancelling = false
    @Published public private(set) var resolvedInput: BilibiliResolveResult?
    @Published public var selectedCandidateID: String?

    private let clientFactory: @Sendable (CacheServerEndpoint) -> any CacheControlClient
    private let operationTimeout: Duration
    private var activeEndpoint: CacheServerEndpoint?
    private var resolvedInputContext: BilibiliResolvedInputContext?
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
        guard !isSubmitting, !isResolving, !isCancelling else {
            return false
        }

        guard !sourceText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            return false
        }

        if isWaitingForCandidateSelection {
            return selectedCandidate != nil
        }

        return true
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
        guard !isSubmitting && !isResolving && !isCancelling else {
            return false
        }

        guard let currentTask else {
            return errorMessage != nil
        }

        return currentTask.isRetryableBilibiliTaskState
    }

    public var canPlay: Bool {
        !isSubmitting && !isResolving && !isCancelling && playableURL != nil
    }

    public var canClear: Bool {
        currentTask != nil || errorMessage != nil || resolvedInput != nil
    }

    public var resolvedCandidates: [BilibiliResolvedCandidate] {
        guard resolvedInputMatchesSource else {
            return []
        }

        return resolvedInput?.candidates ?? []
    }

    public var isWaitingForCandidateSelection: Bool {
        resolvedInputMatchesSource && resolvedInput?.requiresSelection == true && currentTask == nil
    }

    public var selectedCandidate: BilibiliResolvedCandidate? {
        let candidates = resolvedCandidates
        guard !candidates.isEmpty else {
            return nil
        }

        if let selectedCandidateID,
            let candidate = candidates.first(where: { $0.selectionID == selectedCandidateID })
        {
            return candidate
        }

        let defaultSelectionID = resolvedInput?.defaultSelectionID ?? ""
        if !defaultSelectionID.isEmpty,
            let candidate = candidates.first(where: { $0.selectionID == defaultSelectionID })
        {
            return candidate
        }

        return candidates.count == 1 ? candidates[0] : nil
    }

    public var submitButtonTitle: String {
        if isResolving {
            return "Resolving"
        }
        if isSubmitting {
            return "Submitting"
        }
        if isWaitingForCandidateSelection {
            return "Submit Selected"
        }
        return "Submit"
    }

    public var progress: Double? {
        guard let currentTask else {
            return nil
        }

        return currentTask.progress > 0 ? min(max(currentTask.progress, 0), 1) : nil
    }

    public var progressiveCacheStatusBadge: ProgressiveCacheStatusBadge? {
        currentTask.flatMap(Self.progressiveCacheStatusBadge(for:))
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

        let options = currentPlaybackOptions

        guard Self.shouldResolveBeforeSubmittingBilibiliInput(source) else {
            await createPlaybackTask(source: source, selectionID: nil, endpoint: endpoint, options: options)
            return
        }

        if isWaitingForCandidateSelection,
            resolvedInputMatches(source: source, endpoint: endpoint, options: options)
        {
            guard let selectedCandidate else {
                errorMessage = "Select a Bilibili item before submitting playback."
                statusMessage = "Bilibili item selection is required."
                return
            }

            await createPlaybackTask(
                source: source,
                selectionID: selectedCandidate.selectionID,
                endpoint: endpoint,
                options: options
            )
            return
        }

        operationSequence += 1
        activePlaybackTaskID = nil
        let sequence = operationSequence

        stopWatching()
        activeEndpoint = endpoint
        currentTask = nil
        resolvedInput = nil
        resolvedInputContext = nil
        selectedCandidateID = nil
        isSubmitting = true
        isResolving = true
        errorMessage = nil
        statusMessage = "Resolving Bilibili input..."

        let client = clientFactory(endpoint)

        do {
            let resolved = try await Self.withOperationTimeout(operationTimeout) {
                try await client.resolveBilibiliInput(urlOrID: source, options: options)
            }

            guard sequence == operationSequence else {
                return
            }

            guard Self.normalizedBilibiliSource(sourceText) == source,
                currentPlaybackOptions == options
            else {
                currentTask = nil
                resolvedInput = nil
                resolvedInputContext = nil
                selectedCandidateID = nil
                errorMessage = nil
                statusMessage = "Bilibili input changed before resolve completed."
                isResolving = false
                isSubmitting = false
                return
            }

            resolvedInput = resolved
            resolvedInputContext = BilibiliResolvedInputContext(
                source: source,
                endpoint: endpoint,
                options: options
            )
            selectedCandidateID =
                resolved.defaultSelectionID.isEmpty
                ? resolved.candidates.first?.selectionID
                : resolved.defaultSelectionID
            isResolving = false

            guard let candidate = selectedCandidate else {
                isSubmitting = false
                errorMessage = "Bilibili input did not resolve to a playable item."
                statusMessage = "No selectable Bilibili item was found."
                return
            }

            guard !resolved.requiresSelection else {
                isSubmitting = false
                statusMessage = "Select a Bilibili item to play."
                return
            }

            await createPlaybackTask(
                source: source,
                selectionID: candidate.selectionID,
                endpoint: endpoint,
                sequence: sequence,
                client: client,
                options: options
            )
        } catch {
            guard sequence == operationSequence else {
                return
            }

            if Self.isBilibiliResolveUnsupported(error) {
                await createPlaybackTask(
                    source: source,
                    selectionID: nil,
                    endpoint: endpoint,
                    sequence: sequence,
                    client: client,
                    options: options
                )
                return
            }

            currentTask = nil
            resolvedInput = nil
            resolvedInputContext = nil
            selectedCandidateID = nil
            errorMessage = error.localizedDescription
            statusMessage = "Could not resolve Bilibili input."
            isResolving = false
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
        isResolving = false
        isCancelling = false
        resolvedInput = nil
        resolvedInputContext = nil
        selectedCandidateID = nil
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

    private func createPlaybackTask(
        source: String,
        selectionID: String?,
        endpoint: CacheServerEndpoint,
        options: BilibiliPlaybackTaskOptions
    ) async {
        operationSequence += 1
        activePlaybackTaskID = nil
        let sequence = operationSequence
        let client = clientFactory(endpoint)
        await createPlaybackTask(
            source: source,
            selectionID: selectionID,
            endpoint: endpoint,
            sequence: sequence,
            client: client,
            options: options
        )
    }

    private func createPlaybackTask(
        source: String,
        selectionID: String?,
        endpoint: CacheServerEndpoint,
        sequence: Int,
        client: any CacheControlClient,
        options: BilibiliPlaybackTaskOptions
    ) async {
        stopWatching()
        activeEndpoint = endpoint
        isSubmitting = true
        isResolving = false
        errorMessage = nil
        statusMessage = "Submitting Bilibili playback task..."

        do {
            let task = try await Self.withOperationTimeout(operationTimeout) {
                try await client.createBilibiliPlaybackTask(
                    urlOrID: source,
                    selectionID: selectionID,
                    options: options
                )
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

    private static func isBilibiliResolveUnsupported(_ error: Error) -> Bool {
        if let unsupported = error as? CacheControlClientUnsupportedFeature {
            return unsupported == .bilibiliResolve
        }
        return false
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

    private static func progressiveCacheStatusBadge(for task: CacheTask) -> ProgressiveCacheStatusBadge? {
        guard task.isProgressivePlayback else {
            return nil
        }

        if task.isCompletedBilibiliTaskState,
            !task.libraryItemID.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        {
            return ProgressiveCacheStatusBadge(
                label: "Offline ready", systemImage: "externaldrive.fill.badge.checkmark")
        }

        if task.isFailedBilibiliTaskState {
            return ProgressiveCacheStatusBadge(
                label: task.message.isQuotaOrStorageFailureMessage ? "Quota blocked" : "Cache failed",
                systemImage: "exclamationmark.triangle"
            )
        }

        if task.isPlayableBilibiliTaskState {
            let normalizedMessage = task.message.lowercased()
            if normalizedMessage.contains("failed") {
                return ProgressiveCacheStatusBadge(label: "Cache failed; playable online", systemImage: "wifi")
            }
            if normalizedMessage.contains("paused") || normalizedMessage.contains("queued") {
                return ProgressiveCacheStatusBadge(label: "Offline fill queued", systemImage: "clock")
            }
            if normalizedMessage.contains("prewarm") {
                return ProgressiveCacheStatusBadge(label: "Prewarming cache", systemImage: "bolt.horizontal")
            }
            if let percent = task.offlineCachePercentLabel {
                return ProgressiveCacheStatusBadge(
                    label: "Filling offline cache \(percent)", systemImage: "arrow.down.circle")
            }

            return ProgressiveCacheStatusBadge(label: "Playable online; caching", systemImage: "wifi")
        }

        let state = task.normalizedBilibiliTaskState
        if state.contains("preparing") || state.contains("planned") {
            return ProgressiveCacheStatusBadge(label: "Pending offline fill", systemImage: "clock")
        }

        return nil
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

    var offlineCachePercentLabel: String? {
        if totalBytes > 0, downloadedBytes > 0 {
            let ratio = min(max(Double(downloadedBytes) / Double(totalBytes), 0), 0.99)
            return "\(Int((ratio * 100).rounded()))%"
        }

        guard progress > 0, progress < 1 else {
            return nil
        }

        return "\(Int((min(max(progress, 0), 0.99) * 100).rounded()))%"
    }
}

private extension BilibiliTaskViewModel {
    var currentPlaybackOptions: BilibiliPlaybackTaskOptions {
        BilibiliPlaybackTaskOptions(
            qualityPreference: qualityPreference.trimmingCharacters(in: .whitespacesAndNewlines),
            encodingPreference: encodingPreference.trimmingCharacters(in: .whitespacesAndNewlines)
        )
    }

    var resolvedInputMatchesSource: Bool {
        resolvedInputMatches(
            source: Self.normalizedBilibiliSource(sourceText),
            endpoint: nil,
            options: currentPlaybackOptions
        )
    }

    func resolvedInputMatches(
        source: String,
        endpoint: CacheServerEndpoint?,
        options: BilibiliPlaybackTaskOptions
    ) -> Bool {
        guard let resolvedInput, let resolvedInputContext else {
            return false
        }

        if let endpoint, resolvedInputContext.endpoint != endpoint {
            return false
        }

        return resolvedInputContext.source == source
            && resolvedInputContext.options == options
            && Self.normalizedBilibiliSource(resolvedInput.source) == source
    }

    static func normalizedBilibiliSource(_ source: String) -> String {
        source.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    static func shouldResolveBeforeSubmittingBilibiliInput(_ source: String) -> Bool {
        !isCollectionOrFeedBilibiliInput(source)
    }

    static func isCollectionOrFeedBilibiliInput(_ source: String) -> Bool {
        let trimmed = normalizedBilibiliSource(source)
        let lowercased = trimmed.lowercased()
        let collectionFeedKeywords: Set<String> = [
            "following",
            "history",
            "later",
            "recommend",
            "recommendation",
            "recommendations",
            "toview",
            "watch-later",
            "watch_later",
            "watchlater",
        ]
        if collectionFeedKeywords.contains(lowercased) {
            return true
        }

        for prefix in ["collection", "fav", "mid", "series"] {
            guard lowercased.hasPrefix(prefix) else {
                continue
            }

            let suffix = lowercased.dropFirst(prefix.count)
            if isNonEmptyASCIIDigits(suffix) {
                return true
            }
        }

        guard let components = URLComponents(string: trimmed),
            let host = components.host?.lowercased()
        else {
            return false
        }

        let pathSegments = components.path
            .split(separator: "/")
            .map { $0.lowercased() }
        if host == "space.bilibili.com",
            pathSegments.first.map(isNonEmptyASCIIDigits) == true
        {
            return true
        }

        if host == "t.bilibili.com" {
            return pathSegments.isEmpty
        }

        guard host == "www.bilibili.com" || host == "bilibili.com" else {
            return false
        }

        let queryItemNames = Set((components.queryItems ?? []).map { $0.name.lowercased() })
        if queryItemNames.contains("ep_id") || queryItemNames.contains("season_id") {
            return false
        }

        if pathSegments.isEmpty {
            return true
        }
        if pathSegments == ["account", "dynamic"] || pathSegments == ["account", "history"] {
            return true
        }
        if pathSegments.first == "watchlater" || pathSegments == ["list", "watchlater"] {
            return true
        }
        if pathSegments.contains("medialist") || pathSegments.first == "list" {
            return true
        }

        return false
    }

    static func isNonEmptyASCIIDigits<S: StringProtocol>(_ value: S) -> Bool {
        !value.isEmpty
            && value.unicodeScalars.allSatisfy { scalar in
                scalar.value >= 48 && scalar.value <= 57
            }
    }
}

private extension String {
    var isQuotaOrStorageFailureMessage: Bool {
        let normalized = lowercased()
        return normalized.contains("quota")
            || normalized.contains("watermark")
            || normalized.contains("storage")
            || normalized.contains("disk")
            || normalized.contains("no space")
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
