import AVFoundation
import Foundation
import XCTest
import TVOSNetPlayerCacheClient
@testable import TVOSNetPlayerCore

final class HLSPlaybackSnapshotTests: XCTestCase {
    func testContinuousClockConversionFailsClosedAndRoundsSubnanosecondsUp() {
        let epoch = ContinuousClock().now
        let missing = HLSPlaybackMonotonicClock(epoch: epoch, readInstant: { nil })
        let backward = HLSPlaybackMonotonicClock(epoch: epoch, readInstant: { epoch.advanced(by: .seconds(-1)) })

        XCTAssertNil(missing.nowNanoseconds())
        XCTAssertNil(backward.nowNanoseconds())
        XCTAssertEqual(
            HLSPlaybackMonotonicClock.checkedNanoseconds(
                for: Duration(secondsComponent: 0, attosecondsComponent: 1)
            ),
            1
        )
        XCTAssertNil(
            HLSPlaybackMonotonicClock.checkedNanoseconds(
                for: Duration(secondsComponent: -1, attosecondsComponent: 0)
            )
        )
        XCTAssertNil(
            HLSPlaybackMonotonicClock.checkedNanoseconds(
                for: Duration(secondsComponent: Int64.max, attosecondsComponent: 0)
            )
        )
    }

    func testCaptureSortsAndJoinsOverlappingAndTouchingRanges() throws {
        let snapshot = try XCTUnwrap(
            capture(
                playhead: 4,
                ranges: [range(8, 2), range(0, 3), range(2.5, 2), range(4.5, 3.5), range(10, 1)]
            ))

        XCTAssertEqual(snapshot.contiguousBufferedEndSeconds, 11)
    }

    func testCaptureDoesNotBridgeTrueGapAndKeepsUncoveredSnapshot() throws {
        let snapshot = try XCTUnwrap(capture(playhead: 2, ranges: [range(0, 3), range(3.01, 4)]))

        XCTAssertEqual(snapshot.contiguousBufferedEndSeconds, 3)
        XCTAssertEqual(snapshot.playheadSeconds, 2)
    }

    func testRangesMustCoverPlayheadBeforeFrontierIsKnown() throws {
        let snapshot = try XCTUnwrap(capture(playhead: 2, ranges: [range(3, 2)]))

        XCTAssertNil(snapshot.contiguousBufferedEndSeconds)
    }

    func testInvalidRangeMakesFrontierUnknownWithoutDiscardingPlaybackState() throws {
        let snapshot = try XCTUnwrap(capture(playhead: 1, ranges: [range(0, 2), range(2, 0)]))

        XCTAssertNil(snapshot.contiguousBufferedEndSeconds)
        XCTAssertTrue(snapshot.isPlaying)
        XCTAssertEqual(snapshot.playheadSeconds, 1)
    }

    func testRangeCountBoundaryAccepts128AndRejects129() throws {
        let accepted = try XCTUnwrap(
            capture(
                playhead: 1,
                ranges: Array(repeating: range(0, 100), count: HLSPlaybackSnapshot.maximumBufferedRangeCount)
            ))
        XCTAssertEqual(accepted.contiguousBufferedEndSeconds, 100)

        let ranges = Array(repeating: range(0, 100), count: 129)
        let snapshot = try XCTUnwrap(capture(playhead: 1, ranges: ranges))

        XCTAssertNil(snapshot.contiguousBufferedEndSeconds)
    }

    func testNestedDuplicateRangesAndPlayheadAtEnd() throws {
        let snapshot = try XCTUnwrap(
            capture(
                playhead: 5,
                ranges: [range(0, 5), range(1, 2), range(0, 5)]
            ))

        XCTAssertEqual(snapshot.contiguousBufferedEndSeconds, 5)
    }

    func testPositiveRationalGapIsNotBridgedWhenDoubleSecondsCollapse() throws {
        let edge = CMTime(value: 8_589_934_588_000_001, timescale: 2_147_483_647)
        let nextTick = CMTime(value: 8_589_934_580_000_001, timescale: 2_147_483_645)
        XCTAssertLessThan(CMTimeCompare(edge, nextTick), 0)
        XCTAssertEqual(CMTimeGetSeconds(edge), CMTimeGetSeconds(nextTick))
        let ranges = [
            CMTimeRange(start: CMTime(value: 0, timescale: 2_147_483_647), duration: edge),
            CMTimeRange(start: nextTick, duration: CMTime(value: 2_147_483_645, timescale: 2_147_483_645)),
        ]
        let snapshot = try XCTUnwrap(
            HLSPlaybackSnapshot.capture(
                timeControlStatus: .playing,
                playbackRate: 1,
                currentTime: CMTime(seconds: 1, preferredTimescale: 600),
                loadedTimeRanges: ranges,
                observedAtNanoseconds: 1
            ))

        let frontier = try XCTUnwrap(snapshot.contiguousBufferedEndSeconds)
        let reference = CMTimeGetSeconds(edge)
        XCTAssertLessThanOrEqual(frontier, reference)
        XCTAssertEqual(frontier, reference, accuracy: 2 * reference.ulp)
    }

    func testConvertsRationalEndWithoutOverestimatingFrontier() throws {
        let start = CMTime(value: 1, timescale: 7)
        let duration = CMTime(value: 1, timescale: 7)
        let snapshot = try XCTUnwrap(
            HLSPlaybackSnapshot.capture(
                timeControlStatus: .playing,
                playbackRate: 1,
                currentTime: CMTime(value: 1, timescale: 7),
                loadedTimeRanges: [CMTimeRange(start: start, duration: duration)],
                observedAtNanoseconds: 1
            ))

        let frontier = try XCTUnwrap(snapshot.contiguousBufferedEndSeconds)
        XCTAssertLessThan(frontier, 2.0 / 7.0)
        XCTAssertEqual(frontier, 2.0 / 7.0, accuracy: 2 * Double.ulpOfOne)
    }

    func testConservativeSecondsFloorsOneTenthAndPreservesExactIntegerBoundaries() throws {
        let fractional = try XCTUnwrap(
            capture(
                playhead: 0,
                ranges: [CMTimeRange(start: .zero, duration: CMTime(value: 1, timescale: 10))]
            ))
        XCTAssertLessThan(try XCTUnwrap(fractional.contiguousBufferedEndSeconds), 0.1)

        let exactInteger = try XCTUnwrap(
            capture(
                playhead: 0,
                ranges: [CMTimeRange(start: .zero, duration: CMTime(value: 30, timescale: 10))]
            ))
        XCTAssertEqual(exactInteger.contiguousBufferedEndSeconds, 3)
    }

    func testRejectsNonintegralSourceNumeratorAboveExactDoubleRange() throws {
        let duration = CMTime(value: 9_007_199_254_740_993, timescale: 2)
        let snapshot = try XCTUnwrap(
            capture(
                playhead: 0,
                ranges: [CMTimeRange(start: .zero, duration: duration)]
            ))

        XCTAssertNil(snapshot.contiguousBufferedEndSeconds)
    }

    func testInvalidPlayheadAndPlayingRatesCannotBeCaptured() {
        XCTAssertNil(
            HLSPlaybackSnapshot.capture(
                timeControlStatus: .playing,
                playbackRate: 1,
                currentTime: .invalid,
                loadedTimeRanges: [],
                observedAtNanoseconds: 1
            ))
        XCTAssertNil(capture(playhead: 1, playbackRate: 0))
        XCTAssertNil(capture(playhead: 1, playbackRate: .infinity))
        XCTAssertNil(capture(playhead: 1, playbackRate: .nan))
    }

    func testRejectsNegativeAndNonzeroEpochTimes() {
        XCTAssertNil(
            HLSPlaybackSnapshot.capture(
                timeControlStatus: .playing,
                playbackRate: 1,
                currentTime: CMTime(seconds: -1, preferredTimescale: 600),
                loadedTimeRanges: [],
                observedAtNanoseconds: 1
            ))
        XCTAssertNil(
            HLSPlaybackSnapshot.capture(
                timeControlStatus: .playing,
                playbackRate: 1,
                currentTime: CMTime(value: 1, timescale: 1, flags: .valid, epoch: 2),
                loadedTimeRanges: [],
                observedAtNanoseconds: 1
            ))
    }

    func testRejectsInvalidRangeClocksAndOverflowedEnds() throws {
        let invalidStart = CMTimeRange(start: .indefinite, duration: CMTime(seconds: 1, preferredTimescale: 600))
        let invalidDuration = CMTimeRange(
            start: CMTime(seconds: 0, preferredTimescale: 600), duration: .positiveInfinity)
        let indefiniteDuration = CMTimeRange(start: .zero, duration: .indefinite)
        let nonzeroEpochDuration = CMTimeRange(
            start: .zero,
            duration: CMTime(value: 1, timescale: 1, flags: .valid, epoch: 1)
        )
        let nonzeroEpoch = CMTimeRange(
            start: CMTime(value: 1, timescale: 1, flags: .valid, epoch: 1),
            duration: CMTime(seconds: 1, preferredTimescale: 600)
        )
        let overflowedEnd = CMTimeRange(
            start: CMTime(seconds: Double.greatestFiniteMagnitude / 2, preferredTimescale: 1),
            duration: CMTime(seconds: Double.greatestFiniteMagnitude, preferredTimescale: 1)
        )

        for invalidRange in [
            invalidStart, invalidDuration, indefiniteDuration, nonzeroEpochDuration, nonzeroEpoch, overflowedEnd,
        ] {
            let snapshot = try XCTUnwrap(capture(playhead: 0.5, ranges: [range(0, 2), invalidRange]))
            XCTAssertNil(snapshot.contiguousBufferedEndSeconds)
        }
    }

    func testBenignRoundedSourceTimeFlagsDoNotDiscardRationalFrontier() throws {
        let start = CMTime(seconds: 3.01, preferredTimescale: 600)
        let duration = CMTime(value: 4, timescale: 1)
        XCTAssertTrue(start.flags.contains(.hasBeenRounded))

        let snapshot = try XCTUnwrap(
            HLSPlaybackSnapshot.capture(
                timeControlStatus: .playing,
                playbackRate: 1,
                currentTime: CMTime(value: 4, timescale: 1),
                loadedTimeRanges: [CMTimeRange(start: start, duration: duration)],
                observedAtNanoseconds: 1
            ))

        let frontier = try XCTUnwrap(snapshot.contiguousBufferedEndSeconds)
        let reference = CMTimeGetSeconds(CMTime(value: 4205, timescale: 600))
        XCTAssertLessThanOrEqual(frontier, reference)
        XCTAssertEqual(frontier, reference, accuracy: 2 * reference.ulp)
    }

    func testNewRoundingFromAddingDistinctNativeScalesMakesFrontierUnknown() throws {
        let start = CMTime(value: 1, timescale: 46_337)
        let duration = CMTime(value: 1, timescale: 46_349)
        XCTAssertFalse(start.flags.contains(.hasBeenRounded))
        XCTAssertFalse(duration.flags.contains(.hasBeenRounded))

        let snapshot = try XCTUnwrap(
            HLSPlaybackSnapshot.capture(
                timeControlStatus: .playing,
                playbackRate: 1,
                currentTime: start,
                loadedTimeRanges: [CMTimeRange(start: start, duration: duration)],
                observedAtNanoseconds: 1
            ))

        XCTAssertNil(snapshot.contiguousBufferedEndSeconds)
    }

    func testUsesActualRateAndPreservesPausedOrWaitingState() throws {
        let fast = try XCTUnwrap(capture(playhead: 1, playbackRate: 1.75))
        let paused = try XCTUnwrap(capture(playhead: 1, playbackRate: 0, status: .paused))
        let waiting = try XCTUnwrap(capture(playhead: 1, playbackRate: 0, status: .waitingToPlayAtSpecifiedRate))

        XCTAssertEqual(fast.playbackRate, 1.75)
        XCTAssertFalse(paused.isPlaying)
        XCTAssertEqual(paused.playbackRate, 0)
        XCTAssertFalse(waiting.isPlaying)
        XCTAssertEqual(waiting.playbackRate, 0)
    }

    func testContextRejectsInvalidRequestsAndClockRegressionAndRoundsAgeUp() throws {
        let snapshot = try XCTUnwrap(capture(playhead: 1, ranges: [range(0, 5)], observedAt: 10))
        let request = makeRequest()

        XCTAssertNil(
            snapshot.context(
                for: makeRequest(sessionID: ""),
                targetVideoBitrateBitsPerSecond: nil,
                targetAudioBitrateBitsPerSecond: nil,
                requiresAudio: false,
                targetDecoderSupported: nil,
                expectedSessionGeneration: nil,
                nowNanoseconds: 10
            ))
        XCTAssertNil(
            makeSnapshot(observedAt: 11).context(
                for: request,
                targetVideoBitrateBitsPerSecond: nil,
                targetAudioBitrateBitsPerSecond: nil,
                requiresAudio: false,
                targetDecoderSupported: nil,
                expectedSessionGeneration: nil,
                nowNanoseconds: 10
            ))

        let context = try XCTUnwrap(
            snapshot.context(
                for: request,
                targetVideoBitrateBitsPerSecond: nil,
                targetAudioBitrateBitsPerSecond: nil,
                requiresAudio: false,
                targetDecoderSupported: nil,
                expectedSessionGeneration: nil,
                nowNanoseconds: 1_000_011
            ))
        XCTAssertEqual(context.contextAgeMillis, 2)
        XCTAssertNil(
            snapshot.context(
                for: request,
                targetVideoBitrateBitsPerSecond: nil,
                targetAudioBitrateBitsPerSecond: nil,
                requiresAudio: false,
                targetDecoderSupported: nil,
                expectedSessionGeneration: nil,
                nowNanoseconds: 9
            ))
    }

    func testMaximumElapsedAgeUsesExistingCeilingSemantics() throws {
        let snapshot = try XCTUnwrap(capture(playhead: 1, ranges: [range(0, 5)], observedAt: 0))
        let context = try XCTUnwrap(
            snapshot.context(
                for: makeRequest(),
                targetVideoBitrateBitsPerSecond: nil,
                targetAudioBitrateBitsPerSecond: nil,
                requiresAudio: false,
                targetDecoderSupported: true,
                expectedSessionGeneration: nil,
                nowNanoseconds: .max
            ))
        let elapsedMillis = UInt64.max / 1_000_000 + (UInt64.max % 1_000_000 == 0 ? 0 : 1)

        XCTAssertEqual(context.contextAgeMillis, elapsedMillis)
    }

    func testSimulatedSleepGapMakesSnapshotStale() throws {
        let snapshot = try XCTUnwrap(capture(playhead: 1, ranges: [range(0, 10)], observedAt: 100))
        let context = try XCTUnwrap(
            snapshot.context(
                for: makeRequest(),
                targetVideoBitrateBitsPerSecond: nil,
                targetAudioBitrateBitsPerSecond: nil,
                requiresAudio: false,
                targetDecoderSupported: true,
                expectedSessionGeneration: nil,
                nowNanoseconds: 5_000_000_101
            )
        )

        XCTAssertEqual(context.contextAgeMillis, 5_001)
        XCTAssertEqual(HLSUpshiftPolicy.preflightHoldReason(for: context), .staleContext)
    }

    func testPolicyCompositionRequiresPlayingCoverageAndExplicitDecoderSupport() throws {
        let request = makeRequest()
        let readiness = readableReadiness(for: request)

        let paused = try XCTUnwrap(
            try XCTUnwrap(
                capture(
                    playhead: 1,
                    playbackRate: 0,
                    status: .paused,
                    ranges: [range(0, 100)]
                )
            ).context(
                for: request,
                targetVideoBitrateBitsPerSecond: nil,
                targetAudioBitrateBitsPerSecond: nil,
                requiresAudio: false,
                targetDecoderSupported: true,
                expectedSessionGeneration: nil,
                nowNanoseconds: 100
            ))
        XCTAssertEqual(HLSUpshiftPolicy.assess(context: paused, readiness: readiness).outcome, .hold(.notPlaying))

        let futureRequest = HLSPlaybackReadinessRequest(
            sessionID: "session",
            variantID: "variant",
            windowStartSeconds: 4,
            windowDurationSeconds: 1
        )
        let gapSnapshot = try XCTUnwrap(capture(playhead: 2, ranges: [range(0, 3), range(3.01, 30)]))
        let gapContext = try XCTUnwrap(
            gapSnapshot.context(
                for: futureRequest,
                targetVideoBitrateBitsPerSecond: nil,
                targetAudioBitrateBitsPerSecond: nil,
                requiresAudio: false,
                targetDecoderSupported: true,
                expectedSessionGeneration: nil,
                nowNanoseconds: 100
            ))
        XCTAssertEqual(
            HLSUpshiftPolicy.assess(context: gapContext, readiness: readableReadiness(for: futureRequest)).outcome,
            .hold(.insufficientPreparationTime)
        )

        let decoderUnknown = try XCTUnwrap(
            try XCTUnwrap(capture(playhead: 1, ranges: [range(0, 100)])).context(
                for: request,
                targetVideoBitrateBitsPerSecond: nil,
                targetAudioBitrateBitsPerSecond: nil,
                requiresAudio: false,
                targetDecoderSupported: nil,
                expectedSessionGeneration: nil,
                nowNanoseconds: 100
            ))
        XCTAssertEqual(
            HLSUpshiftPolicy.assess(context: decoderUnknown, readiness: readiness).outcome,
            .hold(.decoderUnknown)
        )

        let decoderSupported = try XCTUnwrap(
            try XCTUnwrap(capture(playhead: 1, ranges: [range(0, 100)])).context(
                for: request,
                targetVideoBitrateBitsPerSecond: nil,
                targetAudioBitrateBitsPerSecond: nil,
                requiresAudio: false,
                targetDecoderSupported: true,
                expectedSessionGeneration: nil,
                nowNanoseconds: 100
            ))
        XCTAssertEqual(
            HLSUpshiftPolicy.assess(context: decoderSupported, readiness: readiness).outcome,
            .cacheBackedOpportunity
        )
    }

    func testContextPreservesUnknownTargetMetadataAndNonPlayingEvidence() throws {
        let snapshot = try XCTUnwrap(
            capture(playhead: 1, playbackRate: 0, status: .waitingToPlayAtSpecifiedRate, ranges: [range(0, 5)]))
        let context = try XCTUnwrap(
            snapshot.context(
                for: makeRequest(),
                targetVideoBitrateBitsPerSecond: nil,
                targetAudioBitrateBitsPerSecond: nil,
                requiresAudio: true,
                targetDecoderSupported: nil,
                expectedSessionGeneration: 8,
                nowNanoseconds: 100
            ))

        XCTAssertFalse(context.isPlaying)
        XCTAssertEqual(context.playbackRate, 0)
        XCTAssertNil(context.targetVideoBitrateBitsPerSecond)
        XCTAssertNil(context.targetAudioBitrateBitsPerSecond)
        XCTAssertNil(context.targetDecoderSupported)
        XCTAssertEqual(context.expectedSessionGeneration, 8)
    }

    private func capture(
        playhead: Double,
        playbackRate: Float = 1,
        status: AVPlayer.TimeControlStatus = .playing,
        ranges: [CMTimeRange]? = nil,
        observedAt: UInt64 = 100
    ) -> HLSPlaybackSnapshot? {
        HLSPlaybackSnapshot.capture(
            timeControlStatus: status,
            playbackRate: playbackRate,
            currentTime: CMTime(seconds: playhead, preferredTimescale: 600),
            loadedTimeRanges: ranges ?? [Self.range(0, 10)],
            observedAtNanoseconds: observedAt
        )
    }

    private func makeSnapshot(observedAt: UInt64) -> HLSPlaybackSnapshot {
        capture(playhead: 1, observedAt: observedAt)!
    }

    private func makeRequest(sessionID: String = "session") -> HLSPlaybackReadinessRequest {
        HLSPlaybackReadinessRequest(
            sessionID: sessionID,
            variantID: "variant",
            windowStartSeconds: 40,
            windowDurationSeconds: 8
        )
    }

    private func readableReadiness(for request: HLSPlaybackReadinessRequest) -> HLSPlaybackReadiness {
        HLSPlaybackReadiness(
            sessionID: request.sessionID,
            variantID: request.variantID,
            windowStartSeconds: request.windowStartSeconds,
            windowDurationSeconds: request.windowDurationSeconds,
            sessionGeneration: 1,
            cacheState: .readable,
            cacheReason: .none,
            inspectionBudgetUsedBytes: 0,
            videoOrigin: nil,
            audioOrigin: nil
        )
    }

    private static func time(_ seconds: Double) -> CMTime {
        CMTime(seconds: seconds, preferredTimescale: 600)
    }

    private static func range(_ start: Double, _ duration: Double) -> CMTimeRange {
        CMTimeRange(start: time(start), duration: time(duration))
    }

    private func range(_ start: Double, _ duration: Double) -> CMTimeRange {
        Self.range(start, duration)
    }
}
