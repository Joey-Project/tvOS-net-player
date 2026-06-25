import Foundation
import TVOSNetPlayerCacheClient

public enum CacheStatusBadgeTone: String, Equatable, Sendable {
    case ready
    case info
    case warning
    case error
}

public struct CacheStatusBadge: Identifiable, Equatable, Sendable {
    public let id: String
    public let label: String
    public let detail: String?
    public let systemImage: String
    public let tone: CacheStatusBadgeTone

    public init(
        id: String,
        label: String,
        detail: String? = nil,
        systemImage: String,
        tone: CacheStatusBadgeTone
    ) {
        self.id = id
        self.label = label
        self.detail = detail
        self.systemImage = systemImage
        self.tone = tone
    }
}

public enum HLSCacheStatusPresentation {
    public static func badges(for status: HLSCacheStatus?) -> [CacheStatusBadge] {
        guard let status else {
            return []
        }

        var badges: [CacheStatusBadge] = []
        if let quotaBadge = quotaBadge(for: status) {
            badges.append(quotaBadge)
        }
        if let weakNetwork = status.weakNetwork,
            let weakNetworkBadge = weakNetworkBadge(for: weakNetwork)
        {
            badges.append(weakNetworkBadge)
        }
        if let playbackBadge = playbackBadge(for: status.playback) {
            badges.append(playbackBadge)
        }
        return badges
    }

    public static func weakNetworkDiagnosticRow(_ status: HLSWeakNetworkStatus) -> CacheServerDiagnosticRow {
        let badge =
            weakNetworkBadge(for: status)
            ?? CacheStatusBadge(
                id: "weakNetwork",
                label: "Normal",
                detail: weakNetworkDetail(status),
                systemImage: "wifi",
                tone: .ready
            )

        return CacheServerDiagnosticRow(
            id: "weakNetwork",
            title: "Weak Network",
            value: badge.label,
            detail: badge.detail,
            systemImage: badge.systemImage,
            severity: diagnosticSeverity(for: badge.tone)
        )
    }

    private static func quotaBadge(for status: HLSCacheStatus) -> CacheStatusBadge? {
        if let eviction = status.lastEviction,
            isQuotaCurrentlyBlocked(status, eviction: eviction)
        {
            let target = ByteCountFormatter.string(fromByteCount: eviction.targetUsedBytes, countStyle: .file)
            return CacheStatusBadge(
                id: "quotaBlocked",
                label: "Quota blocked",
                detail: "Cleanup could not trim HLS cache to \(target).",
                systemImage: "externaldrive.badge.xmark",
                tone: .error
            )
        }

        guard status.evictionEnabled, status.highWatermarkBytes > 0 else {
            return nil
        }

        if status.usedBytes >= status.highWatermarkBytes {
            let high = ByteCountFormatter.string(fromByteCount: status.highWatermarkBytes, countStyle: .file)
            return CacheStatusBadge(
                id: "cleanupWatermark",
                label: "Cleanup watermark reached",
                detail: "Auto cleanup starts at \(status.highWatermarkPercent)% (\(high)).",
                systemImage: "externaldrive.badge.exclamationmark",
                tone: .warning
            )
        }

        let nearWatermarkBytes = nearWatermarkBytes(for: status.highWatermarkBytes)
        guard status.usedBytes >= nearWatermarkBytes else {
            return nil
        }

        return CacheStatusBadge(
            id: "nearCleanupWatermark",
            label: "Near cleanup watermark",
            detail: "HLS cache is approaching the \(status.highWatermarkPercent)% cleanup watermark.",
            systemImage: "externaldrive.badge.timemachine",
            tone: .info
        )
    }

    private static func isQuotaCurrentlyBlocked(
        _ status: HLSCacheStatus,
        eviction: HLSCacheEvictionSummary
    ) -> Bool {
        guard !eviction.targetReached, status.evictionEnabled else {
            return false
        }

        if status.highWatermarkBytes > 0 {
            return status.usedBytes >= status.highWatermarkBytes
        }

        return status.usedBytes > eviction.targetUsedBytes
    }

    private static func weakNetworkBadge(for status: HLSWeakNetworkStatus) -> CacheStatusBadge? {
        let detail = weakNetworkDetail(status)
        if status.isCacheOnly {
            return CacheStatusBadge(
                id: "cacheOnly",
                label: "Cache-only playback",
                detail: detail ?? "Serving cached HLS while upstream is degraded.",
                systemImage: "externaldrive.fill.badge.checkmark",
                tone: .warning
            )
        }
        if status.isUpstreamFailed {
            return CacheStatusBadge(
                id: "upstreamFailed",
                label: "Upstream failed",
                detail: detail ?? "Playback may continue from cached bytes.",
                systemImage: "wifi.slash",
                tone: .error
            )
        }
        if status.isVariantDowngraded {
            return CacheStatusBadge(
                id: "variantDowngraded",
                label: "Variant downgraded",
                detail: detail ?? "Lower HLS variants are advertised until upstream recovers.",
                systemImage: "arrow.down.forward.circle",
                tone: .warning
            )
        }
        if status.isRetrying {
            return CacheStatusBadge(
                id: "retryingUpstream",
                label: "Retrying upstream",
                detail: detail ?? "Trying backup URLs before declaring the variant unhealthy.",
                systemImage: "arrow.clockwise.circle",
                tone: .warning
            )
        }
        if status.isActive {
            return CacheStatusBadge(
                id: "weakNetworkActive",
                label: "Weak network active",
                detail: detail ?? "Server reported an unrecognized weak-network state.",
                systemImage: "wifi.exclamationmark",
                tone: .warning
            )
        }
        return nil
    }

    private static func nearWatermarkBytes(for highWatermarkBytes: Int64) -> Int64 {
        let quotient = highWatermarkBytes / 10
        let remainder = highWatermarkBytes % 10
        return quotient * 9 + remainder * 9 / 10
    }

    private static func playbackBadge(for status: HLSPlaybackProgressStatus?) -> CacheStatusBadge? {
        guard let status, status.isActive || status.isRecentlyStopped else {
            return nil
        }

        let detail = status.positionLabel ?? trimmed(status.message)
        return CacheStatusBadge(
            id: "playbackPosition",
            label: status.isActive ? "Playback-aware cache" : "Recent playback protected",
            detail: detail,
            systemImage: status.isActive ? "play.circle" : "pause.circle",
            tone: .info
        )
    }

    private static func weakNetworkDetail(_ status: HLSWeakNetworkStatus) -> String? {
        var parts: [String] = []
        if let message = trimmed(status.message) {
            parts.append(message)
        }
        if status.degradedSessionCount > 0 {
            parts.append(pluralized(status.degradedSessionCount, singular: "degraded session"))
        }
        if status.unhealthyVariantCount > 0 {
            parts.append(pluralized(status.unhealthyVariantCount, singular: "unhealthy variant"))
        }
        if status.retryingVariantCount > 0 {
            parts.append(pluralized(status.retryingVariantCount, singular: "retrying variant"))
        }
        if status.cacheOnlySessionCount > 0 {
            parts.append(pluralized(status.cacheOnlySessionCount, singular: "cache-only session"))
        }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    private static func diagnosticSeverity(for tone: CacheStatusBadgeTone) -> CacheServerDiagnosticSeverity {
        switch tone {
        case .ready:
            return .ready
        case .info:
            return .info
        case .warning:
            return .warning
        case .error:
            return .error
        }
    }

    private static func pluralized(_ count: Int, singular: String) -> String {
        "\(count) \(singular)\(count == 1 ? "" : "s")"
    }

    private static func trimmed(_ text: String) -> String? {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }
}
