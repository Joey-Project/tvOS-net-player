public protocol CacheControlClient: Sendable {
    func getServerInfo() async throws -> CacheServerSummary
    func listLibraryItems(pageSize: Int) async throws -> [CacheLibraryItem]
    func getPlaybackSource(itemID: String, variantID: String?) async throws -> CachePlaybackSource
}
