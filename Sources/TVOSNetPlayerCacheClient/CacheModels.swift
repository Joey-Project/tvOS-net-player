import Foundation

public enum CacheServerEndpointScheme: String, Equatable, Sendable {
    case http
    case https
}

public struct CacheServerEndpoint: Equatable, Sendable {
    public static let defaultPort = 50_051
    public static let defaultTLSPort = 443

    public let scheme: CacheServerEndpointScheme
    public let host: String
    public let port: Int

    public init(host: String, port: Int = Self.defaultPort) {
        self.init(uncheckedHost: host, port: port, scheme: .http)
    }

    public init?(host: String, scheme: CacheServerEndpointScheme) {
        self.init(
            host: host,
            port: Self.defaultPortValue(for: scheme, plaintextDefaultPort: Self.defaultPort),
            scheme: scheme
        )
    }

    public init?(host: String, port: Int, scheme: CacheServerEndpointScheme) {
        guard Self.canUse(host: host, port: port, scheme: scheme) else {
            return nil
        }

        self.init(uncheckedHost: host, port: port, scheme: scheme)
    }

    private init(uncheckedHost host: String, port: Int, scheme: CacheServerEndpointScheme) {
        self.scheme = scheme
        self.host = host
        self.port = port
    }

    public var displayAddress: String {
        let formattedHost = hostForDisplay
        guard scheme == .https else {
            return "\(formattedHost):\(port)"
        }

        if port == Self.defaultTLSPort {
            return "https://\(formattedHost)"
        }

        return "https://\(formattedHost):\(port)"
    }

    public var usesTLS: Bool {
        scheme == .https
    }

    private var hostForDisplay: String {
        if host.contains(":") && !(host.hasPrefix("[") && host.hasSuffix("]")) {
            return "[\(host)]"
        }

        return host
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
            let rawScheme = components.scheme?.lowercased(),
            let scheme = CacheServerEndpointScheme(rawValue: rawScheme),
            var host = components.host?.trimmingCharacters(in: .whitespacesAndNewlines)
        else {
            return nil
        }

        guard
            components.user == nil,
            components.password == nil,
            components.query == nil,
            components.fragment == nil,
            components.path.isEmpty || components.path == "/"
        else {
            return nil
        }

        guard !(components.port == nil && hasExplicitPortSpecifier(in: candidate, scheme: rawScheme)) else {
            return nil
        }

        if host.hasPrefix("[") && host.hasSuffix("]") {
            host = String(host.dropFirst().dropLast())
        }

        guard !host.isEmpty else {
            return nil
        }

        guard scheme == .http || !isIPLiteral(host) else {
            return nil
        }

        let port = components.port ?? defaultPortValue(for: scheme, plaintextDefaultPort: defaultPort)
        guard (1...65_535).contains(port) else {
            return nil
        }

        return Self(host: host, port: port, scheme: scheme)
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

    private static func defaultPortValue(
        for scheme: CacheServerEndpointScheme,
        plaintextDefaultPort: Int
    ) -> Int {
        switch scheme {
        case .http:
            return plaintextDefaultPort
        case .https:
            return Self.defaultTLSPort
        }
    }

    private static func canUse(host: String, port: Int, scheme: CacheServerEndpointScheme) -> Bool {
        guard !host.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            return false
        }
        guard (1...65_535).contains(port) else {
            return false
        }
        guard scheme == .http || !isIPLiteral(host) else {
            return false
        }

        return true
    }

    private static func hasExplicitPortSpecifier(in candidate: String, scheme: String) -> Bool {
        let prefix = "\(scheme)://"
        guard candidate.lowercased().hasPrefix(prefix) else {
            return false
        }

        let authorityStart = candidate.index(candidate.startIndex, offsetBy: prefix.count)
        let authorityEnd =
            candidate[authorityStart...].firstIndex(where: { $0 == "/" || $0 == "?" || $0 == "#" })
            ?? candidate.endIndex
        let authority = candidate[authorityStart..<authorityEnd]
        if authority.hasPrefix("[") {
            guard let closingBracket = authority.firstIndex(of: "]") else {
                return false
            }
            let portSeparator = authority.index(after: closingBracket)
            return portSeparator < authority.endIndex && authority[portSeparator] == ":"
        }

        return authority.contains(":")
    }

    private static func isIPLiteral(_ host: String) -> Bool {
        host.contains(":") || isIPv4Literal(host)
    }

    private static func isIPv4Literal(_ host: String) -> Bool {
        let parts = host.split(separator: ".", omittingEmptySubsequences: false)
        guard parts.count == 4 else {
            return false
        }

        return parts.allSatisfy { part in
            guard
                !part.isEmpty,
                part.utf8.allSatisfy({ $0 >= UInt8(ascii: "0") && $0 <= UInt8(ascii: "9") })
            else {
                return false
            }
            guard let value = Int(part), (0...255).contains(value) else {
                return false
            }
            return true
        }
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

    public var supportsBilibiliCredentialStatus: Bool {
        capabilities.contains(CacheServerCapability.bilibiliCredentialStatus)
    }

    public var supportsLanTranscoding: Bool {
        capabilities.contains(CacheServerCapability.lanTranscoding)
    }
}

public struct CacheHealthStatus: Equatable, Sendable {
    public let state: String
    public let message: String
    public let checkedAt: Date?

    public init(state: String, message: String, checkedAt: Date?) {
        self.state = state
        self.message = message
        self.checkedAt = checkedAt
    }

    public var isServing: Bool {
        state.normalizedCacheProtocolName.removingHealthStatePrefix == "serving"
    }

    public var isDegraded: Bool {
        state.normalizedCacheProtocolName.removingHealthStatePrefix == "degraded"
    }

    public var isNotServing: Bool {
        state.normalizedCacheProtocolName.removingHealthStatePrefix == "notserving"
    }
}

public enum CacheServerCapability {
    public static let bilibiliCredentialStatus = "bilibiliCredentialStatus"
    public static let bilibiliResolve = "bilibiliResolve"
    public static let bilibiliTaskSelection = "bilibiliTaskSelection"
    public static let lanTranscoding = "lanTranscoding"
    public static let libraryItemDelete = "libraryItemDelete"
}

public struct BilibiliCredentialStatus: Equatable, Sendable {
    public let state: String
    public let message: String
    public let credentialPathConfigured: Bool
    public let credentialFileLoaded: Bool
    public let hasWebCookie: Bool
    public let hasAccessKey: Bool
    public let hasTVAccessKey: Bool
    public let restrictedArea: String
    public let restrictedPlayURLProxyCount: UInt32
    public let restrictedAPIProxyCount: UInt32
    public let checkedAt: Date?

    public init(
        state: String,
        message: String,
        credentialPathConfigured: Bool,
        credentialFileLoaded: Bool,
        hasWebCookie: Bool,
        hasAccessKey: Bool,
        hasTVAccessKey: Bool,
        restrictedArea: String,
        restrictedPlayURLProxyCount: UInt32,
        restrictedAPIProxyCount: UInt32,
        checkedAt: Date?
    ) {
        self.state = state
        self.message = message
        self.credentialPathConfigured = credentialPathConfigured
        self.credentialFileLoaded = credentialFileLoaded
        self.hasWebCookie = hasWebCookie
        self.hasAccessKey = hasAccessKey
        self.hasTVAccessKey = hasTVAccessKey
        self.restrictedArea = restrictedArea
        self.restrictedPlayURLProxyCount = restrictedPlayURLProxyCount
        self.restrictedAPIProxyCount = restrictedAPIProxyCount
        self.checkedAt = checkedAt
    }
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
    public let weakNetwork: HLSWeakNetworkStatus?
    public let transcoding: LanTranscodingStatus?
    public let playback: HLSPlaybackProgressStatus?

    public init(
        evictionEnabled: Bool,
        maxBytes: Int64,
        highWatermarkPercent: Int,
        lowWatermarkPercent: Int,
        highWatermarkBytes: Int64,
        lowWatermarkBytes: Int64,
        usedBytes: Int64,
        completedSessionCount: Int,
        lastEviction: HLSCacheEvictionSummary?,
        weakNetwork: HLSWeakNetworkStatus? = nil,
        transcoding: LanTranscodingStatus? = nil,
        playback: HLSPlaybackProgressStatus? = nil
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
        self.weakNetwork = weakNetwork
        self.transcoding = transcoding
        self.playback = playback
    }
}

public enum PlaybackProgressIntent: String, Equatable, Sendable {
    case started
    case playing
    case seek
    case paused
    case stopped
}

public struct PlaybackProgressReport: Equatable, Sendable {
    public let playbackURI: String
    public let libraryItemID: String
    public let variantID: String
    public let positionSeconds: Double
    public let durationSeconds: Double?
    public let intent: PlaybackProgressIntent

    public init(
        playbackURI: String,
        libraryItemID: String = "",
        variantID: String = "",
        positionSeconds: Double,
        durationSeconds: Double? = nil,
        intent: PlaybackProgressIntent
    ) {
        self.playbackURI = playbackURI
        self.libraryItemID = libraryItemID
        self.variantID = variantID
        self.positionSeconds = positionSeconds
        self.durationSeconds = durationSeconds
        self.intent = intent
    }
}

public struct PlaybackProgressReportResult: Equatable, Sendable {
    public let accepted: Bool
    public let sessionID: String
    public let message: String

    public init(accepted: Bool, sessionID: String, message: String) {
        self.accepted = accepted
        self.sessionID = sessionID
        self.message = message
    }
}

public struct HLSPlaybackProgressStatus: Equatable, Sendable {
    private static let maximumFormattedPlaybackSeconds = 100 * 365 * 24 * 60 * 60

    public let state: String
    public let message: String
    public let sessionID: String
    public let libraryItemID: String
    public let variantID: String
    public let playbackURI: String
    public let positionSeconds: Double
    public let durationSeconds: Double?
    public let lastIntent: String
    public let updatedAt: Date?

    public init(
        state: String,
        message: String,
        sessionID: String,
        libraryItemID: String,
        variantID: String,
        playbackURI: String,
        positionSeconds: Double,
        durationSeconds: Double?,
        lastIntent: String,
        updatedAt: Date?
    ) {
        self.state = state
        self.message = message
        self.sessionID = sessionID
        self.libraryItemID = libraryItemID
        self.variantID = variantID
        self.playbackURI = playbackURI
        self.positionSeconds = positionSeconds
        self.durationSeconds = durationSeconds
        self.lastIntent = lastIntent
        self.updatedAt = updatedAt
    }

    public var isActive: Bool {
        state.normalizedCacheProtocolName.removingPlaybackActivityStatePrefix == "active"
    }

    public var isRecentlyStopped: Bool {
        state.normalizedCacheProtocolName.removingPlaybackActivityStatePrefix == "recentlystopped"
    }

    public var positionLabel: String? {
        guard positionSeconds.isFinite, positionSeconds >= 0 else {
            return nil
        }

        guard let current = Self.formattedPlaybackTime(positionSeconds) else {
            return nil
        }
        guard let durationSeconds, durationSeconds.isFinite, durationSeconds > 0 else {
            return current
        }

        guard let duration = Self.formattedPlaybackTime(durationSeconds) else {
            return current
        }

        return "\(current) of \(duration)"
    }

    private static func formattedPlaybackTime(_ seconds: Double) -> String? {
        guard seconds.isFinite, seconds >= 0, seconds <= Double(maximumFormattedPlaybackSeconds) else {
            return nil
        }

        let roundedSeconds = max(0, Int(seconds.rounded()))
        let hours = roundedSeconds / 3_600
        let minutes = (roundedSeconds % 3_600) / 60
        let seconds = roundedSeconds % 60
        if hours > 0 {
            return String(format: "%d:%02d:%02d", hours, minutes, seconds)
        }

        return String(format: "%d:%02d", minutes, seconds)
    }
}

public struct LanTranscodingStatus: Equatable, Sendable {
    public let enabled: Bool
    public let state: String
    public let message: String
    public let profileID: String
    public let targetContainer: String
    public let targetVideoCodec: String
    public let targetAudioCodec: String
    public let maxConcurrentJobs: Int
    public let activeJobCount: Int

    public init(
        enabled: Bool,
        state: String,
        message: String,
        profileID: String,
        targetContainer: String,
        targetVideoCodec: String,
        targetAudioCodec: String,
        maxConcurrentJobs: Int,
        activeJobCount: Int
    ) {
        self.enabled = enabled
        self.state = state
        self.message = message
        self.profileID = profileID
        self.targetContainer = targetContainer
        self.targetVideoCodec = targetVideoCodec
        self.targetAudioCodec = targetAudioCodec
        self.maxConcurrentJobs = maxConcurrentJobs
        self.activeJobCount = activeJobCount
    }
}

public struct HLSWeakNetworkStatus: Equatable, Sendable {
    public let state: String
    public let message: String
    public let degradedSessionCount: Int
    public let unhealthyVariantCount: Int
    public let retryingVariantCount: Int
    public let cacheOnlySessionCount: Int
    public let lastChangedAt: Date?

    public init(
        state: String,
        message: String,
        degradedSessionCount: Int,
        unhealthyVariantCount: Int,
        retryingVariantCount: Int,
        cacheOnlySessionCount: Int,
        lastChangedAt: Date?
    ) {
        self.state = state
        self.message = message
        self.degradedSessionCount = degradedSessionCount
        self.unhealthyVariantCount = unhealthyVariantCount
        self.retryingVariantCount = retryingVariantCount
        self.cacheOnlySessionCount = cacheOnlySessionCount
        self.lastChangedAt = lastChangedAt
    }

    public var isActive: Bool {
        switch state.normalizedCacheProtocolName.removingWeakNetworkStatePrefix {
        case "", "unspecified", "normal":
            false
        default:
            true
        }
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

public enum BilibiliSubtitleAIPolicy: String, CaseIterable, Equatable, Hashable, Sendable {
    case unspecified
    case include
    case preferNonAI
    case excludeAI
    case onlyAI
}

public enum BilibiliDanmakuFormat: String, CaseIterable, Equatable, Hashable, Sendable {
    case xml
    case ass
}

public struct BilibiliDownloadTaskOptions: Equatable, Sendable {
    public let qualityPreference: String
    public let encodingPreference: String
    public let audioLanguagePreference: String
    public let preferTVAPI: Bool
    public let downloadSubtitles: Bool
    public let downloadDanmaku: Bool
    public let downloadCover: Bool
    public let subtitleAIPolicy: BilibiliSubtitleAIPolicy
    public let danmakuFormats: [BilibiliDanmakuFormat]

    public init(
        qualityPreference: String = "",
        encodingPreference: String = "",
        audioLanguagePreference: String = "",
        preferTVAPI: Bool = false,
        downloadSubtitles: Bool = false,
        downloadDanmaku: Bool = false,
        downloadCover: Bool = false,
        subtitleAIPolicy: BilibiliSubtitleAIPolicy = .unspecified,
        danmakuFormats: [BilibiliDanmakuFormat] = []
    ) {
        self.qualityPreference = qualityPreference
        self.encodingPreference = encodingPreference
        self.audioLanguagePreference = audioLanguagePreference
        self.preferTVAPI = preferTVAPI
        self.downloadSubtitles = downloadSubtitles
        self.downloadDanmaku = downloadDanmaku
        self.downloadCover = downloadCover
        self.subtitleAIPolicy = subtitleAIPolicy
        self.danmakuFormats = danmakuFormats
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
    public let transcodingPlan: LanTranscodingPlan?

    public init(
        id: String,
        title: String,
        contentID: String,
        selectedVariantID: String,
        selectedVariant: CacheBilibiliPlaybackVariant?,
        variants: [CacheBilibiliPlaybackVariant],
        transcodingPlan: LanTranscodingPlan? = nil
    ) {
        self.id = id
        self.title = title
        self.contentID = contentID
        self.selectedVariantID = selectedVariantID
        self.selectedVariant = selectedVariant
        self.variants = variants
        self.transcodingPlan = transcodingPlan
    }
}

public struct LanTranscodingPlan: Equatable, Sendable {
    public let state: String
    public let profileID: String
    public let reason: String
    public let sourceVariantID: String
    public let targetContainer: String
    public let targetVideoCodec: String
    public let targetAudioCodec: String
    public let outputProtocol: String

    public init(
        state: String,
        profileID: String,
        reason: String,
        sourceVariantID: String,
        targetContainer: String,
        targetVideoCodec: String,
        targetAudioCodec: String,
        outputProtocol: String
    ) {
        self.state = state
        self.profileID = profileID
        self.reason = reason
        self.sourceVariantID = sourceVariantID
        self.targetContainer = targetContainer
        self.targetVideoCodec = targetVideoCodec
        self.targetAudioCodec = targetAudioCodec
        self.outputProtocol = outputProtocol
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

    fileprivate var removingWeakNetworkStatePrefix: String {
        let prefix = "hlsweaknetworkstate"
        guard hasPrefix(prefix) else {
            return self
        }

        return String(dropFirst(prefix.count))
    }

    fileprivate var removingPlaybackActivityStatePrefix: String {
        let prefix = "hlsplaybackactivitystate"
        guard hasPrefix(prefix) else {
            return self
        }

        return String(dropFirst(prefix.count))
    }

    fileprivate var removingHealthStatePrefix: String {
        let prefix = "healthstate"
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
