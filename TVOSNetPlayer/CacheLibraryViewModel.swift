import Combine
import Foundation
import TVOSNetPlayerCacheClient

@MainActor
final class CacheLibraryViewModel: ObservableObject {
    static let serverAddressDefaultsKey = "CacheServerAddress"

    @Published var serverAddressText: String {
        didSet {
            clearLoadedLibraryIfNeeded(previousValue: oldValue)
        }
    }
    @Published private(set) var serverName: String = "LAN cache"
    @Published private(set) var statusMessage: String = "Cache server not connected."
    @Published private(set) var errorMessage: String?
    @Published private(set) var isLoading = false
    @Published private(set) var items: [CacheLibraryItem] = []

    private let defaults: UserDefaults
    private let clientFactory: @Sendable (CacheServerEndpoint) -> any CacheControlClient
    private let operationTimeout: Duration
    private var loadedEndpoint: CacheServerEndpoint?
    private var refreshSequence = 0
    private var playbackSequence = 0

    init(
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

    var canRefresh: Bool {
        !serverAddressText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && !isLoading
    }

    func refresh() async {
        refreshSequence += 1
        playbackSequence += 1
        let requestSequence = refreshSequence

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
            let libraryItems = try await Self.withOperationTimeout(operationTimeout) {
                try await client.listLibraryItems(pageSize: 50)
            }

            guard isCurrentRefresh(requestSequence, endpoint: endpoint) else {
                return
            }

            loadedEndpoint = endpoint
            serverName = serverInfo.name.isEmpty ? endpoint.displayAddress : serverInfo.name
            items = libraryItems
            serverAddressText = endpoint.displayAddress
            defaults.set(endpoint.displayAddress, forKey: Self.serverAddressDefaultsKey)
            statusMessage = "Loaded \(libraryItems.count) cached item(s) from \(serverName)."
        } catch {
            guard isCurrentRefresh(requestSequence, endpoint: endpoint) else {
                return
            }

            loadedEndpoint = nil
            items = []
            serverName = "LAN cache"
            errorMessage = error.localizedDescription
            statusMessage = "Could not load cache library."
        }

        isLoading = false
    }

    func playbackURL(for item: CacheLibraryItem) async -> URL? {
        playbackSequence += 1
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
                errorMessage = "Cache server returned a mismatched playback source."
                statusMessage = "Cannot play \(item.displayTitle)."
                isLoading = false
                return nil
            }

            guard source.isPlayableByTVOSClient else {
                errorMessage = "Cache server returned an unsupported playback protocol."
                statusMessage = "Cannot play \(item.displayTitle)."
                isLoading = false
                return nil
            }

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
            guard
                isCurrentRefresh(requestSequence, endpoint: endpoint),
                isCurrentPlayback(playbackRequestSequence)
            else {
                return nil
            }

            errorMessage = error.localizedDescription
            statusMessage = "Could not prepare \(item.displayTitle)."
            isLoading = false
            return nil
        }
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
        items = []
        serverName = "LAN cache"
        isLoading = false
        self.statusMessage = statusMessage
        self.errorMessage = errorMessage
    }

    private func isCurrentRefresh(_ requestSequence: Int, endpoint: CacheServerEndpoint) -> Bool {
        requestSequence == refreshSequence && CacheServerEndpoint.normalized(from: serverAddressText) == endpoint
    }

    private func isCurrentPlayback(_ requestSequence: Int) -> Bool {
        requestSequence == playbackSequence
    }

    private static func withOperationTimeout<Value: Sendable>(
        _ timeout: Duration,
        operation: @Sendable @escaping () async throws -> Value
    ) async throws -> Value {
        try await withThrowingTaskGroup(of: Value.self) { group in
            defer {
                group.cancelAll()
            }

            group.addTask {
                try await operation()
            }
            group.addTask {
                try await Task.sleep(for: timeout)
                throw CacheLibraryOperationError.timedOut
            }

            let value = try await group.next()!
            return value
        }
    }
}

private enum CacheLibraryOperationError: LocalizedError {
    case timedOut

    var errorDescription: String? {
        "Cache server request timed out."
    }
}
