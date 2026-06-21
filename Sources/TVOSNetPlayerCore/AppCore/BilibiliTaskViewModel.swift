import Combine
import Foundation
import TVOSNetPlayerCacheClient

public struct ProgressiveCacheStatusBadge: Equatable, Sendable {
    public let label: String
    public let systemImage: String
}

public enum BilibiliFetchNoticeTone: String, Equatable, Sendable {
    case info
    case warning
    case error
}

public struct BilibiliFetchNotice: Equatable, Sendable {
    public let title: String
    public let message: String
    public let systemImage: String
    public let tone: BilibiliFetchNoticeTone
    public let actionTitle: String?

    public init(
        title: String,
        message: String,
        systemImage: String,
        tone: BilibiliFetchNoticeTone,
        actionTitle: String? = nil
    ) {
        self.title = title
        self.message = message
        self.systemImage = systemImage
        self.tone = tone
        self.actionTitle = actionTitle
    }
}

public struct BilibiliTaskResultSummary: Equatable, Sendable {
    public let totalCount: Int
    public let readyCount: Int
    public let cachedCount: Int
    public let failedCount: Int
    public let cancelledCount: Int
    public let pendingCount: Int

    public var completedCount: Int {
        readyCount + failedCount + cancelledCount
    }

    public var progress: Double {
        guard totalCount > 0 else {
            return 0
        }

        return min(max(Double(completedCount) / Double(totalCount), 0), 1)
    }

    public var hasPartialSuccess: Bool {
        readyCount > 0 && failedCount + cancelledCount > 0
    }

    public var statusMessage: String {
        if cachedCount == totalCount {
            return "\(totalCount) Bilibili results are cached for LAN playback."
        }

        if readyCount == totalCount {
            return "\(totalCount) Bilibili results are ready to play."
        }

        if readyCount > 0 {
            var message = "\(readyCount) of \(totalCount) Bilibili results are ready"
            if failedCount > 0 {
                message += "; \(failedCount) failed"
            }
            if cancelledCount > 0 {
                message += "; \(cancelledCount) cancelled"
            }
            return "\(message)."
        }

        if failedCount == totalCount {
            return "\(totalCount) Bilibili results failed."
        }

        if cancelledCount == totalCount {
            return "\(totalCount) Bilibili results were cancelled."
        }

        if failedCount + cancelledCount > 0 {
            var message = "No Bilibili results are ready"
            if failedCount > 0 {
                message += "; \(failedCount) failed"
            }
            if cancelledCount > 0 {
                message += "; \(cancelledCount) cancelled"
            }
            if pendingCount > 0 {
                message += "; \(pendingCount) still preparing"
            }
            return "\(message)."
        }

        return "Preparing \(totalCount) Bilibili results..."
    }
}

public enum BilibiliCandidateSelectionMode: String, CaseIterable, Identifiable, Sendable {
    case single
    case multiple
    case range
    case all

    public var id: String { rawValue }

    public var title: String {
        switch self {
        case .single:
            return "Single"
        case .multiple:
            return "Multiple"
        case .range:
            return "Range"
        case .all:
            return "All"
        }
    }
}

public struct BilibiliTaskResultPresentation: Identifiable, Equatable, Sendable {
    public let id: String
    public let selectionID: String
    public let title: String
    public let subtitle: String
    public let state: String
    public let message: String
    public let libraryItemID: String
    public let playbackLibraryItemID: String
    public let playbackURL: URL?
    public let isReady: Bool
    public let isCached: Bool
    public let isFailed: Bool
    public let isCancelled: Bool

    public var statusLabel: String {
        if isCached {
            return "Cached"
        }
        if isReady {
            return "Ready"
        }
        if isFailed {
            return "Failed"
        }
        if isCancelled {
            return "Cancelled"
        }
        return "Pending"
    }

    public var statusSystemImage: String {
        if isCached {
            return "externaldrive.fill.badge.checkmark"
        }
        if isReady {
            return "play.circle"
        }
        if isFailed {
            return "exclamationmark.triangle"
        }
        if isCancelled {
            return "xmark.circle"
        }
        return "clock"
    }
}

private struct BilibiliResolvedInputContext: Equatable {
    let source: String
    let endpoint: CacheServerEndpoint
    let options: BilibiliPlaybackTaskOptions
}

private struct BilibiliCandidateSelectionRequest {
    let selection: BilibiliTaskSelection
    let legacySelectionID: String?
}

@MainActor
public final class BilibiliTaskViewModel: ObservableObject {
    @Published public var sourceText: String
    @Published public var qualityPreference: String
    @Published public var encodingPreference: String
    @Published public var audioLanguagePreference: String
    @Published public private(set) var currentTask: CacheTask?
    @Published public private(set) var statusMessage: String = "No Bilibili playback task submitted."
    @Published public private(set) var errorMessage: String?
    @Published public private(set) var isSubmitting = false
    @Published public private(set) var isResolving = false
    @Published public private(set) var isWatching = false
    @Published public private(set) var isCancelling = false
    @Published public private(set) var resolvedInput: BilibiliResolveResult?
    @Published public var candidateSelectionMode: BilibiliCandidateSelectionMode = .single {
        didSet {
            normalizeCandidateSelectionForMode()
        }
    }
    @Published public var selectedCandidateID: String? {
        didSet {
            normalizeCandidateSelectionForMode()
        }
    }
    @Published public var selectedCandidateIDs: Set<String> = [] {
        didSet {
            normalizeCandidateSelectionForMode()
        }
    }
    @Published public var rangeStartCandidateID: String? {
        didSet {
            normalizeCandidateSelectionForMode()
        }
    }
    @Published public var rangeEndCandidateID: String? {
        didSet {
            normalizeCandidateSelectionForMode()
        }
    }

    private let clientFactory: @Sendable (CacheServerEndpoint) -> any CacheControlClient
    private let operationTimeout: Duration
    private var activeEndpoint: CacheServerEndpoint?
    private var resolvedInputContext: BilibiliResolvedInputContext?
    private var taskWatcher: Task<Void, Never>?
    private var operationSequence = 0
    private var activePlaybackTaskID: String?
    private var activePlaybackLibraryItemID: String?
    private var activePlaybackResultID: String?
    private var isNormalizingCandidateSelection = false
    private var isChoosingRangeEnd = false

    public init(
        sourceText: String = "",
        qualityPreference: String = "",
        encodingPreference: String = "",
        audioLanguagePreference: String = "",
        operationTimeout: Duration = .seconds(10),
        clientFactory: @escaping @Sendable (CacheServerEndpoint) -> any CacheControlClient = {
            GRPCCacheControlClient(endpoint: $0)
        }
    ) {
        self.sourceText = sourceText
        self.qualityPreference = qualityPreference
        self.encodingPreference = encodingPreference
        self.audioLanguagePreference = audioLanguagePreference
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
            return candidateSelectionRequest != nil
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

    public func canPlay(result: BilibiliTaskResultPresentation) -> Bool {
        playableURL(for: result) != nil
    }

    public func playableURL(for result: BilibiliTaskResultPresentation) -> URL? {
        guard !isSubmitting && !isResolving && !isCancelling else {
            return nil
        }

        guard let currentTask,
            !currentTask.isCancellationPendingBilibiliTaskState
        else {
            return nil
        }

        return currentTask
            .bilibiliTaskResults
            .first { $0.id == result.id }?
            .playbackURL
    }

    public var canClear: Bool {
        currentTask != nil || errorMessage != nil || resolvedInput != nil
    }

    public var canReResolve: Bool {
        !isSubmitting
            && !isResolving
            && !isCancelling
            && currentTask == nil
            && resolvedInputMatchesSource
            && !sourceText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    public var canClearCandidateSelection: Bool {
        isWaitingForCandidateSelection && selectedCandidateCount > 0
    }

    public var availableCandidateSelectionModes: [BilibiliCandidateSelectionMode] {
        var modes: [BilibiliCandidateSelectionMode] = [.single, .multiple, .range]
        if canSelectAllResolvedCandidates {
            modes.append(.all)
        }
        return modes
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

    public var selectedCandidateCount: Int {
        guard isWaitingForCandidateSelection else {
            return 0
        }

        switch candidateSelectionMode {
        case .single:
            return selectedCandidate == nil ? 0 : 1
        case .multiple:
            return orderedSelectedCandidateIDs.count
        case .range:
            return selectedRangeCandidateIDs.count
        case .all:
            return canSelectAllResolvedCandidates ? resolvedCandidates.count : 0
        }
    }

    public var candidateSelectionSummary: String? {
        guard isWaitingForCandidateSelection else {
            return nil
        }

        switch candidateSelectionMode {
        case .single:
            guard let selectedCandidate else {
                return "Select one Bilibili item."
            }
            return "Selected \(selectedCandidate.displayTitle)."
        case .multiple:
            let count = orderedSelectedCandidateIDs.count
            return count == 1 ? "1 Bilibili item selected." : "\(count) Bilibili items selected."
        case .range:
            guard let start = rangeStartCandidate, let end = rangeEndCandidate else {
                return "Select a start and end item."
            }
            let count = selectedRangeCandidateIDs.count
            return "Range \(start.displayTitle) to \(end.displayTitle) selects \(count) item\(count == 1 ? "" : "s")."
        case .all:
            guard canSelectAllResolvedCandidates else {
                return "All selection is unavailable because the resolved item list is truncated."
            }
            let count = resolvedCandidates.count
            return "All \(count) Bilibili item\(count == 1 ? "" : "s") selected."
        }
    }

    public var submitButtonTitle: String {
        if isResolving {
            return "Resolving"
        }
        if isSubmitting {
            return "Submitting"
        }
        if isWaitingForCandidateSelection {
            switch candidateSelectionMode {
            case .single:
                return "Submit Selected"
            case .multiple:
                return "Submit Multiple"
            case .range:
                return "Submit Range"
            case .all:
                return "Submit All"
            }
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

    public var taskResults: [BilibiliTaskResultPresentation] {
        currentTask?.bilibiliTaskResults ?? []
    }

    public var taskResultSummary: BilibiliTaskResultSummary? {
        currentTask?.bilibiliTaskResultSummary
    }

    public var fetchNotice: BilibiliFetchNotice? {
        if let errorNotice = Self.errorNotice(for: errorMessage, currentTask: currentTask) {
            return errorNotice
        }

        guard currentTask == nil, resolvedInputMatchesSource, let resolvedInput else {
            return nil
        }

        if resolvedInput.candidates.isEmpty {
            return BilibiliFetchNotice(
                title: "No items found",
                message: "The resolved Bilibili list is empty for the current account or upstream page.",
                systemImage: "tray",
                tone: .warning,
                actionTitle: "Re-resolve"
            )
        }

        if resolvedInput.candidatesTruncated {
            return BilibiliFetchNotice(
                title: "Showing a bounded window",
                message:
                    "Only the first \(resolvedInput.candidates.count) resolved items are shown. Use a narrower URL or re-resolve before submitting a large batch.",
                systemImage: "rectangle.stack.badge.exclamationmark",
                tone: .warning,
                actionTitle: "Re-resolve"
            )
        }

        if Self.isVolatileResolvedSourceKind(resolvedInput.sourceKind) {
            return BilibiliFetchNotice(
                title: "List may change",
                message:
                    "This Bilibili list or feed can reorder between refreshes. Stable item IDs are sent when you submit selected items.",
                systemImage: "arrow.triangle.2.circlepath",
                tone: .info,
                actionTitle: "Re-resolve"
            )
        }

        return nil
    }

    public var playableTaskResults: [BilibiliTaskResultPresentation] {
        taskResults.filter { $0.playbackURL != nil }
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

        if isWaitingForCandidateSelection,
            resolvedInputMatches(source: source, endpoint: endpoint, options: options)
        {
            guard let selectionRequest = candidateSelectionRequest else {
                errorMessage = "Select Bilibili items before submitting playback."
                statusMessage = "Bilibili item selection is required."
                return
            }

            await createPlaybackTask(
                source: source,
                selection: selectionRequest.selection,
                legacySelectionID: selectionRequest.legacySelectionID,
                endpoint: endpoint,
                options: options
            )
            return
        }

        operationSequence += 1
        activePlaybackTaskID = nil
        activePlaybackResultID = nil
        let sequence = operationSequence

        stopWatching()
        activeEndpoint = endpoint
        currentTask = nil
        resolvedInput = nil
        resolvedInputContext = nil
        clearCandidateSelection()
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

            guard currentSubmissionMatches(source: source, options: options) else {
                discardStaleResolveSubmission()
                return
            }

            resolvedInput = resolved
            resolvedInputContext = BilibiliResolvedInputContext(
                source: source,
                endpoint: endpoint,
                options: options
            )
            applyResolvedCandidateDefaults(resolved)
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
                selection: Self.singleSelection(for: candidate.selectionID),
                legacySelectionID: candidate.selectionID,
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
                guard currentSubmissionMatches(source: source, options: options) else {
                    discardStaleResolveSubmission()
                    return
                }

                await createPlaybackTask(
                    source: source,
                    selection: nil,
                    legacySelectionID: nil,
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
            clearCandidateSelection()
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

    public func reResolve(serverAddressText: String) async {
        guard canReResolve else {
            return
        }

        guard CacheServerEndpoint.normalized(from: serverAddressText) != nil else {
            errorMessage = "Use a cache server host and optional port before submitting Bilibili playback."
            statusMessage = "Cache server address is invalid."
            return
        }

        resolvedInput = nil
        resolvedInputContext = nil
        clearCandidateSelection()
        await submit(serverAddressText: serverAddressText)
    }

    public func clearResolvedCandidateSelection() {
        guard isWaitingForCandidateSelection else {
            return
        }

        isNormalizingCandidateSelection = true
        candidateSelectionMode = .multiple
        selectedCandidateID = nil
        selectedCandidateIDs = []
        rangeStartCandidateID = nil
        rangeEndCandidateID = nil
        isChoosingRangeEnd = false
        errorMessage = nil
        statusMessage = "Select a Bilibili item to play."
        isNormalizingCandidateSelection = false
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
            activePlaybackResultID = nil
            activePlaybackLibraryItemID = nil
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
            activePlaybackResultID = nil
            activePlaybackLibraryItemID = currentTask.playableBilibiliLibraryItemID
            statusMessage = "Playing \(currentTask.bilibiliDisplayTitle)."
        } else {
            activePlaybackTaskID = nil
            activePlaybackResultID = nil
            activePlaybackLibraryItemID = nil
            statusMessage = Self.statusMessage(for: currentTask)
        }
    }

    public func finishPreparedPlayback(result: BilibiliTaskResultPresentation, didStartPlayback: Bool) {
        guard let currentTask,
            let currentResult = currentTask.bilibiliTaskResults.first(where: { $0.id == result.id })
        else {
            return
        }

        errorMessage = nil
        if didStartPlayback {
            activePlaybackTaskID = currentTask.id
            activePlaybackResultID = currentResult.id
            activePlaybackLibraryItemID = normalizedNonEmpty(currentResult.playbackLibraryItemID)
            statusMessage = "Playing \(currentResult.title)."
        } else {
            activePlaybackTaskID = nil
            activePlaybackResultID = nil
            activePlaybackLibraryItemID = nil
            statusMessage = Self.statusMessage(for: currentTask)
        }
    }

    public func clearPlaybackStatus() {
        guard activePlaybackTaskID != nil else {
            return
        }

        activePlaybackTaskID = nil
        activePlaybackResultID = nil
        activePlaybackLibraryItemID = nil
        statusMessage = currentTask.map(Self.statusMessage(for:)) ?? "No Bilibili playback task submitted."
    }

    public func isActivePlaybackLibraryItem(id libraryItemID: String) -> Bool {
        guard let currentTask,
            activePlaybackTaskID == currentTask.id,
            let activePlaybackLibraryItemID
        else {
            return false
        }

        return normalizedNonEmpty(libraryItemID) == activePlaybackLibraryItemID
    }

    public func clearTask() {
        operationSequence += 1
        activeEndpoint = nil
        activePlaybackTaskID = nil
        activePlaybackResultID = nil
        activePlaybackLibraryItemID = nil
        currentTask = nil
        errorMessage = nil
        isSubmitting = false
        isResolving = false
        isCancelling = false
        resolvedInput = nil
        resolvedInputContext = nil
        clearCandidateSelection()
        stopWatching()
        statusMessage = "No Bilibili playback task submitted."
    }

    @discardableResult
    public func clearTaskIfCachedLibraryItemDeleted(id libraryItemID: String) -> Bool {
        guard let currentTask,
            !libraryItemID.isEmpty,
            currentTask.hasBilibiliLibraryItem(id: libraryItemID)
        else {
            return false
        }

        clearTask()
        return true
    }

    public func chooseCandidate(_ candidate: BilibiliResolvedCandidate) {
        switch candidateSelectionMode {
        case .single:
            selectedCandidateID = candidate.selectionID
            selectedCandidateIDs = [candidate.selectionID]
        case .multiple:
            if selectedCandidateIDs.contains(candidate.selectionID) {
                selectedCandidateIDs.remove(candidate.selectionID)
                if selectedCandidateID == candidate.selectionID {
                    selectedCandidateID = orderedSelectedCandidateIDs.first
                }
            } else {
                selectedCandidateIDs.insert(candidate.selectionID)
                selectedCandidateID = candidate.selectionID
            }
        case .range:
            chooseRangeCandidate(candidate)
        case .all:
            selectedCandidateID = candidate.selectionID
        }
    }

    public func isCandidateSelected(_ candidate: BilibiliResolvedCandidate) -> Bool {
        switch candidateSelectionMode {
        case .single:
            return selectedCandidate?.selectionID == candidate.selectionID
        case .multiple:
            return selectedCandidateIDs.contains(candidate.selectionID)
        case .range:
            return selectedRangeCandidateIDs.contains(candidate.selectionID)
        case .all:
            return true
        }
    }

    private var orderedSelectedCandidateIDs: [String] {
        let selectedIDs = selectedCandidateIDs
        return
            resolvedCandidates
            .map(\.selectionID)
            .filter { selectedIDs.contains($0) }
    }

    private var rangeStartCandidate: BilibiliResolvedCandidate? {
        candidate(withID: rangeStartCandidateID) ?? resolvedCandidates.first
    }

    private var rangeEndCandidate: BilibiliResolvedCandidate? {
        candidate(withID: rangeEndCandidateID) ?? rangeStartCandidate
    }

    private var selectedRangeCandidateIDs: Set<String> {
        guard let start = rangeStartCandidate,
            let end = rangeEndCandidate
        else {
            return []
        }

        let bounds = sortedRangeBounds(start: start, end: end)
        return Set(
            resolvedCandidates
                .filter { candidate in
                    let index = candidateSelectionIndex(candidate)
                    return index >= bounds.start && index <= bounds.end
                }
                .map(\.selectionID)
        )
    }

    private var canSelectAllResolvedCandidates: Bool {
        resolvedInput?.candidatesTruncated != true && !resolvedCandidates.isEmpty
    }

    private var candidateSelectionRequest: BilibiliCandidateSelectionRequest? {
        switch candidateSelectionMode {
        case .single:
            guard let candidate = selectedCandidate else {
                return nil
            }
            return BilibiliCandidateSelectionRequest(
                selection: Self.singleSelection(for: candidate.selectionID),
                legacySelectionID: candidate.selectionID
            )
        case .multiple:
            let selectionIDs = orderedSelectedCandidateIDs
            guard !selectionIDs.isEmpty else {
                return nil
            }
            return BilibiliCandidateSelectionRequest(
                selection: Self.multipleSelection(for: selectionIDs),
                legacySelectionID: nil
            )
        case .range:
            guard let start = rangeStartCandidate,
                let end = rangeEndCandidate
            else {
                return nil
            }
            let bounds = sortedRangeBounds(start: start, end: end)
            return BilibiliCandidateSelectionRequest(
                selection: Self.rangeSelection(startIndex: bounds.start, endIndex: bounds.end),
                legacySelectionID: nil
            )
        case .all:
            guard canSelectAllResolvedCandidates else {
                return nil
            }
            return BilibiliCandidateSelectionRequest(
                selection: Self.allSelection(),
                legacySelectionID: nil
            )
        }
    }

    private func candidate(withID id: String?) -> BilibiliResolvedCandidate? {
        guard let id,
            !id.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        else {
            return nil
        }

        return resolvedCandidates.first { $0.selectionID == id }
    }

    private func applyResolvedCandidateDefaults(_ resolved: BilibiliResolveResult) {
        candidateSelectionMode = .single
        isChoosingRangeEnd = false
        let defaultSelectionID =
            resolved.defaultSelectionID.isEmpty
            ? resolved.candidates.first?.selectionID
            : resolved.defaultSelectionID
        selectedCandidateID = defaultSelectionID
        selectedCandidateIDs = defaultSelectionID.map { Set([$0]) } ?? []
        rangeStartCandidateID = resolved.candidates.first?.selectionID
        rangeEndCandidateID = resolved.candidates.last?.selectionID
        normalizeCandidateSelectionForMode()
    }

    private func clearCandidateSelection() {
        candidateSelectionMode = .single
        isChoosingRangeEnd = false
        selectedCandidateID = nil
        selectedCandidateIDs = []
        rangeStartCandidateID = nil
        rangeEndCandidateID = nil
    }

    private func normalizeCandidateSelectionForMode() {
        guard !isNormalizingCandidateSelection else {
            return
        }

        isNormalizingCandidateSelection = true
        defer {
            isNormalizingCandidateSelection = false
        }

        let candidates = resolvedCandidates
        let validIDs = Set(candidates.map(\.selectionID))

        if let selectedCandidateID,
            !validIDs.contains(selectedCandidateID)
        {
            self.selectedCandidateID = nil
        }

        selectedCandidateIDs = selectedCandidateIDs.filter { validIDs.contains($0) }
        if let rangeStartCandidateID,
            !validIDs.contains(rangeStartCandidateID)
        {
            self.rangeStartCandidateID = nil
        }
        if let rangeEndCandidateID,
            !validIDs.contains(rangeEndCandidateID)
        {
            self.rangeEndCandidateID = nil
        }

        switch candidateSelectionMode {
        case .single:
            isChoosingRangeEnd = false
            if selectedCandidateID == nil {
                selectedCandidateID = resolvedInput?.defaultSelectionID.nilIfEmpty ?? candidates.first?.selectionID
            }
            if let selectedCandidateID {
                selectedCandidateIDs = [selectedCandidateID]
            }
        case .multiple:
            isChoosingRangeEnd = false
        case .range:
            if rangeStartCandidateID == nil {
                rangeStartCandidateID = candidates.first?.selectionID
            }
            if rangeEndCandidateID == nil {
                rangeEndCandidateID = rangeStartCandidateID
            }
        case .all:
            isChoosingRangeEnd = false
            break
        }
    }

    private func chooseRangeCandidate(_ candidate: BilibiliResolvedCandidate) {
        if !isChoosingRangeEnd {
            rangeStartCandidateID = candidate.selectionID
            rangeEndCandidateID = candidate.selectionID
            isChoosingRangeEnd = true
            return
        }

        rangeEndCandidateID = candidate.selectionID
        isChoosingRangeEnd = false
    }

    private func sortedRangeBounds(
        start: BilibiliResolvedCandidate,
        end: BilibiliResolvedCandidate
    ) -> (start: Int, end: Int) {
        let startIndex = candidateSelectionIndex(start)
        let endIndex = candidateSelectionIndex(end)
        return (
            start: min(startIndex, endIndex),
            end: max(startIndex, endIndex)
        )
    }

    private func candidateSelectionIndex(_ candidate: BilibiliResolvedCandidate) -> Int {
        if candidate.index > 0 {
            return candidate.index
        }

        guard let offset = resolvedCandidates.firstIndex(where: { $0.selectionID == candidate.selectionID }) else {
            return 1
        }

        return offset + 1
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
        await createPlaybackTask(
            source: source,
            selection: nil,
            legacySelectionID: selectionID,
            endpoint: endpoint,
            options: options
        )
    }

    private func createPlaybackTask(
        source: String,
        selection: BilibiliTaskSelection?,
        legacySelectionID: String?,
        endpoint: CacheServerEndpoint,
        options: BilibiliPlaybackTaskOptions
    ) async {
        operationSequence += 1
        activePlaybackTaskID = nil
        activePlaybackResultID = nil
        let sequence = operationSequence
        let client = clientFactory(endpoint)
        await createPlaybackTask(
            source: source,
            selection: selection,
            legacySelectionID: legacySelectionID,
            endpoint: endpoint,
            sequence: sequence,
            client: client,
            options: options
        )
    }

    private func createPlaybackTask(
        source: String,
        selection: BilibiliTaskSelection?,
        legacySelectionID: String?,
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
                try await Self.createBilibiliPlaybackTask(
                    client: client,
                    source: source,
                    selection: selection,
                    legacySelectionID: legacySelectionID,
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

    private static func createBilibiliPlaybackTask(
        client: any CacheControlClient,
        source: String,
        selection: BilibiliTaskSelection?,
        legacySelectionID: String?,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask {
        guard let selection else {
            return try await client.createBilibiliPlaybackTask(
                urlOrID: source,
                selectionID: legacySelectionID,
                options: options
            )
        }

        do {
            return try await client.createBilibiliPlaybackTask(
                urlOrID: source,
                selection: selection,
                options: options
            )
        } catch let unsupported as CacheControlClientUnsupportedFeature
            where unsupported == .bilibiliTaskSelection
        {
            let normalizedLegacySelectionID =
                legacySelectionID?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
            guard !normalizedLegacySelectionID.isEmpty else {
                throw unsupported
            }

            return try await client.createBilibiliPlaybackTask(
                urlOrID: source,
                selectionID: normalizedLegacySelectionID,
                options: options
            )
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
        if activePlaybackTaskID == task.id {
            updateActivePlaybackTracking(for: task)
        }
        if task.isFailedBilibiliTaskState {
            errorMessage = Self.failureMessage(for: task)
        } else {
            errorMessage = nil
        }

        if activePlaybackTaskID == task.id {
            statusMessage = activePlaybackStatusMessage(for: task)
        } else {
            statusMessage = Self.statusMessage(for: task)
        }
        if task.isTerminalBilibiliTaskState {
            isCancelling = false
            stopWatching()
        }
    }

    private func updateActivePlaybackTracking(for task: CacheTask) {
        guard task.isPlayableBilibiliTaskState,
            !task.isCancellationPendingBilibiliTaskState
        else {
            activePlaybackTaskID = nil
            activePlaybackResultID = nil
            activePlaybackLibraryItemID = nil
            return
        }

        if let activePlaybackResultID {
            guard let result = task.bilibiliTaskResults.first(where: { $0.id == activePlaybackResultID }),
                result.playbackURL != nil
            else {
                activePlaybackTaskID = nil
                self.activePlaybackResultID = nil
                activePlaybackLibraryItemID = nil
                return
            }

            activePlaybackLibraryItemID = normalizedNonEmpty(result.playbackLibraryItemID)
            return
        }

        activePlaybackLibraryItemID = task.playableBilibiliLibraryItemID
    }

    private func activePlaybackStatusMessage(for task: CacheTask) -> String {
        if let activePlaybackResultID,
            let result = task.bilibiliTaskResults.first(where: { $0.id == activePlaybackResultID })
        {
            return "Playing \(result.title)."
        }

        return "Playing \(task.bilibiliDisplayTitle)."
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
        if task.isCancellationPendingBilibiliTaskState {
            let message = task.message.trimmingCharacters(in: .whitespacesAndNewlines)
            return message.isEmpty ? "Cancelling \(task.bilibiliDisplayTitle)..." : message
        }

        if task.isCancelledBilibiliTaskState {
            return "\(task.bilibiliDisplayTitle) was cancelled."
        }

        if let summary = task.bilibiliTaskResultSummary,
            summary.totalCount > 1
        {
            return summary.statusMessage
        }

        if task.isCompletedBilibiliTaskState {
            return "\(task.bilibiliDisplayTitle) is cached for LAN playback."
        }

        if task.isPlayableBilibiliTaskState, task.playableBilibiliURL != nil {
            return "\(task.bilibiliDisplayTitle) is ready to play."
        }

        if task.isFailedBilibiliTaskState {
            return failureMessage(for: task)
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

    private static func errorNotice(
        for errorMessage: String?,
        currentTask: CacheTask?
    ) -> BilibiliFetchNotice? {
        guard let errorMessage else {
            return nil
        }

        let normalized = errorMessage.lowercased()
        if normalized.contains("login")
            || normalized.contains("cookie")
            || normalized.contains("credential")
            || normalized.contains("auth")
            || normalized.contains("unauthorized")
        {
            return BilibiliFetchNotice(
                title: "Credentials required",
                message:
                    "This Bilibili page needs server-side web credentials. Refresh the LAN cache server credential file, then retry.",
                systemImage: "person.crop.circle.badge.exclamationmark",
                tone: .warning,
                actionTitle: "Retry"
            )
        }

        if normalized.contains("empty") || normalized.contains("no item") || normalized.contains("no selectable") {
            return BilibiliFetchNotice(
                title: "No items found",
                message: "The resolved Bilibili list is empty for the current account or upstream page.",
                systemImage: "tray",
                tone: .warning,
                actionTitle: "Retry"
            )
        }

        guard currentTask?.isRetryableBilibiliTaskState == true || currentTask == nil else {
            return nil
        }

        if normalized.contains("timed out")
            || normalized.contains("timeout")
            || normalized.contains("upstream")
            || normalized.contains("rate")
            || normalized.contains("api returned")
            || normalized.contains("network")
            || currentTask?.isRetryableBilibiliTaskState == true
        {
            return BilibiliFetchNotice(
                title: "Retry available",
                message:
                    "The Bilibili request failed or timed out. Retry after the cache server reconnects or upstream rate limits clear.",
                systemImage: "arrow.clockwise.circle",
                tone: .error,
                actionTitle: "Retry"
            )
        }

        return nil
    }

    private static func progressiveCacheStatusBadge(for task: CacheTask) -> ProgressiveCacheStatusBadge? {
        guard task.isProgressivePlayback else {
            return nil
        }

        if let summary = task.bilibiliTaskResultSummary,
            summary.totalCount > 1
        {
            if summary.cachedCount == summary.totalCount {
                return ProgressiveCacheStatusBadge(
                    label: "Offline ready", systemImage: "externaldrive.fill.badge.checkmark")
            }

            if summary.cachedCount > 0 {
                return ProgressiveCacheStatusBadge(
                    label: "\(summary.cachedCount) of \(summary.totalCount) offline ready",
                    systemImage: "externaldrive.badge.checkmark"
                )
            }

            if summary.hasPartialSuccess {
                return ProgressiveCacheStatusBadge(label: "Partial result success", systemImage: "checkmark.circle")
            }

            if summary.readyCount > 0 {
                return ProgressiveCacheStatusBadge(label: "Playable online; caching", systemImage: "wifi")
            }
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

    private static func singleSelection(for selectionID: String) -> BilibiliTaskSelection {
        BilibiliTaskSelection(mode: "single", selectionIDs: [selectionID])
    }

    private static func multipleSelection(for selectionIDs: [String]) -> BilibiliTaskSelection {
        BilibiliTaskSelection(mode: "multiple", selectionIDs: selectionIDs)
    }

    private static func rangeSelection(startIndex: Int, endIndex: Int) -> BilibiliTaskSelection {
        BilibiliTaskSelection(mode: "range", rangeStartIndex: startIndex, rangeEndIndex: endIndex)
    }

    private static func allSelection() -> BilibiliTaskSelection {
        BilibiliTaskSelection(mode: "all")
    }

    private static func isVolatileResolvedSourceKind(_ sourceKind: String) -> Bool {
        switch normalizedBilibiliSourceKind(sourceKind) {
        case "favorite", "space", "collection", "series", "history", "watchlater", "following", "dynamic",
            "spacedynamic", "recommendation", "recommendations", "homepage", "feed":
            return true
        default:
            return false
        }
    }

    private static func normalizedBilibiliSourceKind(_ sourceKind: String) -> String {
        sourceKind
            .lowercased()
            .filter { $0.isLetter || $0.isNumber }
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

private func normalizedNonEmpty(_ value: String) -> String? {
    let normalized = value.trimmingCharacters(in: .whitespacesAndNewlines)
    return normalized.isEmpty ? nil : normalized
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

        return topLevelPlayableBilibiliURL
            ?? bilibiliTaskResults.first(where: { $0.playbackURL != nil })?.playbackURL
    }

    var playableBilibiliLibraryItemID: String? {
        guard isProgressivePlayback, isPlayableBilibiliTaskState else {
            return nil
        }

        if topLevelPlayableBilibiliURL != nil {
            guard isCompletedBilibiliTaskState else {
                return nil
            }

            return normalizedNonEmpty(libraryItemID)
                ?? playbackSource.flatMap { normalizedNonEmpty($0.itemID) }
        }

        return resultItems.first { $0.playableBilibiliURL != nil }?.playableBilibiliLibraryItemID
    }

    var topLevelPlayableBilibiliURL: URL? {
        guard let expectedItemID = expectedBilibiliPlaybackSourceItemID else {
            return nil
        }

        return playbackSource.flatMap {
            playableURL(for: $0, expectedItemID: expectedItemID)
        }
    }

    var expectedBilibiliPlaybackSourceItemID: String? {
        let itemID = isCompletedBilibiliTaskState ? libraryItemID : id
        let trimmedItemID = itemID.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmedItemID.isEmpty ? nil : trimmedItemID
    }

    var bilibiliTaskResults: [BilibiliTaskResultPresentation] {
        resultItems.map { item in
            BilibiliTaskResultPresentation(
                id: item.id,
                selectionID: item.selectionID,
                title: item.displayTitle,
                subtitle: item.subtitle,
                state: item.state,
                message: item.message,
                libraryItemID: item.libraryItemID,
                playbackLibraryItemID: item.playableBilibiliLibraryItemID ?? "",
                playbackURL: item.playableBilibiliURL,
                isReady: item.isReadyBilibiliResultState,
                isCached: item.isCompletedBilibiliResultState,
                isFailed: item.isFailedBilibiliResultState,
                isCancelled: item.isCancelledBilibiliResultState
            )
        }
    }

    var bilibiliTaskResultSummary: BilibiliTaskResultSummary? {
        guard !resultItems.isEmpty else {
            return nil
        }

        let readyCount = resultItems.filter(\.isReadyBilibiliResultState).count
        let cachedCount = resultItems.filter(\.isCompletedBilibiliResultState).count
        let failedCount = resultItems.filter(\.isFailedBilibiliResultState).count
        let cancelledCount = resultItems.filter(\.isCancelledBilibiliResultState).count
        let pendingCount = resultItems.count - readyCount - failedCount - cancelledCount

        return BilibiliTaskResultSummary(
            totalCount: resultItems.count,
            readyCount: readyCount,
            cachedCount: cachedCount,
            failedCount: failedCount,
            cancelledCount: cancelledCount,
            pendingCount: max(pendingCount, 0)
        )
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
            let byteRatio = min(max(Double(downloadedBytes) / Double(totalBytes), 0), 0.99)
            let overallRatio = progress > 0 ? min(max(progress, 0), 0.99) : byteRatio
            let ratio = min(byteRatio, overallRatio)
            return "\(Int((ratio * 100).rounded()))%"
        }

        guard progress > 0, progress < 1 else {
            return nil
        }

        return "\(Int((min(max(progress, 0), 0.99) * 100).rounded()))%"
    }

    func hasBilibiliLibraryItem(id libraryItemID: String) -> Bool {
        let trimmedLibraryItemID = libraryItemID.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedLibraryItemID.isEmpty else {
            return false
        }

        if self.libraryItemID == trimmedLibraryItemID {
            return true
        }

        return resultItems.contains { $0.libraryItemID == trimmedLibraryItemID }
    }
}

private extension BilibiliTaskResultItem {
    var displayTitle: String {
        let title = title.trimmingCharacters(in: .whitespacesAndNewlines)
        if !title.isEmpty {
            return title
        }

        let subtitle = subtitle.trimmingCharacters(in: .whitespacesAndNewlines)
        if !subtitle.isEmpty {
            return subtitle
        }

        return selectionID.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? id : selectionID
    }

    var playableBilibiliURL: URL? {
        guard isReadyBilibiliResultState else {
            return nil
        }

        guard let expectedItemID else {
            return nil
        }

        return playbackSource.flatMap {
            playableURL(for: $0, expectedItemID: expectedItemID)
        }
    }

    var playableBilibiliLibraryItemID: String? {
        guard playableBilibiliURL != nil, isCompletedBilibiliResultState else {
            return nil
        }

        return normalizedNonEmpty(libraryItemID)
            ?? playbackSource.flatMap { normalizedNonEmpty($0.itemID) }
    }

    var expectedItemID: String? {
        let itemID = isCompletedBilibiliResultState ? libraryItemID : id
        let trimmedItemID = itemID.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmedItemID.isEmpty ? nil : trimmedItemID
    }

    var isReadyBilibiliResultState: Bool {
        let state = normalizedBilibiliResultState
        return state.contains("playable") || state.contains("completed")
    }

    var isCompletedBilibiliResultState: Bool {
        normalizedBilibiliResultState.contains("completed")
    }

    var isFailedBilibiliResultState: Bool {
        normalizedBilibiliResultState.contains("failed")
    }

    var isCancelledBilibiliResultState: Bool {
        normalizedBilibiliResultState.contains("cancelled")
    }

    var normalizedBilibiliResultState: String {
        state.lowercased().filter(\.isLetter)
    }
}

private extension BilibiliResolvedCandidate {
    var displayTitle: String {
        let title = title.trimmingCharacters(in: .whitespacesAndNewlines)
        if !title.isEmpty {
            return title
        }

        let subtitle = subtitle.trimmingCharacters(in: .whitespacesAndNewlines)
        if !subtitle.isEmpty {
            return subtitle
        }

        return selectionID.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? id : selectionID
    }
}

private func playableURL(for source: CachePlaybackSource, expectedItemID: String) -> URL? {
    guard source.isPlayableByTVOSClient,
        source.itemID == expectedItemID
    else {
        return nil
    }

    return source.explicitHTTPURL
}

private extension BilibiliTaskViewModel {
    var currentPlaybackOptions: BilibiliPlaybackTaskOptions {
        BilibiliPlaybackTaskOptions(
            qualityPreference: qualityPreference.trimmingCharacters(in: .whitespacesAndNewlines),
            encodingPreference: encodingPreference.trimmingCharacters(in: .whitespacesAndNewlines),
            audioLanguagePreference: audioLanguagePreference.trimmingCharacters(in: .whitespacesAndNewlines)
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

    func currentSubmissionMatches(source: String, options: BilibiliPlaybackTaskOptions) -> Bool {
        Self.normalizedBilibiliSource(sourceText) == source
            && currentPlaybackOptions == options
    }

    func discardStaleResolveSubmission() {
        currentTask = nil
        resolvedInput = nil
        resolvedInputContext = nil
        clearCandidateSelection()
        errorMessage = nil
        statusMessage = "Bilibili input changed before resolve completed."
        isResolving = false
        isSubmitting = false
    }

    static func normalizedBilibiliSource(_ source: String) -> String {
        source.trimmingCharacters(in: .whitespacesAndNewlines)
    }
}

private extension String {
    var nilIfEmpty: String? {
        let trimmed = trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }

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
