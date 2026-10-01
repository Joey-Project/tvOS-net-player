import Foundation

public struct CacheLibraryItemsPage: Equatable, Sendable {
    public let items: [CacheLibraryItem]
    public let nextPageToken: String

    public init(items: [CacheLibraryItem], nextPageToken: String) {
        self.items = items
        self.nextPageToken = nextPageToken
    }

    public var hasMoreItems: Bool {
        !nextPageToken.isEmpty
    }
}

public protocol CacheControlClient: Sendable {
    func getServerInfo() async throws -> CacheServerSummary
    func getResolverSettings() async throws -> ResolverSettingsSnapshot
    func updateResolverSettings(_ request: UpdateResolverSettingsRequest) async throws -> ResolverSettingsSnapshot
    func checkHealth() async throws -> CacheHealthStatus
    func getBilibiliCredentialStatus() async throws -> BilibiliCredentialStatus
    func listBilibiliCredentialProfiles() async throws -> BilibiliCredentialProfilesSummary
    func startBilibiliLoginSession(
        profileID: String,
        method: BilibiliLoginMethod
    ) async throws -> BilibiliLoginSession
    func getBilibiliLoginSession(id: String) async throws -> BilibiliLoginSession
    func listCacheRoots() async throws -> [CacheRoot]
    func getHLSCacheStatus() async throws -> HLSCacheStatus
    func reportPlaybackProgress(_ report: PlaybackProgressReport) async throws -> PlaybackProgressReportResult
    func listLibraryItemsPage(
        pageToken: String,
        pageSize: Int,
        searchText: String?
    ) async throws -> CacheLibraryItemsPage
    func getPlaybackSource(itemID: String, variantID: String) async throws -> CachePlaybackSource
    func deleteLibraryItem(id: String) async throws -> Bool
    func getTask(id: String) async throws -> CacheTask
    func listTaskResults(
        taskID: String,
        pageToken: String,
        pageSize: Int
    ) async throws -> CacheTaskResultsPage
    func watchTasks(ids: [String]) async -> AsyncThrowingStream<CacheTask, Error>
    func cancelTask(id: String) async throws -> CacheTask
    func resolveBilibiliInput(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> BilibiliResolveResult
    func startBilibiliResolution(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions,
        pageSize: Int
    ) async throws -> BilibiliResolutionPage
    func startBilibiliResolution(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions,
        context: BilibiliRequestContext,
        pageSize: Int
    ) async throws -> BilibiliResolutionPage
    func listBilibiliResolutionCandidates(
        sessionID: String,
        pageToken: String,
        pageSize: Int
    ) async throws -> BilibiliResolutionPage
    func createBilibiliTask(
        urlOrID: String,
        options: BilibiliDownloadTaskOptions
    ) async throws -> CacheTask
    func createBilibiliPlaybackTask(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask
    func createBilibiliPlaybackTask(
        urlOrID: String,
        selectionID: String?,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask
    func createBilibiliPlaybackTask(
        urlOrID: String,
        selection: BilibiliTaskSelection?,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask
    func createBilibiliPlaybackTaskV2(
        sessionID: String,
        selection: BilibiliResolutionSelection
    ) async throws -> CacheTask
    func createBilibiliTaskV2(
        sessionID: String,
        selection: BilibiliResolutionSelection,
        execution: BilibiliTaskExecution
    ) async throws -> CacheTask
}

public enum CacheControlClientUnsupportedFeature: Error, Equatable {
    case healthCheck
    case hlsCacheStatus
    case bilibiliCredentialStatus
    case bilibiliCredentialProfiles
    case bilibiliLoginSessions
    case bilibiliResolve
    case bilibiliResolutionV2
    case bilibiliDownloadTask
    case bilibiliTaskSelection
    case bilibiliPlaybackPolicy
    case playbackProgressReporting
    case resolverSettings
}

public struct CacheControlClientRevisionConflict: Error, Equatable, Sendable {
    public init() {}
}

public enum CacheControlClientInvalidRequest: Error, Equatable, Sendable {
    case bilibiliResolutionInputRequired
    case bilibiliResolutionSessionIDRequired
    case invalidBilibiliResolutionSelection
}

public struct CacheControlClientUnsupportedOperation: Error, Equatable, Sendable {
    public let code: String
    public let message: String

    private init(code: String, message: String) {
        self.code = code
        self.message = message
    }

    public static let bilibiliExecutionV2 = CacheControlClientUnsupportedOperation(
        code: "bilibili_execution_v2",
        message: "Bilibili v2 task execution is not supported by this cache server."
    )

    public static let taskOutputV2 = CacheControlClientUnsupportedOperation(
        code: "task_output_v2",
        message: "Paginated task results are not supported by this cache server."
    )
}

public struct CacheControlClientRequestValidationError: Error, Equatable, Sendable {
    public let code: String
    public let message: String

    private init(code: String, message: String) {
        self.code = code
        self.message = message
    }

    public static let invalidBilibiliDownloadMode = CacheControlClientRequestValidationError(
        code: "invalid_bilibili_download_mode",
        message: "The Bilibili download mode is not recognized."
    )
}

public extension CacheControlClient {
    func getResolverSettings() async throws -> ResolverSettingsSnapshot {
        throw CacheControlClientUnsupportedFeature.resolverSettings
    }

    func updateResolverSettings(_ request: UpdateResolverSettingsRequest) async throws -> ResolverSettingsSnapshot {
        throw CacheControlClientUnsupportedFeature.resolverSettings
    }

    func checkHealth() async throws -> CacheHealthStatus {
        throw CacheControlClientUnsupportedFeature.healthCheck
    }

    func getHLSCacheStatus() async throws -> HLSCacheStatus {
        throw CacheControlClientUnsupportedFeature.hlsCacheStatus
    }

    func reportPlaybackProgress(_ report: PlaybackProgressReport) async throws -> PlaybackProgressReportResult {
        throw CacheControlClientUnsupportedFeature.playbackProgressReporting
    }

    func getBilibiliCredentialStatus() async throws -> BilibiliCredentialStatus {
        throw CacheControlClientUnsupportedFeature.bilibiliCredentialStatus
    }

    func listBilibiliCredentialProfiles() async throws -> BilibiliCredentialProfilesSummary {
        throw CacheControlClientUnsupportedFeature.bilibiliCredentialProfiles
    }

    func startBilibiliLoginSession(
        profileID: String,
        method: BilibiliLoginMethod = .webQR
    ) async throws -> BilibiliLoginSession {
        throw CacheControlClientUnsupportedFeature.bilibiliLoginSessions
    }

    func getBilibiliLoginSession(id: String) async throws -> BilibiliLoginSession {
        throw CacheControlClientUnsupportedFeature.bilibiliLoginSessions
    }

    func resolveBilibiliInput(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> BilibiliResolveResult {
        throw CacheControlClientUnsupportedFeature.bilibiliResolve
    }

    func startBilibiliResolution(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions = BilibiliPlaybackTaskOptions(),
        pageSize: Int = 50
    ) async throws -> BilibiliResolutionPage {
        throw CacheControlClientUnsupportedFeature.bilibiliResolutionV2
    }

    func startBilibiliResolution(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions,
        context: BilibiliRequestContext,
        pageSize: Int
    ) async throws -> BilibiliResolutionPage {
        guard context.isDefault else {
            throw CacheControlClientUnsupportedOperation.bilibiliExecutionV2
        }
        return try await startBilibiliResolution(
            urlOrID: urlOrID,
            options: options,
            pageSize: pageSize
        )
    }

    func listBilibiliResolutionCandidates(
        sessionID: String,
        pageToken: String = "",
        pageSize: Int = 50
    ) async throws -> BilibiliResolutionPage {
        throw CacheControlClientUnsupportedFeature.bilibiliResolutionV2
    }

    func createBilibiliTask(
        urlOrID: String,
        options: BilibiliDownloadTaskOptions
    ) async throws -> CacheTask {
        throw CacheControlClientUnsupportedFeature.bilibiliDownloadTask
    }

    func listTaskResults(
        taskID: String,
        pageToken: String = "",
        pageSize: Int = 50
    ) async throws -> CacheTaskResultsPage {
        throw CacheControlClientUnsupportedOperation.taskOutputV2
    }

    func listLibraryItemsPage(
        pageSize: Int = 50,
        searchText: String? = nil
    ) async throws -> CacheLibraryItemsPage {
        try await listLibraryItemsPage(pageToken: "", pageSize: pageSize, searchText: searchText)
    }

    func createBilibiliPlaybackTask(
        urlOrID: String
    ) async throws -> CacheTask {
        try await createBilibiliPlaybackTask(
            urlOrID: urlOrID,
            options: BilibiliPlaybackTaskOptions()
        )
    }

    func createBilibiliPlaybackTask(
        urlOrID: String,
        selectionID: String?,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask {
        let normalizedSelectionID = selectionID?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        guard normalizedSelectionID.isEmpty else {
            throw CacheControlClientUnsupportedFeature.bilibiliResolve
        }
        return try await createBilibiliPlaybackTask(
            urlOrID: urlOrID,
            options: options
        )
    }

    func createBilibiliPlaybackTask(
        urlOrID: String,
        selection: BilibiliTaskSelection?,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask {
        guard selection == nil else {
            throw CacheControlClientUnsupportedFeature.bilibiliTaskSelection
        }
        return try await createBilibiliPlaybackTask(
            urlOrID: urlOrID,
            options: options
        )
    }

    func createBilibiliPlaybackTaskV2(
        sessionID: String,
        selection: BilibiliResolutionSelection
    ) async throws -> CacheTask {
        throw CacheControlClientUnsupportedFeature.bilibiliResolutionV2
    }

    func createBilibiliTaskV2(
        sessionID: String,
        selection: BilibiliResolutionSelection,
        execution: BilibiliTaskExecution
    ) async throws -> CacheTask {
        throw CacheControlClientUnsupportedOperation.bilibiliExecutionV2
    }

    func watchTask(id: String) async -> AsyncThrowingStream<CacheTask, Error> {
        await watchTasks(ids: [id])
    }
}

extension CacheControlClientUnsupportedFeature: LocalizedError {
    public var errorDescription: String? {
        switch self {
        case .healthCheck:
            return "Health check is not supported by this cache server."
        case .hlsCacheStatus:
            return "HLS cache status is not supported by this cache server."
        case .bilibiliCredentialStatus:
            return "Bilibili credential status is not supported by this cache server."
        case .bilibiliCredentialProfiles:
            return "Bilibili credential profiles are not supported by this cache server."
        case .bilibiliLoginSessions:
            return "Bilibili login sessions are not supported by this cache server."
        case .bilibiliResolve:
            return "Bilibili resolve is not supported by this cache server."
        case .bilibiliResolutionV2:
            return "Paginated Bilibili resolution is not supported by this cache server."
        case .bilibiliDownloadTask:
            return "Bilibili download tasks are not supported by this cache server."
        case .bilibiliTaskSelection:
            return "Bilibili task selection is not supported by this cache server."
        case .bilibiliPlaybackPolicy:
            return "Bilibili playback policy controls are not supported by this cache server."
        case .playbackProgressReporting:
            return "Playback progress reporting is not supported by this cache server."
        case .resolverSettings:
            return "Resolver settings are not supported by this cache server."
        }
    }
}

extension CacheControlClientInvalidRequest: LocalizedError {
    public var errorDescription: String? {
        switch self {
        case .bilibiliResolutionInputRequired:
            return "A Bilibili URL or ID is required to start resolution."
        case .bilibiliResolutionSessionIDRequired:
            return "A Bilibili resolution session ID is required."
        case .invalidBilibiliResolutionSelection:
            return "The Bilibili resolution selection is structurally invalid."
        }
    }
}

extension CacheControlClientUnsupportedOperation: LocalizedError {
    public var errorDescription: String? { message }
}

extension CacheControlClientRequestValidationError: LocalizedError {
    public var errorDescription: String? { message }
}
