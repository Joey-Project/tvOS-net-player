import Foundation
import TVOSNetPlayerCacheClient

public struct HLSUpshiftContext: Equatable, Sendable {
    public let request: HLSPlaybackReadinessRequest
    public let isPlaying: Bool
    public let playheadSeconds: Double
    public let contiguousBufferedEndSeconds: Double
    public let playbackRate: Double
    public let targetVideoBitrateBitsPerSecond: UInt64?
    public let targetAudioBitrateBitsPerSecond: UInt64?
    public let requiresAudio: Bool
    public let targetDecoderSupported: Bool?
    public let expectedSessionGeneration: UInt64?
    public let contextAgeMillis: UInt64

    public init(
        request: HLSPlaybackReadinessRequest,
        isPlaying: Bool,
        playheadSeconds: Double,
        contiguousBufferedEndSeconds: Double,
        playbackRate: Double,
        targetVideoBitrateBitsPerSecond: UInt64?,
        targetAudioBitrateBitsPerSecond: UInt64?,
        requiresAudio: Bool,
        targetDecoderSupported: Bool?,
        expectedSessionGeneration: UInt64?,
        contextAgeMillis: UInt64
    ) {
        self.request = request
        self.isPlaying = isPlaying
        self.playheadSeconds = playheadSeconds
        self.contiguousBufferedEndSeconds = contiguousBufferedEndSeconds
        self.playbackRate = playbackRate
        self.targetVideoBitrateBitsPerSecond = targetVideoBitrateBitsPerSecond
        self.targetAudioBitrateBitsPerSecond = targetAudioBitrateBitsPerSecond
        self.requiresAudio = requiresAudio
        self.targetDecoderSupported = targetDecoderSupported
        self.expectedSessionGeneration = expectedSessionGeneration
        self.contextAgeMillis = contextAgeMillis
    }
}

public enum HLSUpshiftHoldReason: Equatable, Sendable {
    case notPlaying
    case invalidContext
    case staleContext
    case invalidReadiness
    case identityMismatch
    case generationMismatch
    case cacheUnknown
    case cacheNotReadable
    case decoderUnknown
    case decoderUnsupported
    case insufficientOriginEvidence
    case insufficientPreparationTime
    case unsupportedServer
    case cancelled
    case requestFailed
    case clockAnomaly
}

public struct HLSOriginPrewarmEstimate: Equatable, Sendable {
    /// An estimate from recent origin goodput, not a capacity guarantee.
    public let estimatedPreparationSeconds: Double
    public let availablePreparationSeconds: Double

    init(estimatedPreparationSeconds: Double, availablePreparationSeconds: Double) {
        self.estimatedPreparationSeconds = estimatedPreparationSeconds
        self.availablePreparationSeconds = availablePreparationSeconds
    }
}

public enum HLSUpshiftOutcome: Equatable, Sendable {
    case hold(HLSUpshiftHoldReason)
    /// An observation only; callers must revalidate the cache before any later handover.
    case cacheBackedOpportunity
    /// A bounded estimate only; this is not a switch, capacity, or readiness guarantee.
    case originPrewarmOpportunity(HLSOriginPrewarmEstimate)
}

public struct HLSUpshiftRecommendation: Equatable, Sendable {
    public let sessionID: String
    public let variantID: String
    public let windowStartSeconds: Double
    public let windowDurationSeconds: Double
    public let sessionGeneration: UInt64?
    public let outcome: HLSUpshiftOutcome

    init(context: HLSUpshiftContext, sessionGeneration: UInt64?, outcome: HLSUpshiftOutcome) {
        sessionID = context.request.sessionID
        variantID = context.request.variantID
        windowStartSeconds = context.request.windowStartSeconds
        windowDurationSeconds = context.request.windowDurationSeconds
        self.sessionGeneration = sessionGeneration
        self.outcome = outcome
    }
}

public enum HLSUpshiftPolicy {
    private static let maximumSnapshotAgeMillis: UInt64 = 5_000
    private static let maximumOriginFreshnessMillis: UInt64 = 30_000
    private static let protectedBufferMediaSeconds = 20.0
    private static let transferHeadroom = 1.5

    public static func assess(
        context: HLSUpshiftContext,
        readiness: HLSPlaybackReadiness,
        elapsedSinceRequestStartNanoseconds: UInt64 = 0
    ) -> HLSUpshiftRecommendation {
        let matchingSession =
            readiness.sessionID.utf8.elementsEqual(context.request.sessionID.utf8)
            && readiness.variantID.utf8.elementsEqual(context.request.variantID.utf8)
        let responseGeneration = matchingSession ? readiness.sessionGeneration : nil

        if let reason = preflightHoldReason(
            for: context,
            elapsedSinceRequestStartNanoseconds: elapsedSinceRequestStartNanoseconds
        ) {
            return recommendation(context, generation: responseGeneration, hold: reason)
        }

        guard requestIsValid(context.request) else {
            return recommendation(context, generation: responseGeneration, hold: .invalidContext)
        }

        guard readiness.sessionID.utf8.elementsEqual(context.request.sessionID.utf8),
            readiness.variantID.utf8.elementsEqual(context.request.variantID.utf8),
            readiness.windowStartSeconds == context.request.windowStartSeconds,
            readiness.windowDurationSeconds == context.request.windowDurationSeconds
        else {
            return recommendation(context, generation: responseGeneration, hold: .identityMismatch)
        }

        guard readiness.sessionGeneration > 0 else {
            return recommendation(context, generation: readiness.sessionGeneration, hold: .invalidReadiness)
        }
        if let expected = context.expectedSessionGeneration,
            readiness.sessionGeneration != expected
        {
            return recommendation(context, generation: readiness.sessionGeneration, hold: .generationMismatch)
        }

        switch readiness.cacheState {
        case .unknown, .unspecified, .unrecognized(_):
            return recommendation(context, generation: readiness.sessionGeneration, hold: .cacheUnknown)
        case .readable:
            switch readiness.cacheReason {
            case .none:
                break
            case .unspecified, .unrecognized(_):
                return recommendation(context, generation: readiness.sessionGeneration, hold: .cacheUnknown)
            default:
                return recommendation(context, generation: readiness.sessionGeneration, hold: .invalidReadiness)
            }
            guard context.targetDecoderSupported == true else {
                return recommendation(
                    context,
                    generation: readiness.sessionGeneration,
                    hold: context.targetDecoderSupported == false ? .decoderUnsupported : .decoderUnknown
                )
            }
            return recommendation(context, generation: readiness.sessionGeneration, outcome: .cacheBackedOpportunity)
        case .missing:
            switch readiness.cacheReason {
            case .notCached:
                break
            case .outOfRange:
                return recommendation(context, generation: readiness.sessionGeneration, hold: .cacheNotReadable)
            case .unrecognized(_):
                return recommendation(context, generation: readiness.sessionGeneration, hold: .cacheUnknown)
            case .unspecified, .none, .indexUnavailable, .busy, .budgetExceeded,
                .identityChanged, .ioFailure, .unknownGenerationChanged:
                return recommendation(context, generation: readiness.sessionGeneration, hold: .cacheNotReadable)
            }
        }

        guard context.targetDecoderSupported == true else {
            return recommendation(
                context,
                generation: readiness.sessionGeneration,
                hold: context.targetDecoderSupported == false ? .decoderUnsupported : .decoderUnknown
            )
        }

        guard
            let preparationWindow = protectedPreparationWindow(
                context,
                elapsedSinceRequestStartNanoseconds: elapsedSinceRequestStartNanoseconds
            )
        else {
            return recommendation(
                context,
                generation: readiness.sessionGeneration,
                hold: .insufficientPreparationTime
            )
        }
        switch originPrewarmEstimate(
            context: context,
            readiness: readiness,
            elapsedSinceRequestStartNanoseconds: elapsedSinceRequestStartNanoseconds,
            preparationWindow: preparationWindow
        ) {
        case .estimate(let estimate):
            return recommendation(
                context,
                generation: readiness.sessionGeneration,
                outcome: .originPrewarmOpportunity(estimate)
            )
        case .insufficientEvidence:
            return recommendation(
                context,
                generation: readiness.sessionGeneration,
                hold: .insufficientOriginEvidence
            )
        case .insufficientTime:
            return recommendation(
                context,
                generation: readiness.sessionGeneration,
                hold: .insufficientPreparationTime
            )
        }
    }

    static func preflightHoldReason(
        for context: HLSUpshiftContext,
        elapsedSinceRequestStartNanoseconds: UInt64 = 0
    ) -> HLSUpshiftHoldReason? {
        guard context.isPlaying else {
            return .notPlaying
        }
        let nativePlaybackRate = Float(context.playbackRate)
        guard requestIsValid(context.request),
            context.playheadSeconds.isFinite,
            context.playheadSeconds >= 0,
            context.contiguousBufferedEndSeconds.isFinite,
            context.contiguousBufferedEndSeconds > context.playheadSeconds,
            context.playbackRate.isFinite,
            context.playbackRate > 0,
            nativePlaybackRate.isFinite,
            nativePlaybackRate > 0,
            context.expectedSessionGeneration.map({ $0 > 0 }) ?? true
        else {
            return .invalidContext
        }

        guard
            let ageMillis = effectiveContextAgeMillis(
                context.contextAgeMillis,
                elapsedSinceRequestStartNanoseconds: elapsedSinceRequestStartNanoseconds
            ), ageMillis <= maximumSnapshotAgeMillis
        else {
            return .staleContext
        }

        let elapsedSeconds = Double(ageMillis) / 1_000
        let advancedPlayhead = context.playheadSeconds + context.playbackRate * elapsedSeconds
        guard advancedPlayhead.isFinite else {
            return .invalidContext
        }
        let windowEnd = context.request.windowStartSeconds + context.request.windowDurationSeconds
        guard windowEnd > advancedPlayhead else {
            return .insufficientPreparationTime
        }
        guard context.request.windowStartSeconds <= context.contiguousBufferedEndSeconds else {
            return .insufficientPreparationTime
        }
        guard advancedPlayhead < context.contiguousBufferedEndSeconds else {
            return .insufficientPreparationTime
        }
        return nil
    }

    static func holdRecommendation(
        for context: HLSUpshiftContext,
        reason: HLSUpshiftHoldReason,
        generation: UInt64? = nil
    ) -> HLSUpshiftRecommendation {
        recommendation(context, generation: generation ?? context.expectedSessionGeneration, hold: reason)
    }

    private static func recommendation(
        _ context: HLSUpshiftContext,
        generation: UInt64?,
        hold: HLSUpshiftHoldReason
    ) -> HLSUpshiftRecommendation {
        recommendation(context, generation: generation, outcome: .hold(hold))
    }

    private static func recommendation(
        _ context: HLSUpshiftContext,
        generation: UInt64?,
        outcome: HLSUpshiftOutcome
    ) -> HLSUpshiftRecommendation {
        HLSUpshiftRecommendation(context: context, sessionGeneration: generation, outcome: outcome)
    }

    private static func requestIsValid(_ request: HLSPlaybackReadinessRequest) -> Bool {
        do {
            try request.validate()
            return true
        } catch {
            return false
        }
    }

    private static func effectiveContextAgeMillis(
        _ contextAgeMillis: UInt64,
        elapsedSinceRequestStartNanoseconds: UInt64
    ) -> UInt64? {
        let elapsedMillis =
            elapsedSinceRequestStartNanoseconds / 1_000_000
            + (elapsedSinceRequestStartNanoseconds % 1_000_000 == 0 ? 0 : 1)
        let (age, overflow) = contextAgeMillis.addingReportingOverflow(elapsedMillis)
        return overflow ? nil : age
    }

    private static func protectedPreparationWindow(
        _ context: HLSUpshiftContext,
        elapsedSinceRequestStartNanoseconds: UInt64
    ) -> Double? {
        guard
            let ageMillis = effectiveContextAgeMillis(
                context.contextAgeMillis,
                elapsedSinceRequestStartNanoseconds: elapsedSinceRequestStartNanoseconds
            )
        else {
            return nil
        }
        let advanced =
            context.playheadSeconds
            + context.playbackRate * (Double(ageMillis) / 1_000)
        guard advanced.isFinite else {
            return nil
        }

        let protectedEnd = advanced + protectedBufferMediaSeconds
        guard protectedEnd.isFinite,
            context.request.windowStartSeconds > protectedEnd,
            context.request.windowStartSeconds <= context.contiguousBufferedEndSeconds
        else {
            return nil
        }

        let remainingMediaSeconds = context.request.windowStartSeconds - protectedEnd
        let available = (remainingMediaSeconds / context.playbackRate).nextDown
        guard available.isFinite, available > 0 else {
            return nil
        }
        return available
    }

    private static func originPrewarmEstimate(
        context: HLSUpshiftContext,
        readiness: HLSPlaybackReadiness,
        elapsedSinceRequestStartNanoseconds: UInt64,
        preparationWindow: Double
    ) -> OriginEstimateResult {
        guard let videoBitrate = context.targetVideoBitrateBitsPerSecond,
            videoBitrate > 0,
            let video = validatedSupply(
                readiness.videoOrigin,
                elapsedSinceRequestStartNanoseconds: elapsedSinceRequestStartNanoseconds
            )
        else {
            return .insufficientEvidence
        }

        var totalBitrate = videoBitrate
        var totalTransferSeconds = transferSeconds(
            windowDuration: context.request.windowDurationSeconds,
            targetBitrate: videoBitrate,
            throughput: video.throughputBitsPerSecond
        )
        var totalLatencySeconds = (Double(video.firstByteLatencyMicros) / 1_000_000).nextUp
        var effectiveThroughput = video.throughputBitsPerSecond

        if context.requiresAudio {
            guard let audioBitrate = context.targetAudioBitrateBitsPerSecond,
                audioBitrate > 0,
                let audio = validatedSupply(
                    readiness.audioOrigin,
                    elapsedSinceRequestStartNanoseconds: elapsedSinceRequestStartNanoseconds
                )
            else {
                return .insufficientEvidence
            }
            let (combinedBitrate, overflow) = totalBitrate.addingReportingOverflow(audioBitrate)
            guard !overflow else {
                return .insufficientEvidence
            }
            totalBitrate = combinedBitrate
            totalTransferSeconds =
                (totalTransferSeconds
                + transferSeconds(
                    windowDuration: context.request.windowDurationSeconds,
                    targetBitrate: audioBitrate,
                    throughput: audio.throughputBitsPerSecond
                )).nextUp
            totalLatencySeconds = (totalLatencySeconds + Double(audio.firstByteLatencyMicros) / 1_000_000).nextUp
            effectiveThroughput = min(effectiveThroughput, audio.throughputBitsPerSecond)
        }

        let sustainedRate = Double(totalBitrate) * context.playbackRate
        let requiredThroughput = sustainedRate * transferHeadroom
        let transferWithLatency = (totalTransferSeconds + totalLatencySeconds).nextUp
        let estimated = (transferWithLatency * transferHeadroom).nextUp
        guard
            totalTransferSeconds.isFinite,
            totalLatencySeconds.isFinite,
            transferWithLatency.isFinite,
            estimated.isFinite,
            requiredThroughput.isFinite,
            Double(effectiveThroughput) >= requiredThroughput
        else {
            return .insufficientEvidence
        }
        guard estimated <= preparationWindow else {
            return .insufficientTime
        }
        return .estimate(
            HLSOriginPrewarmEstimate(
                estimatedPreparationSeconds: estimated,
                availablePreparationSeconds: preparationWindow
            ))
    }

    private static func validatedSupply(
        _ evidence: HLSOriginSupplyEvidence?,
        elapsedSinceRequestStartNanoseconds: UInt64
    ) -> (throughputBitsPerSecond: UInt64, firstByteLatencyMicros: UInt64)? {
        guard let evidence,
            evidence.throughputBitsPerSecond > 0,
            let latency = evidence.firstByteLatencyMicros,
            evidence.freshnessLimitMillis > 0
        else {
            return nil
        }

        let elapsedMillis =
            elapsedSinceRequestStartNanoseconds / 1_000_000
            + (elapsedSinceRequestStartNanoseconds % 1_000_000 == 0 ? 0 : 1)
        let (effectiveAge, overflow) = evidence.ageMillis.addingReportingOverflow(elapsedMillis)
        let freshnessLimit = min(evidence.freshnessLimitMillis, maximumOriginFreshnessMillis)
        guard !overflow, effectiveAge <= freshnessLimit else {
            return nil
        }
        return (evidence.throughputBitsPerSecond, latency)
    }

    private static func transferSeconds(
        windowDuration: Double,
        targetBitrate: UInt64,
        throughput: UInt64
    ) -> Double {
        let estimate = windowDuration * Double(targetBitrate) / Double(throughput)
        return estimate.isFinite ? estimate.nextUp : .infinity
    }

    private enum OriginEstimateResult {
        case estimate(HLSOriginPrewarmEstimate)
        case insufficientEvidence
        case insufficientTime
    }
}
