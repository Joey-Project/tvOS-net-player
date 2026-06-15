import Combine
import Foundation
import TVOSNetPlayerCacheClient

@MainActor
public final class CacheLibraryViewModel: ObservableObject {
    public static let serverAddressDefaultsKey = "CacheServerAddress"
    private static let libraryPageSize = 50

    @Published public var serverAddressText: String {
        didSet {
            clearLoadedLibraryIfNeeded(previousValue: oldValue)
        }
    }
    @Published public var searchText: String = "" {
        didSet {
            markSearchPendingIfNeeded(previousValue: oldValue)
        }
    }
    @Published public private(set) var serverName: String = "LAN cache"
    @Published public private(set) var statusMessage: String = "Cache server not connected."
    @Published public private(set) var errorMessage: String?
    @Published public private(set) var isLoading = false
    @Published public private(set) var isLoadingMore = false
    @Published public private(set) var activeSearchText = ""
    @Published public private(set) var items: [CacheLibraryItem] = []
    @Published public private(set) var cacheRoots: [CacheRoot] = []
    @Published public private(set) var deletingItemIDs: Set<String> = []
    @Published public private(set) var canDeleteLibraryItems = false

    private let defaults: UserDefaults
    private let clientFactory: @Sendable (CacheServerEndpoint) -> any CacheControlClient
    private let operationTimeout: Duration
    private var loadedEndpoint: CacheServerEndpoint?
    private var nextPageToken = ""
    private var requestedLibraryPageTokens: Set<String> = []
    private var refreshSequence = 0
    private var loadMoreSequence = 0
    private var deleteOperationSequence = 0
    private var deletingItemOperationIDs: [String: Int] = [:]
    private var playbackSequence = 0
    private var pendingPlaybackItemID: String?
    private var activePlaybackItemID: String?

    public init(
        defaultServerAddressText: String? = nil,
        defaults: UserDefaults = .standard,
        operationTimeout: Duration = .seconds(10),
        clientFactory: @escaping @Sendable (CacheServerEndpoint) -> any CacheControlClient = {
            GRPCCacheControlClient(endpoint: $0)
        }
    ) {
        self.defaults = defaults
        self.clientFactory = clientFactory
        self.operationTimeout = operationTimeout
        serverAddressText =
            defaultServerAddressText ?? defaults.string(forKey: Self.serverAddressDefaultsKey) ?? ""
    }

    public var canRefresh: Bool {
        !serverAddressText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && !isLoading
            && deletingItemIDs.isEmpty
    }

    public var hasMoreItems: Bool {
        !nextPageToken.isEmpty && !hasPendingSearch
    }

    public var canLoadMore: Bool {
        loadedEndpoint != nil && hasMoreItems && !isLoading && !isLoadingMore && deletingItemIDs.isEmpty
    }

    public var cacheRootSummary: String {
        guard !cacheRoots.isEmpty else {
            return "Cache roots unavailable."
        }

        return
            cacheRoots
            .map { "\($0.displayLabel): \($0.capacityLabel)" }
            .joined(separator: "  ")
    }

    public var hasPendingSearch: Bool {
        normalizedSearchText != activeSearchText
    }

    public func canDelete(_ item: CacheLibraryItem) -> Bool {
        loadedEndpoint != nil
            && canDeleteLibraryItems
            && items.contains(where: { $0.id == item.id })
            && !deletingItemIDs.contains(item.id)
            && !isLoading
            && !isLoadingMore
    }

    public func refresh() async {
        guard deletingItemIDs.isEmpty else {
            return
        }

        refreshSequence += 1
        loadMoreSequence += 1
        playbackSequence += 1
        pendingPlaybackItemID = nil
        activePlaybackItemID = nil
        deletingItemIDs = []
        deletingItemOperationIDs = [:]
        isLoadingMore = false
        let requestSequence = refreshSequence
        let requestedSearchText = normalizedSearchText
        let requestSearchText = Self.searchTextForRequest(requestedSearchText)

        guard let endpoint = CacheServerEndpoint.normalized(from: serverAddressText) else {
            clearLoadedLibrary(
                statusMessage: "Cache server address is invalid.",
                errorMessage: "Use a host and optional port, such as mac-mini.local:50051."
            )
            return
        }

        isLoading = true
        errorMessage = nil
        statusMessage = "Connecting to \(endpoint.displayAddress)..."

        do {
            let client = clientFactory(endpoint)
            let serverInfo = try await Self.withOperationTimeout(operationTimeout) {
                try await client.getServerInfo()
            }
            let cacheRoots = try await Self.withOperationTimeout(operationTimeout) {
                try await client.listCacheRoots()
            }
            let libraryPage = try await Self.withOperationTimeout(operationTimeout) {
                try await client.listLibraryItemsPage(
                    pageToken: "",
                    pageSize: Self.libraryPageSize,
                    searchText: requestSearchText
                )
            }

            guard isCurrentRefresh(requestSequence, endpoint: endpoint, searchText: requestedSearchText) else {
                return
            }

            loadedEndpoint = endpoint
            serverName = serverInfo.name.isEmpty ? endpoint.displayAddress : serverInfo.name
            canDeleteLibraryItems = serverInfo.supportsLibraryItemDelete
            self.cacheRoots = cacheRoots
            items = libraryPage.items
            nextPageToken = libraryPage.nextPageToken
            requestedLibraryPageTokens = [""]
            activeSearchText = requestedSearchText
            serverAddressText = endpoint.displayAddress
            defaults.set(endpoint.displayAddress, forKey: Self.serverAddressDefaultsKey)
            statusMessage = loadedLibraryStatusMessage
        } catch {
            guard isCurrentRefresh(requestSequence, endpoint: endpoint, searchText: requestedSearchText) else {
                return
            }

            loadedEndpoint = nil
            items = []
            cacheRoots = []
            deletingItemIDs = []
            canDeleteLibraryItems = false
            nextPageToken = ""
            requestedLibraryPageTokens = []
            activeSearchText = ""
            serverName = "LAN cache"
            errorMessage = error.localizedDescription
            statusMessage = "Could not load cache library."
        }

        isLoading = false
    }

    public func loadMore() async {
        guard canLoadMore else {
            if loadedEndpoint == nil {
                statusMessage = "Refresh cache server to load videos."
            }
            return
        }

        guard let endpoint = CacheServerEndpoint.normalized(from: serverAddressText), endpoint == loadedEndpoint else {
            clearLoadedLibrary(
                statusMessage: "Refresh cache server to load videos.",
                errorMessage: nil
            )
            return
        }

        let requestSequence = refreshSequence
        loadMoreSequence += 1
        let loadMoreRequestSequence = loadMoreSequence
        let requestedSearchText = activeSearchText
        let requestSearchText = Self.searchTextForRequest(requestedSearchText)
        let requestedPageToken = nextPageToken
        isLoading = true
        isLoadingMore = true
        errorMessage = nil
        statusMessage = "Loading more cached videos..."
        defer {
            if isCurrentLoadMore(loadMoreRequestSequence) {
                isLoadingMore = false
                if isCurrentRefresh(requestSequence, endpoint: endpoint, searchText: requestedSearchText) {
                    isLoading = false
                }
            }
        }

        do {
            let client = clientFactory(endpoint)
            let libraryPage = try await Self.withOperationTimeout(operationTimeout) {
                try await client.listLibraryItemsPage(
                    pageToken: requestedPageToken,
                    pageSize: Self.libraryPageSize,
                    searchText: requestSearchText
                )
            }

            guard
                isCurrentLoadMore(loadMoreRequestSequence),
                isCurrentRefresh(requestSequence, endpoint: endpoint, searchText: requestedSearchText)
            else {
                return
            }

            guard !isRepeatedLibraryPageToken(libraryPage.nextPageToken, requestedPageToken: requestedPageToken) else {
                nextPageToken = ""
                errorMessage = "Cache server returned a repeated library page token."
                statusMessage = "Could not load more cached videos."
                return
            }

            items.append(contentsOf: libraryPage.items)
            requestedLibraryPageTokens.insert(requestedPageToken)
            nextPageToken = libraryPage.nextPageToken
            statusMessage = loadedLibraryStatusMessage
        } catch {
            guard
                isCurrentLoadMore(loadMoreRequestSequence),
                isCurrentRefresh(requestSequence, endpoint: endpoint, searchText: requestedSearchText)
            else {
                return
            }

            errorMessage = error.localizedDescription
            statusMessage = "Could not load more cached videos."
        }

    }

    public func deleteItem(
        _ item: CacheLibraryItem,
        onDeleteConfirmed: (() -> Void)? = nil
    ) async -> Bool {
        guard canDelete(item) else {
            if loadedEndpoint == nil {
                statusMessage = "Refresh cache server to manage cached videos."
            }
            return false
        }

        guard let endpoint = CacheServerEndpoint.normalized(from: serverAddressText), endpoint == loadedEndpoint else {
            clearLoadedLibrary(
                statusMessage: "Refresh cache server to manage cached videos.",
                errorMessage: nil
            )
            return false
        }

        let requestSequence = refreshSequence
        deleteOperationSequence += 1
        let deleteOperationID = deleteOperationSequence
        deletingItemOperationIDs[item.id] = deleteOperationID
        deletingItemIDs.insert(item.id)
        defer {
            finishDeletingItem(id: item.id, operationID: deleteOperationID)
        }
        errorMessage = nil
        statusMessage = "Deleting \(item.displayTitle)..."

        let client = clientFactory(endpoint)
        let deleted: Bool
        do {
            deleted = try await Self.withOperationTimeout(operationTimeout) {
                try await client.deleteLibraryItem(id: item.id)
            }
        } catch {
            guard isCurrentRefresh(requestSequence, endpoint: endpoint) else {
                return false
            }

            errorMessage = error.localizedDescription
            statusMessage = "Could not delete \(item.displayTitle)."
            return false
        }

        guard isCurrentRefresh(requestSequence, endpoint: endpoint) else {
            return true
        }

        clearPlaybackIfNeeded(forDeletedItemID: item.id)
        onDeleteConfirmed?()

        await refreshCacheRoots(client: client, requestSequence: requestSequence, endpoint: endpoint)
        guard isCurrentRefresh(requestSequence, endpoint: endpoint) else {
            return true
        }

        if nextPageToken.isEmpty {
            items.removeAll { $0.id == item.id }
        } else {
            do {
                let libraryPage = try await Self.withOperationTimeout(operationTimeout) {
                    try await client.listLibraryItemsPage(
                        pageToken: "",
                        pageSize: Self.libraryPageSize,
                        searchText: Self.searchTextForRequest(self.activeSearchText)
                    )
                }

                guard isCurrentRefresh(requestSequence, endpoint: endpoint) else {
                    return false
                }

                items = libraryPage.items
                nextPageToken = libraryPage.nextPageToken
                requestedLibraryPageTokens = [""]
            } catch {
                guard isCurrentRefresh(requestSequence, endpoint: endpoint) else {
                    return false
                }

                items.removeAll { $0.id == item.id }
                nextPageToken = ""
                requestedLibraryPageTokens = []
                errorMessage = error.localizedDescription
                statusMessage =
                    deleted
                    ? "Deleted \(item.displayTitle), but refresh cache server to load more videos."
                    : "Removed stale \(item.displayTitle), but refresh cache server to load more videos."
                return true
            }
        }

        statusMessage =
            deleted
            ? "Deleted \(item.displayTitle) from \(serverName). \(loadedLibraryStatusMessage)"
            : "Removed stale \(item.displayTitle) from \(serverName). \(loadedLibraryStatusMessage)"
        return true
    }

    private func refreshCacheRoots(
        client: CacheControlClient,
        requestSequence: Int,
        endpoint: CacheServerEndpoint
    ) async {
        do {
            let cacheRoots = try await Self.withOperationTimeout(operationTimeout) {
                try await client.listCacheRoots()
            }
            guard isCurrentRefresh(requestSequence, endpoint: endpoint) else {
                return
            }

            self.cacheRoots = cacheRoots
        } catch {
            // Cache root capacity is advisory; a transient refresh failure should not make
            // the already-completed delete look like it failed.
        }
    }

    public func playbackURL(for item: CacheLibraryItem) async -> URL? {
        guard !deletingItemIDs.contains(item.id) else {
            return nil
        }

        playbackSequence += 1
        pendingPlaybackItemID = nil
        activePlaybackItemID = nil
        let playbackRequestSequence = playbackSequence

        guard let endpoint = CacheServerEndpoint.normalized(from: serverAddressText) else {
            isLoading = false
            errorMessage = "Use a host and optional port, such as mac-mini.local:50051."
            statusMessage = "Cache server address is invalid."
            return nil
        }

        guard loadedEndpoint == endpoint else {
            clearLoadedLibrary(
                statusMessage: "Refresh cache server to load videos.",
                errorMessage: nil
            )
            return nil
        }

        guard let variantID = item.primaryVariantID else {
            isLoading = false
            errorMessage = "Cached item has no playable media variants."
            statusMessage = "Cannot play \(item.displayTitle)."
            return nil
        }

        let requestSequence = refreshSequence
        pendingPlaybackItemID = item.id
        isLoading = true
        errorMessage = nil
        statusMessage = "Preparing \(item.displayTitle)..."

        do {
            let client = clientFactory(endpoint)
            let source = try await Self.withOperationTimeout(operationTimeout) {
                try await client.getPlaybackSource(
                    itemID: item.id,
                    variantID: variantID
                )
            }

            guard
                isCurrentRefresh(requestSequence, endpoint: endpoint),
                isCurrentPlayback(playbackRequestSequence)
            else {
                return nil
            }

            guard source.itemID == item.id && source.variantID == variantID else {
                pendingPlaybackItemID = nil
                errorMessage = "Cache server returned a mismatched playback source."
                statusMessage = "Cannot play \(item.displayTitle)."
                isLoading = false
                return nil
            }

            guard source.isPlayableByTVOSClient else {
                pendingPlaybackItemID = nil
                errorMessage = "Cache server returned an unsupported playback protocol."
                statusMessage = "Cannot play \(item.displayTitle)."
                isLoading = false
                return nil
            }

            guard let url = source.explicitHTTPURL else {
                pendingPlaybackItemID = nil
                errorMessage = "Cache server returned a non-HTTP playback URL."
                statusMessage = "Cannot play \(item.displayTitle)."
                isLoading = false
                return nil
            }

            pendingPlaybackItemID = nil
            statusMessage = "Prepared \(item.displayTitle) from \(serverName)."
            isLoading = false
            return url
        } catch {
            guard
                isCurrentRefresh(requestSequence, endpoint: endpoint),
                isCurrentPlayback(playbackRequestSequence)
            else {
                return nil
            }

            pendingPlaybackItemID = nil
            errorMessage = error.localizedDescription
            statusMessage = "Could not prepare \(item.displayTitle)."
            isLoading = false
            return nil
        }
    }

    public func finishPreparedPlayback(for item: CacheLibraryItem, didStartPlayback: Bool) {
        guard items.contains(where: { $0.id == item.id }) else {
            return
        }

        isLoading = false
        errorMessage = nil
        if didStartPlayback {
            activePlaybackItemID = item.id
            statusMessage = "Playing \(item.displayTitle) from \(serverName)."
        } else {
            activePlaybackItemID = nil
            statusMessage = loadedLibraryStatusMessage
        }
    }

    public func isActivePlaybackItem(_ item: CacheLibraryItem) -> Bool {
        activePlaybackItemID == item.id
    }

    public func clearPlaybackStatus() {
        guard pendingPlaybackItemID != nil || activePlaybackItemID != nil else {
            return
        }

        playbackSequence += 1
        pendingPlaybackItemID = nil
        activePlaybackItemID = nil
        errorMessage = nil
        if isLoadingMore {
            isLoading = true
            statusMessage = "Loading more cached videos..."
        } else {
            isLoading = false
            statusMessage = loadedLibraryStatusMessage
        }
    }

    private var loadedLibraryStatusMessage: String {
        let searchSuffix = activeSearchText.isEmpty ? "" : " matching \"\(activeSearchText)\""
        let moreSuffix = nextPageToken.isEmpty ? "" : " More items available."
        return "Loaded \(items.count) cached item(s)\(searchSuffix) from \(serverName).\(moreSuffix)"
    }

    private var normalizedSearchText: String {
        Self.normalizedSearchText(searchText)
    }

    private static func searchTextForRequest(_ text: String) -> String? {
        text.isEmpty ? nil : text
    }

    private static func normalizedSearchText(_ text: String) -> String {
        text.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private func clearLoadedLibraryIfNeeded(previousValue: String) {
        guard serverAddressText != previousValue else {
            return
        }

        let trimmedAddress = serverAddressText.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmedAddress.isEmpty {
            defaults.removeObject(forKey: Self.serverAddressDefaultsKey)
        }

        let previousEndpoint = CacheServerEndpoint.normalized(from: previousValue)
        let currentEndpoint = CacheServerEndpoint.normalized(from: serverAddressText)
        let endpointChanged = currentEndpoint != previousEndpoint
        let loadedEndpointChanged = loadedEndpoint != nil && currentEndpoint != loadedEndpoint
        let unusableAddress = currentEndpoint == nil
        guard endpointChanged || loadedEndpointChanged || unusableAddress else {
            return
        }

        refreshSequence += 1
        let nextStatusMessage: String
        let nextErrorMessage: String?
        if trimmedAddress.isEmpty {
            nextStatusMessage = "Cache server not connected."
            nextErrorMessage = nil
        } else if currentEndpoint == nil {
            nextStatusMessage = "Cache server address is invalid."
            nextErrorMessage = "Use a host and optional port, such as mac-mini.local:50051."
        } else {
            nextStatusMessage = "Refresh cache server to load videos."
            nextErrorMessage = nil
        }

        clearLoadedLibrary(
            statusMessage: nextStatusMessage,
            errorMessage: nextErrorMessage
        )
    }

    private func clearLoadedLibrary(statusMessage: String, errorMessage: String?) {
        loadedEndpoint = nil
        loadMoreSequence += 1
        pendingPlaybackItemID = nil
        activePlaybackItemID = nil
        items = []
        cacheRoots = []
        deletingItemIDs = []
        deletingItemOperationIDs = [:]
        canDeleteLibraryItems = false
        nextPageToken = ""
        requestedLibraryPageTokens = []
        activeSearchText = ""
        isLoadingMore = false
        serverName = "LAN cache"
        isLoading = false
        self.statusMessage = statusMessage
        self.errorMessage = errorMessage
    }

    private func markSearchPendingIfNeeded(previousValue: String) {
        guard
            normalizedSearchText != Self.normalizedSearchText(previousValue),
            loadedEndpoint != nil || isLoading,
            deletingItemIDs.isEmpty
        else {
            return
        }

        playbackSequence += 1
        refreshSequence += 1
        loadMoreSequence += 1
        pendingPlaybackItemID = nil
        activePlaybackItemID = nil
        deletingItemIDs = []
        deletingItemOperationIDs = [:]
        isLoading = false
        isLoadingMore = false
        errorMessage = nil
        if loadedEndpoint == nil {
            statusMessage = "Refresh cache server to load videos."
        } else if hasPendingSearch {
            statusMessage = "Search cache library to update results."
        } else {
            statusMessage = loadedLibraryStatusMessage
        }
    }

    private func isCurrentRefresh(
        _ requestSequence: Int,
        endpoint: CacheServerEndpoint,
        searchText: String
    ) -> Bool {
        isCurrentRefresh(requestSequence, endpoint: endpoint)
            && normalizedSearchText == searchText
    }

    private func isCurrentRefresh(_ requestSequence: Int, endpoint: CacheServerEndpoint) -> Bool {
        requestSequence == refreshSequence
            && CacheServerEndpoint.normalized(from: serverAddressText) == endpoint
    }

    private func isCurrentLoadMore(_ requestSequence: Int) -> Bool {
        requestSequence == loadMoreSequence
    }

    private func isRepeatedLibraryPageToken(_ pageToken: String, requestedPageToken: String) -> Bool {
        !pageToken.isEmpty && (pageToken == requestedPageToken || requestedLibraryPageTokens.contains(pageToken))
    }

    private func isCurrentPlayback(_ requestSequence: Int) -> Bool {
        requestSequence == playbackSequence
    }

    private func clearPlaybackIfNeeded(forDeletedItemID itemID: String) {
        guard pendingPlaybackItemID == itemID || activePlaybackItemID == itemID else {
            return
        }

        playbackSequence += 1
        pendingPlaybackItemID = nil
        activePlaybackItemID = nil
    }

    private func finishDeletingItem(id itemID: String, operationID: Int) {
        guard deletingItemOperationIDs[itemID] == operationID else {
            return
        }

        deletingItemOperationIDs[itemID] = nil
        deletingItemIDs.remove(itemID)
    }

    private static func withOperationTimeout<Value: Sendable>(
        _ timeout: Duration,
        operation: @Sendable @escaping () async throws -> Value
    ) async throws -> Value {
        try await withCheckedThrowingContinuation { continuation in
            let race = CacheLibraryOperationTimeoutRace(continuation: continuation)
            race.start(timeout: timeout, operation: operation)
        }
    }
}

private final class CacheLibraryOperationTimeoutRace<Value: Sendable>: @unchecked Sendable {
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
                self.complete(.failure(CacheLibraryOperationError.timedOut))
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

private enum CacheLibraryOperationError: LocalizedError {
    case timedOut

    var errorDescription: String? {
        "Cache server request timed out."
    }
}
