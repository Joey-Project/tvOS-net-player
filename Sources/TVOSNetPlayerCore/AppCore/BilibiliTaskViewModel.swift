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
    public let playbackVariantID: String
    public let playbackURL: URL?
    public let artifacts: [BilibiliTaskArtifactPresentation]
    public let isReady: Bool
    public let isCached: Bool
    public let isFailed: Bool
    public let isCancelled: Bool
    public let hlsCacheFillStatus: HlsCacheFillStatus?

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

public struct BilibiliTaskArtifactPresentation: Identifiable, Equatable, Sendable {
    public let id: String
    public let kind: String
    public let state: String
    public let title: String
    public let format: String
    public let languageTag: String
    public let isAIGenerated: Bool
    public let resourceURL: URL?
    public let contentType: String
    public let sizeBytes: Int64
    public let sizeKnown: Bool
    public let expiresAt: Date?
    public let libraryItemID: String
    public let message: String

    public var isAvailable: Bool {
        state.normalizedBilibiliState.contains("available")
    }

    public var canOpenResource: Bool {
        guard isAvailable, resourceURL != nil else {
            return false
        }
        return expiresAt.map { $0 > Date() } ?? true
    }

    public var canOpenInLibrary: Bool {
        !libraryItemID.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }
}

private struct BilibiliResolvedInputContext: Equatable {
    let source: String
    let endpoint: CacheServerEndpoint
    let options: BilibiliPlaybackResolutionOptions
}

private struct BilibiliPlaybackResolutionOptions: Equatable {
    let qualityPreference: String
    let encodingPreference: String
    let audioLanguagePreference: String
    let preferTVAPI: Bool

    init(_ options: BilibiliPlaybackTaskOptions) {
        qualityPreference = options.qualityPreference
        encodingPreference = options.encodingPreference
        audioLanguagePreference = options.audioLanguagePreference
        preferTVAPI = options.preferTVAPI
    }
}

private struct BilibiliCandidateSelectionRequest {
    let selection: BilibiliResolutionSelection
}

private enum BilibiliTaskViewModelError: LocalizedError {
    case upgradeRequired
    case taskOutputUnavailable
    case invalidQuality
    case invalidCodec
    case tooManyCandidates
    case resolutionExpired
    case candidateSnapshotChanged

    var errorDescription: String? {
        switch self {
        case .upgradeRequired:
            return "Upgrade the cache server to a version that supports Bilibili v2 resolution and task execution."
        case .taskOutputUnavailable:
            return
                "Paginated Bilibili task results are unavailable. Restore task-output v2 support or upgrade the cache server."
        case .invalidQuality:
            return "The Bilibili quality preference is not recognized."
        case .invalidCodec:
            return "The Bilibili codec preference is not recognized."
        case .tooManyCandidates:
            return "A Bilibili task cannot include more than 100 items. Narrow the selection and try again."
        case .resolutionExpired:
            return "The Bilibili resolution expired. Re-resolve the input before submitting."
        case .candidateSnapshotChanged:
            return "The Bilibili candidate snapshot changed. Re-resolve the input before submitting."
        }
    }
}

private enum BilibiliRetryIntent {
    case reResolve
}

public enum BilibiliTaskSubmissionMode: String, CaseIterable, Identifiable {
    case playback
    case download

    public var id: String { rawValue }

    public var title: String {
        switch self {
        case .playback:
            return "Playback"
        case .download:
            return "Download"
        }
    }
}

public extension BilibiliSubtitleAIPolicy {
    var title: String {
        switch self {
        case .unspecified:
            return "Default"
        case .include:
            return "Include AI"
        case .preferNonAI:
            return "Prefer Non-AI"
        case .excludeAI:
            return "Exclude AI"
        case .onlyAI:
            return "Only AI"
        }
    }
}

public extension BilibiliDanmakuFormat {
    var title: String {
        switch self {
        case .xml:
            return "XML"
        case .ass:
            return "ASS"
        }
    }
}

public extension BilibiliTranscodingPreference {
    var title: String {
        switch self {
        case .auto:
            return "Auto"
        case .never:
            return "Never"
        case .force:
            return "Force"
        }
    }

    var summaryTitle: String {
        switch self {
        case .auto:
            return "auto transcode"
        case .never:
            return "never transcode"
        case .force:
            return "force transcode"
        }
    }
}

public extension BilibiliCompatibleVariantPreference {
    var title: String {
        switch self {
        case .preferCompatible:
            return "Compatible"
        case .preferRequested:
            return "Requested"
        }
    }

    var summaryTitle: String {
        switch self {
        case .preferCompatible:
            return "prefer compatible"
        case .preferRequested:
            return "prefer requested"
        }
    }
}

public extension BilibiliWeakNetworkPreference {
    var title: String {
        switch self {
        case .adaptive:
            return "Adaptive"
        case .holdDowngrade:
            return "Hold Downgrade"
        case .avPlayerManaged:
            return "AVPlayer Managed"
        }
    }

    var summaryTitle: String {
        switch self {
        case .adaptive:
            return "adaptive network"
        case .holdDowngrade:
            return "hold downgrade"
        case .avPlayerManaged:
            return "AVPlayer managed"
        }
    }
}

public extension BilibiliPlaybackPolicy {
    var summaryText: String {
        [
            transcodingPreference.summaryTitle,
            compatibleVariantPreference.summaryTitle,
            weakNetworkPreference.summaryTitle,
        ].joined(separator: ", ")
    }
}

public struct ClientPreparedResumption: Equatable, Sendable {
    public let url: URL
    public let progressContext: PlayerPlaybackProgressContext
    public let checkpoint: ClientPlaybackCheckpoint
}

private struct SavedPlaybackSourceSignature: Equatable {
    let itemID: String
    let variantID: String
    let playbackProtocol: String
    let uri: String
}

private struct SavedPlaybackObservationSignature: Equatable {
    let taskID: String
    let resultID: String?
    let libraryItemID: String
    let outputRevision: UInt64?
    let isPlayable: Bool
    let source: SavedPlaybackSourceSignature?

    func hasSameSelectedSource(as other: Self) -> Bool {
        guard taskID == other.taskID, resultID == other.resultID else {
            return false
        }
        if source != nil || other.source != nil {
            guard let source, let otherSource = other.source else {
                return false
            }
            return libraryItemID == other.libraryItemID && isPlayable == other.isPlayable
                && source == otherSource
        }
        return outputRevision == other.outputRevision && isPlayable == other.isPlayable
            && libraryItemID == other.libraryItemID
    }
}

private struct SavedPlaybackValidationTrigger: Equatable {
    let identity: ClientPlaybackIdentity
    let signature: SavedPlaybackObservationSignature

    func representsSameSelectedSource(as other: Self) -> Bool {
        identity == other.identity && signature.hasSameSelectedSource(as: other.signature)
    }
}

private enum SavedPlaybackPreparationFence {
    case current
    case obsolete
    case selectedSourceChanged
}

@MainActor
public final class BilibiliTaskViewModel: ObservableObject {
    public static let playbackTranscodingPreferenceDefaultsKey = "BilibiliPlaybackTranscodingPreference"
    public static let playbackCompatibleVariantPreferenceDefaultsKey =
        "BilibiliPlaybackCompatibleVariantPreference"
    public static let playbackWeakNetworkPreferenceDefaultsKey = "BilibiliPlaybackWeakNetworkPreference"

    private static let cacheServerAddressGuidance =
        "Use a cache server address or URL, such as mac-mini.local:50051 or https://cache.example.com."

    @Published public var sourceText: String
    @Published public var qualityPreference: String
    @Published public var encodingPreference: String
    @Published public var audioLanguagePreference: String
    @Published public var playbackTranscodingPreference: BilibiliTranscodingPreference {
        didSet {
            persistPlaybackPolicy()
        }
    }
    @Published public var playbackCompatibleVariantPreference: BilibiliCompatibleVariantPreference {
        didSet {
            persistPlaybackPolicy()
        }
    }
    @Published public var playbackWeakNetworkPreference: BilibiliWeakNetworkPreference {
        didSet {
            persistPlaybackPolicy()
        }
    }
    @Published public var submissionMode: BilibiliTaskSubmissionMode = .playback {
        didSet {
            if oldValue != submissionMode {
                operationSequence += 1
                isSubmitting = false
                isResolving = false
                clearResolutionSession()
            }
        }
    }
    @Published public var downloadSubtitles = false {
        didSet {
            if !downloadSubtitles {
                subtitleAIPolicy = .unspecified
            }
        }
    }
    @Published public var downloadDanmaku = false {
        didSet {
            if !downloadDanmaku {
                danmakuFormats = []
            }
        }
    }
    @Published public var downloadCover = false
    @Published public var subtitleAIPolicy: BilibiliSubtitleAIPolicy = .unspecified
    @Published public var danmakuFormats: Set<BilibiliDanmakuFormat> = []
    @Published public private(set) var currentTask: CacheTask?
    @Published public private(set) var statusMessage: String = "No Bilibili playback task submitted."
    @Published public private(set) var errorMessage: String?
    @Published public private(set) var isSubmitting = false
    @Published public private(set) var isResolving = false
    @Published public private(set) var isRestoringTask = false
    @Published public private(set) var resumptionMessage: String?
    /// True while the task observation lifecycle is active, including recovery delays and reconciliation.
    @Published public private(set) var isWatching = false
    @Published public private(set) var isReconnecting = false
    @Published public private(set) var watchErrorMessage: String?
    @Published public private(set) var isCancelling = false
    @Published public private(set) var resolvedInput: BilibiliResolveResult?
    @Published public private(set) var resolutionSession: BilibiliResolutionSession?
    @Published public private(set) var isLoadingMoreCandidates = false
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

    private let defaults: UserDefaults
    private let resumptionStore: ClientResumptionStore?
    private let clientFactory: @Sendable (CacheServerEndpoint) -> any CacheControlClient
    private let operationTimeout: Duration
    private let resolutionTimeout: Duration
    private let watchRecoverySleep: @Sendable (Duration) async throws -> Void
    private let watchReconciliationInterval: Duration
    private let watchReconciliationSleep: @Sendable (Duration) async throws -> Void
    private let watchReconciliationNow: @Sendable () -> ContinuousClock.Instant
    private var activeEndpoint: CacheServerEndpoint?
    private var activeServerID: String?
    private var restoredTaskIdentity: ClientResumptionTaskIdentity?
    private var restoreAttemptIdentity: ClientResumptionTaskIdentity?
    private var restoreGeneration = 0
    private var savedPlaybackValidationGeneration = 0
    private var savedPlaybackValidationTask: Task<Void, Never>?
    private var validatedSavedPlayback: ClientPreparedResumption?
    private var validatedSavedPlaybackSignature: SavedPlaybackObservationSignature?
    private var lastSavedPlaybackValidationTrigger: SavedPlaybackValidationTrigger?
    private var mediaBaseURIs: [String] = []
    private var resolvedInputContext: BilibiliResolvedInputContext?
    private var taskWatcher: Task<Void, Never>?
    private var watchReconciliationTask: Task<Void, Never>?
    private var watchReconciliationGeneration: Int?
    private var operationSequence = 0
    private var taskWatchGeneration = 0
    private var taskSnapshotGeneration = 0
    private var taskWatchActivityGeneration = 0
    private var taskWatchLastActivityInstant: ContinuousClock.Instant?
    private var taskWatchRecoveryAttempts = 0
    private var activePlaybackTaskID: String?
    private var activePlaybackLibraryItemID: String?
    private var activePlaybackResultID: String?
    private var retryIntent: BilibiliRetryIntent?
    private var isNormalizingCandidateSelection = false
    private var isChoosingRangeEnd = false
    private var resolutionPageToken = ""
    private var resolutionSnapshotID = ""
    private var resolutionTotalSize: UInt64 = 0
    private var candidatePageSequence = 0
    private var candidateAutoAttemptedPageTokens: Set<String> = []
    private var taskResultItems: [CacheTaskResult] = []
    private var taskResultPageToken = ""
    private var taskResultSnapshotID = ""
    private var taskResultOutputRevision: UInt64?
    private var taskResultPageSequence = 0
    private var taskResultRefreshTask: Task<Void, Never>?
    private var requestedTaskResultRevision: UInt64?
    @Published public private(set) var isLoadingMoreTaskResults = false
    @Published public private(set) var taskResultsErrorMessage: String?

    private static let resolutionPageSize = 50
    private static let taskResultPageSize = 50
    private static let maxExecutionCandidateCount: UInt64 = 100
    private static let cancelUnconfirmedMessage =
        "The cache server did not confirm task cancellation. Check the task status before retrying."

    public init(
        sourceText: String = "",
        qualityPreference: String = "",
        encodingPreference: String = "",
        audioLanguagePreference: String = "",
        playbackPolicy: BilibiliPlaybackPolicy? = nil,
        defaults: UserDefaults = .standard,
        resumptionStore: ClientResumptionStore? = nil,
        operationTimeout: Duration = .seconds(10),
        resolutionTimeout: Duration = .seconds(120),
        watchReconciliationInterval: Duration = .seconds(30),
        watchReconciliationSleep: (@Sendable (Duration) async throws -> Void)? = nil,
        watchRecoverySleep: (@Sendable (Duration) async throws -> Void)? = nil,
        watchReconciliationNow: (@Sendable () -> ContinuousClock.Instant)? = nil,
        clientFactory: @escaping @Sendable (CacheServerEndpoint) -> any CacheControlClient = {
            GRPCCacheControlClient(endpoint: $0)
        }
    ) {
        self.sourceText = sourceText
        self.qualityPreference = qualityPreference
        self.encodingPreference = encodingPreference
        self.audioLanguagePreference = audioLanguagePreference
        self.defaults = defaults
        self.resumptionStore = resumptionStore
        let initialPlaybackPolicy = playbackPolicy ?? Self.loadPlaybackPolicy(from: defaults)
        playbackTranscodingPreference = initialPlaybackPolicy.transcodingPreference
        playbackCompatibleVariantPreference = initialPlaybackPolicy.compatibleVariantPreference
        playbackWeakNetworkPreference = initialPlaybackPolicy.weakNetworkPreference
        self.operationTimeout = operationTimeout
        self.resolutionTimeout = resolutionTimeout
        self.watchReconciliationInterval = watchReconciliationInterval
        self.watchReconciliationSleep =
            watchReconciliationSleep ?? { duration in
                try await Task.sleep(for: duration)
            }
        self.watchRecoverySleep =
            watchRecoverySleep ?? { duration in
                try await Task.sleep(for: duration)
            }
        self.watchReconciliationNow = watchReconciliationNow ?? { ContinuousClock().now }
        self.clientFactory = clientFactory
    }

    deinit {
        taskWatcher?.cancel()
        watchReconciliationTask?.cancel()
        taskResultRefreshTask?.cancel()
        savedPlaybackValidationTask?.cancel()
    }

    public var hasSavedTask: Bool {
        resumptionStore?.record != nil
    }

    public var canContinueSavedPlayback: Bool {
        guard !isSubmitting, !isResolving, !isRestoringTask, !isCancelling,
            let record = resumptionStore?.record,
            let checkpoint = record.playback,
            let validatedSavedPlayback,
            checkpoint.identity == validatedSavedPlayback.checkpoint.identity,
            validatedSavedPlayback.progressContext.resumptionIdentity == checkpoint.identity,
            validatedSavedPlayback.progressContext.endpoint == record.task.endpoint,
            validatedSavedPlayback.progressContext.playbackURI == validatedSavedPlayback.url.absoluteString,
            let currentTask,
            currentTask.id == record.task.taskID,
            let validatedSavedPlaybackSignature,
            let currentSignature = savedPlaybackObservationSignature(for: currentTask, checkpoint: checkpoint),
            validatedSavedPlaybackSignature.hasSameSelectedSource(as: currentSignature),
            activeEndpoint == record.task.endpoint,
            activeServerID == record.task.serverID
        else {
            return false
        }

        return true
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

    public var canRetryTaskUpdates: Bool {
        !isWatching
            && watchErrorMessage != nil
            && currentTask?.shouldKeepWatchingBilibiliTask == true
            && activeEndpoint != nil
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

    public func retryTaskUpdates() {
        guard canRetryTaskUpdates,
            let currentTask,
            let activeEndpoint
        else {
            return
        }
        startWatching(
            taskID: currentTask.id,
            endpoint: activeEndpoint,
            sequence: operationSequence,
            reconcileBeforeSubscribe: true
        )
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

        return taskResults.first { $0.id == result.id }?.playbackURL
    }

    public func artifactURL(for artifact: BilibiliTaskArtifactPresentation) -> URL? {
        guard
            taskResults.contains(where: { result in
                result.artifacts.contains { $0.id == artifact.id }
            }), artifact.canOpenResource
        else {
            return nil
        }
        return artifact.resourceURL
    }

    public var canClear: Bool {
        currentTask != nil || errorMessage != nil || resolvedInput != nil || resolutionSession != nil
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
        isWaitingForCandidateSelection
            && !isSubmitting
            && !isResolving
            && !isCancelling
            && selectedCandidateCount > 0
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
        resolvedInputMatchesSource
            && resolutionTotalSize > 1
            && currentTask == nil
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
            return selectedCandidateToken == nil ? 0 : 1
        case .multiple:
            return orderedSelectedCandidateIDs.count
        case .range:
            return selectedRangeCandidateIDs.count
        case .all:
            return canSelectAllResolvedCandidates
                ? Int(resolutionTotalSize)
                : 0
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
            if count > Int(Self.maxExecutionCandidateCount) {
                return "Select no more than 100 Bilibili items."
            }
            return count == 1 ? "1 Bilibili item selected." : "\(count) Bilibili items selected."
        case .range:
            guard let start = rangeStartCandidate, let end = rangeEndCandidate else {
                return "Select a start and end item."
            }
            let count = selectedRangeCandidateIDs.count
            if let bounds = selectedRangeBounds,
                bounds.end - bounds.start + 1 > Int(Self.maxExecutionCandidateCount)
            {
                return "Select a range of no more than 100 Bilibili items."
            }
            let windowSuffix = isResolvedCandidateWindowTruncated ? " in this limited list" : ""
            return
                ("Range \(start.displayTitle) to \(end.displayTitle) selects \(count) item\(count == 1 ? "" : "s")\(windowSuffix).")
        case .all:
            guard canSelectAllResolvedCandidates else {
                return "All selection is available only for lists of up to 100 items."
            }
            let count = Int(resolutionTotalSize)
            if isResolvedCandidateWindowTruncated {
                return "All \(count) items in this limited list are selected."
            }
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
                return submissionMode == .download ? "Download Selected" : "Submit Selected"
            case .multiple:
                return submissionMode == .download ? "Download Multiple" : "Submit Multiple"
            case .range:
                return submissionMode == .download ? "Download Range" : "Submit Range"
            case .all:
                if isResolvedCandidateWindowTruncated {
                    return submissionMode == .download ? "Download Window" : "Submit Window"
                }
                return submissionMode == .download ? "Download All" : "Submit All"
            }
        }
        if submissionMode == .download {
            return "Download"
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
        currentTask.flatMap { task in
            Self.progressiveCacheStatusBadge(
                for: task,
                additionalFillStatuses: taskResultItems.compactMap(\.bilibiliCacheFillStatus)
            )
        }
    }

    public var progressiveCacheFillStatus: HlsCacheFillStatus? {
        guard let currentTask else {
            return nil
        }
        return Self.cacheFillStatus(
            for: currentTask,
            additionalFillStatuses: taskResultItems.compactMap(\.bilibiliCacheFillStatus)
        )
    }

    public var taskResults: [BilibiliTaskResultPresentation] {
        guard let currentTask else {
            return []
        }

        if let outputSummary = currentTask.outputSummary {
            guard let taskResultOutputRevision,
                taskResultOutputRevision >= outputSummary.revision
            else {
                return []
            }
            return taskResultItems.map { $0.bilibiliPresentation(mediaBaseURIs: mediaBaseURIs) }
        }

        return currentTask.bilibiliTaskResults
    }

    public var hasMoreResolvedCandidates: Bool {
        !resolutionPageToken.isEmpty && resolutionSession != nil
    }

    public var isResolvedCandidateWindowTruncated: Bool {
        resolutionSession?.candidatesTruncated ?? false
    }

    public var canRetryResolvedCandidatePage: Bool {
        errorMessage != nil && statusMessage == "Could not load more Bilibili candidates."
    }

    public var hasMoreTaskResults: Bool {
        guard !taskResultPageToken.isEmpty,
            let currentTask,
            let outputSummary = currentTask.outputSummary
        else {
            return false
        }

        return taskResultOutputRevision == outputSummary.revision
    }

    public var taskResultSummary: BilibiliTaskResultSummary? {
        currentTask?.bilibiliTaskResultSummary
    }

    public var activePlaybackPolicySummary: String? {
        guard let currentTask else {
            return nil
        }

        return Self.activePlaybackPolicySummary(for: currentTask)
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

        if isResolvedCandidateWindowTruncated {
            return BilibiliFetchNotice(
                title: "Limited list",
                message: "Only part of the upstream list is included in this result.",
                systemImage: "ellipsis",
                tone: .info,
                actionTitle: hasMoreResolvedCandidates ? "Load More" : nil
            )
        }

        if hasMoreResolvedCandidates || resolvedInput.candidatesTruncated {
            return BilibiliFetchNotice(
                title: "More items available",
                message: "Load more candidates from this frozen Bilibili result snapshot.",
                systemImage: "arrow.down.to.line",
                tone: .info,
                actionTitle: hasMoreResolvedCandidates ? "Load More" : nil
            )
        }

        if Self.isVolatileResolvedSourceKind(resolvedInput.sourceKind) {
            return BilibiliFetchNotice(
                title: "List may change",
                message:
                    "This Bilibili list or feed can reorder between refreshes. Selections use candidate tokens from this immutable resolution.",
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

    public var availableSubtitleAIPolicies: [BilibiliSubtitleAIPolicy] {
        BilibiliSubtitleAIPolicy.allCases
    }

    public var availableTranscodingPreferences: [BilibiliTranscodingPreference] {
        BilibiliTranscodingPreference.allCases
    }

    public var availableCompatibleVariantPreferences: [BilibiliCompatibleVariantPreference] {
        BilibiliCompatibleVariantPreference.allCases
    }

    public var availableWeakNetworkPreferences: [BilibiliWeakNetworkPreference] {
        BilibiliWeakNetworkPreference.allCases
    }

    public var availableDanmakuFormats: [BilibiliDanmakuFormat] {
        BilibiliDanmakuFormat.allCases
    }

    public func isDanmakuFormatSelected(_ format: BilibiliDanmakuFormat) -> Bool {
        danmakuFormats.contains(format)
    }

    public func setDanmakuFormat(_ format: BilibiliDanmakuFormat, selected: Bool) {
        if selected {
            downloadDanmaku = true
            danmakuFormats.insert(format)
        } else {
            danmakuFormats.remove(format)
        }
    }

    public var playableURL: URL? {
        currentTask?.playableBilibiliURL
    }

    public func playbackProgressContext(serverAddressText: String) -> PlayerPlaybackProgressContext? {
        guard let currentTask,
            let endpoint = playbackProgressEndpoint(serverAddressText: serverAddressText),
            let playbackURL = currentTask.playableBilibiliURL,
            let taskIdentity = currentResumptionTaskIdentity(endpoint: endpoint, taskID: currentTask.id)
        else {
            return nil
        }

        let playbackSource = currentTask.playableBilibiliPlaybackSource
        let resultID: String?
        if currentTask.topLevelPlayableBilibiliURL != nil {
            resultID = nil
        } else if let result = currentTask.bilibiliTaskResults.first(where: { $0.playbackURL == playbackURL }) {
            resultID = result.id
        } else {
            return nil
        }
        let libraryItemID = currentTask.playableBilibiliLibraryItemID ?? ""
        return PlayerPlaybackProgressContext(
            endpoint: endpoint,
            playbackURI: playbackURL.absoluteString,
            libraryItemID: currentTask.playableBilibiliLibraryItemID ?? playbackSource?.itemID ?? "",
            variantID: playbackSource?.variantID ?? "",
            resumptionIdentity: ClientPlaybackIdentity(
                task: taskIdentity,
                resultID: resultID,
                libraryItemID: libraryItemID,
                variantID: playbackSource?.variantID ?? ""
            )
        )
    }

    public func playbackProgressContext(
        for result: BilibiliTaskResultPresentation,
        serverAddressText: String
    ) -> PlayerPlaybackProgressContext? {
        guard let currentTask,
            let endpoint = playbackProgressEndpoint(serverAddressText: serverAddressText),
            let taskIdentity = currentResumptionTaskIdentity(endpoint: endpoint, taskID: currentTask.id),
            let resultItem = taskResults.first(where: { $0.id == result.id }),
            let playbackURL = resultItem.playbackURL
        else {
            return nil
        }

        return PlayerPlaybackProgressContext(
            endpoint: endpoint,
            playbackURI: playbackURL.absoluteString,
            libraryItemID: resultItem.playbackLibraryItemID,
            variantID: resultItem.playbackVariantID,
            resumptionIdentity: ClientPlaybackIdentity(
                task: taskIdentity,
                resultID: resultItem.id,
                libraryItemID: resultItem.playbackLibraryItemID,
                variantID: resultItem.playbackVariantID
            )
        )
    }

    private func currentResumptionTaskIdentity(
        endpoint: CacheServerEndpoint,
        taskID: String
    ) -> ClientResumptionTaskIdentity? {
        guard activeEndpoint == endpoint, let activeServerID else {
            return nil
        }
        return resumptionTaskIdentity(endpoint: endpoint, serverID: activeServerID, taskID: taskID)
    }

    private func resumptionTaskIdentity(
        endpoint: CacheServerEndpoint,
        serverID: String,
        taskID: String
    ) -> ClientResumptionTaskIdentity? {
        let trimmedServerID = serverID.trimmingCharacters(in: .whitespacesAndNewlines)
        let trimmedTaskID = taskID.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedServerID.isEmpty, !trimmedTaskID.isEmpty else {
            return nil
        }
        let identity = ClientResumptionTaskIdentity(
            endpoint: endpoint,
            serverID: trimmedServerID,
            taskID: trimmedTaskID
        )
        return identity.isValidForPersistence ? identity : nil
    }

    private func invalidateSavedPlaybackValidation(clearCached: Bool = false) {
        savedPlaybackValidationGeneration += 1
        savedPlaybackValidationTask?.cancel()
        savedPlaybackValidationTask = nil
        if clearCached {
            clearValidatedSavedPlayback()
            lastSavedPlaybackValidationTrigger = nil
        }
    }

    private func clearValidatedSavedPlayback() {
        validatedSavedPlayback = nil
        validatedSavedPlaybackSignature = nil
    }

    private func invalidateSavedRestore(message: String?) {
        let scopedIdentity = resumptionStore?.record?.task ?? restoreAttemptIdentity ?? restoredTaskIdentity
        restoreGeneration += 1
        invalidateSavedPlaybackValidation(clearCached: true)

        let belongsToSavedTask =
            scopedIdentity.map { identity in
                restoreAttemptIdentity == identity
                    || restoredTaskIdentity == identity
                    || (currentTask?.id == identity.taskID && activeEndpoint == identity.endpoint)
            } ?? false
        if belongsToSavedTask {
            operationSequence += 1
            stopWatching()
            stopTaskResultPaging()
            currentTask = nil
            activeEndpoint = nil
            activeServerID = nil
            mediaBaseURIs = []
            restoredTaskIdentity = nil
            activePlaybackTaskID = nil
            activePlaybackResultID = nil
            activePlaybackLibraryItemID = nil
            taskSnapshotGeneration += 1
        }
        restoreAttemptIdentity = nil
        isRestoringTask = false
        resumptionMessage = message
    }

    private func isSavedRestoreCurrent(
        _ identity: ClientResumptionTaskIdentity,
        generation: Int,
        sequence: Int
    ) -> Bool {
        restoreGeneration == generation
            && operationSequence == sequence
            && restoreAttemptIdentity == identity
            && resumptionStore?.record?.task == identity
            && isRestoringTask
            && !Task.isCancelled
    }

    public func restoreSavedTask(serverAddressText: String) async {
        await restoreSavedTask(serverAddressText: serverAddressText, force: false)
    }

    public func retrySavedTask(serverAddressText: String) async {
        await restoreSavedTask(serverAddressText: serverAddressText, force: true)
    }

    private func restoreSavedTask(serverAddressText: String, force: Bool) async {
        guard let record = resumptionStore?.record else {
            invalidateSavedRestore(message: nil)
            return
        }

        guard let requestedEndpoint = CacheServerEndpoint.normalized(from: serverAddressText),
            ClientResumptionTaskIdentity.canonicalEndpoint(requestedEndpoint) == record.task.endpoint
        else {
            invalidateSavedRestore(
                message:
                    "The saved task belongs to a different cache server. Restore it from its original endpoint, or forget it."
            )
            return
        }

        if !force {
            if isRestoringTask,
                restoreAttemptIdentity == record.task
            {
                return
            }
            if restoredTaskIdentity == record.task,
                currentTask?.id == record.task.taskID,
                activeEndpoint == record.task.endpoint,
                isWatching
            {
                return
            }
        }

        guard !isSubmitting, !isResolving, !isCancelling else {
            resumptionMessage = "Wait for the current task operation to finish before restoring the saved task."
            return
        }

        restoreGeneration += 1
        let generation = restoreGeneration
        operationSequence += 1
        let sequence = operationSequence
        invalidateSavedPlaybackValidation(clearCached: true)
        stopWatching()
        stopTaskResultPaging()
        activeEndpoint = nil
        activeServerID = nil
        mediaBaseURIs = []
        activePlaybackTaskID = nil
        activePlaybackResultID = nil
        activePlaybackLibraryItemID = nil
        restoredTaskIdentity = nil
        restoreAttemptIdentity = record.task
        currentTask = nil
        taskSnapshotGeneration += 1
        isSubmitting = false
        isResolving = false
        isCancelling = false
        errorMessage = nil
        retryIntent = nil
        clearResolutionSession()
        isRestoringTask = true
        resumptionMessage = "Checking the saved Bilibili task..."
        statusMessage = "Restoring saved Bilibili task..."

        defer {
            if restoreGeneration == generation {
                isRestoringTask = false
                restoreAttemptIdentity = nil
            }
        }

        let client = clientFactory(record.task.endpoint)
        do {
            let serverInfo = try await Self.withCancellableOperationTimeout(operationTimeout) {
                try await client.getServerInfo()
            }
            guard isSavedRestoreCurrent(record.task, generation: generation, sequence: sequence) else {
                return
            }
            let liveServerID = serverInfo.id.trimmingCharacters(in: .whitespacesAndNewlines)
            guard liveServerID == record.task.serverID else {
                throw SavedTaskResumptionIssue.serverIdentityChanged
            }
            let task = try await Self.withCancellableOperationTimeout(operationTimeout) {
                try await client.getTask(id: record.task.taskID)
            }
            guard isSavedRestoreCurrent(record.task, generation: generation, sequence: sequence) else {
                return
            }
            guard task.id == record.task.taskID else {
                throw SavedTaskResumptionIssue.taskIdentityChanged
            }
            guard task.kind.lowercased().contains("bilibili") else {
                throw SavedTaskResumptionIssue.notBilibiliTask
            }

            activeEndpoint = record.task.endpoint
            activeServerID = liveServerID
            mediaBaseURIs = serverInfo.mediaBaseURIs
            restoredTaskIdentity = record.task
            resumptionStore?.rememberTask(record.task)
            applyTaskUpdate(task)

            if let latestRecord = resumptionStore?.record,
                latestRecord.task == record.task,
                let checkpoint = latestRecord.playback
            {
                let restoreSourceSignature = savedPlaybackObservationSignature(for: task, checkpoint: checkpoint)
                rememberSavedPlaybackValidationAttempt(for: task, taskIdentity: record.task)
                do {
                    let prepared = try await prepareSavedPlayback(
                        record: latestRecord,
                        serverInfo: serverInfo,
                        task: task,
                        client: client
                    )
                    guard isSavedRestoreCurrent(record.task, generation: generation, sequence: sequence) else {
                        return
                    }
                    guard resumptionStore?.record?.playback == checkpoint else {
                        resumptionMessage =
                            "The saved playback checkpoint changed during restore. Retry the saved task."
                        return
                    }
                    guard let currentTask,
                        let restoreSourceSignature,
                        let currentSignature = savedPlaybackObservationSignature(
                            for: currentTask,
                            checkpoint: checkpoint
                        ),
                        restoreSourceSignature.hasSameSelectedSource(as: currentSignature)
                    else {
                        clearValidatedSavedPlayback()
                        resumptionMessage = "The selected saved source changed during restore. Retry the saved task."
                        return
                    }
                    validatedSavedPlayback = prepared
                    validatedSavedPlaybackSignature = currentSignature
                    rememberSavedPlaybackValidationAttempt(for: currentTask, taskIdentity: record.task)
                    resumptionMessage = nil
                } catch {
                    guard isSavedRestoreCurrent(record.task, generation: generation, sequence: sequence) else {
                        return
                    }
                    clearValidatedSavedPlayback()
                    resumptionMessage = SavedTaskResumptionIssue.message(for: error)
                }
            } else {
                resumptionMessage = "The saved task was restored. No playback checkpoint is available yet."
            }

            guard isSavedRestoreCurrent(record.task, generation: generation, sequence: sequence) else {
                return
            }
            if task.shouldKeepWatchingBilibiliTask {
                startWatching(
                    taskID: task.id,
                    endpoint: record.task.endpoint,
                    sequence: sequence,
                    reconcileBeforeSubscribe: true
                )
            }
        } catch {
            guard isSavedRestoreCurrent(record.task, generation: generation, sequence: sequence) else {
                return
            }
            resumptionMessage = SavedTaskResumptionIssue.message(for: error)
            statusMessage = "Could not restore the saved Bilibili task."
        }
    }

    public func forgetSavedTask() {
        clearTask()
        resumptionMessage = "The saved task was forgotten. Server work was not cancelled."
    }

    public func prepareSavedPlayback(serverAddressText: String) async -> ClientPreparedResumption? {
        guard let record = resumptionStore?.record, record.playback != nil else {
            resumptionMessage = "No saved playback checkpoint is available."
            return nil
        }
        guard let requestedEndpoint = CacheServerEndpoint.normalized(from: serverAddressText),
            ClientResumptionTaskIdentity.canonicalEndpoint(requestedEndpoint) == record.task.endpoint
        else {
            invalidateSavedRestore(
                message:
                    "The saved playback belongs to a different cache server. Restore it from its original endpoint, or forget it."
            )
            return nil
        }
        guard !isSubmitting, !isResolving, !isCancelling, !isRestoringTask,
            currentTask?.id == record.task.taskID,
            activeEndpoint == record.task.endpoint,
            activeServerID == record.task.serverID,
            restoredTaskIdentity == record.task
        else {
            resumptionMessage = "Restore the original saved task before continuing playback."
            return nil
        }

        savedPlaybackValidationGeneration += 1
        let validationGeneration = savedPlaybackValidationGeneration
        savedPlaybackValidationTask?.cancel()
        savedPlaybackValidationTask = nil
        return await runSavedPlaybackPreparation(record, validationGeneration: validationGeneration)
    }

    public func finishPreparedResumption(
        _ prepared: ClientPreparedResumption,
        didStartPlayback: Bool
    ) {
        guard didStartPlayback else {
            if let currentTask {
                statusMessage = Self.statusMessage(for: currentTask)
            }
            return
        }

        guard let record = resumptionStore?.record,
            let checkpoint = record.playback,
            checkpoint == prepared.checkpoint,
            checkpoint.identity.task == record.task,
            prepared.progressContext.resumptionIdentity == checkpoint.identity,
            prepared.progressContext.endpoint == record.task.endpoint,
            prepared.progressContext.playbackURI == prepared.url.absoluteString,
            validatedSavedPlayback == prepared,
            currentTask?.id == record.task.taskID,
            activeEndpoint == record.task.endpoint,
            activeServerID == record.task.serverID,
            restoredTaskIdentity == record.task,
            !isSubmitting, !isResolving, !isRestoringTask, !isCancelling
        else {
            resumptionMessage = "The saved task changed before playback started. Recheck it before continuing."
            return
        }

        errorMessage = nil
        activePlaybackTaskID = checkpoint.identity.task.taskID
        activePlaybackResultID = checkpoint.identity.resultID
        activePlaybackLibraryItemID = normalizedNonEmpty(prepared.progressContext.libraryItemID)
        statusMessage = "Playing \(currentTask?.bilibiliDisplayTitle ?? "Bilibili video")."
        resumptionMessage = nil
    }

    private func runSavedPlaybackPreparation(
        _ record: ClientResumptionRecord,
        validationGeneration: Int
    ) async -> ClientPreparedResumption? {
        guard let checkpoint = record.playback,
            let observedTask = currentTask,
            let initialSignature = savedPlaybackObservationSignature(for: observedTask, checkpoint: checkpoint)
        else {
            return nil
        }

        let restoreGeneration = self.restoreGeneration
        let operation = operationSequence
        isRestoringTask = true
        resumptionMessage = "Rechecking the saved playback source..."
        defer {
            if validationGeneration == savedPlaybackValidationGeneration,
                restoreGeneration == self.restoreGeneration
            {
                isRestoringTask = false
            }
        }

        let client = clientFactory(record.task.endpoint)
        do {
            let serverInfo = try await Self.withCancellableOperationTimeout(operationTimeout) {
                try await client.getServerInfo()
            }
            guard
                handleSavedPlaybackPreparationFence(
                    record,
                    validationGeneration: validationGeneration,
                    restoreGeneration: restoreGeneration,
                    operation: operation,
                    initialSignature: initialSignature,
                    checkpoint: checkpoint
                )
            else {
                return nil
            }

            let liveServerID = serverInfo.id.trimmingCharacters(in: .whitespacesAndNewlines)
            guard liveServerID == record.task.serverID else {
                throw SavedTaskResumptionIssue.serverIdentityChanged
            }
            let task = try await Self.withCancellableOperationTimeout(operationTimeout) {
                try await client.getTask(id: record.task.taskID)
            }
            guard
                handleSavedPlaybackPreparationFence(
                    record,
                    validationGeneration: validationGeneration,
                    restoreGeneration: restoreGeneration,
                    operation: operation,
                    initialSignature: initialSignature,
                    checkpoint: checkpoint
                )
            else {
                return nil
            }
            guard task.id == record.task.taskID else {
                throw SavedTaskResumptionIssue.taskIdentityChanged
            }

            let prepared = try await prepareSavedPlayback(
                record: record,
                serverInfo: serverInfo,
                task: task,
                client: client
            )
            guard
                handleSavedPlaybackPreparationFence(
                    record,
                    validationGeneration: validationGeneration,
                    restoreGeneration: restoreGeneration,
                    operation: operation,
                    initialSignature: initialSignature,
                    checkpoint: checkpoint
                )
            else {
                return nil
            }

            validatedSavedPlayback = prepared
            validatedSavedPlaybackSignature = initialSignature
            rememberSavedPlaybackValidationAttempt(for: observedTask, taskIdentity: record.task)
            resumptionMessage = nil
            return prepared
        } catch {
            guard
                isSavedPlaybackPreparationOperationCurrent(
                    record,
                    validationGeneration: validationGeneration,
                    restoreGeneration: restoreGeneration,
                    operation: operation
                )
            else {
                return nil
            }
            if let issue = error as? SavedTaskResumptionIssue, case .serverIdentityChanged = issue {
                invalidateSavedRestore(message: SavedTaskResumptionIssue.message(for: issue))
                statusMessage = "The saved Bilibili task is unavailable on this cache server."
                return nil
            }
            if let currentTask,
                let currentSignature = savedPlaybackObservationSignature(for: currentTask, checkpoint: checkpoint),
                !initialSignature.hasSameSelectedSource(as: currentSignature)
            {
                clearValidatedSavedPlayback()
                resumptionMessage =
                    "The selected saved source changed during validation. Retry the saved task before continuing."
            } else {
                clearValidatedSavedPlayback()
                resumptionMessage = SavedTaskResumptionIssue.message(for: error)
            }
            return nil
        }
    }

    private func handleSavedPlaybackPreparationFence(
        _ record: ClientResumptionRecord,
        validationGeneration: Int,
        restoreGeneration: Int,
        operation: Int,
        initialSignature: SavedPlaybackObservationSignature,
        checkpoint: ClientPlaybackCheckpoint
    ) -> Bool {
        switch savedPlaybackPreparationFence(
            record,
            validationGeneration: validationGeneration,
            restoreGeneration: restoreGeneration,
            operation: operation,
            initialSignature: initialSignature,
            checkpoint: checkpoint
        ) {
        case .current:
            return true
        case .obsolete:
            return false
        case .selectedSourceChanged:
            clearValidatedSavedPlayback()
            resumptionMessage =
                "The selected saved source changed during validation. Retry the saved task before continuing."
            return false
        }
    }

    private func savedPlaybackPreparationFence(
        _ record: ClientResumptionRecord,
        validationGeneration: Int,
        restoreGeneration: Int,
        operation: Int,
        initialSignature: SavedPlaybackObservationSignature,
        checkpoint: ClientPlaybackCheckpoint
    ) -> SavedPlaybackPreparationFence {
        guard
            isSavedPlaybackPreparationOperationCurrent(
                record,
                validationGeneration: validationGeneration,
                restoreGeneration: restoreGeneration,
                operation: operation
            ), let currentTask,
            let currentSignature = savedPlaybackObservationSignature(for: currentTask, checkpoint: checkpoint)
        else {
            return .obsolete
        }
        return initialSignature.hasSameSelectedSource(as: currentSignature) ? .current : .selectedSourceChanged
    }

    private func isSavedPlaybackPreparationOperationCurrent(
        _ record: ClientResumptionRecord,
        validationGeneration: Int,
        restoreGeneration: Int,
        operation: Int
    ) -> Bool {
        validationGeneration == savedPlaybackValidationGeneration
            && restoreGeneration == self.restoreGeneration
            && operation == operationSequence
            && !Task.isCancelled
            && resumptionStore?.record == record
            && currentTask?.id == record.task.taskID
            && activeEndpoint == record.task.endpoint
            && activeServerID == record.task.serverID
            && restoredTaskIdentity == record.task
    }

    private func savedPlaybackObservationSignature(
        for task: CacheTask,
        checkpoint: ClientPlaybackCheckpoint
    ) -> SavedPlaybackObservationSignature? {
        guard task.id == checkpoint.identity.task.taskID else {
            return nil
        }
        if let resultID = checkpoint.identity.resultID {
            guard let item = task.resultItems.first(where: { $0.id == resultID }) else {
                return SavedPlaybackObservationSignature(
                    taskID: task.id,
                    resultID: resultID,
                    libraryItemID: "",
                    outputRevision: task.outputSummary?.revision,
                    isPlayable: false,
                    source: nil
                )
            }
            return SavedPlaybackObservationSignature(
                taskID: task.id,
                resultID: resultID,
                libraryItemID: item.libraryItemID,
                outputRevision: task.outputSummary?.revision,
                isPlayable: item.playableBilibiliURL != nil,
                source: item.playbackSource.map {
                    SavedPlaybackSourceSignature(
                        itemID: $0.itemID,
                        variantID: $0.variantID,
                        playbackProtocol: $0.playbackProtocol,
                        uri: $0.uri
                    )
                }
            )
        }
        return SavedPlaybackObservationSignature(
            taskID: task.id,
            resultID: nil,
            libraryItemID: task.libraryItemID,
            outputRevision: task.outputSummary?.revision,
            isPlayable: task.playableBilibiliURL != nil,
            source: task.playbackSource.map {
                SavedPlaybackSourceSignature(
                    itemID: $0.itemID,
                    variantID: $0.variantID,
                    playbackProtocol: $0.playbackProtocol,
                    uri: $0.uri
                )
            }
        )
    }

    private func rememberSavedPlaybackValidationAttempt(
        for task: CacheTask,
        taskIdentity: ClientResumptionTaskIdentity
    ) {
        guard let checkpoint = resumptionStore?.record?.playback,
            checkpoint.identity.task == taskIdentity,
            let signature = savedPlaybackObservationSignature(for: task, checkpoint: checkpoint)
        else {
            return
        }
        lastSavedPlaybackValidationTrigger = SavedPlaybackValidationTrigger(
            identity: checkpoint.identity,
            signature: signature
        )
    }

    private func scheduleSavedPlaybackValidationIfNeeded(for task: CacheTask) {
        guard let record = resumptionStore?.record,
            let checkpoint = record.playback,
            record.task == checkpoint.identity.task,
            restoredTaskIdentity == record.task,
            task.id == record.task.taskID,
            let signature = savedPlaybackObservationSignature(for: task, checkpoint: checkpoint)
        else {
            return
        }
        if let validatedSavedPlayback,
            validatedSavedPlayback.checkpoint.identity == checkpoint.identity,
            validatedSavedPlayback.progressContext.resumptionIdentity == checkpoint.identity,
            let validatedSavedPlaybackSignature,
            validatedSavedPlaybackSignature.hasSameSelectedSource(as: signature)
        {
            return
        }
        clearValidatedSavedPlayback()
        guard !isRestoringTask, !isSubmitting, !isResolving, !isCancelling else {
            return
        }
        let trigger = SavedPlaybackValidationTrigger(identity: checkpoint.identity, signature: signature)
        guard lastSavedPlaybackValidationTrigger?.representsSameSelectedSource(as: trigger) != true else {
            return
        }

        lastSavedPlaybackValidationTrigger = trigger
        savedPlaybackValidationTask?.cancel()
        savedPlaybackValidationGeneration += 1
        let generation = savedPlaybackValidationGeneration
        savedPlaybackValidationTask = Task { [weak self] in
            guard let self else { return }
            _ = await self.runSavedPlaybackPreparation(record, validationGeneration: generation)
            if self.savedPlaybackValidationGeneration == generation {
                self.savedPlaybackValidationTask = nil
            }
        }
    }

    private func prepareSavedPlayback(
        record: ClientResumptionRecord,
        serverInfo: CacheServerSummary,
        task: CacheTask,
        client: any CacheControlClient
    ) async throws -> ClientPreparedResumption {
        guard let checkpoint = record.playback,
            checkpoint.identity.task == record.task,
            task.id == record.task.taskID,
            task.kind.lowercased().contains("bilibili"),
            task.isProgressivePlayback,
            task.isPlayableBilibiliTaskState,
            !task.isCancellationPendingBilibiliTaskState
        else {
            throw SavedTaskResumptionIssue.playbackUnavailable
        }

        let exactResultID = checkpoint.identity.resultID
        var embeddedSource: CachePlaybackSource?
        var sourceItemID: String
        var currentLibraryItemID = ""

        if let exactResultID {
            let result = try await savedResult(
                id: exactResultID,
                task: task,
                client: client
            )
            let state = result.state.lowercased().filter(\.isLetter)
            let isCompleted = state.contains("completed") || state.contains("succeeded")
            let isPlayable =
                state.contains("playable") || isCompleted
                || (state.contains("failed")
                    && result.problem?.message.localizedCaseInsensitiveContains("offline cache fill failed") == true)
            guard isPlayable else {
                throw SavedTaskResumptionIssue.playbackUnavailable
            }

            embeddedSource = result.playbackSource
            currentLibraryItemID = normalizedNonEmpty(result.libraryItemID) ?? ""
            if currentLibraryItemID.isEmpty,
                let embeddedSource,
                embeddedSource.itemID != exactResultID
            {
                currentLibraryItemID = embeddedSource.itemID
            }
            sourceItemID = currentLibraryItemID.isEmpty ? exactResultID : currentLibraryItemID
            if let embeddedSource, embeddedSource.itemID != sourceItemID {
                throw SavedTaskResumptionIssue.libraryIdentityChanged
            }
        } else {
            guard task.topLevelPlayableBilibiliURL != nil else {
                throw SavedTaskResumptionIssue.playbackUnavailable
            }
            embeddedSource = task.playbackSource
            currentLibraryItemID = normalizedNonEmpty(task.libraryItemID) ?? ""
            if currentLibraryItemID.isEmpty,
                let embeddedSource,
                embeddedSource.itemID != task.id
            {
                currentLibraryItemID = embeddedSource.itemID
            }
            sourceItemID = currentLibraryItemID.isEmpty ? task.id : currentLibraryItemID
            if let embeddedSource, embeddedSource.itemID != sourceItemID {
                throw SavedTaskResumptionIssue.libraryIdentityChanged
            }
        }

        let savedLibraryItemID = checkpoint.identity.libraryItemID
        if !savedLibraryItemID.isEmpty, savedLibraryItemID != currentLibraryItemID {
            throw SavedTaskResumptionIssue.libraryIdentityChanged
        }

        let source: CachePlaybackSource
        if !currentLibraryItemID.isEmpty {
            let requestedVariantID =
                checkpoint.identity.variantID.isEmpty
                ? (embeddedSource?.variantID ?? "")
                : checkpoint.identity.variantID
            let requestedLibraryItemID = currentLibraryItemID
            let freshSource = try await Self.withCancellableOperationTimeout(operationTimeout) {
                try await client.getPlaybackSource(
                    itemID: requestedLibraryItemID,
                    variantID: requestedVariantID
                )
            }
            guard freshSource.itemID == currentLibraryItemID else {
                throw SavedTaskResumptionIssue.libraryIdentityChanged
            }
            source = freshSource
        } else if let embeddedSource {
            source = embeddedSource
        } else {
            throw SavedTaskResumptionIssue.playbackUnavailable
        }

        guard source.itemID == sourceItemID,
            checkpoint.identity.variantID.isEmpty || source.variantID == checkpoint.identity.variantID,
            let url = TVOSNetPlayerCore.playableURL(for: source, expectedItemID: sourceItemID),
            let safeURL = serverOwnedMediaURL(url.absoluteString, mediaBaseURIs: serverInfo.mediaBaseURIs)
        else {
            throw SavedTaskResumptionIssue.invalidPlaybackSource
        }

        let progressContext = PlayerPlaybackProgressContext(
            endpoint: record.task.endpoint,
            playbackURI: safeURL.absoluteString,
            libraryItemID: currentLibraryItemID,
            variantID: source.variantID,
            resumptionIdentity: checkpoint.identity
        )
        return ClientPreparedResumption(url: safeURL, progressContext: progressContext, checkpoint: checkpoint)
    }

    private func savedResult(
        id resultID: String,
        task: CacheTask,
        client: any CacheControlClient
    ) async throws -> CacheTaskResult {
        if let summary = task.outputSummary {
            guard summary.resultCount > 0,
                summary.resultCount <= 100
            else {
                throw SavedTaskResumptionIssue.resultLimitExceeded
            }

            var pageToken = ""
            var visitedTokens: Set<String> = []
            var visitedResultIDs: Set<String> = []
            var snapshotID: String?
            var totalSize: UInt64?
            var outputRevision: UInt64?
            var scannedCount = 0
            var pageCount = 0
            var exactResult: CacheTaskResult?
            repeat {
                guard pageCount < 100, visitedTokens.insert(pageToken).inserted else {
                    throw SavedTaskResumptionIssue.incoherentResultSnapshot
                }
                let requestedPageToken = pageToken
                let page = try await Self.withCancellableOperationTimeout(operationTimeout) {
                    try await client.listTaskResults(
                        taskID: task.id,
                        pageToken: requestedPageToken,
                        pageSize: Self.taskResultPageSize
                    )
                }
                pageCount += 1
                guard !page.snapshotID.isEmpty,
                    page.outputRevision == summary.revision,
                    page.totalSize == summary.resultCount,
                    page.results.count <= Self.taskResultPageSize,
                    scannedCount + page.results.count <= 100
                else {
                    throw SavedTaskResumptionIssue.incoherentResultSnapshot
                }
                if let snapshotID {
                    guard snapshotID == page.snapshotID,
                        totalSize == page.totalSize,
                        outputRevision == page.outputRevision
                    else {
                        throw SavedTaskResumptionIssue.incoherentResultSnapshot
                    }
                } else {
                    snapshotID = page.snapshotID
                    totalSize = page.totalSize
                    outputRevision = page.outputRevision
                }
                for result in page.results {
                    guard visitedResultIDs.insert(result.id).inserted else {
                        throw SavedTaskResumptionIssue.duplicateResultID
                    }
                    if result.id == resultID {
                        exactResult = result
                    }
                }
                scannedCount += page.results.count
                pageToken = page.nextPageToken
            } while !pageToken.isEmpty

            guard scannedCount == summary.resultCount, let exactResult else {
                throw SavedTaskResumptionIssue.resultUnavailable
            }
            return exactResult
        }

        guard task.resultItems.count <= 100 else {
            throw SavedTaskResumptionIssue.resultLimitExceeded
        }
        var seenIDs: Set<String> = []
        for item in task.resultItems {
            guard seenIDs.insert(item.id).inserted else {
                throw SavedTaskResumptionIssue.duplicateResultID
            }
        }
        guard let item = task.resultItems.first(where: { $0.id == resultID }) else {
            throw SavedTaskResumptionIssue.resultUnavailable
        }
        return CacheTaskResult(
            id: item.id,
            state: item.state,
            title: item.title,
            subtitle: item.subtitle,
            progress: nil,
            problem: item.message.isEmpty
                ? nil
                : CacheTaskProblem(category: "", code: "", message: item.message, retryable: false),
            libraryItemID: item.libraryItemID,
            playbackSource: item.playbackSource,
            artifacts: [],
            createdAt: nil,
            updatedAt: nil
        )
    }

    private func playbackProgressEndpoint(serverAddressText: String) -> CacheServerEndpoint? {
        activeEndpoint ?? CacheServerEndpoint.normalized(from: serverAddressText)
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
        let submittedMode = submissionMode
        retryIntent = nil

        guard let endpoint = CacheServerEndpoint.normalized(from: serverAddressText) else {
            errorMessage = Self.cacheServerAddressGuidance
            statusMessage = "Cache server address is invalid."
            return
        }

        let source = sourceText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !source.isEmpty else {
            errorMessage = "Enter a Bilibili URL, BV, av, season, feed, history, or watch-later input."
            statusMessage = "Bilibili input is required."
            return
        }

        let options = currentResolutionOptions
        let playbackSpec: BilibiliPlaybackSpec?
        let downloadSpec: BilibiliDownloadSpec?
        do {
            playbackSpec = submittedMode == .playback ? try Self.playbackSpec(from: options) : nil
            downloadSpec = submittedMode == .download ? try Self.downloadSpec(from: currentDownloadOptions) : nil
        } catch {
            errorMessage = Self.userFacingMessage(for: error)
            statusMessage = "Bilibili preferences are invalid."
            return
        }

        if currentTask == nil,
            resolvedInputMatches(source: source, endpoint: endpoint, options: options),
            cachedResolvedPlaybackRequest != nil
        {
            guard let selectionRequest = cachedResolvedPlaybackRequest,
                let resolutionSession
            else {
                errorMessage =
                    submittedMode == .download
                    ? "Select Bilibili items before downloading."
                    : "Select Bilibili items before submitting playback."
                statusMessage = "Bilibili item selection is required."
                return
            }
            let execution: BilibiliTaskExecution
            switch submittedMode {
            case .playback:
                guard let playbackSpec else { return }
                execution = .playback(playbackSpec)
            case .download:
                guard let downloadSpec else { return }
                execution = .download(downloadSpec)
            }
            await createTaskV2(
                sessionID: resolutionSession.id,
                selection: selectionRequest.selection,
                execution: execution,
                endpoint: endpoint,
                sequence: operationSequence,
                client: clientFactory(endpoint),
                isPlayback: submittedMode == .playback
            )
            return
        }

        operationSequence += 1
        invalidateSavedPlaybackValidation(clearCached: true)
        activePlaybackTaskID = nil
        activePlaybackResultID = nil
        activePlaybackLibraryItemID = nil
        restoredTaskIdentity = nil
        restoreAttemptIdentity = nil
        let sequence = operationSequence

        stopWatching()
        stopTaskResultPaging()
        activeEndpoint = endpoint
        activeServerID = nil
        mediaBaseURIs = []
        currentTask = nil
        clearResolutionSession()
        isSubmitting = true
        isResolving = true
        errorMessage = nil
        statusMessage = "Resolving Bilibili input..."

        let client = clientFactory(endpoint)

        do {
            let (page, serverInfo) = try await Self.startBilibiliResolution(
                client: client,
                source: source,
                options: options,
                operationTimeout: operationTimeout,
                resolutionTimeout: resolutionTimeout
            )

            guard submissionMode == submittedMode else {
                if operationSequence == sequence + 1 {
                    discardStaleResolveSubmission(
                        statusMessage: "Bilibili submission mode changed before resolve completed."
                    )
                }
                return
            }
            guard sequence == operationSequence else {
                return
            }

            guard currentSubmissionMatches(source: source, options: options) else {
                discardStaleResolveSubmission()
                return
            }

            mediaBaseURIs = serverInfo.mediaBaseURIs
            activeServerID = serverInfo.id.trimmingCharacters(in: .whitespacesAndNewlines)
            installResolutionPage(
                page,
                source: source,
                endpoint: endpoint,
                options: options,
                resetSelection: true
            )
            isResolving = false

            guard page.totalSize > 0,
                !page.session.defaultCandidateToken.isEmpty || !page.candidates.isEmpty
            else {
                isSubmitting = false
                errorMessage = "Bilibili input did not resolve to a playable item."
                statusMessage = "No selectable Bilibili item was found."
                return
            }

            if page.totalSize > 1 {
                isSubmitting = false
                statusMessage =
                    submittedMode == .download
                    ? "Select Bilibili items to download."
                    : "Select a Bilibili item to play."
                return
            }

            let execution: BilibiliTaskExecution
            switch submittedMode {
            case .playback:
                guard let playbackSpec else { return }
                execution = .playback(playbackSpec)
            case .download:
                guard let downloadSpec else { return }
                execution = .download(downloadSpec)
            }
            guard
                let defaultCandidateToken = page.session.defaultCandidateToken.nilIfEmpty
                    ?? page.candidates.first?.candidateToken
            else {
                isSubmitting = false
                errorMessage = "Bilibili input did not resolve to a playable item."
                statusMessage = "No selectable Bilibili item was found."
                return
            }
            await createTaskV2(
                sessionID: page.session.id,
                selection: .single(candidateToken: defaultCandidateToken),
                execution: execution,
                endpoint: endpoint,
                sequence: sequence,
                client: client,
                isPlayback: submittedMode == .playback
            )
        } catch {
            guard submissionMode == submittedMode else {
                if operationSequence == sequence + 1 {
                    discardStaleResolveSubmission(
                        statusMessage: "Bilibili submission mode changed before resolve completed."
                    )
                }
                return
            }
            guard sequence == operationSequence else {
                return
            }
            guard currentSubmissionMatches(source: source, options: options) else {
                discardStaleResolveSubmission()
                return
            }

            currentTask = nil
            clearResolutionSession()
            errorMessage = Self.userFacingMessage(for: error)
            statusMessage =
                submittedMode == .download
                ? "Could not resolve Bilibili download input."
                : "Could not resolve Bilibili input."
            isResolving = false
            isSubmitting = false
        }
    }

    public func retry(serverAddressText: String) async {
        if retryIntent == .reResolve, canReResolve {
            await reResolve(serverAddressText: serverAddressText)
            return
        }

        retryIntent = nil
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
        retryIntent = nil

        guard let endpoint = CacheServerEndpoint.normalized(from: serverAddressText) else {
            retryIntent = .reResolve
            errorMessage = Self.cacheServerAddressGuidance
            statusMessage = "Cache server address is invalid."
            return
        }

        let source = sourceText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !source.isEmpty else {
            retryIntent = .reResolve
            errorMessage = "Enter a Bilibili URL, BV, av, season, feed, history, or watch-later input."
            statusMessage = "Bilibili input is required."
            return
        }

        let options = currentResolutionOptions

        operationSequence += 1
        activePlaybackTaskID = nil
        activePlaybackResultID = nil
        candidatePageSequence += 1
        isLoadingMoreCandidates = false
        let sequence = operationSequence

        stopWatching()
        stopTaskResultPaging()
        activeEndpoint = endpoint
        activeServerID = nil
        mediaBaseURIs = []
        isResolving = true
        isSubmitting = false
        errorMessage = nil
        statusMessage = "Resolving Bilibili input..."

        let client = clientFactory(endpoint)

        do {
            if submissionMode == .playback {
                _ = try Self.playbackSpec(from: options)
            }
            let (page, serverInfo) = try await Self.startBilibiliResolution(
                client: client,
                source: source,
                options: options,
                operationTimeout: operationTimeout,
                resolutionTimeout: resolutionTimeout
            )

            guard sequence == operationSequence else {
                return
            }

            guard currentSubmissionMatches(source: source, options: options) else {
                discardStaleResolveSubmission()
                return
            }

            activeEndpoint = endpoint
            activeServerID = serverInfo.id.trimmingCharacters(in: .whitespacesAndNewlines)
            mediaBaseURIs = serverInfo.mediaBaseURIs
            installResolutionPage(
                page,
                source: source,
                endpoint: endpoint,
                options: options,
                resetSelection: true
            )
            isResolving = false

            if page.totalSize == 0 {
                statusMessage = "No selectable Bilibili item was found."
            } else if page.totalSize > 1 {
                statusMessage = "Select a Bilibili item to play."
            } else {
                statusMessage = "Bilibili input resolved."
            }
        } catch {
            guard sequence == operationSequence else {
                return
            }

            guard currentSubmissionMatches(source: source, options: options) else {
                discardStaleResolveSubmission()
                return
            }

            currentTask = nil
            errorMessage = Self.userFacingMessage(for: error)
            statusMessage = "Could not resolve Bilibili input."
            retryIntent = .reResolve
            isResolving = false
            isSubmitting = false
        }
    }

    public func loadMoreResolvedCandidatesIfNeeded(
        appearingCandidateID: String,
        serverAddressText: String
    ) async {
        let candidates = resolvedCandidates
        guard !Task.isCancelled,
            hasMoreResolvedCandidates,
            !isLoadingMoreCandidates,
            let candidateIndex = candidates.firstIndex(where: { $0.selectionID == appearingCandidateID }),
            candidateIndex >= max(0, candidates.count - 3),
            !candidateAutoAttemptedPageTokens.contains(resolutionPageToken)
        else {
            return
        }

        candidateAutoAttemptedPageTokens.insert(resolutionPageToken)
        await loadMoreResolvedCandidatesPage(serverAddressText: serverAddressText)
    }

    public func loadMoreResolvedCandidates(serverAddressText: String) async {
        if !isLoadingMoreCandidates {
            candidateAutoAttemptedPageTokens.remove(resolutionPageToken)
        }
        await loadMoreResolvedCandidatesPage(serverAddressText: serverAddressText)
    }

    private func loadMoreResolvedCandidatesPage(serverAddressText: String) async {
        guard !isLoadingMoreCandidates,
            hasMoreResolvedCandidates,
            let session = resolutionSession,
            !Task.isCancelled,
            let endpoint = activeEndpoint ?? CacheServerEndpoint.normalized(from: serverAddressText)
        else {
            return
        }

        candidatePageSequence += 1
        let pageSequence = candidatePageSequence
        let operation = operationSequence
        let pageToken = resolutionPageToken
        let snapshotID = resolutionSnapshotID
        isLoadingMoreCandidates = true
        errorMessage = nil
        var wasCancelled = false

        defer {
            let isCurrentPage =
                operation == operationSequence
                && candidatePageSequence == pageSequence
                && resolutionSession?.id == session.id
            if isCurrentPage {
                isLoadingMoreCandidates = false
                if wasCancelled && resolutionPageToken == pageToken {
                    candidateAutoAttemptedPageTokens.remove(pageToken)
                }
            }
        }

        do {
            let client = clientFactory(endpoint)
            let page = try await Self.withOperationTimeout(operationTimeout) {
                try await client.listBilibiliResolutionCandidates(
                    sessionID: session.id,
                    pageToken: pageToken,
                    pageSize: Self.resolutionPageSize
                )
            }

            guard !Task.isCancelled,
                operation == operationSequence,
                pageSequence == candidatePageSequence,
                resolutionSession?.id == session.id
            else {
                wasCancelled = Task.isCancelled
                return
            }

            guard page.session.id == session.id,
                page.session.candidatesTruncated == session.candidatesTruncated,
                page.snapshotID == snapshotID
            else {
                invalidateResolutionForExpiry()
                return
            }

            appendResolutionPage(page)
            errorMessage = nil
        } catch {
            if Task.isCancelled || error is CancellationError {
                wasCancelled = true
                return
            }
            guard operation == operationSequence,
                pageSequence == candidatePageSequence,
                resolutionSession?.id == session.id
            else {
                return
            }

            if Self.isResolutionPageExpired(error) {
                invalidateResolutionForExpiry()
            } else {
                errorMessage = Self.userFacingMessage(for: error)
                statusMessage = "Could not load more Bilibili candidates."
            }
        }
    }

    public func loadMoreTaskResults(serverAddressText: String) async {
        guard !isLoadingMoreTaskResults,
            hasMoreTaskResults,
            let task = currentTask,
            let expectedRevision = taskResultOutputRevision,
            let endpoint = activeEndpoint ?? CacheServerEndpoint.normalized(from: serverAddressText)
        else {
            return
        }

        taskResultPageSequence += 1
        let pageSequence = taskResultPageSequence
        let operation = operationSequence
        let snapshotID = taskResultSnapshotID
        let pageToken = taskResultPageToken
        isLoadingMoreTaskResults = true
        taskResultsErrorMessage = nil

        defer {
            if taskResultPageSequence == pageSequence {
                isLoadingMoreTaskResults = false
            }
        }

        do {
            let client = clientFactory(endpoint)
            let page = try await Self.withOperationTimeout(operationTimeout) {
                try await client.listTaskResults(
                    taskID: task.id,
                    pageToken: pageToken,
                    pageSize: Self.taskResultPageSize
                )
            }

            guard operation == operationSequence,
                pageSequence == taskResultPageSequence,
                currentTask?.id == task.id
            else {
                return
            }

            guard page.outputRevision == expectedRevision,
                page.snapshotID == snapshotID,
                currentTask?.outputSummary?.revision == expectedRevision
            else {
                refreshTaskResults(for: currentTask, force: true)
                return
            }

            let existingIDs = Set(taskResultItems.map(\.id))
            guard page.results.allSatisfy({ !existingIDs.contains($0.id) }) else {
                refreshTaskResults(for: task, force: true)
                return
            }

            taskResultItems.append(contentsOf: page.results)
            taskResultPageToken = page.nextPageToken
            taskResultsErrorMessage = nil
        } catch {
            guard operation == operationSequence,
                pageSequence == taskResultPageSequence,
                currentTask?.id == task.id
            else {
                return
            }

            if Self.isTaskResultPageExpired(error) {
                taskResultPageToken = ""
                refreshTaskResults(for: currentTask, force: true)
            } else {
                taskResultsErrorMessage = Self.userFacingMessage(for: error)
            }
        }
    }

    public func retryTaskResults(serverAddressText: String) async {
        guard !isLoadingMoreTaskResults,
            let task = currentTask,
            task.outputSummary != nil
        else {
            return
        }
        guard let endpoint = activeEndpoint ?? CacheServerEndpoint.normalized(from: serverAddressText) else {
            taskResultsErrorMessage = Self.cacheServerAddressGuidance
            return
        }

        activeEndpoint = endpoint
        refreshTaskResults(for: task, force: true)
        if let refreshTask = taskResultRefreshTask {
            await refreshTask.value
        }
    }

    public func clearResolvedCandidateSelection() {
        guard canClearCandidateSelection else {
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
            errorMessage = Self.cacheServerAddressGuidance
            statusMessage = "Cache server address is invalid."
            return
        }

        let targetTaskID = currentTask.id
        if activePlaybackTaskID == targetTaskID {
            activePlaybackTaskID = nil
            activePlaybackResultID = nil
            activePlaybackLibraryItemID = nil
        }
        taskSnapshotGeneration += 1
        let cancellationGeneration = taskSnapshotGeneration
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

            if let currentTask = self.currentTask, currentTask.isTerminalBilibiliTaskState {
                applyTaskUpdate(currentTask)
                isCancelling = false
                return
            }

            guard task.id == targetTaskID else {
                statusMessage = "Could not confirm cancellation for \(currentTask.bilibiliDisplayTitle)."
                errorMessage = Self.cancelUnconfirmedMessage
                isCancelling = false
                return
            }

            if let currentTask = self.currentTask,
                currentTask.isCancellationPendingBilibiliTaskState,
                !task.isTerminalBilibiliTaskState
            {
                applyTaskUpdate(currentTask)
                isCancelling = false
                return
            }

            if taskSnapshotGeneration != cancellationGeneration,
                task.isQueuedOrRunningBilibiliTaskState
            {
                errorMessage = Self.cancelUnconfirmedMessage
                isCancelling = false
                return
            }

            applyTaskUpdate(task)
            isCancelling = false
        } catch {
            guard sequence == operationSequence, self.currentTask?.id == targetTaskID else {
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

            errorMessage = Self.cancelUnconfirmedMessage
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
            let currentResult = taskResults.first(where: { $0.id == result.id })
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
        clearTaskState(
            forgetSavedRecord: true,
            statusMessage: "No Bilibili playback task submitted.",
            resumptionMessage: nil
        )
    }

    private func clearTaskState(
        forgetSavedRecord: Bool,
        statusMessage: String,
        resumptionMessage: String?
    ) {
        operationSequence += 1
        restoreGeneration += 1
        invalidateSavedPlaybackValidation(clearCached: true)
        activeEndpoint = nil
        activeServerID = nil
        mediaBaseURIs = []
        activePlaybackTaskID = nil
        activePlaybackResultID = nil
        activePlaybackLibraryItemID = nil
        restoredTaskIdentity = nil
        restoreAttemptIdentity = nil
        retryIntent = nil
        stopTaskResultPaging()
        currentTask = nil
        errorMessage = nil
        isSubmitting = false
        isResolving = false
        isCancelling = false
        isRestoringTask = false
        clearResolutionSession()
        stopWatching()
        if forgetSavedRecord {
            resumptionStore?.forget()
        }
        self.statusMessage = statusMessage
        self.resumptionMessage = resumptionMessage
    }

    @discardableResult
    public func clearTaskIfCachedLibraryItemDeleted(id libraryItemID: String) -> Bool {
        let trimmedLibraryItemID = libraryItemID.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let currentTask, !trimmedLibraryItemID.isEmpty else {
            return false
        }

        let savedRecord = resumptionStore?.record
        let savedCheckpoint = savedRecord?.playback
        let savedTaskIsCurrent =
            savedRecord?.task.taskID == currentTask.id
            && savedRecord?.task.endpoint == activeEndpoint
        let deletedSelectedSource =
            savedTaskIsCurrent
            && savedCheckpoint.map { checkpoint in
                checkpoint.identity.libraryItemID == trimmedLibraryItemID
                    || validatedSavedPlayback?.progressContext.libraryItemID == trimmedLibraryItemID
            } == true
        guard
            currentTask.hasBilibiliLibraryItem(id: trimmedLibraryItemID)
                || taskResultItems.contains(where: { $0.hasBilibiliLibraryItem(id: trimmedLibraryItemID) })
                || deletedSelectedSource
        else {
            return false
        }

        if deletedSelectedSource {
            invalidateSavedPlaybackValidation(clearCached: true)
            resumptionMessage =
                "The saved playback cache was deleted. Retry the saved task to recheck its source, or forget it."
        }
        if deletedSelectedSource
            || (activePlaybackTaskID == currentTask.id && activePlaybackLibraryItemID == trimmedLibraryItemID)
        {
            activePlaybackTaskID = nil
            activePlaybackResultID = nil
            activePlaybackLibraryItemID = nil
        }

        guard !currentTask.hasTopLevelBilibiliLibraryItem(id: trimmedLibraryItemID) else {
            clearTaskState(
                forgetSavedRecord: false,
                statusMessage: savedTaskIsCurrent
                    ? "The cached Bilibili source was deleted."
                    : "No Bilibili playback task submitted.",
                resumptionMessage: deletedSelectedSource
                    ? "The saved playback cache was deleted. Retry the saved task to recheck its source, or forget it."
                    : resumptionMessage
            )
            return true
        }

        if let updatedTask = currentTask.clearingBilibiliResultLibraryItem(id: trimmedLibraryItemID) {
            self.currentTask = updatedTask
        }

        taskSnapshotGeneration += 1
        taskResultItems = taskResultItems.map { result in
            result.hasBilibiliLibraryItem(id: trimmedLibraryItemID)
                ? result.clearingDeletedBilibiliLibraryItem()
                : result
        }
        if let updatedTask = self.currentTask {
            statusMessage = Self.statusMessage(for: updatedTask)
        }
        errorMessage = nil
        retryIntent = nil
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
        guard let bounds = selectedRangeBounds else {
            return []
        }
        return Set(
            resolvedCandidates
                .filter { candidate in
                    let index = candidateSelectionIndex(candidate)
                    return index >= bounds.start && index <= bounds.end
                }
                .map(\.selectionID)
        )
    }

    private var selectedRangeBounds: (start: Int, end: Int)? {
        guard let start = rangeStartCandidate,
            let end = rangeEndCandidate
        else {
            return nil
        }
        return sortedRangeBounds(start: start, end: end)
    }

    private var canSelectAllResolvedCandidates: Bool {
        resolvedInputMatchesSource
            && resolutionSession != nil
            && resolutionTotalSize > 0
            && resolutionTotalSize <= Self.maxExecutionCandidateCount
    }

    private var candidateSelectionRequest: BilibiliCandidateSelectionRequest? {
        switch candidateSelectionMode {
        case .single:
            guard let candidateToken = selectedCandidateToken else {
                return nil
            }
            return BilibiliCandidateSelectionRequest(selection: .single(candidateToken: candidateToken))
        case .multiple:
            let candidateTokens = orderedSelectedCandidateIDs
            guard !candidateTokens.isEmpty,
                candidateTokens.count <= Int(Self.maxExecutionCandidateCount)
            else {
                return nil
            }
            return BilibiliCandidateSelectionRequest(selection: .multiple(candidateTokens: candidateTokens))
        case .range:
            guard let start = rangeStartCandidate,
                let end = rangeEndCandidate
            else {
                return nil
            }
            let bounds = sortedRangeBounds(start: start, end: end)
            guard bounds.end - bounds.start + 1 <= Int(Self.maxExecutionCandidateCount) else {
                return nil
            }
            let orderedCandidates = resolvedCandidates.sorted {
                candidateSelectionIndex($0) < candidateSelectionIndex($1)
            }
            guard let first = orderedCandidates.first(where: { candidateSelectionIndex($0) == bounds.start }),
                let last = orderedCandidates.first(where: { candidateSelectionIndex($0) == bounds.end })
            else {
                return nil
            }
            return BilibiliCandidateSelectionRequest(
                selection: .range(
                    startCandidateToken: first.selectionID,
                    endCandidateToken: last.selectionID
                )
            )
        case .all:
            guard canSelectAllResolvedCandidates else {
                return nil
            }
            return BilibiliCandidateSelectionRequest(selection: .all)
        }
    }

    private var cachedResolvedPlaybackRequest: BilibiliCandidateSelectionRequest? {
        guard resolvedInput != nil else {
            return nil
        }

        if isWaitingForCandidateSelection {
            return candidateSelectionRequest
        }

        guard let candidateToken = selectedCandidateToken ?? resolutionSession?.defaultCandidateToken,
            !candidateToken.isEmpty
        else {
            return nil
        }

        return BilibiliCandidateSelectionRequest(selection: .single(candidateToken: candidateToken))
    }

    private var selectedCandidateToken: String? {
        if let selectedCandidateID,
            !selectedCandidateID.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        {
            return selectedCandidateID
        }

        return resolutionSession?.defaultCandidateToken.nilIfEmpty
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

    private func startWatching(
        taskID: String,
        endpoint: CacheServerEndpoint,
        sequence: Int,
        reconcileBeforeSubscribe: Bool = false,
        recoveryAttempts: Int = 0
    ) {
        stopWatching()
        guard currentTask?.id == taskID,
            currentTask?.shouldKeepWatchingBilibiliTask == true
        else {
            return
        }

        isWatching = true
        isReconnecting = reconcileBeforeSubscribe
        watchErrorMessage = nil
        taskWatchRecoveryAttempts = min(max(recoveryAttempts, 0), Self.maximumTaskWatchRecoveryAttempts)
        let generation = taskWatchGeneration
        let client = clientFactory(endpoint)
        let timeout = operationTimeout
        let sleep = watchRecoverySleep
        let reconciliationInterval = watchReconciliationInterval
        let reconciliationSleep = watchReconciliationSleep
        taskWatcher = Task { [weak self] in
            var shouldReconcileBeforeSubscribe = reconcileBeforeSubscribe
            while !Task.isCancelled {
                guard
                    self?.isTaskWatchCurrent(
                        taskID: taskID,
                        sequence: sequence,
                        generation: generation
                    ) == true
                else {
                    return
                }

                var needsRecovery = false
                var streamError: Error?
                if shouldReconcileBeforeSubscribe {
                    shouldReconcileBeforeSubscribe = false
                    guard
                        let snapshotGeneration = self?.taskWatchSnapshotGeneration(
                            taskID: taskID,
                            sequence: sequence,
                            generation: generation
                        )
                    else {
                        return
                    }
                    do {
                        let task = try await Self.withCancellableOperationTimeout(timeout) {
                            try await client.getTask(id: taskID)
                        }
                        guard !Task.isCancelled else {
                            return
                        }
                        guard
                            let update = self?.applyReconciledTask(
                                task,
                                taskID: taskID,
                                sequence: sequence,
                                generation: generation,
                                expectedSnapshotGeneration: snapshotGeneration
                            )
                        else {
                            return
                        }
                        if !update.accepted {
                            needsRecovery = true
                        }
                        guard update.shouldKeepWatching else {
                            return
                        }
                        if update.progressChanged {
                            self?.taskWatchRecoveryAttempts = 0
                        }
                    } catch {
                        guard !Task.isCancelled else {
                            return
                        }
                        guard Self.canRetryTaskObservation(after: error) else {
                            self?.finishWatching(
                                taskID: taskID,
                                sequence: sequence,
                                generation: generation,
                                exhausted: false
                            )
                            return
                        }
                        streamError = error
                        needsRecovery = true
                    }
                }

                if !needsRecovery {
                    self?.startTaskWatchReconciliation(
                        taskID: taskID,
                        endpoint: endpoint,
                        sequence: sequence,
                        generation: generation,
                        client: client,
                        timeout: timeout,
                        interval: reconciliationInterval,
                        sleep: reconciliationSleep
                    )
                    let stream = await client.watchTask(id: taskID)
                    guard !Task.isCancelled,
                        self?.isTaskWatchCurrent(
                            taskID: taskID,
                            sequence: sequence,
                            generation: generation
                        ) == true
                    else {
                        return
                    }

                    do {
                        for try await task in stream {
                            guard !Task.isCancelled else {
                                return
                            }
                            guard
                                let update = self?.applyWatchedTask(
                                    task,
                                    taskID: taskID,
                                    sequence: sequence,
                                    generation: generation
                                )
                            else {
                                return
                            }
                            guard update.accepted else {
                                continue
                            }
                            if update.progressChanged {
                                self?.taskWatchRecoveryAttempts = 0
                            }
                            guard update.shouldKeepWatching else {
                                return
                            }
                        }
                        needsRecovery = true
                    } catch {
                        guard !Task.isCancelled else {
                            return
                        }
                        streamError = error
                        needsRecovery = true
                    }
                }

                guard !Task.isCancelled else {
                    return
                }
                await self?.stopTaskWatchReconciliation(generation: generation)
                guard !Task.isCancelled else {
                    return
                }

                guard needsRecovery else {
                    continue
                }
                guard
                    self?.taskWatchNeedsRecovery(
                        taskID: taskID,
                        sequence: sequence,
                        generation: generation
                    ) == true
                else {
                    return
                }
                if let streamError, !Self.canRetryTaskObservation(after: streamError) {
                    self?.finishWatching(
                        taskID: taskID,
                        sequence: sequence,
                        generation: generation,
                        exhausted: false
                    )
                    return
                }

                var shouldResubscribe = false
                while !Task.isCancelled,
                    (self?.taskWatchRecoveryAttempts ?? Self.maximumTaskWatchRecoveryAttempts)
                        < Self.maximumTaskWatchRecoveryAttempts
                {
                    guard
                        self?.isTaskWatchCurrent(
                            taskID: taskID,
                            sequence: sequence,
                            generation: generation
                        ) == true
                    else {
                        return
                    }

                    guard let recoveryAttempts = self?.taskWatchRecoveryAttempts else {
                        return
                    }
                    let nextRecoveryAttempt = recoveryAttempts + 1
                    self?.taskWatchRecoveryAttempts = nextRecoveryAttempt
                    self?.markTaskWatchReconnecting(
                        taskID: taskID,
                        sequence: sequence,
                        generation: generation
                    )
                    do {
                        try await sleep(Self.taskWatchRecoveryDelay(forAttempt: nextRecoveryAttempt))
                    } catch {
                        guard !Task.isCancelled else {
                            return
                        }
                        self?.finishWatching(
                            taskID: taskID,
                            sequence: sequence,
                            generation: generation,
                            exhausted: true
                        )
                        return
                    }

                    guard !Task.isCancelled,
                        self?.isTaskWatchCurrent(
                            taskID: taskID,
                            sequence: sequence,
                            generation: generation
                        ) == true,
                        let snapshotGeneration = self?.taskWatchSnapshotGeneration(
                            taskID: taskID,
                            sequence: sequence,
                            generation: generation
                        )
                    else {
                        return
                    }

                    do {
                        let task = try await Self.withCancellableOperationTimeout(timeout) {
                            try await client.getTask(id: taskID)
                        }
                        guard !Task.isCancelled else {
                            return
                        }
                        guard
                            let update = self?.applyReconciledTask(
                                task,
                                taskID: taskID,
                                sequence: sequence,
                                generation: generation,
                                expectedSnapshotGeneration: snapshotGeneration
                            )
                        else {
                            return
                        }
                        guard update.accepted else {
                            continue
                        }
                        if update.progressChanged {
                            self?.taskWatchRecoveryAttempts = 0
                        }
                        guard update.shouldKeepWatching else {
                            return
                        }
                        shouldResubscribe = true
                        break
                    } catch {
                        guard !Task.isCancelled,
                            self?.isTaskWatchCurrent(
                                taskID: taskID,
                                sequence: sequence,
                                generation: generation
                            ) == true
                        else {
                            return
                        }
                        guard Self.canRetryTaskObservation(after: error) else {
                            self?.finishWatching(
                                taskID: taskID,
                                sequence: sequence,
                                generation: generation,
                                exhausted: false
                            )
                            return
                        }
                    }
                }

                guard !Task.isCancelled else {
                    return
                }
                guard shouldResubscribe else {
                    self?.finishWatching(
                        taskID: taskID,
                        sequence: sequence,
                        generation: generation,
                        exhausted: true
                    )
                    return
                }
            }
        }
    }

    private func createTaskV2(
        sessionID: String,
        selection: BilibiliResolutionSelection,
        execution: BilibiliTaskExecution,
        endpoint: CacheServerEndpoint,
        sequence: Int,
        client: any CacheControlClient,
        isPlayback: Bool
    ) async {
        guard sequence == operationSequence else {
            return
        }

        invalidateSavedPlaybackValidation(clearCached: true)
        stopWatching()
        stopTaskResultPaging()
        activeEndpoint = endpoint
        retryIntent = nil
        isSubmitting = true
        isResolving = false
        errorMessage = nil
        statusMessage =
            isPlayback
            ? "Submitting Bilibili playback task..."
            : "Submitting Bilibili download task..."

        do {
            let task = try await Self.withOperationTimeout(operationTimeout) {
                try await client.createBilibiliTaskV2(
                    sessionID: sessionID,
                    selection: selection,
                    execution: execution
                )
            }

            guard sequence == operationSequence else {
                return
            }

            resumptionStore?.forget()
            if let serverID = activeServerID,
                let identity = resumptionTaskIdentity(endpoint: endpoint, serverID: serverID, taskID: task.id)
            {
                resumptionStore?.rememberTask(identity)
            }
            restoredTaskIdentity = nil
            restoreAttemptIdentity = nil
            resumptionMessage = nil
            applyTaskUpdate(task)
            isSubmitting = false
            if task.shouldKeepWatchingBilibiliTask {
                startWatching(taskID: task.id, endpoint: endpoint, sequence: sequence)
            }
        } catch {
            guard sequence == operationSequence else {
                return
            }

            currentTask = nil
            if Self.isResolutionPageExpired(error) {
                invalidateResolutionForExpiry()
                return
            }
            errorMessage = Self.userFacingMessage(for: error)
            statusMessage =
                isPlayback
                ? "Could not submit Bilibili playback task."
                : "Could not submit Bilibili download task."
            isSubmitting = false
        }
    }

    private func installResolutionPage(
        _ page: BilibiliResolutionPage,
        source: String,
        endpoint: CacheServerEndpoint,
        options: BilibiliPlaybackTaskOptions,
        resetSelection: Bool
    ) {
        resolutionSession = page.session
        resolutionPageToken = page.nextPageToken
        resolutionSnapshotID = page.snapshotID
        resolutionTotalSize = page.totalSize
        resolvedInputContext = BilibiliResolvedInputContext(
            source: source,
            endpoint: endpoint,
            options: BilibiliPlaybackResolutionOptions(options)
        )
        resolvedInput = BilibiliResolveResult(
            source: page.session.source,
            title: page.session.title,
            sourceKind: page.session.sourceKind,
            candidates: page.candidates.map(\.resolvedCandidate),
            defaultSelectionID: page.session.defaultCandidateToken,
            candidatesTruncated: page.session.candidatesTruncated || page.hasMoreCandidates
        )
        if resetSelection {
            applyResolvedCandidateDefaults(resolvedInput!)
        } else {
            normalizeCandidateSelectionForMode()
        }
    }

    private func appendResolutionPage(_ page: BilibiliResolutionPage) {
        guard let session = resolutionSession,
            page.session.id == session.id,
            page.snapshotID == resolutionSnapshotID,
            let resolvedInput
        else {
            invalidateResolutionForExpiry()
            return
        }

        let existingTokens = Set(resolvedInput.candidates.map(\.selectionID))
        guard page.candidates.allSatisfy({ !existingTokens.contains($0.candidateToken) }) else {
            invalidateResolutionForExpiry()
            return
        }

        self.resolvedInput = BilibiliResolveResult(
            source: resolvedInput.source,
            title: resolvedInput.title,
            sourceKind: resolvedInput.sourceKind,
            candidates: resolvedInput.candidates + page.candidates.map(\.resolvedCandidate),
            defaultSelectionID: resolvedInput.defaultSelectionID,
            candidatesTruncated: session.candidatesTruncated || page.hasMoreCandidates
        )
        resolutionPageToken = page.nextPageToken
        normalizeCandidateSelectionForMode()
    }

    private func clearResolutionSession() {
        candidatePageSequence += 1
        resolutionSession = nil
        resolutionPageToken = ""
        resolutionSnapshotID = ""
        resolutionTotalSize = 0
        candidateAutoAttemptedPageTokens = []
        isLoadingMoreCandidates = false
        resolvedInput = nil
        resolvedInputContext = nil
        clearCandidateSelection()
    }

    private func invalidateResolutionForExpiry() {
        clearResolutionSession()
        errorMessage = Self.userFacingMessage(for: BilibiliTaskViewModelError.resolutionExpired)
        statusMessage = "Re-resolve the Bilibili input before submitting."
        retryIntent = .reResolve
        isResolving = false
        isSubmitting = false
    }

    private static func startBilibiliResolution(
        client: any CacheControlClient,
        source: String,
        options: BilibiliPlaybackTaskOptions,
        operationTimeout: Duration,
        resolutionTimeout: Duration
    ) async throws -> (BilibiliResolutionPage, CacheServerSummary) {
        do {
            let serverInfo = try await withOperationTimeout(operationTimeout) {
                try await client.getServerInfo()
            }
            guard serverInfo.supportsBilibiliResolutionV2,
                serverInfo.supportsBilibiliExecutionV2
            else {
                throw BilibiliTaskViewModelError.upgradeRequired
            }
            guard serverInfo.supportsTaskOutputV2 else {
                throw BilibiliTaskViewModelError.taskOutputUnavailable
            }
            let page = try await withOperationTimeout(resolutionTimeout) {
                try await client.startBilibiliResolution(
                    urlOrID: source,
                    options: options,
                    pageSize: resolutionPageSize
                )
            }
            return (page, serverInfo)
        } catch {
            if isV2UpgradeError(error) {
                throw BilibiliTaskViewModelError.upgradeRequired
            }
            throw error
        }
    }

    private static func playbackSpec(from options: BilibiliPlaybackTaskOptions) throws -> BilibiliPlaybackSpec {
        BilibiliPlaybackSpec(
            qualityQN: try qualityQN(from: options.qualityPreference),
            codec: try playbackCodec(from: options.encodingPreference),
            audioLanguage: options.audioLanguagePreference,
            policy: options.playbackPolicy
        )
    }

    private static func downloadSpec(from options: BilibiliDownloadTaskOptions) throws -> BilibiliDownloadSpec {
        BilibiliDownloadSpec(
            qualityQN: try qualityQN(from: options.qualityPreference),
            audioLanguage: options.audioLanguagePreference,
            mode: options.downloadMode,
            downloadSubtitles: options.downloadSubtitles,
            subtitleAIPolicy: options.subtitleAIPolicy,
            downloadDanmaku: options.downloadDanmaku,
            danmakuFormats: options.danmakuFormats,
            downloadCover: options.downloadCover
        )
    }

    private static func qualityQN(from value: String) throws -> UInt32 {
        let normalized = normalizedPreferenceToken(value)
        switch normalized {
        case "", "auto", "default", "best":
            return 0
        case "360", "360p":
            return 16
        case "480", "480p":
            return 32
        case "720", "720p":
            return 64
        case "1080", "1080p", "fullhd", "fhd":
            return 80
        case "1080p+", "1080plus", "1080pplus":
            return 112
        case "1080p60", "108060":
            return 116
        case "4k", "2160", "2160p":
            return 120
        case "hdr":
            return 125
        case "dolby":
            return 126
        case "8k", "4320", "4320p":
            return 127
        default:
            guard let value = UInt32(normalized) else {
                throw BilibiliTaskViewModelError.invalidQuality
            }
            return value
        }
    }

    private static func playbackCodec(from value: String) throws -> BilibiliVideoCodec {
        switch normalizedPreferenceToken(value) {
        case "", "auto", "default", "best":
            return .auto
        case "h264", "avc", "avc1":
            return .h264
        case "hevc", "h265", "hev1", "hvc1":
            return .hevc
        case "av1", "av01":
            return .av1
        default:
            throw BilibiliTaskViewModelError.invalidCodec
        }
    }

    private static func normalizedPreferenceToken(_ value: String) -> String {
        value.trimmingCharacters(in: .whitespacesAndNewlines)
            .lowercased()
            .replacingOccurrences(of: " ", with: "")
            .replacingOccurrences(of: "_", with: "")
            .replacingOccurrences(of: "-", with: "")
    }

    private static func isV2UpgradeError(_ error: Error) -> Bool {
        if let unsupported = error as? CacheControlClientUnsupportedFeature {
            return unsupported == .bilibiliResolutionV2
        }
        if let unsupported = error as? CacheControlClientUnsupportedOperation {
            return unsupported == .bilibiliExecutionV2
        }
        return false
    }

    private static func userFacingMessage(for error: Error) -> String {
        if isV2UpgradeError(error) {
            return BilibiliTaskViewModelError.upgradeRequired.localizedDescription
        }
        if let unsupported = error as? CacheControlClientUnsupportedOperation,
            unsupported == .taskOutputV2
        {
            return BilibiliTaskViewModelError.taskOutputUnavailable.localizedDescription
        }
        let message = error.localizedDescription
        if isCredentialFailureMessage(message.lowercased()) {
            return "Bilibili credentials are required or invalid on the cache server."
        }
        return message
    }

    private static func isResolutionPageExpired(_ error: Error) -> Bool {
        let message = error.localizedDescription.lowercased()
        return (message.contains("page token") && (message.contains("expired") || message.contains("invalid")))
            || (message.contains("candidate token") && (message.contains("expired") || message.contains("invalid")))
            || message.contains("resolution session expired")
            || message.contains("resolution session was not found")
            || message.contains("does not belong to this snapshot")
    }

    private static func isTaskResultPageExpired(_ error: Error) -> Bool {
        let message = error.localizedDescription.lowercased()
        return (message.contains("page token") && (message.contains("expired") || message.contains("invalid")))
            || message.contains("snapshot is no longer available")
            || message.contains("snapshot expired")
    }

    private func stopWatching() {
        taskWatchGeneration += 1
        taskWatcher?.cancel()
        taskWatcher = nil
        cancelTaskWatchReconciliation()
        isWatching = false
        isReconnecting = false
        watchErrorMessage = nil
    }

    private func isTaskWatchCurrent(taskID: String, sequence: Int, generation: Int) -> Bool {
        sequence == operationSequence
            && generation == taskWatchGeneration
            && isWatching
            && currentTask?.id == taskID
    }

    private func taskWatchNeedsRecovery(taskID: String, sequence: Int, generation: Int) -> Bool {
        isTaskWatchCurrent(taskID: taskID, sequence: sequence, generation: generation)
            && currentTask?.shouldKeepWatchingBilibiliTask == true
    }

    private func taskWatchSnapshotGeneration(taskID: String, sequence: Int, generation: Int) -> Int? {
        guard isTaskWatchCurrent(taskID: taskID, sequence: sequence, generation: generation),
            currentTask?.shouldKeepWatchingBilibiliTask == true
        else {
            return nil
        }
        return taskSnapshotGeneration
    }

    private func markTaskWatchReconnecting(taskID: String, sequence: Int, generation: Int) {
        guard isTaskWatchCurrent(taskID: taskID, sequence: sequence, generation: generation) else {
            return
        }
        isReconnecting = true
        watchErrorMessage = "Task updates were interrupted. Reconnecting."
    }

    private func markTaskWatchHealthy(taskID: String, sequence: Int, generation: Int) {
        guard isTaskWatchCurrent(taskID: taskID, sequence: sequence, generation: generation) else {
            return
        }
        isReconnecting = false
        watchErrorMessage = nil
    }

    private func finishWatching(
        taskID: String,
        sequence: Int,
        generation: Int,
        exhausted: Bool
    ) {
        guard isTaskWatchCurrent(taskID: taskID, sequence: sequence, generation: generation) else {
            return
        }
        taskWatcher?.cancel()
        taskWatcher = nil
        cancelTaskWatchReconciliation()
        isWatching = false
        isReconnecting = false
        watchErrorMessage =
            exhausted
            ? "Task updates could not be restored. Retry to resume observation."
            : "Task updates are unavailable. Check the cache server, then retry."
    }

    private func startTaskWatchReconciliation(
        taskID: String,
        endpoint: CacheServerEndpoint,
        sequence: Int,
        generation: Int,
        client: any CacheControlClient,
        timeout: Duration,
        interval: Duration,
        sleep: @escaping @Sendable (Duration) async throws -> Void
    ) {
        guard isTaskWatchCurrent(taskID: taskID, sequence: sequence, generation: generation),
            watchReconciliationGeneration != generation
        else {
            return
        }

        watchReconciliationGeneration = generation
        let now = watchReconciliationNow
        let subscriptionStarted = now()
        taskWatchLastActivityInstant = subscriptionStarted
        var nextProbeNotBefore = subscriptionStarted.advanced(by: interval)
        watchReconciliationTask = Task { [weak self] in
            while !Task.isCancelled {
                guard
                    self?.isTaskWatchCurrent(
                        taskID: taskID,
                        sequence: sequence,
                        generation: generation
                    ) == true,
                    let lastActivity = self?.taskWatchLastActivityInstant
                else {
                    return
                }

                let quietDeadline = max(lastActivity.advanced(by: interval), nextProbeNotBefore)
                let remaining = now().duration(to: quietDeadline)
                if remaining > .zero {
                    do {
                        try await sleep(remaining)
                    } catch {
                        return
                    }
                    continue
                }
                if self?.isCancelling == true {
                    nextProbeNotBefore = now().advanced(by: interval)
                    continue
                }
                guard let activityGeneration = self?.taskWatchActivityGeneration else {
                    return
                }
                guard
                    let snapshotGeneration = self?.taskWatchSnapshotGeneration(
                        taskID: taskID,
                        sequence: sequence,
                        generation: generation
                    )
                else {
                    return
                }

                nextProbeNotBefore = now().advanced(by: interval)
                do {
                    let task = try await Self.withCancellableOperationTimeout(timeout) {
                        try await client.getTask(id: taskID)
                    }
                    guard !Task.isCancelled else {
                        return
                    }
                    guard
                        self?.isCurrentTaskWatchReconciliation(
                            taskID: taskID,
                            sequence: sequence,
                            generation: generation,
                            expectedSnapshotGeneration: snapshotGeneration,
                            expectedActivityGeneration: activityGeneration
                        ) == true
                    else {
                        guard
                            self?.isTaskWatchCurrent(
                                taskID: taskID,
                                sequence: sequence,
                                generation: generation
                            ) == true
                        else {
                            return
                        }
                        continue
                    }
                    guard
                        let update = self?.applyReconciledTask(
                            task,
                            taskID: taskID,
                            sequence: sequence,
                            generation: generation,
                            expectedSnapshotGeneration: snapshotGeneration
                        )
                    else {
                        return
                    }
                    guard update.accepted else {
                        continue
                    }
                    guard update.shouldKeepWatching else {
                        return
                    }
                    if update.progressChanged {
                        self?.taskWatchRecoveryAttempts = 0
                    }
                    self?.markTaskWatchHealthy(
                        taskID: taskID,
                        sequence: sequence,
                        generation: generation
                    )
                } catch {
                    guard !Task.isCancelled else {
                        return
                    }
                    guard
                        self?.isTaskWatchCurrent(
                            taskID: taskID,
                            sequence: sequence,
                            generation: generation
                        ) == true
                    else {
                        return
                    }
                    guard
                        self?.isCurrentTaskWatchReconciliation(
                            taskID: taskID,
                            sequence: sequence,
                            generation: generation,
                            expectedSnapshotGeneration: snapshotGeneration,
                            expectedActivityGeneration: activityGeneration
                        ) == true
                    else {
                        continue
                    }
                    guard Self.canRetryTaskObservation(after: error) else {
                        self?.finishWatching(
                            taskID: taskID,
                            sequence: sequence,
                            generation: generation,
                            exhausted: false
                        )
                        return
                    }
                    let nextRecoveryAttempt =
                        (self?.taskWatchRecoveryAttempts
                            ?? Self.maximumTaskWatchRecoveryAttempts) + 1
                    guard nextRecoveryAttempt <= Self.maximumTaskWatchRecoveryAttempts else {
                        self?.finishWatching(
                            taskID: taskID,
                            sequence: sequence,
                            generation: generation,
                            exhausted: true
                        )
                        return
                    }
                    self?.startWatching(
                        taskID: taskID,
                        endpoint: endpoint,
                        sequence: sequence,
                        reconcileBeforeSubscribe: true,
                        recoveryAttempts: nextRecoveryAttempt
                    )
                    return
                }
            }
        }
    }

    private func stopTaskWatchReconciliation(generation: Int) async {
        guard watchReconciliationGeneration == generation else {
            return
        }
        let reconciliationTask = watchReconciliationTask
        watchReconciliationTask = nil
        watchReconciliationGeneration = nil
        reconciliationTask?.cancel()
        await reconciliationTask?.value
    }

    private func cancelTaskWatchReconciliation() {
        watchReconciliationTask?.cancel()
        watchReconciliationTask = nil
        watchReconciliationGeneration = nil
    }

    private func isCurrentTaskWatchReconciliation(
        taskID: String,
        sequence: Int,
        generation: Int,
        expectedSnapshotGeneration: Int,
        expectedActivityGeneration: Int
    ) -> Bool {
        isTaskWatchCurrent(taskID: taskID, sequence: sequence, generation: generation)
            && currentTask?.shouldKeepWatchingBilibiliTask == true
            && !isCancelling
            && taskSnapshotGeneration == expectedSnapshotGeneration
            && taskWatchActivityGeneration == expectedActivityGeneration
    }

    private static let maximumTaskWatchRecoveryAttempts = 6

    private static func taskWatchRecoveryDelay(forAttempt attempt: Int) -> Duration {
        switch attempt {
        case 1: .seconds(1)
        case 2: .seconds(2)
        case 3: .seconds(4)
        case 4: .seconds(8)
        default: .seconds(16)
        }
    }

    private static func canRetryTaskObservation(after error: Error) -> Bool {
        guard let controlError = error as? CacheTaskControlError else {
            return true
        }
        return controlError.canRetryObservation
    }

    private func stopTaskResultPaging() {
        taskResultRefreshTask?.cancel()
        taskResultRefreshTask = nil
        taskResultPageSequence += 1
        taskResultItems = []
        taskResultPageToken = ""
        taskResultSnapshotID = ""
        taskResultOutputRevision = nil
        requestedTaskResultRevision = nil
        isLoadingMoreTaskResults = false
        taskResultsErrorMessage = nil
    }

    private func refreshTaskResults(for task: CacheTask?, force: Bool = false) {
        guard let task,
            let revision = task.outputSummary?.revision,
            let endpoint = activeEndpoint
        else {
            return
        }
        if !force {
            if let taskResultOutputRevision, taskResultOutputRevision >= revision {
                return
            }
            if requestedTaskResultRevision == revision {
                return
            }
        }

        taskResultRefreshTask?.cancel()
        taskResultPageSequence += 1
        let pageSequence = taskResultPageSequence
        let operation = operationSequence
        requestedTaskResultRevision = revision
        taskResultPageToken = ""
        if force {
            taskResultItems = []
            taskResultSnapshotID = ""
            taskResultOutputRevision = nil
        }
        isLoadingMoreTaskResults = true
        taskResultsErrorMessage = nil
        let client = clientFactory(endpoint)
        let timeout = operationTimeout

        taskResultRefreshTask = Task { [weak self] in
            do {
                let page = try await Self.withCancellableOperationTimeout(timeout) {
                    try await client.listTaskResults(
                        taskID: task.id,
                        pageToken: "",
                        pageSize: Self.taskResultPageSize
                    )
                }

                guard !Task.isCancelled, let self else {
                    return
                }
                guard operation == self.operationSequence,
                    pageSequence == self.taskResultPageSequence,
                    self.currentTask?.id == task.id
                else {
                    return
                }
                guard page.outputRevision >= (self.currentTask?.outputSummary?.revision ?? revision) else {
                    self.isLoadingMoreTaskResults = false
                    self.taskResultsErrorMessage = "Waiting for the latest Bilibili result snapshot."
                    self.requestedTaskResultRevision = nil
                    self.taskResultRefreshTask = nil
                    return
                }

                self.taskResultItems = page.results
                self.taskResultPageToken = page.nextPageToken
                self.taskResultSnapshotID = page.snapshotID
                self.taskResultOutputRevision = page.outputRevision
                self.requestedTaskResultRevision = page.outputRevision
                self.taskResultsErrorMessage = nil
                self.isLoadingMoreTaskResults = false
                self.taskResultRefreshTask = nil
            } catch {
                guard !Task.isCancelled, let self else {
                    return
                }
                guard operation == self.operationSequence,
                    pageSequence == self.taskResultPageSequence,
                    self.currentTask?.id == task.id
                else {
                    return
                }
                self.taskResultPageToken = ""
                self.isLoadingMoreTaskResults = false
                if self.requestedTaskResultRevision == revision {
                    self.requestedTaskResultRevision = nil
                }
                if let unsupported = error as? CacheControlClientUnsupportedOperation,
                    unsupported == .taskOutputV2
                {
                    self.taskResultsErrorMessage = BilibiliTaskViewModelError.taskOutputUnavailable.localizedDescription
                } else {
                    self.taskResultsErrorMessage = Self.userFacingMessage(for: error)
                }
                self.taskResultRefreshTask = nil
            }
        }
    }

    private struct WatchedTaskUpdate {
        let accepted: Bool
        let progressChanged: Bool
        let shouldKeepWatching: Bool
    }

    private func applyWatchedTask(
        _ task: CacheTask,
        taskID: String,
        sequence: Int,
        generation: Int
    ) -> WatchedTaskUpdate? {
        guard isTaskWatchCurrent(taskID: taskID, sequence: sequence, generation: generation) else {
            return nil
        }
        guard task.id == taskID,
            let currentTask,
            currentTask.id == taskID,
            Self.canApplyWatchedSnapshot(task, after: currentTask)
        else {
            return WatchedTaskUpdate(accepted: false, progressChanged: false, shouldKeepWatching: true)
        }

        let progressChanged =
            BilibiliTaskWatchProgressSnapshot(task)
            != BilibiliTaskWatchProgressSnapshot(currentTask)
        taskWatchActivityGeneration += 1
        taskWatchLastActivityInstant = watchReconciliationNow()
        applyTaskUpdate(task)
        isReconnecting = false
        watchErrorMessage = nil
        return WatchedTaskUpdate(
            accepted: true,
            progressChanged: progressChanged,
            shouldKeepWatching: task.shouldKeepWatchingBilibiliTask
        )
    }

    private func applyReconciledTask(
        _ task: CacheTask,
        taskID: String,
        sequence: Int,
        generation: Int,
        expectedSnapshotGeneration: Int
    ) -> WatchedTaskUpdate? {
        guard isTaskWatchCurrent(taskID: taskID, sequence: sequence, generation: generation) else {
            return nil
        }
        guard taskSnapshotGeneration == expectedSnapshotGeneration,
            task.id == taskID,
            let currentTask,
            currentTask.id == taskID,
            Self.canApplyWatchedSnapshot(task, after: currentTask)
        else {
            return WatchedTaskUpdate(accepted: false, progressChanged: false, shouldKeepWatching: true)
        }

        let progressChanged =
            BilibiliTaskWatchProgressSnapshot(task)
            != BilibiliTaskWatchProgressSnapshot(currentTask)
        applyTaskUpdate(task)
        return WatchedTaskUpdate(
            accepted: true,
            progressChanged: progressChanged,
            shouldKeepWatching: task.shouldKeepWatchingBilibiliTask
        )
    }

    private static func canApplyWatchedSnapshot(_ task: CacheTask, after currentTask: CacheTask) -> Bool {
        guard !(currentTask.isTerminalBilibiliTaskState && !task.isTerminalBilibiliTaskState),
            !(currentTask.isCancellationPendingBilibiliTaskState
                && !task.isCancellationPendingBilibiliTaskState
                && !task.isTerminalBilibiliTaskState)
        else {
            return false
        }

        if let currentRevision = currentTask.outputSummary?.revision {
            guard let incomingRevision = task.outputSummary?.revision,
                incomingRevision >= currentRevision
            else {
                return false
            }
        }
        return true
    }

    private func applyTaskUpdate(_ task: CacheTask) {
        if currentTask?.id != task.id {
            invalidateSavedPlaybackValidation(clearCached: true)
            stopTaskResultPaging()
        }
        if currentTask != task {
            taskSnapshotGeneration += 1
        }
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
        }
        if !task.shouldKeepWatchingBilibiliTask {
            stopWatching()
        }
        refreshTaskResults(for: task)
        scheduleSavedPlaybackValidationIfNeeded(for: task)
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
            guard let result = taskResults.first(where: { $0.id == activePlaybackResultID }) else {
                return
            }
            guard result.playbackURL != nil else {
                activePlaybackTaskID = nil
                self.activePlaybackResultID = nil
                activePlaybackLibraryItemID = nil
                return
            }

            activePlaybackLibraryItemID =
                normalizedNonEmpty(result.playbackLibraryItemID)
                ?? activePlaybackLibraryItemID
            return
        }

        activePlaybackLibraryItemID = task.playableBilibiliLibraryItemID
    }

    private func activePlaybackStatusMessage(for task: CacheTask) -> String {
        if let activePlaybackResultID,
            let result = taskResults.first(where: { $0.id == activePlaybackResultID })
        {
            return "Playing \(result.title)."
        }

        return "Playing \(task.bilibiliDisplayTitle)."
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

    private static func activePlaybackPolicySummary(for task: CacheTask) -> String? {
        guard let session = task.bilibiliPlaybackSessionForPolicySummary else {
            return nil
        }

        var parts: [String] = []
        if let effectivePolicy = session.effectivePolicy {
            parts.append("Policy: \(effectivePolicy.summaryText)")
        }
        if let transcodingPlan = session.transcodingPlan,
            let planSummary = transcodingPlan.summaryText
        {
            parts.append("Transcoding: \(planSummary)")
        }

        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    private static func errorNotice(
        for errorMessage: String?,
        currentTask: CacheTask?
    ) -> BilibiliFetchNotice? {
        guard let errorMessage else {
            return nil
        }

        let normalized = errorMessage.lowercased()
        if Self.isCredentialFailureMessage(normalized) {
            return BilibiliFetchNotice(
                title: "Credentials required",
                message:
                    "This Bilibili page needs server-side web credentials. Refresh the cache server credential file, then retry.",
                systemImage: "person.crop.circle.badge.exclamationmark",
                tone: .warning,
                actionTitle: "Retry"
            )
        }

        if errorMessage.isQuotaOrStorageFailureMessage {
            return nil
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

    private static func progressiveCacheStatusBadge(
        for task: CacheTask,
        additionalFillStatuses: [HlsCacheFillStatus] = []
    ) -> ProgressiveCacheStatusBadge? {
        guard task.isProgressivePlayback else {
            return nil
        }

        let fillStatus = cacheFillStatus(for: task, additionalFillStatuses: additionalFillStatuses)
        if let fillStatus {
            return ProgressiveCacheStatusBadge(
                label: fillStatus.displayLabel,
                systemImage: fillStatus.systemImage
            )
        }

        if let summary = task.bilibiliTaskResultSummary,
            summary.totalCount > 1
        {
            if summary.cachedCount == summary.totalCount {
                return ProgressiveCacheStatusBadge(
                    label: "Offline ready", systemImage: "externaldrive.fill.badge.checkmark")
            }

            if let failureBadge = multiResultOfflineCacheFailureBadge(for: task, summary: summary) {
                return failureBadge
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
            if task.message.isQuotaOrStorageFailureMessage {
                return ProgressiveCacheStatusBadge(label: "Quota blocked", systemImage: "externaldrive.badge.xmark")
            }
            if task.message.isUpstreamOrNetworkFailureMessage {
                return ProgressiveCacheStatusBadge(label: "Upstream failed", systemImage: "wifi.slash")
            }
            return ProgressiveCacheStatusBadge(
                label: "Cache failed",
                systemImage: "exclamationmark.triangle"
            )
        }

        if task.isPlayableBilibiliTaskState {
            let normalizedMessage = task.message.lowercased()
            if task.message.isQuotaOrStorageFailureMessage {
                return ProgressiveCacheStatusBadge(
                    label: "Quota blocked; playable online",
                    systemImage: "externaldrive.badge.xmark"
                )
            }
            if task.message.isOfflineCacheRetryMessage {
                return ProgressiveCacheStatusBadge(
                    label: "Retrying offline cache", systemImage: "arrow.clockwise.circle")
            }
            if task.message.isUpstreamOrNetworkFailureMessage {
                return ProgressiveCacheStatusBadge(
                    label: "Upstream failed; cache may be partial", systemImage: "wifi.slash")
            }
            if task.message.isGenericFailureMessage {
                return ProgressiveCacheStatusBadge(
                    label: "Cache failed; playable online",
                    systemImage: "exclamationmark.triangle"
                )
            }
            if normalizedMessage.contains("paused") || normalizedMessage.contains("queued") {
                return ProgressiveCacheStatusBadge(label: "Offline fill queued", systemImage: "clock")
            }
            if normalizedMessage.contains("prewarm") {
                return ProgressiveCacheStatusBadge(label: "Prewarming cache", systemImage: "bolt.horizontal")
            }
            if let percent = task.offlineCachePercentLabel {
                return ProgressiveCacheStatusBadge(
                    label: "Partially cached \(percent)", systemImage: "arrow.down.circle")
            }

            return ProgressiveCacheStatusBadge(label: "Playable online; caching", systemImage: "wifi")
        }

        let state = task.normalizedBilibiliTaskState
        if state.contains("preparing") || state.contains("planned") {
            return ProgressiveCacheStatusBadge(label: "Pending offline fill", systemImage: "clock")
        }

        return nil
    }

    private static func cacheFillStatus(
        for task: CacheTask,
        additionalFillStatuses: [HlsCacheFillStatus]
    ) -> HlsCacheFillStatus? {
        if let status = task.hlsCacheFillStatus, status.state != .unspecified {
            return status
        }

        let statuses = task.resultItems.compactMap(\.hlsCacheFillStatus) + additionalFillStatuses
        return statuses.min { fillStatePriority($0.state) < fillStatePriority($1.state) }
            ?? task.hlsCacheFillStatus
    }

    private static func fillStatePriority(_ state: HlsCacheFillState) -> Int {
        switch state {
        case .blockedQuota, .failed, .sourceUnavailable:
            0
        case .retrying:
            1
        case .filling:
            2
        case .queued:
            3
        case .preempted:
            4
        case .cancelled:
            5
        case .completed:
            6
        case .unspecified:
            7
        }
    }

    private static func multiResultOfflineCacheFailureBadge(
        for task: CacheTask,
        summary: BilibiliTaskResultSummary
    ) -> ProgressiveCacheStatusBadge? {
        guard summary.failedCount > 0 else {
            return nil
        }

        let failedCacheMessages = task.resultItems
            .filter(\.isFailedBilibiliResultState)
            .map(\.message)
            .filter(\.isOfflineCacheFailureMessage)
        guard !failedCacheMessages.isEmpty else {
            return nil
        }

        let suffix = multiResultFailureBadgeSuffix(summary)
        if failedCacheMessages.contains(where: \.isQuotaOrStorageFailureMessage) {
            return ProgressiveCacheStatusBadge(
                label: "Quota blocked\(suffix)",
                systemImage: "externaldrive.badge.xmark"
            )
        }
        if failedCacheMessages.contains(where: \.isOfflineCacheRetryMessage) {
            return ProgressiveCacheStatusBadge(
                label: "Retrying offline cache\(suffix)",
                systemImage: "arrow.clockwise.circle"
            )
        }
        if failedCacheMessages.contains(where: \.isUpstreamOrNetworkFailureMessage) {
            return ProgressiveCacheStatusBadge(
                label: "Upstream failed\(suffix)",
                systemImage: "wifi.slash"
            )
        }
        if failedCacheMessages.contains(where: \.isGenericFailureMessage) {
            return ProgressiveCacheStatusBadge(
                label: "Cache failed\(suffix)",
                systemImage: "exclamationmark.triangle"
            )
        }
        return nil
    }

    private static func multiResultFailureBadgeSuffix(_ summary: BilibiliTaskResultSummary) -> String {
        if summary.cachedCount > 0 {
            return "; \(summary.cachedCount) of \(summary.totalCount) offline ready"
        }
        if summary.readyCount > 0 {
            return "; partial result success"
        }
        return ""
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

    private static func isCredentialFailureMessage(_ normalizedMessage: String) -> Bool {
        if normalizedMessage.contains("-101")
            || normalizedMessage.contains("\u{672a}\u{767b}\u{5f55}")
            || normalizedMessage.contains("not logged")
            || normalizedMessage.contains("not login")
            || normalizedMessage.contains("login")
            || normalizedMessage.contains("cookie")
            || normalizedMessage.contains("credential")
            || normalizedMessage.contains("bili_jct")
            || normalizedMessage.contains("access_key")
            || normalizedMessage.contains("unauthorized")
            || normalizedMessage.contains("unauthorised")
            || normalizedMessage.contains("authentication")
            || normalizedMessage.contains("authenticate")
            || normalizedMessage.contains("authorization")
            || normalizedMessage.contains("authorisation")
        {
            return true
        }

        let tokens = Set(
            normalizedMessage
                .components(separatedBy: CharacterSet.alphanumerics.inverted)
                .filter { !$0.isEmpty }
        )
        return tokens.contains("auth")
            || tokens.contains("sessdata")
            || tokens.contains("csrf")
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

    private static func withCancellableOperationTimeout<Value: Sendable>(
        _ timeout: Duration,
        operation: @Sendable @escaping () async throws -> Value
    ) async throws -> Value {
        let race = BilibiliCancellableTaskOperationTimeoutRace<Value>()
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                race.start(continuation: continuation, timeout: timeout, operation: operation)
            }
        } onCancel: {
            race.cancel()
        }
    }
}

private struct BilibiliTaskWatchProgressSnapshot: Equatable {
    private struct FillProgress: Equatable {
        let state: HlsCacheFillState
        let failureKind: HlsCacheFillFailureKind
        let completedBytes: UInt64
        let totalBytes: UInt64
        let totalBytesKnown: Bool
        let representationID: String

        init?(_ fill: HlsCacheFillStatus?) {
            guard let fill else {
                return nil
            }
            state = fill.state
            failureKind = fill.failureKind
            completedBytes = fill.completedBytes
            totalBytes = fill.totalBytes
            totalBytesKnown = fill.totalBytesKnown
            representationID = fill.representationID
        }
    }

    private struct ResultProgress: Equatable {
        let id: String
        let state: String
        let libraryItemID: String
        let playbackSource: CachePlaybackSource?
        let playbackSession: CacheBilibiliPlaybackSession?
        let identity: BilibiliContentIdentity?
        let fillProgress: FillProgress?

        init(_ result: BilibiliTaskResultItem) {
            id = result.id
            state = result.state.lowercased().filter(\.isLetter)
            libraryItemID = result.libraryItemID
            playbackSource = result.playbackSource
            playbackSession = result.playbackSession
            identity = result.identity
            fillProgress = FillProgress(result.hlsCacheFillStatus)
        }
    }

    let state: String
    let progress: Double?
    let downloadedBytes: Int64
    let totalBytes: Int64
    let libraryItemID: String
    let playbackSource: CachePlaybackSource?
    let playbackSession: CacheBilibiliPlaybackSession?
    let outputSummary: CacheTaskOutputSummary?
    private let fillProgress: FillProgress?
    private let results: [ResultProgress]

    init(_ task: CacheTask) {
        state = task.state.lowercased().filter(\.isLetter)
        progress = task.progress.isFinite ? task.progress : nil
        downloadedBytes = task.downloadedBytes
        totalBytes = task.totalBytes
        libraryItemID = task.libraryItemID
        playbackSource = task.playbackSource
        playbackSession = task.playbackSession
        outputSummary = task.outputSummary
        fillProgress = FillProgress(task.hlsCacheFillStatus)
        results = task.resultItems.map(ResultProgress.init)
    }
}

private func normalizedNonEmpty(_ value: String) -> String? {
    let normalized = value.trimmingCharacters(in: .whitespacesAndNewlines)
    return normalized.isEmpty ? nil : normalized
}

private extension CacheTask {
    var bilibiliPlaybackSessionForPolicySummary: CacheBilibiliPlaybackSession? {
        playbackSession ?? resultItems.lazy.compactMap(\.playbackSession).first
    }

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

    var playableBilibiliPlaybackSource: CachePlaybackSource? {
        guard isProgressivePlayback, isPlayableBilibiliTaskState else {
            return nil
        }

        if topLevelPlayableBilibiliURL != nil {
            return playbackSource
        }

        return resultItems.first { $0.playableBilibiliURL != nil }?.playbackSource
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
                playbackVariantID: item.playbackSource?.variantID ?? "",
                playbackURL: item.playableBilibiliURL,
                artifacts: [],
                isReady: item.isReadyBilibiliResultState,
                isCached: item.isCompletedBilibiliResultState,
                isFailed: item.isFailedBilibiliResultState,
                isCancelled: item.isCancelledBilibiliResultState,
                hlsCacheFillStatus: item.hlsCacheFillStatus
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
        normalizedBilibiliTaskState.contains("playable") || isCompletedBilibiliTaskState
    }

    var isCompletedBilibiliTaskState: Bool {
        let state = normalizedBilibiliTaskState
        return state.contains("completed") || state.contains("succeeded")
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

    var isQueuedOrRunningBilibiliTaskState: Bool {
        normalizedBilibiliTaskState.contains("queued")
            || normalizedBilibiliTaskState.contains("running")
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

    var shouldKeepWatchingBilibiliTask: Bool {
        if !isTerminalBilibiliTaskState {
            return true
        }
        guard isCompletedBilibiliTaskState else {
            return false
        }
        return resultItems.contains { item in
            item.isReadyBilibiliResultState && !item.isCompletedBilibiliResultState
        }
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

        if hasTopLevelBilibiliLibraryItem(id: trimmedLibraryItemID) {
            return true
        }

        return resultItems.contains { $0.hasBilibiliLibraryItem(id: trimmedLibraryItemID) }
    }

    func hasTopLevelBilibiliLibraryItem(id libraryItemID: String) -> Bool {
        let trimmedLibraryItemID = libraryItemID.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedLibraryItemID.isEmpty else {
            return false
        }

        if self.libraryItemID == trimmedLibraryItemID {
            return true
        }

        return playbackSource?.itemID == trimmedLibraryItemID
    }

    func clearingBilibiliResultLibraryItem(id libraryItemID: String) -> CacheTask? {
        let trimmedLibraryItemID = libraryItemID.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedLibraryItemID.isEmpty else {
            return nil
        }

        var didClearResult = false
        let updatedResultItems = resultItems.map { item in
            guard item.hasBilibiliLibraryItem(id: trimmedLibraryItemID) else {
                return item
            }

            didClearResult = true
            return item.clearingDeletedBilibiliLibraryItem()
        }

        guard didClearResult else {
            return nil
        }

        return CacheTask(
            id: id,
            kind: kind,
            state: state,
            source: source,
            title: title,
            progress: progress,
            downloadedBytes: downloadedBytes,
            totalBytes: totalBytes,
            message: message,
            libraryItemID: self.libraryItemID,
            playbackSource: playbackSource,
            playbackSession: playbackSession,
            bilibiliSelection: bilibiliSelection,
            resultItems: updatedResultItems,
            outputSummary: outputSummary,
            hlsCacheFillStatus: hlsCacheFillStatus
        )
    }
}

private extension BilibiliTaskResultItem {
    func hasBilibiliLibraryItem(id libraryItemID: String) -> Bool {
        let trimmedLibraryItemID = libraryItemID.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedLibraryItemID.isEmpty else {
            return false
        }

        return self.libraryItemID == trimmedLibraryItemID
            || playbackSource?.itemID == trimmedLibraryItemID
    }

    func clearingDeletedBilibiliLibraryItem() -> BilibiliTaskResultItem {
        BilibiliTaskResultItem(
            id: id,
            selectionID: selectionID,
            title: title,
            subtitle: subtitle,
            sourceKind: sourceKind,
            contentID: contentID,
            index: index,
            state: "TASK_STATE_FAILED",
            message: "Cached Bilibili result was deleted.",
            libraryItemID: "",
            playbackSource: nil,
            playbackSession: nil,
            hlsCacheFillStatus: hlsCacheFillStatus
        )
    }

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
        guard isPlayableBilibiliResultState else {
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
        normalizedBilibiliResultState.contains("playable") || isCompletedBilibiliResultState
    }

    var isPlayableBilibiliResultState: Bool {
        isReadyBilibiliResultState
            || (isFailedBilibiliResultState
                && playbackSource != nil
                && message.localizedCaseInsensitiveContains("offline cache fill failed"))
    }

    var isCompletedBilibiliResultState: Bool {
        let state = normalizedBilibiliResultState
        return state.contains("completed") || state.contains("succeeded")
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

private extension BilibiliResolutionCandidate {
    var resolvedCandidate: BilibiliResolvedCandidate {
        let contentID: String
        if !identity.bvid.isEmpty {
            contentID = identity.bvid
        } else if identity.aid > 0 {
            contentID = "av\(identity.aid)"
        } else if identity.epid > 0 {
            contentID = "ep\(identity.epid)"
        } else {
            contentID = "cid\(identity.cid)"
        }

        return BilibiliResolvedCandidate(
            selectionID: candidateToken,
            title: title,
            subtitle: subtitle,
            sourceKind: sourceKind,
            contentID: contentID,
            index: index,
            durationSeconds: durationSeconds,
            coverURI: ""
        )
    }
}

private extension CacheTaskResult {
    func hasBilibiliLibraryItem(id libraryItemID: String) -> Bool {
        let trimmedLibraryItemID = libraryItemID.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedLibraryItemID.isEmpty else {
            return false
        }

        return self.libraryItemID == trimmedLibraryItemID
            || playbackSource?.itemID == trimmedLibraryItemID
    }

    func clearingDeletedBilibiliLibraryItem() -> CacheTaskResult {
        CacheTaskResult(
            id: id,
            state: "TASK_RESULT_STATE_FAILED",
            title: title,
            subtitle: subtitle,
            progress: progress,
            problem: CacheTaskProblem(
                category: "",
                code: "",
                message: "Cached Bilibili result was deleted.",
                retryable: false
            ),
            libraryItemID: "",
            playbackSource: nil,
            artifacts: artifacts,
            createdAt: createdAt,
            updatedAt: updatedAt,
            subject: subject,
            providerDetails: providerDetails
        )
    }

    func bilibiliPresentation(mediaBaseURIs: [String]) -> BilibiliTaskResultPresentation {
        let normalizedState = state.normalizedBilibiliState
        let message = problem?.message ?? ""
        let isFailed = normalizedState.contains("failed")
        let isCancelled = normalizedState.contains("cancelled")
        let isCached = normalizedState.contains("completed") || normalizedState.contains("succeeded")
        let isReady = isCached || normalizedState.contains("playable") || normalizedState.contains("ready")
        let expectedItemID = isCached ? libraryItemID : id
        let url =
            isReady || (isFailed && message.localizedCaseInsensitiveContains("offline cache fill failed"))
            ? playbackSource.flatMap { source in
                expectedItemID.isEmpty ? nil : playableURL(for: source, expectedItemID: expectedItemID)
            }
            : nil
        let artifacts = self.artifacts.map { artifact in
            let resource = artifact.resource
            return BilibiliTaskArtifactPresentation(
                id: artifact.id,
                kind: artifact.kind,
                state: artifact.state,
                title: artifact.title,
                format: artifact.format,
                languageTag: artifact.languageTag,
                isAIGenerated: artifact.isAIGenerated,
                resourceURL: resource.flatMap { serverOwnedResourceURL($0.uri, mediaBaseURIs: mediaBaseURIs) },
                contentType: resource?.contentType ?? "",
                sizeBytes: resource?.sizeBytes ?? 0,
                sizeKnown: resource?.sizeKnown ?? false,
                expiresAt: resource?.expiresAt,
                libraryItemID: artifact.libraryItemID,
                message: artifact.problem?.message ?? ""
            )
        }

        return BilibiliTaskResultPresentation(
            id: id,
            selectionID: id,
            title: title,
            subtitle: subtitle,
            state: state,
            message: message,
            libraryItemID: libraryItemID,
            playbackLibraryItemID: url != nil && isCached ? libraryItemID : "",
            playbackVariantID: playbackSource?.variantID ?? "",
            playbackURL: url,
            artifacts: artifacts,
            isReady: isReady,
            isCached: isCached,
            isFailed: isFailed,
            isCancelled: isCancelled,
            hlsCacheFillStatus: bilibiliCacheFillStatus
        )
    }

    var bilibiliCacheFillStatus: HlsCacheFillStatus? {
        guard case .bilibili(let details)? = providerDetails else {
            return nil
        }
        return details.hlsCacheFillStatus
    }
}

private extension String {
    var normalizedBilibiliState: String {
        lowercased().filter(\.isLetter)
    }
}

private enum SavedTaskResumptionIssue: Error {
    case serverIdentityChanged
    case taskIdentityChanged
    case notBilibiliTask
    case playbackUnavailable
    case resultUnavailable
    case resultLimitExceeded
    case duplicateResultID
    case incoherentResultSnapshot
    case libraryIdentityChanged
    case invalidPlaybackSource

    static func message(for error: Error) -> String {
        guard let issue = error as? Self else {
            return "The saved playback source could not be verified. Retry the saved task, or forget it."
        }
        switch issue {
        case .serverIdentityChanged:
            return
                "The cache server identity changed. The saved task is unavailable here. Retry it on the original server, or forget the saved record."
        case .taskIdentityChanged:
            return
                "The cache server did not return the exact saved task. The saved record is retained for retry or forget."
        case .notBilibiliTask:
            return "The saved task is no longer a Bilibili task. The saved record is retained for retry or forget."
        case .playbackUnavailable:
            return "The exact saved Bilibili source is not playable yet. Retry the saved task after its output changes."
        case .resultUnavailable:
            return
                "The exact saved Bilibili result is not in the current task output. Retry the saved task or forget it."
        case .resultLimitExceeded:
            return
                "The saved task output is larger than the bounded restore scan. Retry later or forget the saved task."
        case .duplicateResultID, .incoherentResultSnapshot:
            return "The saved task output changed while being checked. Retry the saved task or forget it."
        case .libraryIdentityChanged:
            return
                "The exact saved result now refers to a different library item. The saved record is retained for retry or forget."
        case .invalidPlaybackSource:
            return
                "The exact saved playback source is not an allowed LAN media URL or variant. Retry the saved task or forget it."
        }
    }
}

private func serverOwnedMediaURL(_ uri: String, mediaBaseURIs: [String]) -> URL? {
    guard let candidate = URLComponents(string: uri),
        let candidateScheme = candidate.scheme?.lowercased(),
        ["http", "https"].contains(candidateScheme),
        let candidateHost = candidate.host, !candidateHost.isEmpty,
        candidate.user == nil, candidate.password == nil,
        candidate.query == nil, candidate.fragment == nil,
        let candidateURL = candidate.url,
        SelfPath.isSafe(candidate.percentEncodedPath)
    else {
        return nil
    }

    for baseURI in mediaBaseURIs {
        guard let base = URLComponents(string: baseURI),
            let baseScheme = base.scheme?.lowercased(),
            ["http", "https"].contains(baseScheme),
            let baseHost = base.host, !baseHost.isEmpty,
            base.user == nil, base.password == nil,
            base.query == nil, base.fragment == nil,
            SelfPath.isSafe(base.percentEncodedPath),
            candidateScheme == baseScheme,
            candidateHost.lowercased() == baseHost.lowercased(),
            SelfPath.effectivePort(candidate) == SelfPath.effectivePort(base),
            SelfPath.isWithin(candidate.percentEncodedPath, base.percentEncodedPath)
        else {
            continue
        }
        return candidateURL
    }
    return nil
}

private enum SelfPath {
    static func effectivePort(_ components: URLComponents) -> Int? {
        components.port ?? (components.scheme?.lowercased() == "https" ? 443 : 80)
    }

    static func isWithin(_ candidatePath: String, _ basePath: String) -> Bool {
        var normalizedBase = basePath
        while normalizedBase.count > 1, normalizedBase.hasSuffix("/") {
            normalizedBase.removeLast()
        }
        if normalizedBase.isEmpty || normalizedBase == "/" {
            return candidatePath.hasPrefix("/")
        }
        return candidatePath == normalizedBase || candidatePath.hasPrefix(normalizedBase + "/")
    }

    static func isSafe(_ encodedPath: String) -> Bool {
        guard let decodedPath = encodedPath.removingPercentEncoding else {
            return false
        }
        return !decodedPath.split(separator: "/", omittingEmptySubsequences: false)
            .contains(where: { $0 == "." || $0 == ".." })
    }
}

private func serverOwnedResourceURL(_ uri: String, mediaBaseURIs: [String]) -> URL? {
    guard let resource = URLComponents(string: uri),
        resource.user == nil,
        resource.password == nil,
        resource.query == nil,
        resource.fragment == nil
    else {
        return nil
    }

    for baseURI in mediaBaseURIs {
        guard let base = URLComponents(string: baseURI),
            let baseScheme = base.scheme?.lowercased(),
            ["http", "https"].contains(baseScheme),
            let baseHost = base.host, !baseHost.isEmpty,
            base.user == nil, base.password == nil,
            base.query == nil, base.fragment == nil
        else {
            continue
        }

        var basePath = base.percentEncodedPath
        while basePath.hasSuffix("/") {
            basePath.removeLast()
        }
        let resourcePrefix = basePath + "/resources/"
        var candidate = resource
        if resource.scheme == nil, resource.host == nil,
            uri.hasPrefix("/resources/"), !uri.hasPrefix("//")
        {
            candidate = base
            candidate.percentEncodedPath = basePath + resource.percentEncodedPath
        } else if resource.scheme?.lowercased() != baseScheme
            || resource.host?.lowercased() != baseHost.lowercased()
            || resource.port != base.port
        {
            continue
        }

        guard candidate.percentEncodedPath.hasPrefix(resourcePrefix) else {
            continue
        }
        let resourceID = String(candidate.percentEncodedPath.dropFirst(resourcePrefix.count))
        guard !resourceID.isEmpty, resourceID.utf8.count <= 200,
            resourceID.utf8.allSatisfy({ byte in
                (65...90).contains(byte) || (97...122).contains(byte)
                    || (48...57).contains(byte) || byte == 45 || byte == 95
            })
        else {
            continue
        }
        return candidate.url
    }

    return nil
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

private extension LanTranscodingPlan {
    var summaryText: String? {
        let reason = reason.trimmingCharacters(in: .whitespacesAndNewlines)
        if !reason.isEmpty {
            return reason
        }

        let profileID = profileID.trimmingCharacters(in: .whitespacesAndNewlines)
        if !profileID.isEmpty {
            return profileID
        }

        let state = state.trimmingCharacters(in: .whitespacesAndNewlines)
        return state.isEmpty ? nil : state
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
    var currentPlaybackPolicy: BilibiliPlaybackPolicy {
        BilibiliPlaybackPolicy(
            transcodingPreference: playbackTranscodingPreference,
            compatibleVariantPreference: playbackCompatibleVariantPreference,
            weakNetworkPreference: playbackWeakNetworkPreference
        )
    }

    var currentPlaybackOptions: BilibiliPlaybackTaskOptions {
        BilibiliPlaybackTaskOptions(
            qualityPreference: qualityPreference.trimmingCharacters(in: .whitespacesAndNewlines),
            encodingPreference: encodingPreference.trimmingCharacters(in: .whitespacesAndNewlines),
            audioLanguagePreference: audioLanguagePreference.trimmingCharacters(in: .whitespacesAndNewlines),
            playbackPolicy: currentPlaybackPolicy
        )
    }

    var currentResolutionOptions: BilibiliPlaybackTaskOptions {
        guard submissionMode == .download else {
            return currentPlaybackOptions
        }
        return BilibiliPlaybackTaskOptions(
            qualityPreference: qualityPreference.trimmingCharacters(in: .whitespacesAndNewlines),
            audioLanguagePreference: audioLanguagePreference.trimmingCharacters(in: .whitespacesAndNewlines)
        )
    }

    var currentDownloadOptions: BilibiliDownloadTaskOptions {
        BilibiliDownloadTaskOptions(
            qualityPreference: qualityPreference.trimmingCharacters(in: .whitespacesAndNewlines),
            encodingPreference: "",
            audioLanguagePreference: audioLanguagePreference.trimmingCharacters(in: .whitespacesAndNewlines),
            downloadSubtitles: downloadSubtitles,
            downloadDanmaku: downloadDanmaku,
            downloadCover: downloadCover,
            subtitleAIPolicy: subtitleAIPolicy,
            danmakuFormats: availableDanmakuFormats.filter { danmakuFormats.contains($0) }
        )
    }

    var resolvedInputMatchesSource: Bool {
        resolvedInputMatches(
            source: Self.normalizedBilibiliSource(sourceText),
            endpoint: nil,
            options: currentResolutionOptions
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
            && resolvedInputContext.options == BilibiliPlaybackResolutionOptions(options)
            && Self.normalizedBilibiliSource(resolvedInput.source) == source
    }

    func currentSubmissionMatches(source: String, options: BilibiliPlaybackTaskOptions) -> Bool {
        Self.normalizedBilibiliSource(sourceText) == source
            && BilibiliPlaybackResolutionOptions(currentResolutionOptions)
                == BilibiliPlaybackResolutionOptions(options)
    }

    func discardStaleResolveSubmission(
        statusMessage: String = "Bilibili input changed before resolve completed."
    ) {
        currentTask = nil
        clearResolutionSession()
        errorMessage = nil
        self.statusMessage = statusMessage
        isResolving = false
        isSubmitting = false
    }

    static func normalizedBilibiliSource(_ source: String) -> String {
        source.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    static func loadPlaybackPolicy(from defaults: UserDefaults) -> BilibiliPlaybackPolicy {
        BilibiliPlaybackPolicy(
            transcodingPreference: BilibiliTranscodingPreference(
                rawValue: defaults.string(forKey: playbackTranscodingPreferenceDefaultsKey) ?? ""
            ) ?? .auto,
            compatibleVariantPreference: BilibiliCompatibleVariantPreference(
                rawValue: defaults.string(forKey: playbackCompatibleVariantPreferenceDefaultsKey) ?? ""
            ) ?? .preferCompatible,
            weakNetworkPreference: BilibiliWeakNetworkPreference(
                rawValue: defaults.string(forKey: playbackWeakNetworkPreferenceDefaultsKey) ?? ""
            ) ?? .adaptive
        )
    }

    func persistPlaybackPolicy() {
        persist(
            playbackTranscodingPreference,
            defaultValue: .auto,
            key: Self.playbackTranscodingPreferenceDefaultsKey
        )
        persist(
            playbackCompatibleVariantPreference,
            defaultValue: .preferCompatible,
            key: Self.playbackCompatibleVariantPreferenceDefaultsKey
        )
        persist(
            playbackWeakNetworkPreference,
            defaultValue: .adaptive,
            key: Self.playbackWeakNetworkPreferenceDefaultsKey
        )
    }

    func persist<T: RawRepresentable & Equatable>(
        _ value: T,
        defaultValue: T,
        key: String
    ) where T.RawValue == String {
        if value == defaultValue {
            defaults.removeObject(forKey: key)
        } else {
            defaults.set(value.rawValue, forKey: key)
        }
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

    var isUpstreamOrNetworkFailureMessage: Bool {
        let normalized = lowercased()
        return normalized.contains("upstream")
            || normalized.contains("network")
            || normalized.contains("timed out")
            || normalized.contains("timeout")
            || normalized.contains("connection")
    }

    var isOfflineCacheFailureMessage: Bool {
        return isQuotaOrStorageFailureMessage
            || isUpstreamOrNetworkFailureMessage
            || isOfflineCacheContextMessage
    }

    var isOfflineCacheRetryMessage: Bool {
        isRetryingFailureMessage && isOfflineCacheContextMessage
    }

    var isOfflineCacheContextMessage: Bool {
        let normalized = lowercased()
        return normalized.contains("offline cache")
            || normalized.contains("cache fill")
            || normalized.contains("cache-fill")
            || normalized.contains("cache offline")
    }

    var isRetryingFailureMessage: Bool {
        let normalized = lowercased()
        return normalized.contains("retry")
            || normalized.contains("backup url")
            || normalized.contains("backup urls")
    }

    var isGenericFailureMessage: Bool {
        let normalized = lowercased()
        return normalized.contains("failed")
            || normalized.contains("failure")
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

private final class BilibiliCancellableTaskOperationTimeoutRace<Value: Sendable>: @unchecked Sendable {
    private let lock = NSLock()
    private var continuation: CheckedContinuation<Value, Error>?
    private var operationTask: Task<Void, Never>?
    private var timeoutTask: Task<Void, Never>?
    private var cancellationRequested = false

    func start(
        continuation: CheckedContinuation<Value, Error>,
        timeout: Duration,
        operation: @Sendable @escaping () async throws -> Value
    ) {
        lock.lock()
        guard !cancellationRequested else {
            lock.unlock()
            continuation.resume(throwing: CancellationError())
            return
        }
        self.continuation = continuation
        lock.unlock()

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
        guard self.continuation != nil else {
            lock.unlock()
            operationTask.cancel()
            timeoutTask.cancel()
            return
        }
        self.operationTask = operationTask
        self.timeoutTask = timeoutTask
        lock.unlock()
    }

    func cancel() {
        lock.lock()
        cancellationRequested = true
        lock.unlock()
        complete(.failure(CancellationError()))
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
