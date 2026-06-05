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
                target: .dns(host: endpoint.host, port: endpoint.port),
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
                target: .dns(host: endpoint.host, port: endpoint.port),
                transportSecurity: .plaintext
            )
        ) { client in
            let service = TvosNetPlayer_V1_LibraryService.Client(wrapping: client)
            var request = TvosNetPlayer_V1_ListLibraryItemsRequest()
            request.pageSize = Int32(clamping: pageSize)
            let response = try await service.listLibraryItems(request)
            return response.items.map(CacheLibraryItem.init)
        }
    }

    public func getPlaybackSource(itemID: String, variantID: String? = nil) async throws -> CachePlaybackSource {
        try await withGRPCClient(
            transport: .http2NIOTS(
                target: .dns(host: endpoint.host, port: endpoint.port),
                transportSecurity: .plaintext
            )
        ) { client in
            let service = TvosNetPlayer_V1_LibraryService.Client(wrapping: client)
            var request = TvosNetPlayer_V1_GetPlaybackSourceRequest()
            request.itemID = itemID
            if let variantID {
                request.variantID = variantID
            }
            let response = try await service.getPlaybackSource(request)
            return CachePlaybackSource(response)
        }
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
