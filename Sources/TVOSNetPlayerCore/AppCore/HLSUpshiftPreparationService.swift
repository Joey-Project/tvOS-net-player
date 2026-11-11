import Foundation
import TVOSNetPlayerCacheClient

final class HLSPreparationCancellationFlag: @unchecked Sendable {
    private let lock = NSLock()
    private var cancelled = false

    var isCancelled: Bool {
        lock.lock()
        defer { lock.unlock() }
        return cancelled
    }

    func cancel() {
        lock.lock()
        cancelled = true
        lock.unlock()
    }
}

struct HLSUpshiftPreparationService: Sendable {
    struct PreparedAssessment: Sendable {
        let rendition: HLSRenditionDescriptor
        let recommendation: HLSUpshiftRecommendation
    }

    private let client: any CacheControlClient
    private let serverSummary: CacheServerSummary
    private let monotonicNowNanoseconds: @Sendable () -> UInt64

    init(
        client: any CacheControlClient,
        serverSummary: CacheServerSummary,
        monotonicNowNanoseconds: @escaping @Sendable () -> UInt64
    ) {
        self.client = client
        self.serverSummary = serverSummary
        self.monotonicNowNanoseconds = monotonicNowNanoseconds
    }

    func assess(
        sessionID: String,
        targetVariantID: String,
        windowStartSeconds: Double,
        windowDurationSeconds: Double,
        targetDecoderSupported: Bool?,
        snapshot: HLSPlaybackSnapshot,
        isCancelled: @Sendable () -> Bool,
        isCurrent: @MainActor @Sendable () -> Bool
    ) async -> PreparedAssessment? {
        guard !Task.isCancelled, !isCancelled(), await isCurrent(),
            serverSummary.supportsHLSPlaybackReadiness,
            serverSummary.supportsHLSRenditionCatalog,
            let catalogRequest = try? HLSRenditionCatalogRequest(sessionID: sessionID)
        else {
            return nil
        }

        let request = HLSPlaybackReadinessRequest(
            sessionID: sessionID,
            variantID: targetVariantID,
            windowStartSeconds: windowStartSeconds,
            windowDurationSeconds: windowDurationSeconds
        )
        guard (try? request.validate()) != nil else { return nil }

        let catalog: HLSRenditionCatalog
        do {
            catalog = try await client.getHLSRenditionCatalog(catalogRequest)
        } catch {
            return nil
        }
        guard !Task.isCancelled, !isCancelled(), await isCurrent(),
            (try? catalog.validate()) != nil,
            catalog.sessionID.utf8.elementsEqual(sessionID.utf8),
            let rendition = catalog.rendition(variantID: targetVariantID)
        else {
            return nil
        }

        let now = monotonicNowNanoseconds()
        guard
            let context = snapshot.context(
                for: request,
                targetVideoBitrateBitsPerSecond: rendition.videoResourceBandwidthBitsPerSecond,
                targetAudioBitrateBitsPerSecond: rendition.audioResourceBandwidthBitsPerSecond,
                requiresAudio: rendition.hasSeparateAudio,
                targetDecoderSupported: targetDecoderSupported,
                expectedSessionGeneration: catalog.sessionGeneration,
                nowNanoseconds: now
            )
        else { return nil }

        guard Self.contextAgeIsValid(context) else {
            return nil
        }
        guard !Task.isCancelled, !isCancelled(), await isCurrent() else {
            return nil
        }

        guard
            let readinessContext = snapshot.context(
                for: request,
                targetVideoBitrateBitsPerSecond: rendition.videoResourceBandwidthBitsPerSecond,
                targetAudioBitrateBitsPerSecond: rendition.audioResourceBandwidthBitsPerSecond,
                requiresAudio: rendition.hasSeparateAudio,
                targetDecoderSupported: targetDecoderSupported,
                expectedSessionGeneration: catalog.sessionGeneration,
                nowNanoseconds: monotonicNowNanoseconds()
            ), Self.contextAgeIsValid(readinessContext)
        else { return nil }

        let readinessService = HLSUpshiftAssessmentService(
            client: client,
            serverSummary: serverSummary,
            monotonicNowNanoseconds: monotonicNowNanoseconds
        )
        let recommendation = await readinessService.assess(readinessContext)
        guard !Task.isCancelled, !isCancelled(), await isCurrent() else {
            return nil
        }
        if case .hold(let reason) = recommendation.outcome {
            switch reason {
            case .cancelled, .requestFailed, .unsupportedServer, .clockAnomaly, .staleContext, .invalidContext:
                return nil
            default:
                break
            }
        }
        return PreparedAssessment(rendition: rendition, recommendation: recommendation)
    }

    static func isFresh(
        snapshot: HLSPlaybackSnapshot,
        rendition: HLSRenditionDescriptor,
        recommendation: HLSUpshiftRecommendation,
        nowNanoseconds: UInt64
    ) -> Bool {
        let request = HLSPlaybackReadinessRequest(
            sessionID: recommendation.sessionID,
            variantID: recommendation.variantID,
            windowStartSeconds: recommendation.windowStartSeconds,
            windowDurationSeconds: recommendation.windowDurationSeconds
        )
        guard
            let context = snapshot.context(
                for: request,
                targetVideoBitrateBitsPerSecond: rendition.videoResourceBandwidthBitsPerSecond,
                targetAudioBitrateBitsPerSecond: rendition.audioResourceBandwidthBitsPerSecond,
                requiresAudio: rendition.hasSeparateAudio,
                targetDecoderSupported: true,
                expectedSessionGeneration: nil,
                nowNanoseconds: nowNanoseconds
            )
        else { return false }
        return contextAgeIsValid(context)
    }

    private static func contextAgeIsValid(_ context: HLSUpshiftContext) -> Bool {
        let freshnessContext = HLSUpshiftContext(
            request: context.request,
            isPlaying: true,
            playheadSeconds: context.playheadSeconds,
            contiguousBufferedEndSeconds: context.contiguousBufferedEndSeconds,
            playbackRate: context.playbackRate > 0 ? context.playbackRate : 1,
            targetVideoBitrateBitsPerSecond: context.targetVideoBitrateBitsPerSecond,
            targetAudioBitrateBitsPerSecond: context.targetAudioBitrateBitsPerSecond,
            requiresAudio: context.requiresAudio,
            targetDecoderSupported: context.targetDecoderSupported,
            expectedSessionGeneration: nil,
            contextAgeMillis: context.contextAgeMillis
        )
        let reason = HLSUpshiftPolicy.preflightHoldReason(for: freshnessContext)
        return reason != .staleContext && reason != .invalidContext && reason != .clockAnomaly
    }
}
