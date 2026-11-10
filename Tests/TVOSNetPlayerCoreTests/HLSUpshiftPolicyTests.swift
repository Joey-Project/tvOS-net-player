import XCTest
import TVOSNetPlayerCacheClient
@testable import TVOSNetPlayerCore

final class HLSUpshiftPolicyTests: XCTestCase {
    func testReadableExactWindowDoesNotNeedOriginOrBitrateEvidence() {
        let context = makeContext(
            videoBitrate: nil,
            audioBitrate: nil,
            requiresAudio: true
        )
        let readiness = makeReadiness(
            state: .readable,
            reason: .none,
            video: nil,
            audio: nil
        )

        let result = assess(context, readiness)

        XCTAssertEqual(result.outcome, .cacheBackedOpportunity)
        XCTAssertEqual(result.sessionGeneration, 7)
        XCTAssertEqual(
            assess(makeContext(playbackRate: 0.1, videoBitrate: nil, audioBitrate: nil), readiness).outcome,
            .cacheBackedOpportunity
        )
    }

    func testReadableCacheRemainsOpportunityWithPoorOriginEvidence() {
        let context = makeContext(videoBitrate: nil, audioBitrate: nil)
        let readiness = makeReadiness(
            state: .readable,
            reason: .none,
            video: supply(age: 30_001, latency: nil, freshness: 30_000),
            audio: nil
        )

        XCTAssertEqual(assess(context, readiness).outcome, .cacheBackedOpportunity)
    }

    func testReadableWindowMustRemainUpcomingAndBridged() {
        let cases: [(String, HLSUpshiftContext, UInt64, HLSUpshiftOutcome)] = [
            (
                "already passed",
                makeContext(request: makeRequest(start: 0, duration: 1), playhead: 2, bufferedEnd: 10),
                0,
                .hold(.insufficientPreparationTime)
            ),
            (
                "passed during RPC",
                makeContext(request: makeRequest(start: 0, duration: 1), bufferedEnd: 10),
                2_000_000_000,
                .hold(.insufficientPreparationTime)
            ),
            (
                "2x playhead progression",
                makeContext(
                    request: makeRequest(start: 0, duration: 1),
                    bufferedEnd: 10,
                    playbackRate: 2
                ),
                500_000_000,
                .hold(.insufficientPreparationTime)
            ),
            (
                "future window across an unbridged gap",
                makeContext(request: makeRequest(start: 11, duration: 1), bufferedEnd: 10),
                0,
                .hold(.insufficientPreparationTime)
            ),
            (
                "current readable window",
                makeContext(
                    request: makeRequest(start: 0, duration: 1),
                    bufferedEnd: 5,
                    videoBitrate: nil,
                    audioBitrate: nil
                ),
                0,
                .cacheBackedOpportunity
            ),
            (
                "bridged readable window without protected-margin requirement",
                makeContext(
                    request: makeRequest(start: 4, duration: 1),
                    bufferedEnd: 5,
                    videoBitrate: nil,
                    audioBitrate: nil
                ),
                0,
                .cacheBackedOpportunity
            ),
        ]

        for (name, context, elapsedNanoseconds, expected) in cases {
            let readiness = makeReadiness(
                request: context.request,
                state: .readable,
                reason: .none,
                video: nil,
                audio: nil
            )
            XCTAssertEqual(
                assess(context, readiness, elapsedNanoseconds: elapsedNanoseconds).outcome,
                expected,
                name
            )
        }
    }

    func testMissingCacheRequiresFreshExactVideoEvidenceAndProducesOnlyPrewarm() {
        let result = assess(makeContext(), makeReadiness())

        guard case .originPrewarmOpportunity(let estimate) = result.outcome else {
            return XCTFail("Expected an origin prewarm opportunity, got \(result.outcome)")
        }
        XCTAssertGreaterThan(estimate.estimatedPreparationSeconds, 0)
        XCTAssertGreaterThan(estimate.availablePreparationSeconds, estimate.estimatedPreparationSeconds)
    }

    func testOutOfRangeHoldsEvenWithStrongOriginEvidence() {
        let readiness = makeReadiness(state: .missing, reason: .outOfRange)

        XCTAssertEqual(assess(makeContext(), readiness).outcome, .hold(.cacheNotReadable))
    }

    func testUnknownAndUnrecognizedCacheEnumsHold() {
        let cases: [(HLSPlaybackCacheState, HLSPlaybackCacheReason)] = [
            (.unknown, .notCached),
            (.unspecified, .none),
            (.unrecognized(81), .notCached),
            (.readable, .unrecognized(82)),
            (.missing, .unrecognized(83)),
        ]

        for (state, reason) in cases {
            XCTAssertEqual(
                assess(makeContext(), makeReadiness(state: state, reason: reason)).outcome,
                .hold(.cacheUnknown),
                "Expected unknown state/reason to hold: \(state), \(reason)"
            )
        }
    }

    func testInconsistentCacheStateAndReasonHold() {
        XCTAssertEqual(
            assess(makeContext(), makeReadiness(state: .readable, reason: .notCached)).outcome,
            .hold(.invalidReadiness)
        )
        XCTAssertEqual(
            assess(makeContext(), makeReadiness(state: .missing, reason: .none)).outcome,
            .hold(.cacheNotReadable)
        )
        XCTAssertEqual(
            assess(makeContext(), makeReadiness(state: .missing, reason: .indexUnavailable)).outcome,
            .hold(.cacheNotReadable)
        )
    }

    func testResponseIdentityWindowAndGenerationMustMatch() {
        let context = makeContext(expectedGeneration: 7)
        XCTAssertEqual(
            assess(context, makeReadiness(sessionID: "other-session")).outcome,
            .hold(.identityMismatch)
        )
        XCTAssertEqual(
            assess(context, makeReadiness(windowStart: 41)).outcome,
            .hold(.identityMismatch)
        )
        XCTAssertEqual(
            assess(context, makeReadiness(generation: 8)).outcome,
            .hold(.generationMismatch)
        )
        XCTAssertEqual(
            assess(context, makeReadiness(generation: 0)).outcome,
            .hold(.invalidReadiness)
        )

        let byteDistinctRequest = HLSPlaybackReadinessRequest(
            sessionID: "\u{00E9}",
            variantID: "variant-720p",
            windowStartSeconds: 40,
            windowDurationSeconds: 8
        )
        let byteDistinctContext = makeContext(request: byteDistinctRequest)
        let byteDistinctReadiness = makeReadiness(request: byteDistinctRequest, sessionID: "e\u{0301}")
        let byteDistinctResult = assess(byteDistinctContext, byteDistinctReadiness)
        XCTAssertEqual(byteDistinctResult.outcome, .hold(.identityMismatch))
        XCTAssertNil(byteDistinctResult.sessionGeneration)
    }

    func testContextAgeAndRequestRoundTripExpireSnapshotConservatively() {
        let readiness = makeReadiness(state: .readable, reason: .none, video: nil, audio: nil)
        XCTAssertEqual(
            assess(makeContext(contextAge: 5_000), readiness).outcome,
            .cacheBackedOpportunity
        )
        XCTAssertEqual(
            assess(makeContext(contextAge: 5_001), readiness).outcome,
            .hold(.staleContext)
        )
        XCTAssertEqual(
            assess(makeContext(contextAge: 5_000), readiness, elapsedNanoseconds: 1).outcome,
            .hold(.staleContext)
        )
    }

    func testOriginFreshnessUsesQueryAgeAndThirtySecondCap() {
        let context = makeContext()
        let exactLimit = makeReadiness(video: supply(age: 29_999, freshness: 45_000))
        let beyondLimit = makeReadiness(video: supply(age: 30_000, freshness: 45_000))

        XCTAssertTrue(isOriginOpportunity(assess(context, exactLimit, elapsedNanoseconds: 1_000_000)))
        XCTAssertEqual(
            assess(context, beyondLimit, elapsedNanoseconds: 1_000_000).outcome,
            .hold(.insufficientOriginEvidence)
        )
        XCTAssertEqual(
            assess(context, makeReadiness(video: supply(age: 0, freshness: 0))).outcome,
            .hold(.insufficientOriginEvidence)
        )
    }

    func testRequiredAudioNeedsBitrateFreshSupplyAndLatency() {
        let noAudioBitrate = makeContext(audioBitrate: nil, requiresAudio: true)
        let noAudioSupply = makeReadiness(audio: nil)
        let noAudioLatency = makeReadiness(audio: supply(latency: nil))

        XCTAssertEqual(assess(noAudioBitrate, makeReadiness()).outcome, .hold(.insufficientOriginEvidence))
        XCTAssertEqual(assess(makeContext(), noAudioSupply).outcome, .hold(.insufficientOriginEvidence))
        XCTAssertEqual(assess(makeContext(), noAudioLatency).outcome, .hold(.insufficientOriginEvidence))
    }

    func testVideoSupplyNeedsKnownLatencyAndPositiveThroughput() {
        XCTAssertEqual(
            assess(makeContext(), makeReadiness(video: supply(latency: nil))).outcome,
            .hold(.insufficientOriginEvidence)
        )
        XCTAssertEqual(
            assess(makeContext(), makeReadiness(video: supply(throughput: 0))).outcome,
            .hold(.insufficientOriginEvidence)
        )
    }

    func testDecoderAndPlayingStateAreExplicitRequirements() {
        XCTAssertEqual(
            assess(makeContext(isPlaying: false), makeReadiness()).outcome,
            .hold(.notPlaying)
        )
        XCTAssertEqual(
            assess(makeContext(decoderSupported: nil), makeReadiness()).outcome,
            .hold(.decoderUnknown)
        )
        XCTAssertEqual(
            assess(makeContext(decoderSupported: false), makeReadiness()).outcome,
            .hold(.decoderUnsupported)
        )
    }

    func testInvalidRateAndMissingProtectedCoverageHold() {
        let protectedBoundaryRequest = makeRequest(start: 20)
        XCTAssertEqual(
            assess(makeContext(playbackRate: 0), makeReadiness()).outcome,
            .hold(.invalidContext)
        )
        XCTAssertEqual(
            assess(
                makeContext(request: protectedBoundaryRequest),
                makeReadiness(request: protectedBoundaryRequest)
            ).outcome,
            .hold(.insufficientPreparationTime)
        )
        XCTAssertEqual(
            assess(makeContext(bufferedEnd: 39, windowStart: 40), makeReadiness()).outcome,
            .hold(.insufficientPreparationTime)
        )
        let passedWindowRequest = makeRequest(start: 0, duration: 1)
        XCTAssertEqual(
            assess(
                makeContext(request: passedWindowRequest, playhead: 1),
                makeReadiness(request: passedWindowRequest, state: .readable, reason: .none, video: nil, audio: nil)
            ).outcome,
            .hold(.insufficientPreparationTime)
        )
    }

    func testPlaybackRateScalesAvailableWallTimeFromMediaBuffer() {
        let request = makeRequest(start: 21.4, duration: 1)
        let contextAtPointSevenFive = makeContext(
            request: request,
            playbackRate: 0.75,
            videoBitrate: 1_000_000,
            audioBitrate: nil,
            requiresAudio: false
        )
        let contextAtTwo = makeContext(
            request: request,
            playbackRate: 2,
            videoBitrate: 1_000_000,
            audioBitrate: nil,
            requiresAudio: false
        )
        let readiness = makeReadiness(
            request: request,
            video: supply(throughput: 20_000_000, latency: 600_000),
            audio: nil
        )

        let slower = assess(contextAtPointSevenFive, readiness)
        guard case .originPrewarmOpportunity(let slowerEstimate) = slower.outcome else {
            return XCTFail("Expected 0.75x playback to leave enough wall time, got \(slower.outcome)")
        }
        XCTAssertEqual(slowerEstimate.availablePreparationSeconds, 1.8666666666666665, accuracy: 0.000_001)
        XCTAssertEqual(
            assess(contextAtTwo, readiness).outcome,
            .hold(.insufficientPreparationTime)
        )
    }

    func testRequiredOngoingSupplyUsesCombinedBitrateAndPlaybackRate() {
        let context = makeContext(playbackRate: 2)
        let weakVideo = makeReadiness(
            video: supply(throughput: 10_000_000),
            audio: supply(throughput: 10_000_000)
        )

        XCTAssertEqual(assess(context, weakVideo).outcome, .hold(.insufficientOriginEvidence))
    }

    func testIntegerAndFloatingPointOverflowFailClosed() {
        XCTAssertEqual(
            assess(makeContext(contextAge: .max), makeReadiness()).outcome,
            .hold(.staleContext)
        )
        XCTAssertEqual(
            assess(
                makeContext(playbackRate: .greatestFiniteMagnitude, contextAge: 5_000),
                makeReadiness()
            ).outcome,
            .hold(.invalidContext)
        )
        XCTAssertEqual(
            assess(makeContext(playbackRate: .leastNonzeroMagnitude), makeReadiness()).outcome,
            .hold(.invalidContext)
        )
        XCTAssertEqual(
            assess(
                makeContext(videoBitrate: .max, audioBitrate: 1, requiresAudio: true),
                makeReadiness()
            ).outcome,
            .hold(.insufficientOriginEvidence)
        )
        XCTAssertEqual(
            assess(
                makeContext(),
                makeReadiness(video: supply(age: .max, freshness: .max)),
                elapsedNanoseconds: .max
            ).outcome,
            .hold(.staleContext)
        )
        XCTAssertEqual(
            assess(
                makeContext(),
                makeReadiness(video: supply(age: .max, freshness: .max)),
                elapsedNanoseconds: 1_000_000
            ).outcome,
            .hold(.insufficientOriginEvidence)
        )
    }

    private func assess(
        _ context: HLSUpshiftContext,
        _ readiness: HLSPlaybackReadiness,
        elapsedNanoseconds: UInt64 = 0
    ) -> HLSUpshiftRecommendation {
        HLSUpshiftPolicy.assess(
            context: context,
            readiness: readiness,
            elapsedSinceRequestStartNanoseconds: elapsedNanoseconds
        )
    }

    private func isOriginOpportunity(_ recommendation: HLSUpshiftRecommendation) -> Bool {
        if case .originPrewarmOpportunity = recommendation.outcome {
            return true
        }
        return false
    }

    private func makeContext(
        request: HLSPlaybackReadinessRequest? = nil,
        isPlaying: Bool = true,
        playhead: Double = 0,
        bufferedEnd: Double = 100,
        playbackRate: Double = 1,
        videoBitrate: UInt64? = 6_000_000,
        audioBitrate: UInt64? = 192_000,
        requiresAudio: Bool = true,
        decoderSupported: Bool? = true,
        expectedGeneration: UInt64? = 7,
        contextAge: UInt64 = 0,
        windowStart: Double = 40
    ) -> HLSUpshiftContext {
        HLSUpshiftContext(
            request: request ?? makeRequest(start: windowStart),
            isPlaying: isPlaying,
            playheadSeconds: playhead,
            contiguousBufferedEndSeconds: bufferedEnd,
            playbackRate: playbackRate,
            targetVideoBitrateBitsPerSecond: videoBitrate,
            targetAudioBitrateBitsPerSecond: audioBitrate,
            requiresAudio: requiresAudio,
            targetDecoderSupported: decoderSupported,
            expectedSessionGeneration: expectedGeneration,
            contextAgeMillis: contextAge
        )
    }

    private func makeRequest(start: Double = 40, duration: Double = 8) -> HLSPlaybackReadinessRequest {
        HLSPlaybackReadinessRequest(
            sessionID: "session-1",
            variantID: "variant-720p",
            windowStartSeconds: start,
            windowDurationSeconds: duration
        )
    }

    private func makeReadiness(
        request: HLSPlaybackReadinessRequest? = nil,
        sessionID: String = "session-1",
        variantID: String = "variant-720p",
        windowStart: Double = 40,
        state: HLSPlaybackCacheState = .missing,
        reason: HLSPlaybackCacheReason = .notCached,
        generation: UInt64 = 7,
        video: HLSOriginSupplyEvidence? = HLSOriginSupplyEvidence(
            ageMillis: 1_000,
            throughputBitsPerSecond: 30_000_000,
            firstByteLatencyMicros: 100_000,
            freshnessLimitMillis: 30_000
        ),
        audio: HLSOriginSupplyEvidence? = HLSOriginSupplyEvidence(
            ageMillis: 1_000,
            throughputBitsPerSecond: 20_000_000,
            firstByteLatencyMicros: 50_000,
            freshnessLimitMillis: 30_000
        )
    ) -> HLSPlaybackReadiness {
        let exactRequest = request ?? makeRequest(start: windowStart)
        return HLSPlaybackReadiness(
            sessionID: sessionID,
            variantID: variantID,
            windowStartSeconds: exactRequest.windowStartSeconds,
            windowDurationSeconds: exactRequest.windowDurationSeconds,
            sessionGeneration: generation,
            cacheState: state,
            cacheReason: reason,
            inspectionBudgetUsedBytes: .max,
            videoOrigin: video,
            audioOrigin: audio
        )
    }

    private func supply(
        age: UInt64 = 1_000,
        throughput: UInt64 = 30_000_000,
        latency: UInt64? = 100_000,
        freshness: UInt64 = 30_000
    ) -> HLSOriginSupplyEvidence {
        HLSOriginSupplyEvidence(
            ageMillis: age,
            throughputBitsPerSecond: throughput,
            firstByteLatencyMicros: latency,
            freshnessLimitMillis: freshness
        )
    }
}
