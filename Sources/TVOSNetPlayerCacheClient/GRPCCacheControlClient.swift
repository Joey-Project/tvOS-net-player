import Foundation
import GRPCCore
import GRPCNIOTransportHTTP2TransportServices

public final class GRPCCacheControlClient: CacheControlClient {
    private let endpoint: CacheServerEndpoint

    public init(endpoint: CacheServerEndpoint) {
        self.endpoint = endpoint
    }

    public func getServerInfo() async throws -> CacheServerSummary {
        try await withGRPCClient(
            transport: .http2NIOTS(
                target: endpoint.grpcTarget,
                transportSecurity: .plaintext
            )
        ) { client in
            let service = TvosNetPlayer_V1_ServerService.Client(wrapping: client)
            let response = try await service.getServerInfo(TvosNetPlayer_V1_GetServerInfoRequest())
            return CacheServerSummary(response)
        }
    }

    public func listLibraryItems(pageSize: Int = 50) async throws -> [CacheLibraryItem] {
        try await withGRPCClient(
            transport: .http2NIOTS(
                target: endpoint.grpcTarget,
                transportSecurity: .plaintext
            )
        ) { client in
            let service = TvosNetPlayer_V1_LibraryService.Client(wrapping: client)
            return try await collectCacheLibraryItems { pageToken in
                var request = TvosNetPlayer_V1_ListLibraryItemsRequest()
                request.pageSize = Int32(clamping: max(1, pageSize))
                request.pageToken = pageToken

                let response = try await service.listLibraryItems(request)
                return CacheLibraryItemsPage(
                    items: response.items.map(CacheLibraryItem.init),
                    nextPageToken: response.nextPageToken
                )
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
            let response = try await service.getPlaybackSource(request)
            return CachePlaybackSource(response)
        }
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

struct CacheLibraryItemsPage: Equatable, Sendable {
    let items: [CacheLibraryItem]
    let nextPageToken: String
}

enum CacheLibraryPaginationError: Error, Equatable {
    case repeatedPageToken(String)
}

func collectCacheLibraryItems(
    fetchPage: (String) async throws -> CacheLibraryItemsPage
) async throws -> [CacheLibraryItem] {
    var allItems: [CacheLibraryItem] = []
    var pageToken = ""
    var seenNextPageTokens = Set<String>()

    while true {
        let page = try await fetchPage(pageToken)
        allItems.append(contentsOf: page.items)

        guard !page.nextPageToken.isEmpty else {
            return allItems
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
