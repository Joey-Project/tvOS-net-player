import Foundation

public struct HLSPlaybackReadinessRequest: Equatable, Sendable {
    public let sessionID: String
    public let variantID: String
    public let windowStartSeconds: Double
    public let windowDurationSeconds: Double

    public init(
        sessionID: String,
        variantID: String,
        windowStartSeconds: Double,
        windowDurationSeconds: Double
    ) {
        self.sessionID = sessionID
        self.variantID = variantID
        self.windowStartSeconds = windowStartSeconds
        self.windowDurationSeconds = windowDurationSeconds
    }

    public func validate() throws {
        guard
            Self.isValidIdentifier(sessionID),
            Self.isValidIdentifier(variantID),
            windowStartSeconds.isFinite,
            windowStartSeconds >= 0,
            windowDurationSeconds.isFinite,
            windowDurationSeconds > 0,
            windowDurationSeconds <= 60
        else {
            throw HLSPlaybackReadinessValidationError.invalidRequest
        }

        let end = windowStartSeconds + windowDurationSeconds
        guard end.isFinite, end > windowStartSeconds else {
            throw HLSPlaybackReadinessValidationError.invalidRequest
        }
    }

    private static func isValidIdentifier(_ value: String) -> Bool {
        let scalars = value.unicodeScalars
        guard !value.isEmpty, value.utf8.count <= 256,
            let first = scalars.first,
            let last = scalars.last
        else {
            return false
        }
        guard !first.properties.isWhitespace, !last.properties.isWhitespace else {
            return false
        }
        return !scalars.contains { $0.properties.generalCategory == .control }
    }
}

public enum HLSPlaybackReadinessValidationError: Error, Equatable, Sendable {
    case invalidRequest
    case invalidResponse
}

public enum HLSPlaybackCacheState: Equatable, Sendable {
    case unspecified
    case readable
    case missing
    case unknown
    case unrecognized(Int)
}

public enum HLSPlaybackCacheReason: Equatable, Sendable {
    case unspecified
    case none
    case notCached
    case indexUnavailable
    case outOfRange
    case busy
    case budgetExceeded
    case identityChanged
    case ioFailure
    case unknownGenerationChanged
    case unrecognized(Int)
}

public struct HLSOriginSupplyEvidence: Equatable, Sendable {
    public let ageMillis: UInt64
    public let throughputBitsPerSecond: UInt64
    public let firstByteLatencyMicros: UInt64?
    public let freshnessLimitMillis: UInt64

    public init(
        ageMillis: UInt64,
        throughputBitsPerSecond: UInt64,
        firstByteLatencyMicros: UInt64?,
        freshnessLimitMillis: UInt64
    ) {
        self.ageMillis = ageMillis
        self.throughputBitsPerSecond = throughputBitsPerSecond
        self.firstByteLatencyMicros = firstByteLatencyMicros
        self.freshnessLimitMillis = freshnessLimitMillis
    }
}

/// A read-only snapshot, not a lease, capacity guarantee, or ready-to-play verdict.
public struct HLSPlaybackReadiness: Equatable, Sendable {
    public let sessionID: String
    public let variantID: String
    public let windowStartSeconds: Double
    public let windowDurationSeconds: Double
    public let sessionGeneration: UInt64
    public let cacheState: HLSPlaybackCacheState
    public let cacheReason: HLSPlaybackCacheReason
    public let inspectionBudgetUsedBytes: UInt64
    public let videoOrigin: HLSOriginSupplyEvidence?
    public let audioOrigin: HLSOriginSupplyEvidence?

    public init(
        sessionID: String,
        variantID: String,
        windowStartSeconds: Double,
        windowDurationSeconds: Double,
        sessionGeneration: UInt64,
        cacheState: HLSPlaybackCacheState,
        cacheReason: HLSPlaybackCacheReason,
        inspectionBudgetUsedBytes: UInt64,
        videoOrigin: HLSOriginSupplyEvidence?,
        audioOrigin: HLSOriginSupplyEvidence?
    ) {
        self.sessionID = sessionID
        self.variantID = variantID
        self.windowStartSeconds = windowStartSeconds
        self.windowDurationSeconds = windowDurationSeconds
        self.sessionGeneration = sessionGeneration
        self.cacheState = cacheState
        self.cacheReason = cacheReason
        self.inspectionBudgetUsedBytes = inspectionBudgetUsedBytes
        self.videoOrigin = videoOrigin
        self.audioOrigin = audioOrigin
    }

    init(
        _ proto: TvosNetPlayer_V1_HlsPlaybackReadiness,
        matching request: HLSPlaybackReadinessRequest
    ) throws {
        try request.validate()
        let end = proto.windowStartSeconds + proto.windowDurationSeconds
        guard
            proto.sessionID.utf8.elementsEqual(request.sessionID.utf8),
            proto.variantID.utf8.elementsEqual(request.variantID.utf8),
            proto.windowStartSeconds == request.windowStartSeconds,
            proto.windowDurationSeconds == request.windowDurationSeconds,
            proto.windowStartSeconds.isFinite,
            proto.windowDurationSeconds.isFinite,
            end.isFinite,
            end > proto.windowStartSeconds,
            proto.windowStartSeconds >= 0,
            proto.windowDurationSeconds > 0,
            proto.windowDurationSeconds <= 60
        else {
            throw HLSPlaybackReadinessValidationError.invalidResponse
        }

        self.init(
            sessionID: proto.sessionID,
            variantID: proto.variantID,
            windowStartSeconds: proto.windowStartSeconds,
            windowDurationSeconds: proto.windowDurationSeconds,
            sessionGeneration: proto.sessionGeneration,
            cacheState: HLSPlaybackCacheState(proto.cacheState),
            cacheReason: HLSPlaybackCacheReason(proto.cacheReason),
            inspectionBudgetUsedBytes: proto.inspectionBudgetUsedBytes,
            videoOrigin: proto.hasVideoOrigin ? HLSOriginSupplyEvidence(proto.videoOrigin) : nil,
            audioOrigin: proto.hasAudioOrigin ? HLSOriginSupplyEvidence(proto.audioOrigin) : nil
        )
    }
}

extension HLSOriginSupplyEvidence {
    init(_ proto: TvosNetPlayer_V1_HlsOriginSupplyEvidence) {
        self.init(
            ageMillis: proto.ageMillis,
            throughputBitsPerSecond: proto.throughputBitsPerSecond,
            firstByteLatencyMicros: proto.hasFirstByteLatencyMicros ? proto.firstByteLatencyMicros : nil,
            freshnessLimitMillis: proto.freshnessLimitMillis
        )
    }

    var proto: TvosNetPlayer_V1_HlsOriginSupplyEvidence {
        var result = TvosNetPlayer_V1_HlsOriginSupplyEvidence()
        result.ageMillis = ageMillis
        result.throughputBitsPerSecond = throughputBitsPerSecond
        if let firstByteLatencyMicros {
            result.firstByteLatencyMicros = firstByteLatencyMicros
        }
        result.freshnessLimitMillis = freshnessLimitMillis
        return result
    }
}

extension HLSPlaybackCacheState {
    init(_ proto: TvosNetPlayer_V1_HlsPlaybackCacheState) {
        switch proto {
        case .unspecified: self = .unspecified
        case .readable: self = .readable
        case .missing: self = .missing
        case .unknown: self = .unknown
        case .UNRECOGNIZED(let rawValue): self = .unrecognized(rawValue)
        }
    }

    var proto: TvosNetPlayer_V1_HlsPlaybackCacheState {
        switch self {
        case .unspecified: .unspecified
        case .readable: .readable
        case .missing: .missing
        case .unknown: .unknown
        case .unrecognized(let rawValue): .UNRECOGNIZED(rawValue)
        }
    }
}

extension HLSPlaybackCacheReason {
    init(_ proto: TvosNetPlayer_V1_HlsPlaybackCacheReason) {
        switch proto {
        case .unspecified: self = .unspecified
        case .none: self = .none
        case .notCached: self = .notCached
        case .indexUnavailable: self = .indexUnavailable
        case .outOfRange: self = .outOfRange
        case .busy: self = .busy
        case .budgetExceeded: self = .budgetExceeded
        case .identityChanged: self = .identityChanged
        case .ioFailure: self = .ioFailure
        case .unknownGenerationChanged: self = .unknownGenerationChanged
        case .UNRECOGNIZED(let rawValue): self = .unrecognized(rawValue)
        }
    }

    var proto: TvosNetPlayer_V1_HlsPlaybackCacheReason {
        switch self {
        case .unspecified: .unspecified
        case .none: .none
        case .notCached: .notCached
        case .indexUnavailable: .indexUnavailable
        case .outOfRange: .outOfRange
        case .busy: .busy
        case .budgetExceeded: .budgetExceeded
        case .identityChanged: .identityChanged
        case .ioFailure: .ioFailure
        case .unknownGenerationChanged: .unknownGenerationChanged
        case .unrecognized(let rawValue): .UNRECOGNIZED(rawValue)
        }
    }
}

extension TvosNetPlayer_V1_GetHlsPlaybackReadinessRequest {
    init(_ request: HLSPlaybackReadinessRequest) {
        self.init()
        sessionID = request.sessionID
        variantID = request.variantID
        windowStartSeconds = request.windowStartSeconds
        windowDurationSeconds = request.windowDurationSeconds
    }
}

extension TvosNetPlayer_V1_HlsPlaybackReadiness {
    init(_ readiness: HLSPlaybackReadiness) {
        self.init()
        sessionID = readiness.sessionID
        variantID = readiness.variantID
        windowStartSeconds = readiness.windowStartSeconds
        windowDurationSeconds = readiness.windowDurationSeconds
        sessionGeneration = readiness.sessionGeneration
        cacheState = readiness.cacheState.proto
        cacheReason = readiness.cacheReason.proto
        inspectionBudgetUsedBytes = readiness.inspectionBudgetUsedBytes
        if let videoOrigin = readiness.videoOrigin {
            self.videoOrigin = videoOrigin.proto
        }
        if let audioOrigin = readiness.audioOrigin {
            self.audioOrigin = audioOrigin.proto
        }
    }
}
