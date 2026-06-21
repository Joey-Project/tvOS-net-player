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

    public var supportsLibraryItemDelete: Bool {
        capabilities.contains(CacheServerCapability.libraryItemDelete)
    }

    public var supportsBilibiliResolve: Bool {
        capabilities.contains(CacheServerCapability.bilibiliResolve)
    }

    public var supportsBilibiliTaskSelection: Bool {
        capabilities.contains(CacheServerCapability.bilibiliTaskSelection)
    }
}

public enum CacheServerCapability {
    public static let bilibiliResolve = "bilibiliResolve"
    public static let bilibiliTaskSelection = "bilibiliTaskSelection"
    public static let libraryItemDelete = "libraryItemDelete"
}

public struct CacheRoot: Identifiable, Equatable, Sendable {
    public let id: String
    public let label: String
    public let kind: String
    public let path: String
    public let writable: Bool
    public let freeBytes: Int64
    public let totalBytes: Int64

    public init(
        id: String,
        label: String,
        kind: String,
        path: String,
        writable: Bool,
        freeBytes: Int64,
        totalBytes: Int64
    ) {
        self.id = id
        self.label = label
        self.kind = kind
        self.path = path
        self.writable = writable
        self.freeBytes = freeBytes
        self.totalBytes = totalBytes
    }

    public var displayLabel: String {
        label.isEmpty ? id : label
    }

    public var accessLabel: String {
        writable ? "Writable" : "Read only"
    }

    public var capacityLabel: String {
        guard totalBytes > 0 else {
            return accessLabel
        }

        let free = ByteCountFormatter.string(fromByteCount: freeBytes, countStyle: .file)
        let total = ByteCountFormatter.string(fromByteCount: totalBytes, countStyle: .file)
        return "\(free) free of \(total)"
    }
}

public struct HLSCacheStatus: Equatable, Sendable {
    public let evictionEnabled: Bool
    public let maxBytes: Int64
    public let highWatermarkPercent: Int
    public let lowWatermarkPercent: Int
    public let highWatermarkBytes: Int64
    public let lowWatermarkBytes: Int64
    public let usedBytes: Int64
    public let completedSessionCount: Int
    public let lastEviction: HLSCacheEvictionSummary?

    public init(
        evictionEnabled: Bool,
        maxBytes: Int64,
        highWatermarkPercent: Int,
        lowWatermarkPercent: Int,
        highWatermarkBytes: Int64,
        lowWatermarkBytes: Int64,
        usedBytes: Int64,
        completedSessionCount: Int,
        lastEviction: HLSCacheEvictionSummary?
    ) {
        self.evictionEnabled = evictionEnabled
        self.maxBytes = maxBytes
        self.highWatermarkPercent = highWatermarkPercent
        self.lowWatermarkPercent = lowWatermarkPercent
        self.highWatermarkBytes = highWatermarkBytes
        self.lowWatermarkBytes = lowWatermarkBytes
        self.usedBytes = usedBytes
        self.completedSessionCount = completedSessionCount
        self.lastEviction = lastEviction
    }
}

public struct HLSCacheEvictionSummary: Equatable, Sendable {
    public let reason: String
    public let startedUsedBytes: Int64
    public let finishedUsedBytes: Int64
    public let targetUsedBytes: Int64
    public let projectedAddedBytes: Int64
    public let evictedBytes: Int64
    public let evictedSessionIDs: [String]
    public let targetReached: Bool
    public let completedAt: Date?

    public init(
        reason: String,
        startedUsedBytes: Int64,
        finishedUsedBytes: Int64,
        targetUsedBytes: Int64,
        projectedAddedBytes: Int64,
        evictedBytes: Int64,
        evictedSessionIDs: [String],
        targetReached: Bool,
        completedAt: Date?
    ) {
        self.reason = reason
        self.startedUsedBytes = startedUsedBytes
        self.finishedUsedBytes = finishedUsedBytes
        self.targetUsedBytes = targetUsedBytes
        self.projectedAddedBytes = projectedAddedBytes
        self.evictedBytes = evictedBytes
        self.evictedSessionIDs = evictedSessionIDs
        self.targetReached = targetReached
        self.completedAt = completedAt
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

    public var isOfflineHLSCache: Bool {
        source.normalizedCacheProtocolName.removingLibrarySourcePrefix == "bilibili"
            && primaryVariant?.isHLS == true
    }

    public var availabilityLabel: String {
        if isOfflineHLSCache {
            return "Offline HLS"
        }

        switch source.normalizedCacheProtocolName.removingLibrarySourcePrefix {
        case "localcache":
            return "LAN file"
        case "mountedsmb":
            return "Mounted SMB"
        case "bilibili":
            return "Bilibili"
        default:
            return source
        }
    }

    public var availabilitySystemImage: String {
        if isOfflineHLSCache {
            return "externaldrive.fill.badge.checkmark"
        }

        switch source.normalizedCacheProtocolName.removingLibrarySourcePrefix {
        case "localcache":
            return "internaldrive"
        case "mountedsmb":
            return "network"
        case "bilibili":
            return "play.tv"
        default:
            return hasPlayableVariant ? "play.rectangle" : "xmark.octagon"
        }
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

    public var isHLS: Bool {
        playbackProtocol.normalizedCacheProtocolName.removingPlaybackProtocolPrefix == "hls"
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
    public let audioLanguagePreference: String
    public let preferTVAPI: Bool

    public init(
        qualityPreference: String = "",
        encodingPreference: String = "",
        audioLanguagePreference: String = "",
        preferTVAPI: Bool = false
    ) {
        self.qualityPreference = qualityPreference
        self.encodingPreference = encodingPreference
        self.audioLanguagePreference = audioLanguagePreference
        self.preferTVAPI = preferTVAPI
    }
}

public struct BilibiliResolveResult: Equatable, Sendable {
    public let source: String
    public let title: String
    public let sourceKind: String
    public let candidates: [BilibiliResolvedCandidate]
    public let defaultSelectionID: String
    public let candidatesTruncated: Bool

    public init(
        source: String,
        title: String,
        sourceKind: String,
        candidates: [BilibiliResolvedCandidate],
        defaultSelectionID: String
    ) {
        self.init(
            source: source,
            title: title,
            sourceKind: sourceKind,
            candidates: candidates,
            defaultSelectionID: defaultSelectionID,
            candidatesTruncated: false
        )
    }

    public init(
        source: String,
        title: String,
        sourceKind: String,
        candidates: [BilibiliResolvedCandidate],
        defaultSelectionID: String,
        candidatesTruncated: Bool
    ) {
        self.source = source
        self.title = title
        self.sourceKind = sourceKind
        self.candidates = candidates
        self.defaultSelectionID = defaultSelectionID
        self.candidatesTruncated = candidatesTruncated
    }

    public var requiresSelection: Bool {
        candidates.count > 1
    }
}

public struct BilibiliResolvedCandidate: Identifiable, Equatable, Sendable {
    public var id: String { selectionID }

    public let selectionID: String
    public let title: String
    public let subtitle: String
    public let sourceKind: String
    public let contentID: String
    public let index: Int
    public let durationSeconds: Int64
    public let coverURI: String

    public init(
        selectionID: String,
        title: String,
        subtitle: String,
        sourceKind: String,
        contentID: String,
        index: Int,
        durationSeconds: Int64,
        coverURI: String
    ) {
        self.selectionID = selectionID
        self.title = title
        self.subtitle = subtitle
        self.sourceKind = sourceKind
        self.contentID = contentID
        self.index = index
        self.durationSeconds = durationSeconds
        self.coverURI = coverURI
    }
}

public struct BilibiliTaskSelection: Equatable, Sendable {
    public let mode: String
    public let selectionIDs: [String]
    public let rangeStartIndex: Int
    public let rangeEndIndex: Int

    public init(
        mode: String,
        selectionIDs: [String] = [],
        rangeStartIndex: Int = 0,
        rangeEndIndex: Int = 0
    ) {
        self.mode = mode
        self.selectionIDs = selectionIDs
        self.rangeStartIndex = rangeStartIndex
        self.rangeEndIndex = rangeEndIndex
    }
}

public struct BilibiliTaskResultItem: Identifiable, Equatable, Sendable {
    public let id: String
    public let selectionID: String
    public let title: String
    public let subtitle: String
    public let sourceKind: String
    public let contentID: String
    public let index: Int
    public let state: String
    public let message: String
    public let libraryItemID: String
    public let playbackSource: CachePlaybackSource?
    public let playbackSession: CacheBilibiliPlaybackSession?

    public init(
        id: String,
        selectionID: String,
        title: String,
        subtitle: String,
        sourceKind: String,
        contentID: String,
        index: Int,
        state: String,
        message: String,
        libraryItemID: String,
        playbackSource: CachePlaybackSource? = nil,
        playbackSession: CacheBilibiliPlaybackSession? = nil
    ) {
        self.id = id
        self.selectionID = selectionID
        self.title = title
        self.subtitle = subtitle
        self.sourceKind = sourceKind
        self.contentID = contentID
        self.index = index
        self.state = state
        self.message = message
        self.libraryItemID = libraryItemID
        self.playbackSource = playbackSource
        self.playbackSession = playbackSession
    }
}

public struct CacheTask: Identifiable, Equatable, Sendable {
    public let id: String
    public let kind: String
    public let state: String
    public let source: String
    public let title: String
    public let progress: Double
    public let downloadedBytes: Int64
    public let totalBytes: Int64
    public let message: String
    public let libraryItemID: String
    public let playbackSource: CachePlaybackSource?
    public let playbackSession: CacheBilibiliPlaybackSession?
    public let bilibiliSelection: BilibiliTaskSelection?
    public let resultItems: [BilibiliTaskResultItem]

    public init(
        id: String,
        kind: String,
        state: String,
        source: String,
        title: String,
        progress: Double,
        downloadedBytes: Int64 = 0,
        totalBytes: Int64 = 0,
        message: String,
        libraryItemID: String,
        playbackSource: CachePlaybackSource?,
        playbackSession: CacheBilibiliPlaybackSession?,
        bilibiliSelection: BilibiliTaskSelection? = nil,
        resultItems: [BilibiliTaskResultItem] = []
    ) {
        self.id = id
        self.kind = kind
        self.state = state
        self.source = source
        self.title = title
        self.progress = progress
        self.downloadedBytes = downloadedBytes
        self.totalBytes = totalBytes
        self.message = message
        self.libraryItemID = libraryItemID
        self.playbackSource = playbackSource
        self.playbackSession = playbackSession
        self.bilibiliSelection = bilibiliSelection
        self.resultItems = resultItems
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

    fileprivate var removingLibrarySourcePrefix: String {
        let prefix = "librarysource"
        guard hasPrefix(prefix) else {
            return self
        }

        return String(dropFirst(prefix.count))
    }
}
