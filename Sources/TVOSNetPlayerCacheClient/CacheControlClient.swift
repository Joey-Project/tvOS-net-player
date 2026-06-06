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
    func listLibraryItemsPage(
        pageToken: String,
        pageSize: Int,
        searchText: String?
    ) async throws -> CacheLibraryItemsPage
    func getPlaybackSource(itemID: String, variantID: String) async throws -> CachePlaybackSource
}

public extension CacheControlClient {
    func listLibraryItemsPage(
        pageSize: Int = 50,
        searchText: String? = nil
    ) async throws -> CacheLibraryItemsPage {
        try await listLibraryItemsPage(pageToken: "", pageSize: pageSize, searchText: searchText)
    }
}
