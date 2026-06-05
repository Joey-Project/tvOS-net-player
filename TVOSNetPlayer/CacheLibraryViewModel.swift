import Foundation
import TVOSNetPlayerCacheClient

@MainActor
final class CacheLibraryViewModel: ObservableObject {
    static let serverAddressDefaultsKey = "CacheServerAddress"

    @Published var serverAddressText: String
    @Published private(set) var serverName: String = "LAN cache"
    @Published private(set) var statusMessage: String = "Cache server not connected."
    @Published private(set) var errorMessage: String?
    @Published private(set) var isLoading = false
    @Published private(set) var items: [CacheLibraryItem] = []

    private let defaults: UserDefaults
    private let clientFactory: @Sendable (CacheServerEndpoint) -> any CacheControlClient

    init(
        defaultServerAddressText: String? = nil,
        defaults: UserDefaults = .standard,
        clientFactory: @escaping @Sendable (CacheServerEndpoint) -> any CacheControlClient = {
            GRPCCacheControlClient(endpoint: $0)
        }
    ) {
        self.defaults = defaults
        self.clientFactory = clientFactory
        serverAddressText =
            defaultServerAddressText ?? defaults.string(forKey: Self.serverAddressDefaultsKey) ?? ""
    }

    var canRefresh: Bool {
        !serverAddressText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && !isLoading
    }

    func refresh() async {
        guard let endpoint = CacheServerEndpoint.normalized(from: serverAddressText) else {
            errorMessage = "Use a host and optional port, such as mac-mini.local:50051."
            statusMessage = "Cache server address is invalid."
            return
        }

        isLoading = true
        errorMessage = nil
        statusMessage = "Connecting to \(endpoint.displayAddress)..."

        do {
            let client = clientFactory(endpoint)
            let serverInfo = try await client.getServerInfo()
            let libraryItems = try await client.listLibraryItems(pageSize: 50)

            serverName = serverInfo.name.isEmpty ? endpoint.displayAddress : serverInfo.name
            items = libraryItems
            serverAddressText = endpoint.displayAddress
            defaults.set(endpoint.displayAddress, forKey: Self.serverAddressDefaultsKey)
            statusMessage = "Loaded \(libraryItems.count) cached item(s) from \(serverName)."
        } catch {
            errorMessage = error.localizedDescription
            statusMessage = "Could not load cache library."
        }

        isLoading = false
    }

    func playbackURL(for item: CacheLibraryItem) async -> URL? {
        guard let endpoint = CacheServerEndpoint.normalized(from: serverAddressText) else {
            errorMessage = "Use a host and optional port, such as mac-mini.local:50051."
            statusMessage = "Cache server address is invalid."
            return nil
        }

        isLoading = true
        errorMessage = nil
        statusMessage = "Preparing \(item.displayTitle)..."

        do {
            let source = try await clientFactory(endpoint).getPlaybackSource(
                itemID: item.id,
                variantID: item.primaryVariantID
            )

            guard let url = StreamURLNormalizer.normalizedHTTPURL(from: source.uri) else {
                errorMessage = "Cache server returned a non-HTTP playback URL."
                statusMessage = "Cannot play \(item.displayTitle)."
                isLoading = false
                return nil
            }

            statusMessage = "Playing \(item.displayTitle) from \(serverName)."
            isLoading = false
            return url
        } catch {
            errorMessage = error.localizedDescription
            statusMessage = "Could not prepare \(item.displayTitle)."
            isLoading = false
            return nil
        }
    }
}
