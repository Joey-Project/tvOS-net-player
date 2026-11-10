import Foundation
import TVOSNetPlayerCacheClient

public struct HLSUpshiftAssessmentService: Sendable {
    private let client: any CacheControlClient
    private let serverSummary: CacheServerSummary
    private let monotonicNowNanoseconds: @Sendable () -> UInt64

    public init(
        client: any CacheControlClient,
        serverSummary: CacheServerSummary,
        monotonicNowNanoseconds: @escaping @Sendable () -> UInt64 = { DispatchTime.now().uptimeNanoseconds }
    ) {
        self.client = client
        self.serverSummary = serverSummary
        self.monotonicNowNanoseconds = monotonicNowNanoseconds
    }

    /// Returns sanitized holds for cancellation and transport failures; client errors are not exposed.
    public func assess(_ context: HLSUpshiftContext) async -> HLSUpshiftRecommendation {
        if Task.isCancelled {
            return HLSUpshiftPolicy.holdRecommendation(for: context, reason: .cancelled)
        }
        if let reason = HLSUpshiftPolicy.preflightHoldReason(for: context) {
            return HLSUpshiftPolicy.holdRecommendation(for: context, reason: reason)
        }
        guard serverSummary.supportsHLSPlaybackReadiness else {
            return HLSUpshiftPolicy.holdRecommendation(for: context, reason: .unsupportedServer)
        }
        if Task.isCancelled {
            return HLSUpshiftPolicy.holdRecommendation(for: context, reason: .cancelled)
        }

        let requestStarted = monotonicNowNanoseconds()
        if Task.isCancelled {
            return HLSUpshiftPolicy.holdRecommendation(for: context, reason: .cancelled)
        }

        do {
            let readiness = try await client.getHLSPlaybackReadiness(context.request)
            let assessmentTime = monotonicNowNanoseconds()
            if Task.isCancelled {
                return cancellationRecommendation(context, readiness: readiness)
            }
            guard assessmentTime >= requestStarted else {
                return HLSUpshiftPolicy.holdRecommendation(for: context, reason: .clockAnomaly)
            }

            let recommendation = HLSUpshiftPolicy.assess(
                context: context,
                readiness: readiness,
                elapsedSinceRequestStartNanoseconds: assessmentTime - requestStarted
            )
            if Task.isCancelled {
                return cancellationRecommendation(context, readiness: readiness)
            }
            return recommendation
        } catch is CancellationError {
            return HLSUpshiftPolicy.holdRecommendation(for: context, reason: .cancelled)
        } catch let error as CacheControlClientUnsupportedOperation
            where error == .hlsPlaybackReadiness
        {
            return HLSUpshiftPolicy.holdRecommendation(
                for: context,
                reason: Task.isCancelled ? .cancelled : .unsupportedServer
            )
        } catch {
            return HLSUpshiftPolicy.holdRecommendation(
                for: context,
                reason: Task.isCancelled ? .cancelled : .requestFailed
            )
        }
    }

    private func cancellationRecommendation(
        _ context: HLSUpshiftContext,
        readiness: HLSPlaybackReadiness
    ) -> HLSUpshiftRecommendation {
        let responseMatchesSession =
            readiness.sessionID.utf8.elementsEqual(context.request.sessionID.utf8)
            && readiness.variantID.utf8.elementsEqual(context.request.variantID.utf8)
        return HLSUpshiftRecommendation(
            context: context,
            sessionGeneration: responseMatchesSession ? readiness.sessionGeneration : nil,
            outcome: .hold(.cancelled)
        )
    }
}
