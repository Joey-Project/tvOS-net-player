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
    func getBilibiliCredentialStatus() async throws -> BilibiliCredentialStatus
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
    func watchTasks(ids: [String]) async -> AsyncThrowingStream<CacheTask, Error>
    func cancelTask(id: String) async throws -> CacheTask
    func resolveBilibiliInput(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> BilibiliResolveResult
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
}

public enum CacheControlClientUnsupportedFeature: Error, Equatable {
    case hlsCacheStatus
    case bilibiliCredentialStatus
    case bilibiliResolve
    case bilibiliDownloadTask
    case bilibiliTaskSelection
    case playbackProgressReporting
}

public extension CacheControlClient {
    func getHLSCacheStatus() async throws -> HLSCacheStatus {
        throw CacheControlClientUnsupportedFeature.hlsCacheStatus
    }

    func reportPlaybackProgress(_ report: PlaybackProgressReport) async throws -> PlaybackProgressReportResult {
        throw CacheControlClientUnsupportedFeature.playbackProgressReporting
    }

    func getBilibiliCredentialStatus() async throws -> BilibiliCredentialStatus {
        throw CacheControlClientUnsupportedFeature.bilibiliCredentialStatus
    }

    func resolveBilibiliInput(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> BilibiliResolveResult {
        throw CacheControlClientUnsupportedFeature.bilibiliResolve
    }

    func createBilibiliTask(
        urlOrID: String,
        options: BilibiliDownloadTaskOptions
    ) async throws -> CacheTask {
        throw CacheControlClientUnsupportedFeature.bilibiliDownloadTask
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

    func watchTask(id: String) async -> AsyncThrowingStream<CacheTask, Error> {
        await watchTasks(ids: [id])
    }
}
