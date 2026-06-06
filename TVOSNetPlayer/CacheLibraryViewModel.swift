import Combine
import Foundation
import TVOSNetPlayerCacheClient

@MainActor
final class CacheLibraryViewModel: ObservableObject {
    static let serverAddressDefaultsKey = "CacheServerAddress"
    private static let libraryPreviewPageSize = 200

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
            GRPCCacheControlClient(
                endpoint: $0,
                maxLibraryPages: 1,
                maxLibraryItems: 200,
                allowPartialLibraryResults: true
            )
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
            let libraryPage = try await Self.withOperationTimeout(operationTimeout) {
                try await client.listLibraryItemsPage(pageSize: Self.libraryPreviewPageSize)
            }

            guard isCurrentRefresh(requestSequence, endpoint: endpoint) else {
                return
            }

            loadedEndpoint = endpoint
            serverName = serverInfo.name.isEmpty ? endpoint.displayAddress : serverInfo.name
            items = libraryPage.items
            serverAddressText = endpoint.displayAddress
            defaults.set(endpoint.displayAddress, forKey: Self.serverAddressDefaultsKey)
            statusMessage = "Loaded \(libraryPage.items.count) cached item(s) from \(serverName)."
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

            errorMessage = error.localizedDescription
            statusMessage = "Could not prepare \(item.displayTitle)."
            isLoading = false
            return nil
        }
    }

    func finishPreparedPlayback(for item: CacheLibraryItem, didStartPlayback: Bool) {
        guard items.contains(where: { $0.id == item.id }) else {
            return
        }

        isLoading = false
        errorMessage = nil
        if didStartPlayback {
            statusMessage = "Playing \(item.displayTitle) from \(serverName)."
        } else {
            statusMessage = "Loaded \(items.count) cached item(s) from \(serverName)."
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
