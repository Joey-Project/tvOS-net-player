import Foundation
import GRPCCore
import GRPCNIOTransportHTTP2TransportServices

public final class GRPCCacheControlClient: CacheControlClient {
    private let endpoint: CacheServerEndpoint
    private let rpcTimeout: Duration
    private let maxLibraryPages: Int
    private let maxLibraryItems: Int
    private let allowPartialLibraryResults: Bool

    public init(
        endpoint: CacheServerEndpoint,
        rpcTimeout: Duration = .seconds(10),
        maxLibraryPages: Int = 100,
        maxLibraryItems: Int = 5_000,
        allowPartialLibraryResults: Bool = false
    ) {
        self.endpoint = endpoint
        self.rpcTimeout = rpcTimeout
        self.maxLibraryPages = max(1, maxLibraryPages)
        self.maxLibraryItems = max(1, maxLibraryItems)
        self.allowPartialLibraryResults = allowPartialLibraryResults
    }

    public func getServerInfo() async throws -> CacheServerSummary {
        try await withGRPCClient(
            transport: .http2NIOTS(
                target: endpoint.grpcTarget,
                transportSecurity: .plaintext
            )
        ) { client in
            let service = TvosNetPlayer_V1_ServerService.Client(wrapping: client)
            let response = try await service.getServerInfo(
                TvosNetPlayer_V1_GetServerInfoRequest(),
                options: callOptions
            )
            return CacheServerSummary(response)
        }
    }

    public func listLibraryItemsPage(
        pageToken: String = "",
        pageSize: Int = 50,
        searchText: String? = nil
    ) async throws -> CacheLibraryItemsPage {
        try await withGRPCClient(
            transport: .http2NIOTS(
                target: endpoint.grpcTarget,
                transportSecurity: .plaintext
            )
        ) { client in
            let service = TvosNetPlayer_V1_LibraryService.Client(wrapping: client)
            let request = Self.listLibraryItemsRequest(
                pageToken: pageToken,
                pageSize: pageSize,
                searchText: searchText
            )
            let response = try await service.listLibraryItems(request, options: callOptions)
            return CacheLibraryItemsPage(response)
        }
    }

    public func listLibraryItems(pageSize: Int = 50, searchText: String? = nil) async throws -> [CacheLibraryItem] {
        try await withGRPCClient(
            transport: .http2NIOTS(
                target: endpoint.grpcTarget,
                transportSecurity: .plaintext
            )
        ) { client in
            let service = TvosNetPlayer_V1_LibraryService.Client(wrapping: client)
            return try await collectCacheLibraryItems(
                maxPages: maxLibraryPages,
                maxItems: maxLibraryItems,
                allowPartialResults: allowPartialLibraryResults
            ) { pageToken in
                let request = Self.listLibraryItemsRequest(
                    pageToken: pageToken,
                    pageSize: pageSize,
                    searchText: searchText
                )
                let response = try await service.listLibraryItems(request, options: callOptions)
                return CacheLibraryItemsPage(response)
            }
        }
    }

    public func getPlaybackSource(itemID: String, variantID: String) async throws -> CachePlaybackSource {
        try await withGRPCClient(
            transport: .http2NIOTS(
                target: endpoint.grpcTarget,
                transportSecurity: .plaintext
            )
        ) { client in
            let service = TvosNetPlayer_V1_LibraryService.Client(wrapping: client)
            var request = TvosNetPlayer_V1_GetPlaybackSourceRequest()
            request.itemID = itemID
            request.variantID = variantID
            let response = try await service.getPlaybackSource(request, options: callOptions)
            return CachePlaybackSource(response)
        }
    }

    public func createBilibiliPlaybackTask(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions = BilibiliPlaybackTaskOptions()
    ) async throws -> CacheTask {
        try await withGRPCClient(
            transport: .http2NIOTS(
                target: endpoint.grpcTarget,
                transportSecurity: .plaintext
            )
        ) { client in
            let service = TvosNetPlayer_V1_TaskService.Client(wrapping: client)
            var request = TvosNetPlayer_V1_CreateBilibiliPlaybackTaskRequest()
            request.urlOrID = urlOrID
            request.options = TvosNetPlayer_V1_BilibiliPlaybackOptions(options)
            let response = try await service.createBilibiliPlaybackTask(request, options: callOptions)
            return CacheTask(response)
        }
    }

    public func getTask(id: String) async throws -> CacheTask {
        try await withGRPCClient(
            transport: .http2NIOTS(
                target: endpoint.grpcTarget,
                transportSecurity: .plaintext
            )
        ) { client in
            let service = TvosNetPlayer_V1_TaskService.Client(wrapping: client)
            var request = TvosNetPlayer_V1_GetTaskRequest()
            request.id = id
            let response = try await service.getTask(request, options: callOptions)
            return CacheTask(response)
        }
    }

    public func cancelTask(id: String) async throws -> CacheTask {
        try await withGRPCClient(
            transport: .http2NIOTS(
                target: endpoint.grpcTarget,
                transportSecurity: .plaintext
            )
        ) { client in
            let service = TvosNetPlayer_V1_TaskService.Client(wrapping: client)
            var request = TvosNetPlayer_V1_CancelTaskRequest()
            request.id = id
            let response = try await service.cancelTask(request, options: callOptions)
            return CacheTask(response)
        }
    }

    public func watchTasks(ids: [String] = []) async -> AsyncThrowingStream<CacheTask, Error> {
        AsyncThrowingStream { continuation in
            let streamTask = Task {
                do {
                    try await withGRPCClient(
                        transport: .http2NIOTS(
                            target: endpoint.grpcTarget,
                            transportSecurity: .plaintext
                        )
                    ) { client in
                        let service = TvosNetPlayer_V1_TaskService.Client(wrapping: client)
                        var request = TvosNetPlayer_V1_WatchTasksRequest()
                        request.ids = ids
                        try await service.watchTasks(request, options: streamCallOptions) { response in
                            for try await event in response.messages where event.hasTask {
                                continuation.yield(CacheTask(event.task))
                            }
                        }
                    }
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }

            continuation.onTermination = { _ in
                streamTask.cancel()
            }
        }
    }

    private var callOptions: CallOptions {
        var options = CallOptions.defaults
        options.timeout = rpcTimeout
        return options
    }

    private var streamCallOptions: CallOptions {
        CallOptions.defaults
    }

    private static func listLibraryItemsRequest(
        pageToken: String,
        pageSize: Int,
        searchText: String?
    ) -> TvosNetPlayer_V1_ListLibraryItemsRequest {
        var request = TvosNetPlayer_V1_ListLibraryItemsRequest()
        request.pageSize = Int32(clamping: max(1, min(pageSize, 200)))
        request.pageToken = pageToken

        let trimmedSearchText = searchText?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        if !trimmedSearchText.isEmpty {
            request.filter.searchText = trimmedSearchText
        }

        return request
    }
}

extension CacheServerEndpoint {
    var grpcTarget: any ResolvableTarget {
        if isIPv6Literal {
            return .ipv6(address: host, port: port)
        }

        return .dns(host: host, port: port)
    }
}

enum CacheLibraryPaginationError: Error, Equatable {
    case repeatedPageToken(String)
    case exceededPageLimit(Int)
    case exceededItemLimit(Int)
}

func collectCacheLibraryItems(
    maxPages: Int = 100,
    maxItems: Int = 5_000,
    allowPartialResults: Bool = false,
    fetchPage: (String) async throws -> CacheLibraryItemsPage
) async throws -> [CacheLibraryItem] {
    var allItems: [CacheLibraryItem] = []
    var pageToken = ""
    var seenNextPageTokens = Set<String>()
    var pageCount = 0

    while true {
        pageCount += 1
        let page = try await fetchPage(pageToken)
        allItems.append(contentsOf: page.items)

        guard allItems.count <= maxItems else {
            throw CacheLibraryPaginationError.exceededItemLimit(maxItems)
        }

        guard !page.nextPageToken.isEmpty else {
            return allItems
        }

        guard pageCount < maxPages else {
            if allowPartialResults {
                return allItems
            }

            throw CacheLibraryPaginationError.exceededPageLimit(maxPages)
        }

        guard seenNextPageTokens.insert(page.nextPageToken).inserted else {
            throw CacheLibraryPaginationError.repeatedPageToken(page.nextPageToken)
        }

        pageToken = page.nextPageToken
    }
}

extension CacheServerSummary {
    fileprivate init(_ proto: TvosNetPlayer_V1_ServerInfo) {
        self.init(
            id: proto.id,
            name: proto.name,
            version: proto.version,
            mediaBaseURIs: proto.mediaBaseUris,
            capabilities: proto.capabilities.map { String(describing: $0) }
        )
    }
}

extension CacheLibraryItem {
    fileprivate init(_ proto: TvosNetPlayer_V1_LibraryItem) {
        self.init(
            id: proto.id,
            title: proto.title,
            subtitle: proto.subtitle,
            source: String(describing: proto.source),
            sourceID: proto.sourceID,
            posterURI: proto.posterUri,
            variants: proto.variants.map(CacheMediaVariant.init)
        )
    }
}

extension CacheLibraryItemsPage {
    fileprivate init(_ proto: TvosNetPlayer_V1_ListLibraryItemsResponse) {
        self.init(
            items: proto.items.map(CacheLibraryItem.init),
            nextPageToken: proto.nextPageToken
        )
    }
}

extension CacheMediaVariant {
    fileprivate init(_ proto: TvosNetPlayer_V1_MediaVariant) {
        self.init(
            id: proto.id,
            label: proto.label,
            playbackProtocol: String(describing: proto.`protocol`),
            container: proto.container,
            videoCodec: proto.videoCodec,
            audioCodec: proto.audioCodec,
            width: Int(proto.width),
            height: Int(proto.height),
            bitrate: proto.bitrate,
            sizeBytes: proto.sizeBytes
        )
    }
}

extension CachePlaybackSource {
    fileprivate init(_ proto: TvosNetPlayer_V1_PlaybackSource) {
        self.init(
            itemID: proto.itemID,
            variantID: proto.variantID,
            playbackProtocol: String(describing: proto.`protocol`),
            uri: proto.uri
        )
    }
}

extension CacheTask {
    fileprivate init(_ proto: TvosNetPlayer_V1_Task) {
        self.init(
            id: proto.id,
            kind: String(describing: proto.kind),
            state: String(describing: proto.state),
            source: proto.source,
            title: proto.title,
            progress: proto.progress,
            message: proto.message,
            libraryItemID: proto.libraryItemID,
            playbackSource: proto.hasPlaybackSource ? CachePlaybackSource(proto.playbackSource) : nil,
            playbackSession: proto.hasPlaybackSession ? CacheBilibiliPlaybackSession(proto.playbackSession) : nil
        )
    }
}

extension CacheBilibiliPlaybackSession {
    fileprivate init(_ proto: TvosNetPlayer_V1_BilibiliPlaybackSession) {
        self.init(
            id: proto.id,
            title: proto.title,
            contentID: proto.contentID,
            selectedVariantID: proto.selectedVariantID,
            selectedVariant: proto.hasSelectedVariant
                ? CacheBilibiliPlaybackVariant(proto.selectedVariant)
                : nil,
            variants: proto.variants.map(CacheBilibiliPlaybackVariant.init)
        )
    }
}

extension CacheBilibiliPlaybackVariant {
    fileprivate init(_ proto: TvosNetPlayer_V1_BilibiliPlaybackVariant) {
        self.init(
            id: proto.id,
            label: proto.label,
            sourceKind: proto.sourceKind,
            container: proto.container,
            videoCodec: proto.videoCodec,
            audioCodec: proto.audioCodec,
            width: Int(proto.width),
            height: Int(proto.height),
            bitrate: proto.bitrate,
            sizeBytes: proto.sizeBytes
        )
    }
}

extension TvosNetPlayer_V1_BilibiliPlaybackOptions {
    fileprivate init(_ options: BilibiliPlaybackTaskOptions) {
        self.init()
        qualityPreference = options.qualityPreference
        encodingPreference = options.encodingPreference
        preferTvApi = options.preferTVAPI
    }
}
