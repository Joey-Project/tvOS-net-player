import AVFoundation
import Foundation
import TVOSNetPlayerCacheClient

struct HLSPlaybackMonotonicClock: Sendable {
    private let epoch: ContinuousClock.Instant
    private let readInstant: @Sendable () -> ContinuousClock.Instant?

    static let shared = HLSPlaybackMonotonicClock()

    init(clock: ContinuousClock = ContinuousClock()) {
        let epoch = clock.now
        self.epoch = epoch
        self.readInstant = { clock.now }
    }

    init(epoch: ContinuousClock.Instant, readInstant: @escaping @Sendable () -> ContinuousClock.Instant?) {
        self.epoch = epoch
        self.readInstant = readInstant
    }

    func nowNanoseconds() -> UInt64? {
        guard let now = readInstant() else { return nil }
        return Self.checkedNanoseconds(for: epoch.duration(to: now))
    }

    static func checkedNanoseconds(for duration: Duration) -> UInt64? {
        let components = duration.components
        guard components.seconds >= 0, components.attoseconds >= 0,
            let seconds = UInt64(exactly: components.seconds),
            let attoseconds = UInt64(exactly: components.attoseconds)
        else {
            return nil
        }

        let attosecondsPerNanosecond: UInt64 = 1_000_000_000
        let subsecondNanoseconds = attoseconds / attosecondsPerNanosecond
        let hasSubnanosecondRemainder = attoseconds % attosecondsPerNanosecond != 0
        let (roundedSubsecondNanoseconds, roundingOverflow) = subsecondNanoseconds.addingReportingOverflow(
            hasSubnanosecondRemainder ? 1 : 0
        )
        let (wholeSecondsNanoseconds, multiplicationOverflow) = seconds.multipliedReportingOverflow(
            by: attosecondsPerNanosecond
        )
        let (nanoseconds, additionOverflow) = wholeSecondsNanoseconds.addingReportingOverflow(
            roundedSubsecondNanoseconds
        )
        guard !roundingOverflow, !multiplicationOverflow, !additionOverflow else { return nil }
        return nanoseconds
    }
}

/// Immutable playback timing evidence captured at one point in time.
public struct HLSPlaybackSnapshot: Equatable, Sendable {
    private struct BufferedInterval {
        let start: CMTime
        let end: CMTime
    }

    static let maximumBufferedRangeCount = 128

    public let isPlaying: Bool
    public let playheadSeconds: Double
    public let contiguousBufferedEndSeconds: Double?
    public let playbackRate: Double
    public let observedAtNanoseconds: UInt64

    private init(
        isPlaying: Bool,
        playheadSeconds: Double,
        contiguousBufferedEndSeconds: Double?,
        playbackRate: Double,
        observedAtNanoseconds: UInt64
    ) {
        self.isPlaying = isPlaying
        self.playheadSeconds = playheadSeconds
        self.contiguousBufferedEndSeconds = contiguousBufferedEndSeconds
        self.playbackRate = playbackRate
        self.observedAtNanoseconds = observedAtNanoseconds
    }

    public static func capture(
        timeControlStatus: AVPlayer.TimeControlStatus,
        playbackRate rate: Float,
        currentTime: CMTime,
        loadedTimeRanges: [CMTimeRange],
        observedAtNanoseconds: UInt64
    ) -> HLSPlaybackSnapshot? {
        guard let playhead = finiteSeconds(currentTime), playhead >= 0 else {
            return nil
        }

        let playbackRate = Double(rate)
        let isPlaying = timeControlStatus == .playing
        guard playbackRate.isFinite, !isPlaying || playbackRate > 0 else {
            return nil
        }

        return HLSPlaybackSnapshot(
            isPlaying: isPlaying,
            playheadSeconds: playhead,
            contiguousBufferedEndSeconds: contiguousBufferedEnd(
                containing: currentTime,
                ranges: loadedTimeRanges
            ),
            playbackRate: playbackRate,
            observedAtNanoseconds: observedAtNanoseconds
        )
    }

    public func context(
        for request: HLSPlaybackReadinessRequest,
        targetVideoBitrateBitsPerSecond: UInt64?,
        targetAudioBitrateBitsPerSecond: UInt64?,
        requiresAudio: Bool,
        targetDecoderSupported: Bool?,
        expectedSessionGeneration: UInt64?,
        nowNanoseconds: UInt64
    ) -> HLSUpshiftContext? {
        guard nowNanoseconds >= observedAtNanoseconds,
            let contiguousBufferedEndSeconds,
            (try? request.validate()) != nil
        else {
            return nil
        }

        let elapsedNanoseconds = nowNanoseconds - observedAtNanoseconds
        let ageMillis =
            elapsedNanoseconds / 1_000_000
            + (elapsedNanoseconds % 1_000_000 == 0 ? 0 : 1)

        return HLSUpshiftContext(
            request: request,
            isPlaying: isPlaying,
            playheadSeconds: playheadSeconds,
            contiguousBufferedEndSeconds: contiguousBufferedEndSeconds,
            playbackRate: playbackRate,
            targetVideoBitrateBitsPerSecond: targetVideoBitrateBitsPerSecond,
            targetAudioBitrateBitsPerSecond: targetAudioBitrateBitsPerSecond,
            requiresAudio: requiresAudio,
            targetDecoderSupported: targetDecoderSupported,
            expectedSessionGeneration: expectedSessionGeneration,
            contextAgeMillis: ageMillis
        )
    }

    private static func finiteSeconds(_ time: CMTime) -> Double? {
        guard time.isValid, time.isNumeric, time.epoch == 0 else {
            return nil
        }
        let seconds = CMTimeGetSeconds(time)
        return seconds.isFinite ? seconds : nil
    }

    private static func contiguousBufferedEnd(containing playhead: CMTime, ranges: [CMTimeRange]) -> Double? {
        guard ranges.count <= maximumBufferedRangeCount else {
            return nil
        }

        var intervals: [BufferedInterval] = []
        intervals.reserveCapacity(ranges.count)
        for range in ranges {
            guard finiteSeconds(range.start) != nil,
                let duration = finiteSeconds(range.duration),
                range.start.timescale > 0,
                range.duration.timescale > 0,
                CMTimeCompare(range.start, .zero) >= 0,
                duration > 0
            else {
                return nil
            }
            // Normalize source rounding metadata without changing the stored rational value.
            let exactStart = CMTime(value: range.start.value, timescale: range.start.timescale)
            let exactDuration = CMTime(value: range.duration.value, timescale: range.duration.timescale)
            let end = CMTimeAdd(exactStart, exactDuration)
            guard finiteSeconds(end) != nil,
                !end.flags.contains(.hasBeenRounded),
                CMTimeCompare(end, range.start) > 0,
                conservativeSeconds(end) != nil
            else {
                return nil
            }
            intervals.append(BufferedInterval(start: range.start, end: end))
        }

        intervals.sort { CMTimeCompare($0.start, $1.start) < 0 }
        guard
            let coveringIndex = intervals.firstIndex(where: {
                CMTimeCompare($0.start, playhead) <= 0 && CMTimeCompare(playhead, $0.end) <= 0
            })
        else {
            return nil
        }

        var frontier = intervals[coveringIndex].end
        for interval in intervals.dropFirst(coveringIndex + 1) {
            guard CMTimeCompare(interval.start, frontier) <= 0 else {
                break
            }
            if CMTimeCompare(interval.end, frontier) > 0 {
                frontier = interval.end
            }
        }
        return conservativeSeconds(frontier)
    }

    private static func conservativeSeconds(_ time: CMTime) -> Double? {
        guard time.isValid, time.isNumeric, time.epoch == 0, time.timescale > 0, time.value >= 0 else {
            return nil
        }

        let scale = Int64(time.timescale)
        let quotient = time.value / scale
        let remainder = time.value % scale
        let exactDoubleIntegerLimit: Int64 = 9_007_199_254_740_992

        if remainder == 0 {
            guard quotient <= exactDoubleIntegerLimit else { return nil }
            return Double(quotient)
        }

        guard time.value > 0, time.value <= exactDoubleIntegerLimit else { return nil }
        let seconds = CMTimeGetSeconds(time)
        return seconds.isFinite ? seconds.nextDown : nil
    }
}
