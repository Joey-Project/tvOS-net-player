import Foundation

public struct CacheServerEndpoint: Equatable, Sendable {
    public static let defaultPort = 50_051

    public let host: String
    public let port: Int

    public init(host: String, port: Int = Self.defaultPort) {
        self.host = host
        self.port = port
    }

    public var displayAddress: String {
        if host.contains(":") && !(host.hasPrefix("[") && host.hasSuffix("]")) {
            return "[\(host)]:\(port)"
        }

        return "\(host):\(port)"
    }

    var isIPv6Literal: Bool {
        host.contains(":")
    }

    public static func normalized(from text: String, defaultPort: Int = Self.defaultPort) -> Self? {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            return nil
        }

        if let endpoint = unbracketedIPv6Endpoint(from: trimmed, defaultPort: defaultPort) {
            return endpoint
        }

        let candidate = trimmed.contains("://") ? trimmed : "http://\(trimmed)"
        guard
            let components = URLComponents(string: candidate),
            components.scheme?.lowercased() == "http",
            var host = components.host?.trimmingCharacters(in: .whitespacesAndNewlines)
        else {
            return nil
        }

        if host.hasPrefix("[") && host.hasSuffix("]") {
            host = String(host.dropFirst().dropLast())
        }

        guard !host.isEmpty else {
            return nil
        }

        let port = components.port ?? defaultPort
        guard (1...65_535).contains(port) else {
            return nil
        }

        return Self(host: host, port: port)
    }

    private static func unbracketedIPv6Endpoint(from text: String, defaultPort: Int) -> Self? {
        guard
            !text.contains("://"),
            !text.hasPrefix("["),
            !text.contains("/"),
            text.filter({ $0 == ":" }).count > 1
        else {
            return nil
        }

        return Self(host: text, port: defaultPort)
    }
}

public struct CacheServerSummary: Equatable, Sendable {
    public let id: String
    public let name: String
    public let version: String
    public let mediaBaseURIs: [String]
    public let capabilities: [String]

    public init(id: String, name: String, version: String, mediaBaseURIs: [String], capabilities: [String]) {
        self.id = id
        self.name = name
        self.version = version
        self.mediaBaseURIs = mediaBaseURIs
        self.capabilities = capabilities
    }
}

public struct CacheLibraryItem: Identifiable, Equatable, Sendable {
    public let id: String
    public let title: String
    public let subtitle: String
    public let source: String
    public let sourceID: String
    public let posterURI: String
    public let variants: [CacheMediaVariant]

    public init(
        id: String,
        title: String,
        subtitle: String,
        source: String,
        sourceID: String,
        posterURI: String,
        variants: [CacheMediaVariant]
    ) {
        self.id = id
        self.title = title
        self.subtitle = subtitle
        self.source = source
        self.sourceID = sourceID
        self.posterURI = posterURI
        self.variants = variants
    }

    public var displayTitle: String {
        title.isEmpty ? id : title
    }

    public var primaryVariant: CacheMediaVariant? {
        for variant in variants {
            let id = variant.id.trimmingCharacters(in: .whitespacesAndNewlines)
            if !id.isEmpty && variant.isPlayableByTVOSClient {
                return variant
            }
        }

        return nil
    }

    public var primaryVariantID: String? {
        primaryVariant?.id.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    public var hasPlayableVariant: Bool {
        primaryVariantID != nil
    }
}

public struct CacheMediaVariant: Identifiable, Equatable, Sendable {
    public let id: String
    public let label: String
    public let playbackProtocol: String
    public let container: String
    public let videoCodec: String
    public let audioCodec: String
    public let width: Int
    public let height: Int
    public let bitrate: Int64
    public let sizeBytes: Int64

    public init(
        id: String,
        label: String,
        playbackProtocol: String,
        container: String,
        videoCodec: String,
        audioCodec: String,
        width: Int,
        height: Int,
        bitrate: Int64,
        sizeBytes: Int64
    ) {
        self.id = id
        self.label = label
        self.playbackProtocol = playbackProtocol
        self.container = container
        self.videoCodec = videoCodec
        self.audioCodec = audioCodec
        self.width = width
        self.height = height
        self.bitrate = bitrate
        self.sizeBytes = sizeBytes
    }

    public var displayLabel: String {
        if !label.isEmpty {
            return label
        }

        if width > 0 && height > 0 {
            return "\(width)x\(height)"
        }

        return id
    }

    public var isPlayableByTVOSClient: Bool {
        CachePlaybackProtocolSupport.isPlayable(playbackProtocol)
    }
}

public struct CachePlaybackSource: Equatable, Sendable {
    public let itemID: String
    public let variantID: String
    public let playbackProtocol: String
    public let uri: String

    public init(itemID: String, variantID: String, playbackProtocol: String, uri: String) {
        self.itemID = itemID
        self.variantID = variantID
        self.playbackProtocol = playbackProtocol
        self.uri = uri
    }

    public var isPlayableByTVOSClient: Bool {
        CachePlaybackProtocolSupport.isPlayable(playbackProtocol)
    }

    public var explicitHTTPURL: URL? {
        let trimmedURI = uri.trimmingCharacters(in: .whitespacesAndNewlines)
        guard
            let components = URLComponents(string: trimmedURI),
            let scheme = components.scheme?.lowercased(),
            ["http", "https"].contains(scheme),
            let host = components.host,
            !host.isEmpty,
            let url = components.url
        else {
            return nil
        }

        return url
    }
}

public struct BilibiliPlaybackTaskOptions: Equatable, Sendable {
    public let qualityPreference: String
    public let encodingPreference: String
    public let preferTVAPI: Bool

    public init(
        qualityPreference: String = "",
        encodingPreference: String = "",
        preferTVAPI: Bool = false
    ) {
        self.qualityPreference = qualityPreference
        self.encodingPreference = encodingPreference
        self.preferTVAPI = preferTVAPI
    }
}

public struct CacheTask: Identifiable, Equatable, Sendable {
    public let id: String
    public let kind: String
    public let state: String
    public let source: String
    public let title: String
    public let progress: Double
    public let message: String
    public let libraryItemID: String
    public let playbackSource: CachePlaybackSource?
    public let playbackSession: CacheBilibiliPlaybackSession?

    public init(
        id: String,
        kind: String,
        state: String,
        source: String,
        title: String,
        progress: Double,
        message: String,
        libraryItemID: String,
        playbackSource: CachePlaybackSource?,
        playbackSession: CacheBilibiliPlaybackSession?
    ) {
        self.id = id
        self.kind = kind
        self.state = state
        self.source = source
        self.title = title
        self.progress = progress
        self.message = message
        self.libraryItemID = libraryItemID
        self.playbackSource = playbackSource
        self.playbackSession = playbackSession
    }

    public var isProgressivePlayback: Bool {
        kind.normalizedCacheProtocolName.contains("bilibiliprogressiveplayback")
    }
}

public struct CacheBilibiliPlaybackSession: Equatable, Sendable {
    public let id: String
    public let title: String
    public let contentID: String
    public let selectedVariantID: String
    public let selectedVariant: CacheBilibiliPlaybackVariant?
    public let variants: [CacheBilibiliPlaybackVariant]

    public init(
        id: String,
        title: String,
        contentID: String,
        selectedVariantID: String,
        selectedVariant: CacheBilibiliPlaybackVariant?,
        variants: [CacheBilibiliPlaybackVariant]
    ) {
        self.id = id
        self.title = title
        self.contentID = contentID
        self.selectedVariantID = selectedVariantID
        self.selectedVariant = selectedVariant
        self.variants = variants
    }
}

public struct CacheBilibiliPlaybackVariant: Identifiable, Equatable, Sendable {
    public let id: String
    public let label: String
    public let sourceKind: String
    public let container: String
    public let videoCodec: String
    public let audioCodec: String
    public let width: Int
    public let height: Int
    public let bitrate: Int64
    public let sizeBytes: Int64

    public init(
        id: String,
        label: String,
        sourceKind: String,
        container: String,
        videoCodec: String,
        audioCodec: String,
        width: Int,
        height: Int,
        bitrate: Int64,
        sizeBytes: Int64
    ) {
        self.id = id
        self.label = label
        self.sourceKind = sourceKind
        self.container = container
        self.videoCodec = videoCodec
        self.audioCodec = audioCodec
        self.width = width
        self.height = height
        self.bitrate = bitrate
        self.sizeBytes = sizeBytes
    }
}

private enum CachePlaybackProtocolSupport {
    static func isPlayable(_ playbackProtocol: String) -> Bool {
        switch playbackProtocol.normalizedCacheProtocolName.removingPlaybackProtocolPrefix {
        case "httpfile", "hls":
            true
        default:
            false
        }
    }
}

extension String {
    fileprivate var normalizedCacheProtocolName: String {
        lowercased().filter(\.isLetter)
    }

    fileprivate var removingPlaybackProtocolPrefix: String {
        let prefix = "playbackprotocol"
        guard hasPrefix(prefix) else {
            return self
        }

        return String(dropFirst(prefix.count))
    }
}
