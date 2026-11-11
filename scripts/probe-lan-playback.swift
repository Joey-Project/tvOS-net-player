import AVFoundation
import CoreVideo
import Darwin
import Foundation
import QuartzCore

private struct VideoSize: Equatable {
    let width: Int
    let height: Int
}

private enum RecoveryObservationTrigger {
    static func isFreshEmptyRegularFile(at path: String) -> Bool {
        var pathInfo = stat()
        guard lstat(path, &pathInfo) == 0, (pathInfo.st_mode & S_IFMT) == S_IFREG, pathInfo.st_size == 0
        else {
            return false
        }
        let descriptor = open(path, O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK)
        guard descriptor >= 0 else { return false }
        defer { _ = close(descriptor) }
        var openedInfo = stat()
        guard fstat(descriptor, &openedInfo) == 0, (openedInfo.st_mode & S_IFMT) == S_IFREG,
            openedInfo.st_size == 0, openedInfo.st_dev == pathInfo.st_dev,
            openedInfo.st_ino == pathInfo.st_ino
        else {
            return false
        }
        return true
    }

    static func runSelfTest() throws -> [String] {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("probe-trigger-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: directory) }
        let empty = directory.appendingPathComponent("empty")
        let nonempty = directory.appendingPathComponent("nonempty")
        let link = directory.appendingPathComponent("link")
        guard FileManager.default.createFile(atPath: empty.path, contents: Data()),
            FileManager.default.createFile(atPath: nonempty.path, contents: Data([1]))
        else {
            throw ProbeFailure(phase: "self_test", domain: "RecoveryObservationTrigger", code: 1)
        }
        try FileManager.default.createSymbolicLink(at: link, withDestinationURL: empty)
        guard isFreshEmptyRegularFile(at: empty.path),
            !isFreshEmptyRegularFile(at: nonempty.path),
            !isFreshEmptyRegularFile(at: link.path),
            !isFreshEmptyRegularFile(at: directory.appendingPathComponent("missing").path)
        else {
            throw ProbeFailure(phase: "self_test", domain: "RecoveryObservationTrigger", code: 2)
        }
        return ["recovery_trigger_accepts_only_same_identity_empty_regular_file"]
    }
}

private enum RecoverySnapshot {
    static let maximumBytes = 4_096

    static func payload(
        observedMonotonicSeconds: Double,
        playheadSeconds: Double,
        loadedEndSeconds: Double
    ) throws -> Data {
        guard observedMonotonicSeconds.isFinite, observedMonotonicSeconds >= 0,
            playheadSeconds.isFinite, playheadSeconds >= 0,
            loadedEndSeconds.isFinite, loadedEndSeconds >= 0
        else {
            throw ProbeFailure(phase: "recovery_snapshot", domain: "RecoverySnapshot", code: Int(EINVAL))
        }
        let value: [String: Any] = [
            "schema_version": 1,
            "observed_monotonic_seconds": observedMonotonicSeconds,
            "playhead_seconds": playheadSeconds,
            "loaded_end_seconds": loadedEndSeconds,
        ]
        let data = try JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
        guard data.count <= maximumBytes else {
            throw ProbeFailure(phase: "recovery_snapshot", domain: "RecoverySnapshot", code: Int(EFBIG))
        }
        return data
    }

    static func publish(
        path: String,
        observedMonotonicSeconds: Double,
        playheadSeconds: Double,
        loadedEndSeconds: Double
    ) throws {
        let url = URL(fileURLWithPath: path)
        let name = url.lastPathComponent
        guard path.hasPrefix("/"), !path.hasSuffix("/"), !name.isEmpty,
            name != ".", name != ".."
        else {
            throw ProbeFailure(phase: "recovery_snapshot", domain: "RecoverySnapshot", code: Int(EINVAL))
        }
        let data = try payload(
            observedMonotonicSeconds: observedMonotonicSeconds,
            playheadSeconds: playheadSeconds,
            loadedEndSeconds: loadedEndSeconds
        )
        let parentPath = url.deletingLastPathComponent().path
        let directory = parentPath.withCString { open($0, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC) }
        guard directory >= 0 else {
            throw ProbeFailure(phase: "recovery_snapshot", domain: "RecoverySnapshot", code: Int(errno))
        }
        defer { _ = close(directory) }
        var directoryInfo = stat()
        guard fstat(directory, &directoryInfo) == 0, (directoryInfo.st_mode & S_IFMT) == S_IFDIR else {
            throw ProbeFailure(phase: "recovery_snapshot", domain: "RecoverySnapshot", code: Int(ENOTDIR))
        }

        let temporaryName = ".lan-recovery-\(UUID().uuidString).tmp"
        let descriptor = temporaryName.withCString {
            openat(directory, $0, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0o600)
        }
        guard descriptor >= 0 else {
            throw ProbeFailure(phase: "recovery_snapshot", domain: "RecoverySnapshot", code: Int(errno))
        }
        var descriptorOpen = true
        var temporaryExists = true
        defer {
            if descriptorOpen { _ = close(descriptor) }
            if temporaryExists { _ = temporaryName.withCString { unlinkat(directory, $0, 0) } }
        }

        var offset = 0
        while offset < data.count {
            let count = data.withUnsafeBytes { bytes in
                Darwin.write(descriptor, bytes.baseAddress!.advanced(by: offset), data.count - offset)
            }
            if count < 0 && errno == EINTR { continue }
            guard count > 0 else {
                throw ProbeFailure(phase: "recovery_snapshot", domain: "RecoverySnapshot", code: Int(errno))
            }
            offset += count
        }
        guard Darwin.close(descriptor) == 0 else {
            descriptorOpen = false
            throw ProbeFailure(phase: "recovery_snapshot", domain: "RecoverySnapshot", code: Int(errno))
        }
        descriptorOpen = false
        let renameResult = temporaryName.withCString { temporary in
            name.withCString { destination in
                renameatx_np(directory, temporary, directory, destination, UInt32(RENAME_EXCL))
            }
        }
        guard renameResult == 0 else {
            throw ProbeFailure(phase: "recovery_snapshot", domain: "RecoverySnapshot", code: Int(errno))
        }
        temporaryExists = false
    }

    static func runSelfTest() throws -> [String] {
        let valid = try payload(
            observedMonotonicSeconds: 1,
            playheadSeconds: 2,
            loadedEndSeconds: 3
        )
        guard valid.count <= maximumBytes,
            let decoded = try JSONSerialization.jsonObject(with: valid) as? [String: Any],
            decoded["schema_version"] as? Int == 1,
            decoded["observed_monotonic_seconds"] as? Double == 1,
            decoded["playhead_seconds"] as? Double == 2,
            decoded["loaded_end_seconds"] as? Double == 3
        else {
            throw ProbeFailure(phase: "self_test", domain: "RecoverySnapshot", code: 1)
        }
        for values in [
            (Double.nan, 1.0, 2.0), (Double.infinity, 1.0, 2.0), (1.0, -.infinity, 2.0),
            (1.0, -1.0, 2.0), (1.0, 2.0, .nan), (1.0, 2.0, -1.0),
        ] {
            do {
                _ = try payload(
                    observedMonotonicSeconds: values.0,
                    playheadSeconds: values.1,
                    loadedEndSeconds: values.2
                )
                throw ProbeFailure(phase: "self_test", domain: "RecoverySnapshot", code: 2)
            } catch let failure as ProbeFailure where failure.phase == "recovery_snapshot" {
                continue
            }
        }
        do {
            try publish(path: "relative.json", observedMonotonicSeconds: 1, playheadSeconds: 2, loadedEndSeconds: 3)
            throw ProbeFailure(phase: "self_test", domain: "RecoverySnapshot", code: 3)
        } catch let failure as ProbeFailure where failure.phase == "recovery_snapshot" {
            guard failure.code == Int(EINVAL) else { throw failure }
        }
        do {
            try publish(
                path: "/missing-lan-probe-parent-\(UUID().uuidString)/snapshot.json",
                observedMonotonicSeconds: 1,
                playheadSeconds: 2,
                loadedEndSeconds: 3
            )
            throw ProbeFailure(phase: "self_test", domain: "RecoverySnapshot", code: 9)
        } catch let failure as ProbeFailure where failure.phase == "recovery_snapshot" {
            guard failure.code == Int(ENOENT) else { throw failure }
        }

        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("lan-probe-recovery-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(
            at: directory,
            withIntermediateDirectories: false,
            attributes: [.posixPermissions: 0o700]
        )
        defer { try? FileManager.default.removeItem(at: directory) }
        let path = directory.appendingPathComponent("snapshot.json").path
        try publish(path: path, observedMonotonicSeconds: 1, playheadSeconds: 2, loadedEndSeconds: 3)
        let original = try Data(contentsOf: URL(fileURLWithPath: path))
        let attributes = try FileManager.default.attributesOfItem(atPath: path)
        var originalInfo = stat()
        guard lstat(path, &originalInfo) == 0 else {
            throw ProbeFailure(phase: "self_test", domain: "RecoverySnapshot", code: 10)
        }
        guard original.count <= maximumBytes,
            (attributes[.posixPermissions] as? NSNumber)?.intValue == 0o600,
            originalInfo.st_nlink == 1
        else {
            throw ProbeFailure(phase: "self_test", domain: "RecoverySnapshot", code: 8)
        }
        do {
            try publish(path: path, observedMonotonicSeconds: 4, playheadSeconds: 5, loadedEndSeconds: 6)
            throw ProbeFailure(phase: "self_test", domain: "RecoverySnapshot", code: 4)
        } catch let failure as ProbeFailure where failure.phase == "recovery_snapshot" && failure.code == Int(EEXIST) {
            var retainedInfo = stat()
            guard lstat(path, &retainedInfo) == 0,
                retainedInfo.st_dev == originalInfo.st_dev,
                retainedInfo.st_ino == originalInfo.st_ino,
                try Data(contentsOf: URL(fileURLWithPath: path)) == original
            else {
                throw ProbeFailure(phase: "self_test", domain: "RecoverySnapshot", code: 5)
            }
        }
        let target = directory.appendingPathComponent("target.json")
        let symlinkPath = path + ".link"
        try FileManager.default.createSymbolicLink(atPath: symlinkPath, withDestinationPath: target.path)
        var originalLinkInfo = stat()
        guard lstat(symlinkPath, &originalLinkInfo) == 0 else {
            throw ProbeFailure(phase: "self_test", domain: "RecoverySnapshot", code: 11)
        }
        do {
            try publish(
                path: symlinkPath,
                observedMonotonicSeconds: 1,
                playheadSeconds: 2,
                loadedEndSeconds: 3
            )
            throw ProbeFailure(phase: "self_test", domain: "RecoverySnapshot", code: 6)
        } catch let failure as ProbeFailure where failure.phase == "recovery_snapshot" && failure.code == Int(EEXIST) {
            var retainedLinkInfo = stat()
            guard lstat(symlinkPath, &retainedLinkInfo) == 0,
                retainedLinkInfo.st_dev == originalLinkInfo.st_dev,
                retainedLinkInfo.st_ino == originalLinkInfo.st_ino,
                !FileManager.default.fileExists(atPath: target.path)
            else {
                throw ProbeFailure(phase: "self_test", domain: "RecoverySnapshot", code: 7)
            }
        }
        return [
            "recovery_snapshot_rejects_invalid_state_and_atomically_publishes_bounded_private_single_link_json",
            "recovery_snapshot_no_clobber_preserves_existing_object_identity_and_complete_content",
            "recovery_snapshot_no_clobber_preserves_existing_symlink_identity",
        ]
    }
}

private enum LoadedTimeRangeDiagnostics {
    static let maximumObservations = 8

    static func observation(startSeconds: Double, durationSeconds: Double) -> [String: Double]? {
        let endSeconds = startSeconds + durationSeconds
        guard startSeconds.isFinite, startSeconds >= 0,
            durationSeconds.isFinite, durationSeconds >= 0,
            endSeconds.isFinite
        else {
            return nil
        }
        return [
            "start_seconds": startSeconds,
            "duration_seconds": durationSeconds,
            "end_seconds": endSeconds,
        ]
    }

    static func snapshot(_ ranges: [CMTimeRange]) -> [String: Any] {
        var observations: [[String: Double]] = []
        var invalidCount = 0
        var validCount = 0

        for range in ranges {
            let start = CMTimeGetSeconds(range.start)
            let duration = CMTimeGetSeconds(range.duration)
            guard let value = observation(startSeconds: start, durationSeconds: duration) else {
                invalidCount += 1
                continue
            }
            validCount += 1
            if observations.count < maximumObservations {
                observations.append(value)
            }
        }

        return [
            "observations": observations,
            "source_count": ranges.count,
            "valid_count": validCount,
            "invalid_count": invalidCount,
            "truncated": validCount > maximumObservations,
        ]
    }

    static func runSelfTest() throws -> [String] {
        let invalidRanges = [
            observation(startSeconds: .nan, durationSeconds: 1),
            observation(startSeconds: .infinity, durationSeconds: 1),
            observation(startSeconds: -1, durationSeconds: 1),
            observation(startSeconds: 1, durationSeconds: -.infinity),
            observation(startSeconds: 1, durationSeconds: -1),
            observation(startSeconds: Double.greatestFiniteMagnitude, durationSeconds: Double.greatestFiniteMagnitude),
        ]
        guard invalidRanges.allSatisfy({ $0 == nil }) else {
            throw ProbeFailure(phase: "self_test", domain: "LoadedTimeRangeDiagnostics", code: 1)
        }

        let ranges = (0..<maximumObservations + 2).map { index in
            CMTimeRange(
                start: CMTime(seconds: Double(index), preferredTimescale: 600),
                duration: CMTime(seconds: 1, preferredTimescale: 600)
            )
        }
        let snapshot = snapshot(ranges)
        guard (snapshot["observations"] as? [[String: Double]])?.count == maximumObservations,
            snapshot["source_count"] as? Int == maximumObservations + 2,
            snapshot["valid_count"] as? Int == maximumObservations + 2,
            snapshot["invalid_count"] as? Int == 0,
            snapshot["truncated"] as? Bool == true
        else {
            throw ProbeFailure(phase: "self_test", domain: "LoadedTimeRangeDiagnostics", code: 2)
        }
        return [
            "loaded_time_ranges_reject_invalid_nonfinite_negative_and_overflow_values",
            "loaded_time_ranges_cap_observations_and_report_truncation_counts",
        ]
    }
}

private struct PlaybackTimelineDiagnostics {
    static let sampleIntervalSeconds = 10.0
    static let maximumSamples = 96

    private(set) var samples: [[String: Any]] = []
    private(set) var truncated = false

    private var lastElapsedSeconds: Double?
    private var lastMonotonicSeconds: Double?

    static func loadedEndSeconds(playheadSeconds: Double, ranges: [CMTimeRange]) -> Double? {
        guard playheadSeconds.isFinite, playheadSeconds >= 0 else { return nil }
        let intervals = ranges.compactMap { range -> (start: Double, end: Double)? in
            let start = CMTimeGetSeconds(range.start)
            let duration = CMTimeGetSeconds(range.duration)
            guard
                let interval = LoadedTimeRangeDiagnostics.observation(
                    startSeconds: start,
                    durationSeconds: duration
                )
            else {
                return nil
            }
            return (interval["start_seconds"]!, interval["end_seconds"]!)
        }.sorted { $0.start < $1.start }

        let containing = intervals.filter { $0.start <= playheadSeconds && playheadSeconds <= $0.end }
        guard var end = containing.map({ $0.end }).max() else {
            return nil
        }
        for interval in intervals where interval.start > playheadSeconds {
            guard interval.start <= end else { break }
            end = max(end, interval.end)
        }
        return end
    }

    mutating func record(
        elapsedSeconds: Double,
        observedMonotonicSeconds: Double,
        playheadSeconds: Double?,
        ranges: [CMTimeRange],
        decodedWidth: Int?,
        decodedHeight: Int?,
        timeControlStatus: String,
        indicatedBitrate: Double?,
        force: Bool = false
    ) {
        guard elapsedSeconds.isFinite, elapsedSeconds >= 0,
            observedMonotonicSeconds.isFinite, observedMonotonicSeconds > 0,
            lastElapsedSeconds.map({ elapsedSeconds >= $0 }) ?? true,
            lastMonotonicSeconds.map({ observedMonotonicSeconds >= $0 }) ?? true,
            lastMonotonicSeconds != observedMonotonicSeconds,
            shouldRecord(at: elapsedSeconds, force: force)
        else {
            return
        }
        guard samples.count < Self.maximumSamples else {
            truncated = true
            lastElapsedSeconds = elapsedSeconds
            lastMonotonicSeconds = observedMonotonicSeconds
            return
        }

        let validPlayhead = playheadSeconds.flatMap { $0.isFinite && $0 >= 0 ? $0 : nil }
        let loadedEnd = validPlayhead.flatMap { Self.loadedEndSeconds(playheadSeconds: $0, ranges: ranges) }
        let validWidth = decodedWidth.flatMap { $0 > 0 ? $0 : nil }
        let validHeight = decodedHeight.flatMap { $0 > 0 ? $0 : nil }
        let validBitrate = indicatedBitrate.flatMap { $0.isFinite && $0 >= 0 ? $0 : nil }
        samples.append([
            "elapsed_seconds": elapsedSeconds,
            "observed_monotonic_seconds": observedMonotonicSeconds,
            "playhead_seconds": validPlayhead as Any? ?? NSNull(),
            "loaded_end_seconds": loadedEnd as Any? ?? NSNull(),
            "buffered_ahead_seconds": validPlayhead.flatMap { playhead in
                loadedEnd.map { max(0, $0 - playhead) }
            } as Any? ?? NSNull(),
            "decoded_width": validWidth as Any? ?? NSNull(),
            "decoded_height": validHeight as Any? ?? NSNull(),
            "time_control_status": ["paused", "waiting", "playing"].contains(timeControlStatus)
                ? timeControlStatus
                : "other",
            "indicated_bitrate": validBitrate as Any? ?? NSNull(),
        ])
        lastElapsedSeconds = elapsedSeconds
        lastMonotonicSeconds = observedMonotonicSeconds
    }

    func shouldRecord(at elapsedSeconds: Double, force: Bool = false) -> Bool {
        if force { return true }
        guard let lastElapsedSeconds else { return true }
        return elapsedSeconds - lastElapsedSeconds >= Self.sampleIntervalSeconds
    }

    var json: [String: Any] {
        [
            "sample_interval_seconds": Self.sampleIntervalSeconds,
            "maximum_samples": Self.maximumSamples,
            "truncated": truncated,
            "samples": samples,
        ]
    }

    static func runSelfTest() throws -> [String] {
        var recorder = PlaybackTimelineDiagnostics()
        func ranges(_ pairs: [(Double, Double)]) -> [CMTimeRange] {
            pairs.map { start, end in
                CMTimeRange(
                    start: CMTime(seconds: start, preferredTimescale: 600),
                    duration: CMTime(seconds: end - start, preferredTimescale: 600)
                )
            }
        }

        let contiguousEnd = loadedEndSeconds(
            playheadSeconds: 3,
            ranges: ranges([(0, 2), (1.5, 4), (4, 5), (8, 10)]) + [
                CMTimeRange(start: .invalid, duration: CMTime(seconds: 2, preferredTimescale: 600))
            ]
        )
        guard contiguousEnd == 5,
            loadedEndSeconds(playheadSeconds: 6, ranges: ranges([(0, 5), (8, 10)])) == nil,
            loadedEndSeconds(playheadSeconds: .infinity, ranges: ranges([(0, 5)])) == nil
        else {
            throw ProbeFailure(phase: "self_test", domain: "PlaybackTimelineDiagnostics", code: 1)
        }

        let emptyRanges: [CMTimeRange] = []
        recorder.record(
            elapsedSeconds: 0, observedMonotonicSeconds: 100, playheadSeconds: .nan,
            ranges: emptyRanges, decodedWidth: nil, decodedHeight: nil, timeControlStatus: "unexpected",
            indicatedBitrate: .infinity
        )
        recorder.record(
            elapsedSeconds: 9.9, observedMonotonicSeconds: 109.9, playheadSeconds: 2,
            ranges: emptyRanges, decodedWidth: 1920, decodedHeight: 1080,
            timeControlStatus: "playing", indicatedBitrate: 1_000
        )
        guard recorder.samples.count == 1 else {
            throw ProbeFailure(phase: "self_test", domain: "PlaybackTimelineDiagnostics", code: 2)
        }
        recorder.record(
            elapsedSeconds: 10, observedMonotonicSeconds: 110, playheadSeconds: 2,
            ranges: ranges([(0, 3), (8, 9)]), decodedWidth: 1920, decodedHeight: 1080,
            timeControlStatus: "playing", indicatedBitrate: 1_000
        )
        recorder.record(
            elapsedSeconds: 10, observedMonotonicSeconds: 110, playheadSeconds: 2,
            ranges: emptyRanges, decodedWidth: nil, decodedHeight: nil, timeControlStatus: "paused",
            indicatedBitrate: nil, force: true
        )
        let sample = recorder.samples.last ?? [:]
        guard recorder.samples.count == 2,
            sample["loaded_end_seconds"] as? Double == 3,
            sample["buffered_ahead_seconds"] as? Double == 1,
            sample["time_control_status"] as? String == "playing"
        else {
            throw ProbeFailure(phase: "self_test", domain: "PlaybackTimelineDiagnostics", code: 3)
        }

        for index in 1...Self.maximumSamples {
            recorder.record(
                elapsedSeconds: 10 + Double(index) * 10,
                observedMonotonicSeconds: 110 + Double(index) * 10,
                playheadSeconds: nil, ranges: emptyRanges, decodedWidth: nil, decodedHeight: nil,
                timeControlStatus: "paused", indicatedBitrate: nil
            )
        }
        let json = recorder.json
        let encoded = try JSONSerialization.data(withJSONObject: json, options: [.sortedKeys])
        let decoded = try JSONSerialization.jsonObject(with: encoded) as? [String: Any]
        let encodedSamples = decoded?["samples"] as? [[String: Any]]
        let first = encodedSamples?.first
        guard recorder.samples.count == Self.maximumSamples, recorder.truncated,
            decoded?["sample_interval_seconds"] as? Double == 10,
            decoded?["maximum_samples"] as? Int == Self.maximumSamples,
            decoded?["truncated"] as? Bool == true,
            encodedSamples?.count == Self.maximumSamples,
            first?["elapsed_seconds"] as? Double == 0,
            first?["observed_monotonic_seconds"] as? Double == 100,
            first?["playhead_seconds"] is NSNull,
            first?["loaded_end_seconds"] is NSNull,
            first?["buffered_ahead_seconds"] is NSNull,
            first?["decoded_width"] is NSNull,
            first?["decoded_height"] is NSNull,
            first?["indicated_bitrate"] is NSNull,
            first?["time_control_status"] as? String == "other"
        else {
            throw ProbeFailure(phase: "self_test", domain: "PlaybackTimelineDiagnostics", code: 4)
        }
        return [
            "playback_timeline_samples_at_interval_and_bounds_storage_with_truncation",
            "playback_timeline_rejects_nonfinite_values_and_preserves_null_json_fields",
            "playback_timeline_buffer_end_merges_overlaps_but_not_disjoint_ranges",
        ]
    }
}

private struct ProbeOptions {
    let url: URL
    let duration: TimeInterval
    let deadline: TimeInterval
    let validateOnly: Bool
    let expectedVideoSizes: [VideoSize]
    let readyMarkerPath: String?
    let recoveryObservationTriggerPath: String?
    let recoverySnapshotPath: String?
    let minimumSteadyPlaySeconds: TimeInterval?
    let controlsAfterVideoSizeExpectations: Bool

    static func parse(_ arguments: [String]) throws -> ProbeOptions {
        var urlValue: String?
        var duration = 60.0
        var deadline = 120.0
        var validateOnly = false
        var expectedVideoSizes: [VideoSize]?
        var readyMarkerPath: String?
        var recoveryObservationTriggerPath: String?
        let recoverySnapshotPath = ProcessInfo.processInfo.environment["LAN_PLAYBACK_RECOVERY_SNAPSHOT_PATH"]
            .flatMap { $0.isEmpty ? nil : $0 }
        var minimumSteadyPlaySeconds: TimeInterval?
        var controlsAfterVideoSizeExpectations = false
        var index = 0

        while index < arguments.count {
            let option = arguments[index]
            if option == "--validate-only" {
                guard !validateOnly else {
                    throw ProbeFailure(phase: "cli", domain: "ProbeCLI", code: 2)
                }
                validateOnly = true
                index += 1
                continue
            }
            if option == "--controls-after-video-size-expectations" {
                guard !controlsAfterVideoSizeExpectations else {
                    throw ProbeFailure(phase: "cli", domain: "ProbeCLI", code: 2)
                }
                controlsAfterVideoSizeExpectations = true
                index += 1
                continue
            }
            guard index + 1 < arguments.count else {
                throw ProbeFailure(phase: "cli", domain: "ProbeCLI", code: 1)
            }
            let value = arguments[index + 1]
            switch option {
            case "--url":
                guard urlValue == nil else {
                    throw ProbeFailure(phase: "cli", domain: "ProbeCLI", code: 2)
                }
                urlValue = value
            case "--duration":
                guard let parsed = Double(value), parsed.isFinite, (30...600).contains(parsed) else {
                    throw ProbeFailure(phase: "cli", domain: "ProbeCLI", code: 3)
                }
                duration = parsed
            case "--deadline":
                guard let parsed = Double(value), parsed.isFinite, (15...900).contains(parsed) else {
                    throw ProbeFailure(phase: "cli", domain: "ProbeCLI", code: 4)
                }
                deadline = parsed
            case "--expect-video-sizes":
                guard expectedVideoSizes == nil else {
                    throw ProbeFailure(phase: "cli", domain: "ProbeCLI", code: 2)
                }
                expectedVideoSizes = try parseVideoSizes(value)
            case "--ready-marker":
                guard readyMarkerPath == nil, value.hasPrefix("/"), !value.isEmpty else {
                    throw ProbeFailure(phase: "cli", domain: "ProbeCLI", code: 9)
                }
                readyMarkerPath = value
            case "--recovery-observation-trigger":
                guard recoveryObservationTriggerPath == nil, value.hasPrefix("/"), !value.isEmpty else {
                    throw ProbeFailure(phase: "cli", domain: "ProbeCLI", code: 11)
                }
                recoveryObservationTriggerPath = value
            case "--minimum-steady-play-seconds":
                guard minimumSteadyPlaySeconds == nil,
                    let parsed = Double(value), parsed.isFinite, parsed >= 0
                else {
                    throw ProbeFailure(phase: "cli", domain: "ProbeCLI", code: 12)
                }
                minimumSteadyPlaySeconds = parsed
            default:
                throw ProbeFailure(phase: "cli", domain: "ProbeCLI", code: 5)
            }
            index += 2
        }

        guard let urlValue, let components = URLComponents(string: urlValue),
            let scheme = components.scheme?.lowercased(), ["http", "https"].contains(scheme),
            components.host != nil, components.user == nil, components.password == nil,
            components.query == nil, components.fragment == nil,
            let url = components.url, isLANHost(components.host ?? "")
        else {
            throw ProbeFailure(phase: "cli", domain: "ProbeCLI", code: 6)
        }
        guard deadline >= duration + 15 else {
            throw ProbeFailure(phase: "cli", domain: "ProbeCLI", code: 7)
        }
        guard readyMarkerPath == nil || expectedVideoSizes != nil else {
            throw ProbeFailure(phase: "cli", domain: "ProbeCLI", code: 9)
        }
        guard recoveryObservationTriggerPath == nil || expectedVideoSizes != nil else {
            throw ProbeFailure(phase: "cli", domain: "ProbeCLI", code: 11)
        }
        if let recoverySnapshotPath {
            guard
                RecoverySnapshotPolicy.accepts(
                    path: recoverySnapshotPath,
                    triggerPath: recoveryObservationTriggerPath,
                    expectedVideoSizes: expectedVideoSizes ?? [],
                    controlsAfterVideoSizeExpectations: controlsAfterVideoSizeExpectations,
                    duration: duration,
                    deadline: deadline,
                    minimumSteadyPlaySeconds: minimumSteadyPlaySeconds
                )
            else {
                throw ProbeFailure(phase: "cli", domain: "ProbeCLI", code: 13)
            }
        }
        guard
            !controlsAfterVideoSizeExpectations
                || (expectedVideoSizes != nil && duration > 30)
        else {
            throw ProbeFailure(phase: "cli", domain: "ProbeCLI", code: 10)
        }
        guard
            (recoveryObservationTriggerPath == nil && minimumSteadyPlaySeconds == nil)
                || (controlsAfterVideoSizeExpectations
                    && (minimumSteadyPlaySeconds ?? 0) <= duration - VideoSizePhasePolicy.controlReserveSeconds)
        else {
            throw ProbeFailure(phase: "cli", domain: "ProbeCLI", code: 12)
        }

        return ProbeOptions(
            url: url,
            duration: duration,
            deadline: deadline,
            validateOnly: validateOnly,
            expectedVideoSizes: expectedVideoSizes ?? [],
            readyMarkerPath: readyMarkerPath,
            recoveryObservationTriggerPath: recoveryObservationTriggerPath,
            recoverySnapshotPath: recoverySnapshotPath,
            minimumSteadyPlaySeconds: minimumSteadyPlaySeconds,
            controlsAfterVideoSizeExpectations: controlsAfterVideoSizeExpectations
        )
    }

    private static func parseVideoSizes(_ value: String) throws -> [VideoSize] {
        let entries = value.split(separator: ",", omittingEmptySubsequences: false)
        guard (1...8).contains(entries.count) else {
            throw ProbeFailure(phase: "cli", domain: "ProbeCLI", code: 8)
        }
        var sizes: [VideoSize] = []
        for entry in entries {
            let dimensions = entry.split(separator: "x", omittingEmptySubsequences: false)
            guard dimensions.count == 2,
                dimensions.allSatisfy({ part in
                    !part.isEmpty && part.utf8.allSatisfy { (48...57).contains($0) }
                }),
                let width = Int(String(dimensions[0])), (1...8192).contains(width),
                let height = Int(String(dimensions[1])), (1...8192).contains(height)
            else {
                throw ProbeFailure(phase: "cli", domain: "ProbeCLI", code: 8)
            }
            let size = VideoSize(width: width, height: height)
            guard sizes.last != size else {
                throw ProbeFailure(phase: "cli", domain: "ProbeCLI", code: 8)
            }
            sizes.append(size)
        }
        return sizes
    }

    private static func isLANHost(_ host: String) -> Bool {
        let normalized = host.lowercased().trimmingCharacters(in: CharacterSet(charactersIn: "[]"))
        if normalized == "localhost" || normalized.hasSuffix(".localhost") || normalized.hasSuffix(".local") {
            return true
        }

        var address4 = in_addr()
        if normalized.withCString({ inet_pton(AF_INET, $0, &address4) }) == 1 {
            let value = UInt32(bigEndian: address4.s_addr)
            return (value >> 24 == 10)
                || (value >> 20 == 0xAC1)
                || (value >> 16 == 0xC0A8)
                || (value >> 16 == 0xA9FE)
                || (value >> 24 == 127)
        }

        var address6 = in6_addr()
        if normalized.withCString({ inet_pton(AF_INET6, $0, &address6) }) == 1 {
            let bytes = withUnsafeBytes(of: address6) { Array($0) }
            return bytes.prefix(15).allSatisfy { $0 == 0 } && bytes.last == 1
                || (bytes[0] & 0xFE) == 0xFC
                || (bytes[0] == 0xFE && (bytes[1] & 0xC0) == 0x80)
        }
        return false
    }

    @MainActor
    static func runSelfTest() throws -> [String: Any] {
        var passed: [String] = []

        func accept(_ name: String, url: String, duration: Double = 60, deadline: Double = 120) throws {
            let options = try parse([
                "--url", url,
                "--duration", String(duration),
                "--deadline", String(deadline),
                "--validate-only",
            ])
            guard options.validateOnly, options.duration == duration, options.deadline == deadline,
                options.url.scheme == "http" || options.url.scheme == "https",
                isLANHost(options.url.host ?? ""), options.url.user == nil, options.url.password == nil,
                options.url.query == nil, options.url.fragment == nil
            else {
                throw ProbeFailure(phase: "self_test", domain: "ProbeSelfTest", code: 1)
            }
            passed.append(name)
        }

        func reject(_ name: String, arguments: [String], expectedCode: Int) throws {
            do {
                _ = try parse(arguments)
            } catch let failure as ProbeFailure {
                guard failure.phase == "cli", failure.code == expectedCode else {
                    throw ProbeFailure(phase: "self_test", domain: "ProbeSelfTest", code: 2)
                }
                passed.append(name)
                return
            }
            throw ProbeFailure(phase: "self_test", domain: "ProbeSelfTest", code: 3)
        }

        try accept("ipv4_loopback", url: "http://127.0.0.1/live.m3u8")
        try accept("ipv6_loopback", url: "http://[::1]/live.m3u8")
        try accept("ipv4_private_10", url: "http://10.23.4.5/live.m3u8")
        try accept("ipv4_private_172", url: "http://172.20.4.5/live.m3u8")
        try accept("ipv4_private_192", url: "http://192.168.4.5/live.m3u8")
        try accept("ipv4_link_local", url: "http://169.254.4.5/live.m3u8")
        try accept("ipv6_unique_local", url: "http://[fd12:3456::1]/live.m3u8")
        try accept("ipv6_link_local", url: "http://[fe80::1]/live.m3u8")
        try accept("local_hostname", url: "http://media.local/live.m3u8")
        try accept("minimum_duration_deadline", url: "http://localhost/live.m3u8", duration: 30, deadline: 45)
        try accept("custom_duration_deadline", url: "https://localhost/live.m3u8", duration: 300, deadline: 360)
        let sizeOptions = try parse([
            "--url", "http://192.168.1.8/live.m3u8",
            "--expect-video-sizes", "1920x1080,1280x720,1920x1080",
        ])
        guard
            sizeOptions.expectedVideoSizes == [
                VideoSize(width: 1920, height: 1080),
                VideoSize(width: 1280, height: 720),
                VideoSize(width: 1920, height: 1080),
            ]
        else {
            throw ProbeFailure(phase: "self_test", domain: "ProbeSelfTest", code: 4)
        }
        passed.append("video_size_expectations_parse_ordered_high_low_high")
        let markerOptions = try parse([
            "--url", "http://192.168.1.8/live.m3u8",
            "--duration", "130",
            "--deadline", "175",
            "--expect-video-sizes", "640x360,320x180,640x360",
            "--ready-marker", "/tmp/ready.json",
            "--recovery-observation-trigger", "/tmp/recovery.trigger",
            "--minimum-steady-play-seconds", "110",
            "--controls-after-video-size-expectations",
        ])
        guard markerOptions.readyMarkerPath == "/tmp/ready.json",
            markerOptions.recoveryObservationTriggerPath == "/tmp/recovery.trigger",
            markerOptions.minimumSteadyPlaySeconds == 110,
            markerOptions.controlsAfterVideoSizeExpectations
        else {
            throw ProbeFailure(phase: "self_test", domain: "ProbeSelfTest", code: 5)
        }
        passed.append("marker_and_minimum_steady_play_options_parse")
        try reject("public_ipv4", arguments: ["--url", "http://8.8.8.8/live.m3u8"], expectedCode: 6)
        try reject("public_ipv6", arguments: ["--url", "http://[2001:4860:4860::8888]/live.m3u8"], expectedCode: 6)
        try reject("userinfo", arguments: ["--url", "http://name:secret@192.168.1.8/live.m3u8"], expectedCode: 6)
        try reject("query", arguments: ["--url", "http://192.168.1.8/live.m3u8?token=x"], expectedCode: 6)
        try reject("fragment", arguments: ["--url", "http://192.168.1.8/live.m3u8#part"], expectedCode: 6)
        try reject("missing_url", arguments: [], expectedCode: 6)
        try reject(
            "duration_nan",
            arguments: ["--url", "http://192.168.1.8/live.m3u8", "--duration", "NaN"],
            expectedCode: 3
        )
        try reject(
            "duration_out_of_range",
            arguments: ["--url", "http://192.168.1.8/live.m3u8", "--duration", "29"],
            expectedCode: 3
        )
        try reject(
            "deadline_nan",
            arguments: ["--url", "http://192.168.1.8/live.m3u8", "--deadline", "NaN"],
            expectedCode: 4
        )
        try reject(
            "deadline_too_short",
            arguments: ["--url", "http://192.168.1.8/live.m3u8", "--duration", "300", "--deadline", "314"],
            expectedCode: 7
        )
        try reject(
            "deadline_below_setup_allowance",
            arguments: ["--url", "http://192.168.1.8/live.m3u8", "--duration", "30", "--deadline", "44"],
            expectedCode: 7
        )
        try reject(
            "deadline_out_of_range",
            arguments: ["--url", "http://192.168.1.8/live.m3u8", "--deadline", "901"],
            expectedCode: 4
        )
        try reject(
            "video_size_duplicate_flag",
            arguments: [
                "--url", "http://192.168.1.8/live.m3u8",
                "--expect-video-sizes", "1920x1080",
                "--expect-video-sizes", "1280x720",
            ],
            expectedCode: 2
        )
        try reject(
            "ready_marker_requires_video_sizes",
            arguments: ["--url", "http://192.168.1.8/live.m3u8", "--ready-marker", "/tmp/ready.json"],
            expectedCode: 9
        )
        try reject(
            "ready_marker_requires_absolute_path",
            arguments: [
                "--url", "http://192.168.1.8/live.m3u8",
                "--expect-video-sizes", "640x360",
                "--ready-marker", "ready.json",
            ],
            expectedCode: 9
        )
        try reject(
            "recovery_trigger_requires_absolute_path",
            arguments: [
                "--url", "http://192.168.1.8/live.m3u8",
                "--expect-video-sizes", "640x360",
                "--recovery-observation-trigger", "trigger",
            ],
            expectedCode: 11
        )
        try reject(
            "minimum_steady_play_must_leave_control_reserve",
            arguments: [
                "--url", "http://192.168.1.8/live.m3u8",
                "--duration", "130",
                "--deadline", "175",
                "--expect-video-sizes", "640x360",
                "--controls-after-video-size-expectations",
                "--minimum-steady-play-seconds", "111",
            ],
            expectedCode: 12
        )
        try reject(
            "controls_after_expectations_require_video_sizes",
            arguments: [
                "--url", "http://192.168.1.8/live.m3u8",
                "--controls-after-video-size-expectations",
            ],
            expectedCode: 10
        )
        try reject(
            "controls_after_expectations_require_duration_over_30_seconds",
            arguments: [
                "--url", "http://192.168.1.8/live.m3u8",
                "--duration", "30",
                "--expect-video-sizes", "640x360",
                "--controls-after-video-size-expectations",
            ],
            expectedCode: 10
        )
        for (name, value) in [
            ("video_size_empty", ""),
            ("video_size_empty_entry", "1920x1080,"),
            ("video_size_malformed", "1920-1080"),
            ("video_size_zero", "0x720"),
            ("video_size_too_large", "8193x720"),
            ("video_size_overflow", "999999999999999999999999999x720"),
            ("video_size_adjacent_duplicate", "1920x1080,1920x1080"),
            ("video_size_too_many", "1x1,2x2,3x3,4x4,5x5,6x6,7x7,8x8,9x9"),
        ] {
            try reject(
                name,
                arguments: ["--url", "http://192.168.1.8/live.m3u8", "--expect-video-sizes", value],
                expectedCode: 8
            )
        }

        passed.append(contentsOf: try VideoSizeExpectationTracker.runSelfTest())
        passed.append(contentsOf: try ReadyMarker.runSelfTest())
        passed.append(contentsOf: try RecoverySnapshotPolicy.runSelfTest())
        passed.append(contentsOf: try RecoverySnapshot.runSelfTest())
        passed.append(contentsOf: try DecodedFrameContinuity.runSelfTest())
        passed.append(contentsOf: try VideoSizePhasePolicy.runSelfTest())
        passed.append(contentsOf: try DiagnosticObservationWindowPolicy.runSelfTest())
        passed.append(contentsOf: try BoundedObservations.runSelfTest())
        passed.append(contentsOf: try PlayerErrorLogDiagnostics.runSelfTest())
        passed.append(contentsOf: try LoadedTimeRangeDiagnostics.runSelfTest())
        passed.append(contentsOf: try PlaybackTimelineDiagnostics.runSelfTest())
        passed.append(contentsOf: try RecoveryObservationTrigger.runSelfTest())
        passed.append(contentsOf: try PlaybackPolicy.runSelfTest())
        passed.append(contentsOf: try PlayerSetup.runSelfTest())
        return ["schema_version": 1, "status": "complete", "passed": passed.count, "cases": passed]
    }
}

private enum RecoverySnapshotPolicy {
    static let expectedSizes = [
        VideoSize(width: 640, height: 360),
        VideoSize(width: 320, height: 180),
        VideoSize(width: 640, height: 360),
    ]

    static func accepts(
        path: String,
        triggerPath: String?,
        expectedVideoSizes: [VideoSize],
        controlsAfterVideoSizeExpectations: Bool,
        duration: Double,
        deadline: Double,
        minimumSteadyPlaySeconds: Double?
    ) -> Bool {
        path.hasPrefix("/") && !path.hasSuffix("/") && triggerPath != nil
            && expectedVideoSizes == expectedSizes
            && controlsAfterVideoSizeExpectations
            && duration == 600 && deadline == 720 && minimumSteadyPlaySeconds == 110
    }

    static func runSelfTest() throws -> [String] {
        let valid = accepts(
            path: "/tmp/recovery.json",
            triggerPath: "/tmp/recovery.trigger",
            expectedVideoSizes: expectedSizes,
            controlsAfterVideoSizeExpectations: true,
            duration: 600,
            deadline: 720,
            minimumSteadyPlaySeconds: 110
        )
        let invalid =
            accepts(
                path: "relative.json",
                triggerPath: "/tmp/recovery.trigger",
                expectedVideoSizes: expectedSizes,
                controlsAfterVideoSizeExpectations: true,
                duration: 600,
                deadline: 720,
                minimumSteadyPlaySeconds: 110
            )
            || accepts(
                path: "/tmp/recovery.json",
                triggerPath: nil,
                expectedVideoSizes: expectedSizes,
                controlsAfterVideoSizeExpectations: true,
                duration: 600,
                deadline: 720,
                minimumSteadyPlaySeconds: 110
            )
            || accepts(
                path: "/tmp/recovery.json",
                triggerPath: "/tmp/recovery.trigger",
                expectedVideoSizes: [VideoSize(width: 640, height: 360), VideoSize(width: 320, height: 180)],
                controlsAfterVideoSizeExpectations: true,
                duration: 600,
                deadline: 720,
                minimumSteadyPlaySeconds: 110
            )
            || accepts(
                path: "/tmp/recovery.json",
                triggerPath: "/tmp/recovery.trigger",
                expectedVideoSizes: expectedSizes,
                controlsAfterVideoSizeExpectations: true,
                duration: 130,
                deadline: 175,
                minimumSteadyPlaySeconds: 110
            )
        guard valid, !invalid else {
            throw ProbeFailure(phase: "self_test", domain: "RecoverySnapshotPolicy", code: 1)
        }
        var tracker = VideoSizeExpectationTracker(expectedSizes: expectedSizes)
        for index in 0..<6 {
            let size = index < 3 ? expectedSizes[0] : expectedSizes[1]
            tracker.observe(
                width: size.width,
                height: size.height,
                timestampNanoseconds: Int64(index + 1),
                observedElapsedSeconds: Double(index + 1),
                observedMonotonicSeconds: Double(index + 1)
            )
        }
        guard tracker.requiredExpectationsComplete(optionalTrailingCount: 1),
            !tracker.isComplete,
            tracker.diagnosticProgress[2]["matched"] as? Bool == false,
            tracker.diagnosticProgress[2]["distinct_frame_count"] as? Int == 0,
            (tracker.diagnosticProgress[2]["samples"] as? [[String: Any]])?.isEmpty == true,
            tracker.hasValidNumericSamplesAllowingMissingTrailingEntry
        else {
            throw ProbeFailure(phase: "self_test", domain: "RecoverySnapshotPolicy", code: 2)
        }
        tracker.observe(
            width: expectedSizes[2].width,
            height: expectedSizes[2].height,
            timestampNanoseconds: 7,
            observedElapsedSeconds: 7,
            observedMonotonicSeconds: 7
        )
        guard tracker.diagnosticProgress[2]["matched"] as? Bool == false else {
            throw ProbeFailure(phase: "self_test", domain: "RecoverySnapshotPolicy", code: 3)
        }
        tracker.observe(
            width: expectedSizes[2].width,
            height: expectedSizes[2].height,
            timestampNanoseconds: 8,
            observedElapsedSeconds: 8,
            observedMonotonicSeconds: 8
        )
        tracker.observe(
            width: expectedSizes[2].width,
            height: expectedSizes[2].height,
            timestampNanoseconds: 9,
            observedElapsedSeconds: 9,
            observedMonotonicSeconds: 9
        )
        guard tracker.isComplete, tracker.hasValidNumericSamples else {
            throw ProbeFailure(phase: "self_test", domain: "RecoverySnapshotPolicy", code: 4)
        }
        return ["diagnostic_optional_recovery_high_requires_first_two_and_censors_incomplete_third"]
    }
}

private struct ProbeFailure: Error, Sendable {
    let phase: String
    let domain: String
    let code: Int

    var safeDomain: String {
        let scalars = domain.unicodeScalars.prefix(80)
        let safe = String(
            String.UnicodeScalarView(scalars).filter {
                CharacterSet(charactersIn: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-")
                    .contains($0)
            })
        return safe.isEmpty ? "NSError" : safe
    }

    init(phase: String, domain: String, code: Int) {
        self.phase = phase
        self.domain = domain
        self.code = code
    }

    init(phase: String, error: Error) {
        let nsError = error as NSError
        self.init(phase: phase, domain: nsError.domain, code: nsError.code)
    }
}

@MainActor
private enum PlayerSetup {
    static func configure(_ player: AVPlayer) throws {
        player.isMuted = true
        guard player.isMuted else {
            throw ProbeFailure(phase: "mute", domain: "AVPlayer", code: 1)
        }
        player.automaticallyWaitsToMinimizeStalling = true
    }

    static func runSelfTest() throws -> [String] {
        let player = AVPlayer()
        try configure(player)
        guard player.isMuted, player.automaticallyWaitsToMinimizeStalling else {
            throw ProbeFailure(phase: "self_test", domain: "PlayerSetup", code: 1)
        }
        return ["player_setup_enables_automatic_waiting_and_mute_without_playback"]
    }
}

private struct ReadyMarker: Encodable {
    let schemaVersion: Int
    let expectedWidth: Int
    let expectedHeight: Int
    let distinctFrameCount: Int
    let observedMonotonicSeconds: Double

    enum CodingKeys: String, CodingKey {
        case schemaVersion = "schema_version"
        case expectedWidth = "expected_width"
        case expectedHeight = "expected_height"
        case distinctFrameCount = "distinct_frame_count"
        case observedMonotonicSeconds = "observed_monotonic_seconds"
    }

    static func publish(path: String, size: VideoSize, monotonicSeconds: Double) throws {
        guard monotonicSeconds.isFinite, monotonicSeconds > 0 else {
            throw ProbeFailure(phase: "ready_marker", domain: "ReadyMarker", code: 1)
        }
        let marker = ReadyMarker(
            schemaVersion: 1,
            expectedWidth: size.width,
            expectedHeight: size.height,
            distinctFrameCount: 3,
            observedMonotonicSeconds: monotonicSeconds
        )
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        let data = try encoder.encode(marker)
        guard data.count <= 1_024 else {
            throw ProbeFailure(phase: "ready_marker", domain: "ReadyMarker", code: 2)
        }

        let descriptor = Darwin.open(path, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW, mode_t(0o600))
        guard descriptor >= 0 else {
            throw ProbeFailure(phase: "ready_marker", domain: "ReadyMarker", code: Int(errno))
        }
        var descriptorOpen = true
        var removePartial = true
        defer {
            if descriptorOpen { _ = Darwin.close(descriptor) }
            if removePartial { _ = Darwin.unlink(path) }
        }
        guard Darwin.fchmod(descriptor, mode_t(0o600)) == 0 else {
            throw ProbeFailure(phase: "ready_marker", domain: "ReadyMarker", code: Int(errno))
        }
        var offset = 0
        while offset < data.count {
            let count = data.withUnsafeBytes { bytes in
                Darwin.write(descriptor, bytes.baseAddress!.advanced(by: offset), data.count - offset)
            }
            if count < 0 && errno == EINTR { continue }
            guard count > 0 else {
                throw ProbeFailure(phase: "ready_marker", domain: "ReadyMarker", code: Int(errno))
            }
            offset += count
        }
        guard Darwin.close(descriptor) == 0 else {
            descriptorOpen = false
            throw ProbeFailure(phase: "ready_marker", domain: "ReadyMarker", code: Int(errno))
        }
        descriptorOpen = false
        removePartial = false
    }

    static func runSelfTest() throws -> [String] {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("lan-probe-marker-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(
            at: directory,
            withIntermediateDirectories: false,
            attributes: [.posixPermissions: 0o700]
        )
        defer { try? FileManager.default.removeItem(at: directory) }
        let path = directory.appendingPathComponent("ready.json").path
        let size = VideoSize(width: 640, height: 360)
        try publish(path: path, size: size, monotonicSeconds: 123.5)
        let original = try Data(contentsOf: URL(fileURLWithPath: path))
        let attributes = try FileManager.default.attributesOfItem(atPath: path)
        guard original.count <= 1_024,
            (attributes[.posixPermissions] as? NSNumber)?.intValue == 0o600,
            let payload = try JSONSerialization.jsonObject(with: original) as? [String: Any],
            payload["schema_version"] as? Int == 1,
            payload["expected_width"] as? Int == 640,
            payload["expected_height"] as? Int == 360,
            payload["distinct_frame_count"] as? Int == 3,
            payload["observed_monotonic_seconds"] as? Double == 123.5
        else {
            throw ProbeFailure(phase: "self_test", domain: "ReadyMarker", code: 1)
        }
        do {
            try publish(path: path, size: VideoSize(width: 320, height: 180), monotonicSeconds: 124)
            throw ProbeFailure(phase: "self_test", domain: "ReadyMarker", code: 2)
        } catch let failure as ProbeFailure where failure.phase == "ready_marker" && failure.code == EEXIST {
            guard try Data(contentsOf: URL(fileURLWithPath: path)) == original else {
                throw ProbeFailure(phase: "self_test", domain: "ReadyMarker", code: 3)
            }
        }
        return ["ready_marker_is_bounded_private_exclusive_and_no_overwrite"]
    }
}

private enum VideoSizePhasePolicy {
    static let controlReserveSeconds = 20.0

    static func shouldContinueSteadyPlay(
        expectationsComplete: Bool,
        elapsedSeconds: Double,
        requestedDuration: Double,
        minimumSteadyPlaySeconds: Double = 0
    ) -> Bool {
        let steadyEnd = min(
            requestedDuration - controlReserveSeconds,
            max(
                expectationsComplete ? 0 : requestedDuration - controlReserveSeconds,
                minimumSteadyPlaySeconds
            ))
        return elapsedSeconds < steadyEnd
    }

    static func runSelfTest() throws -> [String] {
        guard
            shouldContinueSteadyPlay(
                expectationsComplete: false,
                elapsedSeconds: 20,
                requestedDuration: 90
            ),
            !shouldContinueSteadyPlay(
                expectationsComplete: true,
                elapsedSeconds: 20,
                requestedDuration: 90
            ),
            !shouldContinueSteadyPlay(
                expectationsComplete: false,
                elapsedSeconds: 70,
                requestedDuration: 90
            ),
            shouldContinueSteadyPlay(
                expectationsComplete: true,
                elapsedSeconds: 50,
                requestedDuration: 90,
                minimumSteadyPlaySeconds: 60
            ),
            !shouldContinueSteadyPlay(
                expectationsComplete: true,
                elapsedSeconds: 60,
                requestedDuration: 90,
                minimumSteadyPlaySeconds: 60
            )
        else {
            throw ProbeFailure(phase: "self_test", domain: "VideoSizePhasePolicy", code: 1)
        }
        return ["minimum_steady_play_preserves_control_reserve_and_completion_floor"]
    }
}

private enum DiagnosticObservationWindowPolicy {
    static let durationSeconds = 600.0

    static func contains(started: Double, observed: Double) -> Bool {
        started.isFinite && observed.isFinite && observed >= started
            && observed < started + durationSeconds
    }

    static func includesFrame(
        diagnostic: Bool,
        active: Bool,
        started: Double?,
        observed: Double?
    ) -> Bool {
        guard diagnostic else { return true }
        guard active, let started, let observed else { return false }
        return contains(started: started, observed: observed)
    }

    static func durationRemainder(requestedDuration: Double, elapsed: Double) -> Double {
        max(1, requestedDuration - elapsed)
    }

    static func runSelfTest() throws -> [String] {
        guard contains(started: 10, observed: 10), contains(started: 10, observed: 609.999),
            !contains(started: 10, observed: 610),
            includesFrame(diagnostic: false, active: false, started: nil, observed: nil),
            includesFrame(diagnostic: true, active: true, started: 10, observed: 11),
            !includesFrame(diagnostic: true, active: false, started: 10, observed: 11),
            !includesFrame(diagnostic: true, active: true, started: 10, observed: 611),
            durationRemainder(requestedDuration: 600, elapsed: 612) == 1,
            durationRemainder(requestedDuration: 600, elapsed: 590) == 10
        else {
            throw ProbeFailure(phase: "self_test", domain: "DiagnosticObservationWindow", code: 1)
        }
        return [
            "diagnostic_observation_window_uses_fixed_monotonic_bounds",
            "diagnostic_observation_window_excludes_control_and_post_window_frames",
            "diagnostic_duration_remainder_runs_at_least_one_real_active_second",
        ]
    }
}

private struct VideoSizeExpectationTracker {
    private struct Sample {
        let displayTimeSeconds: Double
        let observedElapsedSeconds: Double?
        let observedMonotonicSeconds: Double?

        var json: [String: Any] {
            [
                "display_time_seconds": displayTimeSeconds,
                "observed_elapsed_seconds": observedElapsedSeconds as Any? ?? NSNull(),
                "observed_monotonic_seconds": observedMonotonicSeconds as Any? ?? NSNull(),
            ]
        }
    }

    private struct Entry {
        let size: VideoSize
        var timestamps = Set<Int64>()
        var samples: [Sample] = []
        var matched: Bool { timestamps.count >= 3 }
    }

    private var entries: [Entry]
    private(set) var currentIndex = 0

    init(expectedSizes: [VideoSize]) {
        entries = expectedSizes.map { Entry(size: $0) }
    }

    var isComplete: Bool { currentIndex == entries.count }

    func requiredExpectationsComplete(optionalTrailingCount: Int) -> Bool {
        currentIndex >= max(0, entries.count - optionalTrailingCount)
    }

    mutating func observe(
        width: Int,
        height: Int,
        timestampNanoseconds: Int64,
        displayTimeSeconds: Double? = nil,
        observedElapsedSeconds: Double? = nil,
        observedMonotonicSeconds: Double? = nil
    ) {
        guard currentIndex < entries.count else { return }
        let size = VideoSize(width: width, height: height)
        guard entries[currentIndex].size == size else { return }
        guard !entries[currentIndex].timestamps.contains(timestampNanoseconds),
            entries[currentIndex].samples.count < 3
        else {
            return
        }
        entries[currentIndex].timestamps.insert(timestampNanoseconds)
        entries[currentIndex].samples.append(
            Sample(
                displayTimeSeconds: displayTimeSeconds ?? Double(timestampNanoseconds) / 1_000_000_000,
                observedElapsedSeconds: observedElapsedSeconds,
                observedMonotonicSeconds: observedMonotonicSeconds
            )
        )
        if entries[currentIndex].matched {
            currentIndex += 1
        }
    }

    var progress: [[String: Any]] {
        entries.enumerated().map { index, entry in
            [
                "expected_width": entry.size.width,
                "expected_height": entry.size.height,
                "matched": index < currentIndex,
                "distinct_frame_count": min(entry.timestamps.count, 3),
                "required_distinct_frame_count": 3,
                "samples": entry.samples.map(\.json),
            ]
        }
    }

    var diagnosticProgress: [[String: Any]] {
        guard entries.count == 3, currentIndex == 2 else { return progress }
        return progress.enumerated().map { index, entry in
            guard index == 2 else { return entry }
            return [
                "expected_width": entries[index].size.width,
                "expected_height": entries[index].size.height,
                "matched": false,
                "distinct_frame_count": 0,
                "required_distinct_frame_count": 3,
                "samples": [[String: Any]](),
            ]
        }
    }

    var hasValidNumericSamples: Bool {
        entries.allSatisfy(isValidNumericEntry)
    }

    var hasValidNumericSamplesAllowingMissingTrailingEntry: Bool {
        guard entries.count == 3, requiredExpectationsComplete(optionalTrailingCount: 1) else { return false }
        return entries.prefix(2).allSatisfy(isValidNumericEntry)
            && (entries[2].matched ? isValidNumericEntry(entries[2]) : true)
    }

    private func isValidNumericEntry(_ entry: Entry) -> Bool {
        guard entry.samples.count == 3 else { return false }
        var previousElapsed: Double = -.infinity
        var previousMonotonic: Double?
        var monotonicSampleCount = 0
        for sample in entry.samples {
            guard sample.displayTimeSeconds.isFinite,
                let elapsed = sample.observedElapsedSeconds,
                elapsed.isFinite, elapsed >= 0, elapsed >= previousElapsed
            else {
                return false
            }
            if let monotonic = sample.observedMonotonicSeconds {
                guard monotonic.isFinite, monotonic > 0,
                    previousMonotonic.map({ monotonic >= $0 }) ?? true
                else {
                    return false
                }
                previousMonotonic = monotonic
                monotonicSampleCount += 1
            }
            previousElapsed = elapsed
        }
        return monotonicSampleCount == 0 || monotonicSampleCount == 3
    }

    static func runSelfTest() throws -> [String] {
        let high = VideoSize(width: 1920, height: 1080)
        let low = VideoSize(width: 1280, height: 720)
        var tracker = VideoSizeExpectationTracker(expectedSizes: [high, low, high])
        tracker.observe(width: low.width, height: low.height, timestampNanoseconds: 1)
        guard tracker.currentIndex == 0 else {
            throw ProbeFailure(phase: "self_test", domain: "VideoSizeExpectationTracker", code: 1)
        }
        tracker.observe(width: high.width, height: high.height, timestampNanoseconds: 10, observedElapsedSeconds: 1)
        tracker.observe(width: high.width, height: high.height, timestampNanoseconds: 10, observedElapsedSeconds: 1.5)
        tracker.observe(width: high.width, height: high.height, timestampNanoseconds: 20, observedElapsedSeconds: 2)
        guard tracker.currentIndex == 0, tracker.progress[0]["distinct_frame_count"] as? Int == 2 else {
            throw ProbeFailure(phase: "self_test", domain: "VideoSizeExpectationTracker", code: 2)
        }
        tracker.observe(width: high.width, height: high.height, timestampNanoseconds: 30, observedElapsedSeconds: 3)
        guard tracker.currentIndex == 1 else {
            throw ProbeFailure(phase: "self_test", domain: "VideoSizeExpectationTracker", code: 3)
        }
        tracker.observe(width: high.width, height: high.height, timestampNanoseconds: 40)
        tracker.observe(width: low.width, height: low.height, timestampNanoseconds: 50, observedElapsedSeconds: 4)
        tracker.observe(width: low.width, height: low.height, timestampNanoseconds: 60, observedElapsedSeconds: 5)
        guard tracker.currentIndex == 1 else {
            throw ProbeFailure(phase: "self_test", domain: "VideoSizeExpectationTracker", code: 4)
        }
        tracker.observe(width: low.width, height: low.height, timestampNanoseconds: 70, observedElapsedSeconds: 6)
        tracker.observe(width: high.width, height: high.height, timestampNanoseconds: 80, observedElapsedSeconds: 7)
        tracker.observe(width: high.width, height: high.height, timestampNanoseconds: 90, observedElapsedSeconds: 8)
        guard !tracker.isComplete else {
            throw ProbeFailure(phase: "self_test", domain: "VideoSizeExpectationTracker", code: 5)
        }
        tracker.observe(width: high.width, height: high.height, timestampNanoseconds: 100, observedElapsedSeconds: 9)
        guard tracker.isComplete, tracker.progress.allSatisfy({ $0["matched"] as? Bool == true }) else {
            throw ProbeFailure(phase: "self_test", domain: "VideoSizeExpectationTracker", code: 6)
        }
        let samples = tracker.progress.compactMap { $0["samples"] as? [[String: Any]] }.flatMap { $0 }
        let elapsedValues = samples.compactMap { $0["observed_elapsed_seconds"] as? Double }
        guard samples.count == 9, elapsedValues.count == 9,
            zip(elapsedValues, elapsedValues.dropFirst()).allSatisfy({ $0 <= $1 }),
            tracker.hasValidNumericSamples
        else {
            throw ProbeFailure(phase: "self_test", domain: "VideoSizeExpectationTracker", code: 7)
        }
        var missingElapsed = VideoSizeExpectationTracker(expectedSizes: [high])
        for index in 0..<3 {
            missingElapsed.observe(
                width: high.width,
                height: high.height,
                timestampNanoseconds: Int64(index + 1)
            )
        }
        guard missingElapsed.isComplete, !missingElapsed.hasValidNumericSamples else {
            throw ProbeFailure(phase: "self_test", domain: "VideoSizeExpectationTracker", code: 8)
        }
        var decreasingElapsed = VideoSizeExpectationTracker(expectedSizes: [high])
        for (index, elapsed) in [1.0, 0.5, 2.0].enumerated() {
            decreasingElapsed.observe(
                width: high.width,
                height: high.height,
                timestampNanoseconds: Int64(index + 1),
                observedElapsedSeconds: elapsed,
                observedMonotonicSeconds: Double(index + 1)
            )
        }
        guard decreasingElapsed.isComplete, !decreasingElapsed.hasValidNumericSamples else {
            throw ProbeFailure(phase: "self_test", domain: "VideoSizeExpectationTracker", code: 9)
        }
        var monotonicTracker = VideoSizeExpectationTracker(expectedSizes: [high])
        for index in 1...3 {
            monotonicTracker.observe(
                width: high.width,
                height: high.height,
                timestampNanoseconds: Int64(index),
                observedElapsedSeconds: Double(index),
                observedMonotonicSeconds: 1_000 + Double(index)
            )
        }
        let monotonicSamples = monotonicTracker.progress[0]["samples"] as? [[String: Any]] ?? []
        guard monotonicTracker.hasValidNumericSamples,
            monotonicSamples.compactMap({ $0["observed_monotonic_seconds"] as? Double }).count == 3
        else {
            throw ProbeFailure(phase: "self_test", domain: "VideoSizeExpectationTracker", code: 10)
        }
        return [
            "video_size_expectations_require_ordered_three_unique_timestamps",
            "duplicate_timestamps_and_out_of_order_sizes_do_not_advance",
            "video_size_expectations_allow_high_low_high",
            "video_size_expectations_retain_three_unique_samples_with_monotonic_elapsed_times",
            "video_size_expectations_require_numeric_elapsed_times_for_completion",
            "video_size_expectations_reject_nonmonotonic_elapsed_times",
            "video_size_expectations_retain_shared_monotonic_timestamps",
        ]
    }
}

private struct DecodedFrameContinuity {
    static let gapThresholdSeconds = 0.5

    private(set) var decodedFrameCount = 0
    private(set) var observedIntervalCount = 0
    private(set) var maximumInterframeGapSeconds: Double?
    private(set) var largeGapCount = 0
    private(set) var observationClockAnomalyCount = 0
    private var previousMonotonicSeconds: Double?

    mutating func observe(monotonicSeconds: Double?) {
        decodedFrameCount += 1
        guard let monotonicSeconds, monotonicSeconds.isFinite, monotonicSeconds >= 0 else {
            observationClockAnomalyCount += 1
            previousMonotonicSeconds = nil
            return
        }
        if let previousMonotonicSeconds {
            let gap = monotonicSeconds - previousMonotonicSeconds
            guard gap >= 0, gap.isFinite else {
                observationClockAnomalyCount += 1
                self.previousMonotonicSeconds = nil
                return
            }
            observedIntervalCount += 1
            maximumInterframeGapSeconds = max(maximumInterframeGapSeconds ?? 0, gap)
            if gap > Self.gapThresholdSeconds {
                largeGapCount += 1
            }
        }
        previousMonotonicSeconds = monotonicSeconds
    }

    var json: [String: Any] {
        [
            "schema_version": 1,
            "decoded_frame_count": decodedFrameCount,
            "observed_interval_count": observedIntervalCount,
            "maximum_interframe_gap_seconds": maximumInterframeGapSeconds as Any? ?? NSNull(),
            "gap_threshold_seconds": Self.gapThresholdSeconds,
            "large_gap_count": largeGapCount,
            "observation_clock_anomaly_count": observationClockAnomalyCount,
            "coverage": "between_observed_decoded_frames_only",
            "interpretation": "decoded_frame_observation_gaps_not_visible_black_screen_acceptance",
        ]
    }

    static func runSelfTest() throws -> [String] {
        var single = DecodedFrameContinuity()
        single.observe(monotonicSeconds: 1)
        guard single.decodedFrameCount == 1, single.observedIntervalCount == 0,
            single.maximumInterframeGapSeconds == nil
        else {
            throw ProbeFailure(phase: "self_test", domain: "DecodedFrameContinuity", code: 1)
        }

        var intervals = DecodedFrameContinuity()
        for timestamp in [1.0, 1.5, 2.2, 2.9, 3.6] {
            intervals.observe(monotonicSeconds: timestamp)
        }
        guard intervals.decodedFrameCount == 5, intervals.observedIntervalCount == 4,
            abs((intervals.maximumInterframeGapSeconds ?? 0) - 0.7) < 0.000_001,
            intervals.largeGapCount == 3
        else {
            throw ProbeFailure(phase: "self_test", domain: "DecodedFrameContinuity", code: 2)
        }

        var anomalies = DecodedFrameContinuity()
        for timestamp in [
            Optional(1.0), Optional(1.5), Optional(1.4), Optional(Double.nan),
            Optional(Double.infinity), Optional(-1.0), nil, Optional(4.0), Optional(4.2),
        ] {
            anomalies.observe(monotonicSeconds: timestamp)
        }
        guard anomalies.decodedFrameCount == 9, anomalies.observedIntervalCount == 2,
            anomalies.maximumInterframeGapSeconds == 0.5, anomalies.largeGapCount == 0,
            anomalies.observationClockAnomalyCount == 5
        else {
            throw ProbeFailure(phase: "self_test", domain: "DecodedFrameContinuity", code: 3)
        }

        let empty = DecodedFrameContinuity().json
        guard empty["decoded_frame_count"] as? Int == 0,
            empty["observed_interval_count"] as? Int == 0,
            empty["maximum_interframe_gap_seconds"] is NSNull,
            empty["gap_threshold_seconds"] as? Double == 0.5,
            empty["coverage"] as? String == "between_observed_decoded_frames_only",
            empty["interpretation"] as? String == "decoded_frame_observation_gaps_not_visible_black_screen_acceptance"
        else {
            throw ProbeFailure(phase: "self_test", domain: "DecodedFrameContinuity", code: 4)
        }
        return [
            "decoded_frame_continuity_counts_single_observation_without_interval",
            "decoded_frame_continuity_counts_two_or_more_intervals_and_exact_threshold",
            "decoded_frame_continuity_breaks_adjacency_for_missing_nonfinite_negative_and_backward_clock",
            "decoded_frame_continuity_empty_summary_is_explicit_and_bounded",
        ]
    }
}

private struct BoundedObservations {
    private let limit: Int
    private var values: [Int: [String: Any]] = [:]
    private(set) var truncated = false

    init(limit: Int = 32) {
        self.limit = limit
    }

    mutating func append(_ value: [String: Any]) {
        let index = values.count
        guard index < limit else {
            truncated = true
            return
        }
        values[index] = value
    }

    mutating func upsert(index: Int, value: [String: Any]) {
        guard index >= 0, index < limit else {
            truncated = true
            return
        }
        values[index] = value
    }

    var observations: [[String: Any]] {
        values.keys.sorted().compactMap { values[$0] }
    }

    static func runSelfTest() throws -> [String] {
        var observations = BoundedObservations(limit: 2)
        observations.append(["width": 1920, "height": 1080])
        observations.append(["width": 1280, "height": 720])
        observations.append(["width": 640, "height": 360])
        observations.upsert(index: 1, value: ["event_index": 1, "observed_bitrate": 1_000])
        guard observations.observations.count == 2, observations.truncated,
            observations.observations[1]["event_index"] as? Int == 1
        else {
            throw ProbeFailure(phase: "self_test", domain: "BoundedObservations", code: 1)
        }
        var accessLog = BoundedObservations()
        for index in 0..<33 {
            accessLog.upsert(index: index, value: ["event_index": index])
        }
        guard accessLog.observations.count == 32, accessLog.truncated,
            accessLog.observations.first?["event_index"] as? Int == 0,
            accessLog.observations.last?["event_index"] as? Int == 31
        else {
            throw ProbeFailure(phase: "self_test", domain: "BoundedObservations", code: 2)
        }
        return [
            "decoded_and_access_log_observations_are_bounded_and_mark_truncation",
            "access_log_retains_first_32_and_marks_33rd_event_truncated",
        ]
    }
}

private enum PlayerErrorLogDiagnostics {
    static let maximumEvents = 32

    static func safeDomain(_ domain: String?) -> String {
        switch domain {
        case .some("AVFoundationErrorDomain"): "avfoundation"
        case .some("CoreMediaErrorDomain"): "core_media"
        case .some("NSOSStatusErrorDomain"): "os_status"
        case .some("NSURLErrorDomain"): "url"
        default: "other"
        }
    }

    static func snapshot(
        _ events: [(statusCode: Int, domain: String?)],
        limit: Int = maximumEvents
    ) -> (observations: [[String: Any]], truncated: Bool) {
        let observations = events.prefix(limit).enumerated().map { index, event in
            [
                "event_index": index,
                "status_code": event.statusCode,
                "domain": safeDomain(event.domain),
            ] as [String: Any]
        }
        return (observations, events.count > limit)
    }

    static func runSelfTest() throws -> [String] {
        let values = (0..<33).map { index in
            (statusCode: index + 400, domain: index == 0 ? "untrusted.example/path" : "NSURLErrorDomain")
        }
        let result = snapshot(values)
        guard result.observations.count == maximumEvents,
            result.truncated,
            result.observations.first?["event_index"] as? Int == 0,
            result.observations.last?["event_index"] as? Int == 31,
            result.observations.first?["status_code"] as? Int == 400,
            result.observations.first?["domain"] as? String == "other",
            result.observations.last?["domain"] as? String == "url"
        else {
            throw ProbeFailure(phase: "self_test", domain: "PlayerErrorLogDiagnostics", code: 1)
        }
        let encoded = try JSONSerialization.data(withJSONObject: result.observations)
        let encodedText = String(decoding: encoded, as: UTF8.self)
        guard !encodedText.contains("untrusted.example") && !encodedText.contains("/path") else {
            throw ProbeFailure(phase: "self_test", domain: "PlayerErrorLogDiagnostics", code: 2)
        }
        return [
            "player_error_log_retains_at_most_32_numeric_records_and_marks_truncation",
            "player_error_log_maps_unknown_domains_to_fixed_other_value",
        ]
    }
}

private enum PlaybackPolicy {
    enum Readiness: Equatable {
        case wait
        case shortInput
        case unsupportedDuration
        case ready
    }

    static func readiness(
        itemReady: Bool,
        durationSeconds: Double,
        requestedDuration: Double
    ) -> Readiness {
        guard itemReady else { return .wait }
        guard durationSeconds.isFinite, durationSeconds > 0 else { return .unsupportedDuration }
        guard durationSeconds >= requestedDuration else { return .shortInput }
        return .ready
    }

    static func canComplete(
        decodedFrameCount: Int,
        audioEvidencePresent: Bool,
        uniqueDisplayTimeCount: Int,
        controlsComplete: Bool
    ) -> Bool {
        decodedFrameCount > 0 && audioEvidencePresent && uniqueDisplayTimeCount >= 3 && controlsComplete
    }

    static func runSelfTest() throws -> [String] {
        guard readiness(itemReady: false, durationSeconds: .nan, requestedDuration: 60) == .wait else {
            throw ProbeFailure(phase: "self_test", domain: "PlaybackPolicy", code: 1)
        }

        let unknownTracksAfterReady = readiness(itemReady: true, durationSeconds: 60, requestedDuration: 60)
        guard unknownTracksAfterReady == .ready,
            !canComplete(
                decodedFrameCount: 0,
                audioEvidencePresent: false,
                uniqueDisplayTimeCount: 3,
                controlsComplete: true
            )
        else {
            throw ProbeFailure(phase: "self_test", domain: "PlaybackPolicy", code: 2)
        }

        guard
            canComplete(
                decodedFrameCount: 3,
                audioEvidencePresent: true,
                uniqueDisplayTimeCount: 3,
                controlsComplete: true
            )
        else {
            throw ProbeFailure(phase: "self_test", domain: "PlaybackPolicy", code: 3)
        }

        guard
            !canComplete(
                decodedFrameCount: 0,
                audioEvidencePresent: true,
                uniqueDisplayTimeCount: 3,
                controlsComplete: true
            )
        else {
            throw ProbeFailure(phase: "self_test", domain: "PlaybackPolicy", code: 4)
        }

        guard
            !canComplete(
                decodedFrameCount: 3,
                audioEvidencePresent: false,
                uniqueDisplayTimeCount: 3,
                controlsComplete: true
            )
        else {
            throw ProbeFailure(phase: "self_test", domain: "PlaybackPolicy", code: 5)
        }

        guard
            !canComplete(
                decodedFrameCount: 3,
                audioEvidencePresent: true,
                uniqueDisplayTimeCount: 2,
                controlsComplete: true
            ),
            !canComplete(
                decodedFrameCount: 3,
                audioEvidencePresent: true,
                uniqueDisplayTimeCount: 3,
                controlsComplete: false
            )
        else {
            throw ProbeFailure(phase: "self_test", domain: "PlaybackPolicy", code: 6)
        }

        guard
            readiness(
                itemReady: true,
                durationSeconds: .infinity,
                requestedDuration: 60
            ) == .unsupportedDuration,
            readiness(
                itemReady: true,
                durationSeconds: 30,
                requestedDuration: 60
            ) == .shortInput,
            readiness(
                itemReady: true,
                durationSeconds: 120,
                requestedDuration: 60
            ) == .ready,
            canComplete(
                decodedFrameCount: 3,
                audioEvidencePresent: true,
                uniqueDisplayTimeCount: 3,
                controlsComplete: true
            )
        else {
            throw ProbeFailure(phase: "self_test", domain: "PlaybackPolicy", code: 7)
        }

        return [
            "unknown_metadata_waits_for_item_ready",
            "ready_unknown_tracks_do_not_fail_preflight",
            "unknown_tracks_require_decoded_frames_and_audio_evidence",
            "missing_video_frames_cannot_complete",
            "missing_audio_evidence_cannot_complete",
            "unique_times_and_all_controls_required",
            "finite_duration_and_full_evidence_required",
        ]
    }
}

@MainActor
private final class PlaybackProbe {
    private let options: ProbeOptions
    private var videoSizeExpectations: VideoSizeExpectationTracker
    private let start = DispatchTime.now().uptimeNanoseconds
    private var end: UInt64 { start + UInt64(options.deadline * 1_000_000_000) }
    private let output = AVPlayerItemVideoOutput(pixelBufferAttributes: [
        kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA
    ])
    private var player: AVPlayer?
    private var item: AVPlayerItem?
    private var decodedFrames = 0
    private var decodedFrameContinuity = DecodedFrameContinuity()
    private var diagnosticWindowContinuity = DecodedFrameContinuity()
    private var diagnosticWindowStartedMonotonicSeconds: Double?
    private var diagnosticWindowActive = false
    private var diagnosticObservationWindow: [String: Any]?
    private var uniqueDisplayTimes = Set<Int64>()
    private var lastDecodedWidth: Int?
    private var lastDecodedHeight: Int?
    private var playbackTimeline = PlaybackTimelineDiagnostics()
    private var decodedVideoSizeObservations = BoundedObservations()
    private var accessLogObservations = BoundedObservations()
    private var frameSamples: [[String: Any]] = []
    private var steps: [[String: Any]] = []
    private var videoTrackPresent = false
    private var audioTrackPresent = false
    private var videoTrackCount = 0
    private var audioTrackCount = 0
    private var enabledVideoTrackCount = 0
    private var enabledAudioTrackCount = 0
    private var itemDurationSeconds: Double?
    private var presentationWidth = 0
    private var presentationHeight = 0
    private var itemReadyAtMetadataCapture = false
    private var audioTrackEvidenceSource = "not_inspected"
    private var audioEvidenceLookupError: [String: Any]?
    private var finalPlayerState: [String: Any]?
    private var recoveryObservation: [String: Any]?
    private var steadyPlayEndObservation: [String: Any]?
    private var finalPlayerErrorLog: [[String: Any]] = []
    private var finalPlayerErrorLogTruncated = false
    private var endedEarly = false
    private var endObserver: NSObjectProtocol?
    private var readyMarkerPublished = false

    init(options: ProbeOptions) {
        self.options = options
        videoSizeExpectations = VideoSizeExpectationTracker(expectedSizes: options.expectedVideoSizes)
    }

    func run() async -> (Int32, [String: Any]) {
        do {
            let status = try await execute()
            captureFinalPlayerState()
            cleanup()
            return (status == "completed" ? 0 : 3, result(status: status, error: nil))
        } catch let failure as ProbeFailure {
            captureFinalPlayerState()
            cleanup()
            return (2, result(status: status(for: failure), error: failure))
        } catch {
            captureFinalPlayerState()
            cleanup()
            let failure = ProbeFailure(phase: "runtime", error: error)
            return (2, result(status: "failed", error: failure))
        }
    }

    private func execute() async throws -> String {
        let playerItem = AVPlayerItem(url: options.url)
        playerItem.add(output)
        item = playerItem
        endObserver = NotificationCenter.default.addObserver(
            forName: .AVPlayerItemDidPlayToEndTime,
            object: playerItem,
            queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                self?.endedEarly = true
            }
        }
        let avPlayer = AVPlayer(playerItem: playerItem)
        try PlayerSetup.configure(avPlayer)
        player = avPlayer

        try await waitUntilReady()
        itemReadyAtMetadataCapture = true
        let tracks = playerItem.tracks
        let videoTracks = tracks.filter { $0.assetTrack?.mediaType == .video }
        let audioTracks = tracks.filter { $0.assetTrack?.mediaType == .audio }
        videoTrackCount = videoTracks.count
        audioTrackCount = audioTracks.count
        enabledVideoTrackCount = videoTracks.filter { $0.isEnabled }.count
        enabledAudioTrackCount = audioTracks.filter { $0.isEnabled }.count
        videoTrackPresent = enabledVideoTrackCount > 0
        let duration = CMTimeGetSeconds(playerItem.duration)
        itemDurationSeconds = duration.isFinite ? duration : nil
        let presentationSize = playerItem.presentationSize
        presentationWidth = Int(presentationSize.width)
        presentationHeight = Int(presentationSize.height)

        switch PlaybackPolicy.readiness(
            itemReady: true,
            durationSeconds: duration,
            requestedDuration: options.duration
        ) {
        case .wait:
            throw ProbeFailure(phase: "ready", domain: "PlaybackPolicy", code: 1)
        case .shortInput:
            return "short_input"
        case .unsupportedDuration:
            return "unsupported_duration"
        case .ready:
            break
        }
        try await inspectAudioEvidence(for: playerItem)

        let playbackStart = DispatchTime.now().uptimeNanoseconds
        try startPlayback(avPlayer, phase: "play")
        if options.controlsAfterVideoSizeExpectations {
            if options.recoverySnapshotPath != nil {
                try await diagnosticObservationWindowPhase()
            } else {
                try await steadyPlayUntilVideoSizeExpectations(playbackStart: playbackStart)
            }
            steadyPlayEndObservation = try playerObservation()
        } else {
            try await activePhase("play", seconds: min(8, options.duration * 0.2))
        }

        avPlayer.pause()
        let pausedAt = seconds(avPlayer.currentTime())
        let pauseStart = DispatchTime.now().uptimeNanoseconds
        while DispatchTime.now().uptimeNanoseconds - pauseStart < 1_000_000_000 {
            try checkDeadline(phase: "pause")
            try await Task.sleep(for: .milliseconds(50))
        }
        let pauseDrift = abs(seconds(avPlayer.currentTime()) - pausedAt)
        steps.append([
            "name": "pause",
            "status": pauseDrift <= 0.25 ? "completed" : "drift",
            "time_seconds": seconds(avPlayer.currentTime()),
            "drift_seconds": pauseDrift,
        ])
        guard pauseDrift <= 0.25 else {
            throw ProbeFailure(phase: "pause", domain: "ProbePlayback", code: 2)
        }

        try startPlayback(avPlayer, phase: "resume")
        try await activePhase("resume", seconds: 3)

        let total = itemDurationSeconds ?? 0
        if total > 1 {
            let current = max(0, seconds(avPlayer.currentTime()))
            let forward = min(current + 5, max(0, total - 0.25))
            if forward > current + 0.25 {
                try await seek(to: forward, name: "seek_forward", player: avPlayer)
                try await activePhase("seek_forward_play", seconds: 2)
                let back = max(0, forward - 3)
                try await seek(to: back, name: "seek_back", player: avPlayer)
                try await activePhase("seek_back_play", seconds: 2)
            } else {
                steps.append(["name": "seek_forward", "status": "skipped", "reason": "end_of_asset"])
                steps.append(["name": "seek_back", "status": "skipped", "reason": "forward_seek_unavailable"])
            }
        } else {
            steps.append(["name": "seek_forward", "status": "skipped", "reason": "duration_unknown"])
            steps.append(["name": "seek_back", "status": "skipped", "reason": "duration_unknown"])
        }

        try setRate(1.25, player: avPlayer, phase: "rate_1_25")
        try await activePhase("rate_1_25", seconds: 3, expectedRate: 1.25)
        try setRate(1, player: avPlayer, phase: "rate_reset")

        let elapsed = TimeInterval(DispatchTime.now().uptimeNanoseconds - playbackStart) / 1_000_000_000
        if options.recoverySnapshotPath != nil {
            try startPlayback(avPlayer, phase: "duration_remainder")
            try await activePhase(
                "duration_remainder",
                seconds: DiagnosticObservationWindowPolicy.durationRemainder(
                    requestedDuration: options.duration,
                    elapsed: elapsed
                )
            )
        } else if elapsed < options.duration {
            try startPlayback(avPlayer, phase: "duration_remainder")
            try await activePhase("duration_remainder", seconds: options.duration - elapsed)
        }

        let optionalRecoveryHigh = options.recoverySnapshotPath != nil
        let requiredSizesComplete =
            optionalRecoveryHigh
            ? videoSizeExpectations.requiredExpectationsComplete(optionalTrailingCount: 1)
            : videoSizeExpectations.isComplete
        guard requiredSizesComplete else {
            throw ProbeFailure(phase: "video_size_expectations", domain: "DecodedVideoSizeTracker", code: 1)
        }
        let validSizeSamples =
            optionalRecoveryHigh
            ? videoSizeExpectations.hasValidNumericSamplesAllowingMissingTrailingEntry
            : videoSizeExpectations.hasValidNumericSamples
        guard validSizeSamples else {
            throw ProbeFailure(phase: "video_size_expectations", domain: "DecodedVideoSizeTracker", code: 2)
        }
        guard uniqueDisplayTimes.count >= 3 else {
            throw ProbeFailure(phase: "decode", domain: "AVPlayerItemVideoOutput", code: 1)
        }
        let controlsComplete = [
            "play", "pause", "resume", "seek_forward", "seek_forward_play", "seek_back", "seek_back_play", "rate_1_25",
            "duration_remainder",
        ].allSatisfy { required in
            steps.contains { $0["name"] as? String == required && $0["status"] as? String == "completed" }
        }
        guard
            PlaybackPolicy.canComplete(
                decodedFrameCount: decodedFrames,
                audioEvidencePresent: audioTrackPresent,
                uniqueDisplayTimeCount: uniqueDisplayTimes.count,
                controlsComplete: controlsComplete
            )
        else {
            return audioTrackPresent ? "failed" : "lack_audio"
        }
        return "completed"
    }

    private func inspectAudioEvidence(for playerItem: AVPlayerItem) async throws {
        if enabledAudioTrackCount > 0 {
            audioTrackPresent = true
            audioTrackEvidenceSource = "enabled_av_player_item_track_after_ready"
            return
        }

        do {
            try checkDeadline(phase: "audible_media_group")
            let group = try await playerItem.asset.loadMediaSelectionGroup(for: .audible)
            try checkDeadline(phase: "audible_media_group")
            guard let group else {
                audioTrackEvidenceSource = "no_audible_group_after_ready"
                return
            }
            if playerItem.currentMediaSelection.selectedMediaOption(in: group) != nil {
                audioTrackPresent = true
                audioTrackEvidenceSource = "selected_audible_media_option_after_ready"
            } else {
                audioTrackEvidenceSource = "no_selected_audible_option_after_ready"
            }
        } catch let failure as ProbeFailure where failure.domain == "ProbeDeadline" {
            throw failure
        } catch {
            let failure = ProbeFailure(phase: "audible_media_group", error: error)
            audioEvidenceLookupError = ["domain": failure.safeDomain, "code": failure.code]
            audioTrackEvidenceSource = "audible_group_load_failed"
        }
    }

    private func waitUntilReady() async throws {
        while true {
            try checkDeadline(phase: "ready")
            guard let item else { throw ProbeFailure(phase: "ready", domain: "Probe", code: 1) }
            switch item.status {
            case .readyToPlay:
                return
            case .failed:
                throw ProbeFailure(phase: "ready", error: item.error ?? NSError(domain: "AVPlayerItem", code: 1))
            case .unknown:
                try await Task.sleep(for: .milliseconds(100))
            @unknown default:
                throw ProbeFailure(phase: "ready", domain: "AVPlayerItem", code: 2)
            }
        }
    }

    private func activePhase(
        _ name: String,
        seconds duration: TimeInterval,
        expectedRate: Float = 1
    ) async throws {
        let before = decodedFrames
        let phaseStart = DispatchTime.now().uptimeNanoseconds
        let mediaStart = player.map { seconds($0.currentTime()) } ?? 0
        let phaseEnd = phaseStart + UInt64(duration * 1_000_000_000)
        while DispatchTime.now().uptimeNanoseconds < phaseEnd {
            try checkDeadline(phase: name)
            if endedEarly {
                throw ProbeFailure(phase: name, domain: "ProbePlayback", code: 1)
            }
            try sampleFrame(phase: name)
            try await Task.sleep(for: .milliseconds(33))
        }
        let wallElapsed = TimeInterval(DispatchTime.now().uptimeNanoseconds - phaseStart) / 1_000_000_000
        let mediaEnd = player.map { seconds($0.currentTime()) } ?? mediaStart
        let mediaAdvance = mediaEnd - mediaStart
        let actualRate = player?.rate ?? 0
        let rateAccepted = abs(actualRate - expectedRate) < 0.01
        let clockAdvanced = mediaAdvance >= max(0.1, wallElapsed * Double(expectedRate) * 0.25)
        steps.append([
            "name": name,
            "status": decodedFrames > before && rateAccepted && clockAdvanced ? "completed" : "failed",
            "frames": decodedFrames - before,
            "elapsed_seconds": wallElapsed,
            "time_seconds": mediaEnd,
            "media_advance_seconds": mediaAdvance,
            "expected_rate": expectedRate,
            "actual_rate": actualRate,
            "rate_accepted": rateAccepted,
            "clock_advanced": clockAdvanced,
        ])
        guard decodedFrames > before, rateAccepted, clockAdvanced else {
            throw ProbeFailure(phase: name, domain: "AVPlayerItemVideoOutput", code: 2)
        }
    }

    private func steadyPlayUntilVideoSizeExpectations(playbackStart: UInt64) async throws {
        let before = decodedFrames
        let phaseStart = DispatchTime.now().uptimeNanoseconds
        let mediaStart = player.map { seconds($0.currentTime()) } ?? 0
        while VideoSizePhasePolicy.shouldContinueSteadyPlay(
            expectationsComplete: options.recoverySnapshotPath == nil
                ? videoSizeExpectations.isComplete
                : videoSizeExpectations.requiredExpectationsComplete(optionalTrailingCount: 1),
            elapsedSeconds: TimeInterval(DispatchTime.now().uptimeNanoseconds - playbackStart)
                / 1_000_000_000,
            requestedDuration: options.duration,
            minimumSteadyPlaySeconds: options.minimumSteadyPlaySeconds ?? 0
        ) {
            try checkDeadline(phase: "play")
            if endedEarly {
                throw ProbeFailure(phase: "play", domain: "ProbePlayback", code: 1)
            }
            try observeRecoveryTriggerIfReady()
            try sampleFrame(phase: "play")
            try await Task.sleep(for: .milliseconds(33))
        }
        let wallElapsed = TimeInterval(DispatchTime.now().uptimeNanoseconds - phaseStart) / 1_000_000_000
        let mediaEnd = player.map { seconds($0.currentTime()) } ?? mediaStart
        let mediaAdvance = mediaEnd - mediaStart
        let rateAccepted = abs((player?.rate ?? 0) - 1) < 0.01
        let clockAdvanced = mediaAdvance >= max(0.1, wallElapsed * 0.25)
        let requiredExpectationsComplete =
            options.recoverySnapshotPath == nil
            ? videoSizeExpectations.isComplete
            : videoSizeExpectations.requiredExpectationsComplete(optionalTrailingCount: 1)
        let complete = requiredExpectationsComplete && decodedFrames > before && rateAccepted && clockAdvanced
        steps.append([
            "name": "play",
            "status": complete ? "completed" : "failed",
            "frames": decodedFrames - before,
            "elapsed_seconds": wallElapsed,
            "time_seconds": mediaEnd,
            "media_advance_seconds": mediaAdvance,
            "expected_rate": 1,
            "actual_rate": player?.rate ?? 0,
            "rate_accepted": rateAccepted,
            "clock_advanced": clockAdvanced,
            "video_size_expectations_complete": videoSizeExpectations.isComplete,
        ])
        guard complete else {
            throw ProbeFailure(phase: "video_size_expectations", domain: "DecodedVideoSizeTracker", code: 3)
        }
    }

    private func diagnosticObservationWindowPhase() async throws {
        let before = decodedFrames
        let phaseStart = DispatchTime.now().uptimeNanoseconds
        let mediaStart = player.map { seconds($0.currentTime()) } ?? 0
        let started = try monotonicClockSeconds()
        diagnosticWindowStartedMonotonicSeconds = started
        diagnosticWindowActive = true
        while DiagnosticObservationWindowPolicy.contains(
            started: started,
            observed: try monotonicClockSeconds()
        ) {
            try checkDeadline(phase: "play")
            if endedEarly {
                throw ProbeFailure(phase: "play", domain: "ProbePlayback", code: 1)
            }
            try observeRecoveryTriggerIfReady()
            try sampleFrame(phase: "play")
            try await Task.sleep(for: .milliseconds(33))
        }
        let ended = try monotonicClockSeconds()
        diagnosticWindowActive = false
        diagnosticObservationWindow = [
            "schema_version": 1,
            "started_monotonic_seconds": started,
            "ended_monotonic_seconds": ended,
            "requested_duration_seconds": DiagnosticObservationWindowPolicy.durationSeconds,
            "controls_excluded": true,
            "decoded_frame_continuity": diagnosticWindowContinuity.json,
        ]
        let wallElapsed = TimeInterval(DispatchTime.now().uptimeNanoseconds - phaseStart) / 1_000_000_000
        let mediaEnd = player.map { seconds($0.currentTime()) } ?? mediaStart
        let mediaAdvance = mediaEnd - mediaStart
        let rateAccepted = abs((player?.rate ?? 0) - 1) < 0.01
        let clockAdvanced = mediaAdvance >= max(0.1, wallElapsed * 0.25)
        let requiredExpectationsComplete = videoSizeExpectations.requiredExpectationsComplete(optionalTrailingCount: 1)
        let complete = requiredExpectationsComplete && decodedFrames > before && rateAccepted && clockAdvanced
        steps.append([
            "name": "play",
            "status": complete ? "completed" : "failed",
            "frames": decodedFrames - before,
            "elapsed_seconds": wallElapsed,
            "time_seconds": mediaEnd,
            "media_advance_seconds": mediaAdvance,
            "expected_rate": 1,
            "actual_rate": player?.rate ?? 0,
            "rate_accepted": rateAccepted,
            "clock_advanced": clockAdvanced,
            "video_size_expectations_complete": videoSizeExpectations.isComplete,
        ])
        guard complete else {
            throw ProbeFailure(phase: "video_size_expectations", domain: "DecodedVideoSizeTracker", code: 3)
        }
    }

    private func seek(to targetSeconds: Double, name: String, player: AVPlayer) async throws {
        try checkDeadline(phase: name)
        let target = CMTime(seconds: targetSeconds, preferredTimescale: 600)
        player.pause()
        let finished = try await withDeadline(phase: name) {
            await player.seek(to: target, toleranceBefore: .zero, toleranceAfter: .zero)
        }
        guard finished, abs(seconds(player.currentTime()) - targetSeconds) < 0.5 else {
            throw ProbeFailure(phase: name, domain: "AVPlayer", code: 1)
        }
        steps.append(["name": name, "status": "completed", "target_seconds": targetSeconds])
        try startPlayback(player, phase: name)
    }

    private func withDeadline<Value: Sendable>(
        phase: String,
        operation: @escaping @Sendable () async throws -> Value
    ) async throws -> Value {
        try checkDeadline(phase: phase)
        let remaining = TimeInterval(end - DispatchTime.now().uptimeNanoseconds) / 1_000_000_000
        return try await withThrowingTaskGroup(of: Value.self) { group in
            group.addTask {
                try await operation()
            }
            group.addTask {
                try await Task.sleep(for: .seconds(remaining))
                throw ProbeFailure(phase: phase, domain: "ProbeDeadline", code: 1)
            }
            defer { group.cancelAll() }
            guard let value = try await group.next() else {
                throw ProbeFailure(phase: phase, domain: "ProbeDeadline", code: 1)
            }
            return value
        }
    }

    private func sampleFrame(phase: String) throws {
        guard item != nil else { return }
        captureAccessLog()
        captureTimeline()
        if options.recoverySnapshotPath != nil && diagnosticWindowActive {
            try observeRecoveryTriggerIfReady()
        }
        let hostTime = CACurrentMediaTime()
        let itemTime = output.itemTime(forHostTime: hostTime)
        guard output.hasNewPixelBuffer(forItemTime: itemTime) else { return }
        var displayTime = CMTime.invalid
        guard let pixelBuffer = output.copyPixelBuffer(forItemTime: itemTime, itemTimeForDisplay: &displayTime),
            CVPixelBufferGetWidth(pixelBuffer) > 0, CVPixelBufferGetHeight(pixelBuffer) > 0,
            CVPixelBufferGetDataSize(pixelBuffer) > 0
        else {
            return
        }
        decodedFrames += 1
        let observedMonotonicSeconds = try? monotonicClockSeconds()
        decodedFrameContinuity.observe(monotonicSeconds: observedMonotonicSeconds)
        let diagnosticFrameAllowed = DiagnosticObservationWindowPolicy.includesFrame(
            diagnostic: options.recoverySnapshotPath != nil,
            active: diagnosticWindowActive,
            started: diagnosticWindowStartedMonotonicSeconds,
            observed: observedMonotonicSeconds
        )
        if options.recoverySnapshotPath != nil && diagnosticFrameAllowed {
            diagnosticWindowContinuity.observe(monotonicSeconds: observedMonotonicSeconds)
        }
        let displaySeconds = CMTimeGetSeconds(displayTime)
        if displaySeconds.isFinite {
            uniqueDisplayTimes.insert(Int64((displaySeconds * 1_000).rounded()))
        }
        let width = CVPixelBufferGetWidth(pixelBuffer)
        let height = CVPixelBufferGetHeight(pixelBuffer)
        lastDecodedWidth = width
        lastDecodedHeight = height
        var sizeObservation: [String: Any] = ["width": width, "height": height]
        let timestampNanoseconds = displaySeconds * 1_000_000_000
        if displaySeconds.isFinite, timestampNanoseconds.isFinite,
            timestampNanoseconds > Double(Int64.min), timestampNanoseconds < Double(Int64.max)
        {
            sizeObservation["display_time_seconds"] = displaySeconds
            let waitingForRecoveryTrigger =
                options.recoverySnapshotPath != nil
                && videoSizeExpectations.currentIndex == 2 && recoveryObservation == nil
            if !waitingForRecoveryTrigger && diagnosticFrameAllowed {
                let expectationMonotonicSeconds: Double?
                if videoSizeExpectations.isComplete {
                    expectationMonotonicSeconds = nil
                } else if let observedMonotonicSeconds {
                    expectationMonotonicSeconds = observedMonotonicSeconds
                } else {
                    expectationMonotonicSeconds = try monotonicClockSeconds()
                }
                videoSizeExpectations.observe(
                    width: width,
                    height: height,
                    timestampNanoseconds: Int64(timestampNanoseconds.rounded()),
                    displayTimeSeconds: displaySeconds,
                    observedElapsedSeconds: TimeInterval(DispatchTime.now().uptimeNanoseconds - start)
                        / 1_000_000_000,
                    observedMonotonicSeconds: expectationMonotonicSeconds
                )
                try publishReadyMarkerIfReady()
            }
        }
        decodedVideoSizeObservations.append(sizeObservation)
        if frameSamples.count < 12 {
            frameSamples.append([
                "phase": phase,
                "display_time_seconds": displaySeconds.isFinite ? displaySeconds : 0,
                "width": width,
                "height": height,
            ])
        }
    }

    private func publishReadyMarkerIfReady() throws {
        guard !readyMarkerPublished, let path = options.readyMarkerPath,
            let first = videoSizeExpectations.progress.first,
            first["matched"] as? Bool == true,
            let samples = first["samples"] as? [[String: Any]], samples.count == 3,
            let timestamp = samples.last?["observed_monotonic_seconds"] as? Double
        else {
            return
        }
        try ReadyMarker.publish(
            path: path,
            size: VideoSize(
                width: first["expected_width"] as? Int ?? 0,
                height: first["expected_height"] as? Int ?? 0
            ),
            monotonicSeconds: timestamp
        )
        readyMarkerPublished = true
    }

    private func captureAccessLog() {
        guard let events = item?.accessLog()?.events else { return }
        for (index, event) in events.prefix(33).enumerated() {
            var observation: [String: Any] = ["event_index": index]
            if event.indicatedBitrate.isFinite, event.indicatedBitrate >= 0 {
                observation["indicated_bitrate"] = event.indicatedBitrate
            }
            if event.observedBitrate.isFinite, event.observedBitrate >= 0 {
                observation["observed_bitrate"] = event.observedBitrate
            }
            if event.numberOfBytesTransferred >= 0 {
                observation["bytes_transferred"] = event.numberOfBytesTransferred
            }
            if event.numberOfStalls >= 0 {
                observation["stalls"] = event.numberOfStalls
            }
            accessLogObservations.upsert(index: index, value: observation)
        }
    }

    private func captureTimeline(force: Bool = false) {
        let elapsed = TimeInterval(DispatchTime.now().uptimeNanoseconds - start) / 1_000_000_000
        guard playbackTimeline.shouldRecord(at: elapsed, force: force),
            let observedMonotonicSeconds = try? monotonicClockSeconds(),
            let player
        else {
            return
        }
        let events = item?.accessLog()?.events ?? []
        let indicatedBitrate = events.reversed().first(where: {
            $0.indicatedBitrate.isFinite && $0.indicatedBitrate >= 0
        })?.indicatedBitrate
        let status: String
        switch player.timeControlStatus {
        case .paused:
            status = "paused"
        case .waitingToPlayAtSpecifiedRate:
            status = "waiting"
        case .playing:
            status = "playing"
        @unknown default:
            status = "other"
        }
        playbackTimeline.record(
            elapsedSeconds: elapsed,
            observedMonotonicSeconds: observedMonotonicSeconds,
            playheadSeconds: CMTimeGetSeconds(player.currentTime()),
            ranges: item?.loadedTimeRanges.map(\.timeRangeValue) ?? [],
            decodedWidth: lastDecodedWidth,
            decodedHeight: lastDecodedHeight,
            timeControlStatus: status,
            indicatedBitrate: indicatedBitrate,
            force: force
        )
    }

    private func startPlayback(_ player: AVPlayer, phase: String) throws {
        try requireMuted(player, phase: phase)
        player.play()
    }

    private func setRate(_ rate: Float, player: AVPlayer, phase: String) throws {
        if rate > 0 {
            try requireMuted(player, phase: phase)
        }
        player.rate = rate
    }

    private func requireMuted(_ player: AVPlayer, phase: String) throws {
        guard player.isMuted else {
            throw ProbeFailure(phase: phase, domain: "AVPlayerMute", code: 1)
        }
    }

    private func checkDeadline(phase: String) throws {
        guard DispatchTime.now().uptimeNanoseconds < end else {
            throw ProbeFailure(phase: phase, domain: "ProbeDeadline", code: 1)
        }
        if let item, item.status == .failed {
            throw ProbeFailure(phase: phase, error: item.error ?? NSError(domain: "AVPlayerItem", code: 1))
        }
    }

    private func seconds(_ time: CMTime) -> Double {
        let value = CMTimeGetSeconds(time)
        return value.isFinite ? value : 0
    }

    private func status(for failure: ProbeFailure) -> String {
        if failure.domain == "ProbePlayback" && failure.code == 1 {
            return "ended_early"
        }
        if failure.domain == "AVFoundationErrorDomain" && failure.code == -11828 {
            return "unsupported"
        }
        return "failed"
    }

    private func result(status: String, error: ProbeFailure?) -> [String: Any] {
        var value: [String: Any] = [
            "schema_version": 1,
            "status": status,
            "requested_duration_seconds": options.duration,
            "overall_deadline_seconds": options.deadline,
            "deadline_mode": "cooperative",
            "ready_marker_published": readyMarkerPublished,
            "elapsed_seconds": TimeInterval(DispatchTime.now().uptimeNanoseconds - start) / 1_000_000_000,
            "decoded_frame_count": decodedFrames,
            "decoded_frame_continuity": decodedFrameContinuity.json,
            "diagnostic_observation_window": diagnosticObservationWindow as Any? ?? NSNull(),
            "unique_display_time_count": uniqueDisplayTimes.count,
            "video_track_present": videoTrackPresent,
            "video_evidence_present": decodedFrames > 0,
            "video_evidence_source": decodedFrames > 0
                ? "decoded_pixel_buffer"
                : enabledVideoTrackCount > 0 ? "enabled_item_track_metadata_only" : "none",
            "audio_track_present": audioTrackPresent,
            "audio_track_evidence_source": audioTrackEvidenceSource,
            "audio_evidence_lookup_error": audioEvidenceLookupError as Any? ?? NSNull(),
            "item_status_at_metadata_capture": itemReadyAtMetadataCapture ? "readyToPlay" as Any : NSNull(),
            "item_video_track_count": videoTrackCount,
            "item_audio_track_count": audioTrackCount,
            "enabled_video_track_count": enabledVideoTrackCount,
            "enabled_audio_track_count": enabledAudioTrackCount,
            "item_duration_seconds": itemDurationSeconds.map { $0 as Any } ?? NSNull(),
            "presentation_width": presentationWidth,
            "presentation_height": presentationHeight,
            "frame_samples": frameSamples,
            "decoded_video_sizes": [
                "observations": decodedVideoSizeObservations.observations,
                "truncated": decodedVideoSizeObservations.truncated,
            ],
            "video_size_expectations": [
                "enabled": !options.expectedVideoSizes.isEmpty,
                "complete": options.expectedVideoSizes.isEmpty ? NSNull() : videoSizeExpectations.isComplete as Any,
                "semantics": "ordered_decoded_size_subsequence",
                "minimum_distinct_display_timestamps_per_size": 3,
                "entries": options.recoverySnapshotPath == nil
                    ? videoSizeExpectations.progress
                    : videoSizeExpectations.diagnosticProgress,
            ],
            "access_log_observations": [
                "events": accessLogObservations.observations,
                "truncated": accessLogObservations.truncated,
            ],
            "playback_timeline": playbackTimeline.json,
            "steps": steps,
            "final_player_state": finalPlayerState as Any? ?? NSNull(),
            "recovery_observation": recoveryObservation as Any? ?? NSNull(),
            "steady_play_end_observation": steadyPlayEndObservation as Any? ?? NSNull(),
            "final_player_error_log": [
                "events": finalPlayerErrorLog,
                "truncated": finalPlayerErrorLogTruncated,
            ],
        ]
        if let error {
            value["error"] = ["phase": error.phase, "domain": error.safeDomain, "code": error.code]
        }
        if options.recoverySnapshotPath != nil {
            let recoveredHighMatched = videoSizeExpectations.isComplete
            let recoveredHighCount =
                recoveredHighMatched
                ? videoSizeExpectations.progress.last?["distinct_frame_count"] as? Int ?? 3
                : 0
            value["diagnostic_optional_recovery_high"] = true
            value["recovered_high_distinct_frame_count"] = recoveredHighCount
            value["recovered_high_missing"] = !recoveredHighMatched
        }
        return value
    }

    private func captureFinalPlayerState() {
        captureFinalPlayerErrorLog()
        finalPlayerState = playerStateSnapshot()
    }

    private func observeRecoveryTriggerIfReady() throws {
        guard recoveryObservation == nil, let path = options.recoveryObservationTriggerPath,
            RecoveryObservationTrigger.isFreshEmptyRegularFile(at: path)
        else {
            return
        }
        guard let observation = try playerObservation() else { return }
        recoveryObservation = observation
        if let snapshotPath = options.recoverySnapshotPath {
            guard let player else {
                throw ProbeFailure(phase: "recovery_snapshot", domain: "RecoverySnapshot", code: Int(EINVAL))
            }
            let playheadSeconds = CMTimeGetSeconds(player.currentTime())
            let ranges = item?.loadedTimeRanges.map(\.timeRangeValue) ?? []
            guard
                let loadedEndSeconds = PlaybackTimelineDiagnostics.loadedEndSeconds(
                    playheadSeconds: playheadSeconds,
                    ranges: ranges
                )
            else {
                throw ProbeFailure(phase: "recovery_snapshot", domain: "RecoverySnapshot", code: Int(EINVAL))
            }
            try RecoverySnapshot.publish(
                path: snapshotPath,
                observedMonotonicSeconds: try monotonicClockSeconds(),
                playheadSeconds: playheadSeconds,
                loadedEndSeconds: loadedEndSeconds
            )
        }
        captureTimeline(force: true)
    }

    private func playerObservation() throws -> [String: Any]? {
        guard let snapshot = playerStateSnapshot() else { return nil }
        return [
            "observed_monotonic_seconds": try monotonicClockSeconds(),
            "player_state": snapshot,
        ]
    }

    private func playerStateSnapshot() -> [String: Any]? {
        guard let player else { return nil }

        let itemStatus: String
        switch item?.status {
        case .unknown:
            itemStatus = "unknown"
        case .readyToPlay:
            itemStatus = "ready_to_play"
        case .failed:
            itemStatus = "failed"
        case nil:
            itemStatus = "no_item"
        @unknown default:
            itemStatus = "other"
        }

        let timeControlStatus: String
        switch player.timeControlStatus {
        case .paused:
            timeControlStatus = "paused"
        case .waitingToPlayAtSpecifiedRate:
            timeControlStatus = "waiting"
        case .playing:
            timeControlStatus = "playing"
        @unknown default:
            timeControlStatus = "other"
        }

        let waitingReason: String
        switch player.reasonForWaitingToPlay {
        case .evaluatingBufferingRate:
            waitingReason = "evaluating_buffering_rate"
        case .toMinimizeStalls:
            waitingReason = "minimize_stalls"
        case .noItemToPlay:
            waitingReason = "no_item"
        case nil:
            waitingReason = "none"
        default:
            waitingReason = "other"
        }

        let rate = player.rate
        let playheadSeconds = CMTimeGetSeconds(player.currentTime())
        let preferredForwardBufferDuration = player.currentItem.map { $0.preferredForwardBufferDuration }
        let loadedTimeRanges = item.map {
            LoadedTimeRangeDiagnostics.snapshot($0.loadedTimeRanges.map(\.timeRangeValue))
        }
        return [
            "automatically_waits_to_minimize_stalling": player.automaticallyWaitsToMinimizeStalling,
            "item_status": itemStatus,
            "time_control_status": timeControlStatus,
            "rate": rate.isFinite ? rate as Any : NSNull(),
            "buffer_empty": item?.isPlaybackBufferEmpty as Any? ?? NSNull(),
            "likely_to_keep_up": item?.isPlaybackLikelyToKeepUp as Any? ?? NSNull(),
            "buffer_full": item?.isPlaybackBufferFull as Any? ?? NSNull(),
            "playhead_seconds": playheadSeconds.isFinite && playheadSeconds >= 0 ? playheadSeconds as Any : NSNull(),
            "preferred_forward_buffer_duration_seconds": preferredForwardBufferDuration.map {
                $0.isFinite && $0 >= 0 ? $0 as Any : NSNull()
            } ?? NSNull(),
            "loaded_time_ranges": loadedTimeRanges as Any? ?? NSNull(),
            "waiting_reason": waitingReason,
        ]
    }

    private func captureFinalPlayerErrorLog() {
        guard let events = item?.errorLog()?.events else { return }
        let boundedEvents = events.prefix(PlayerErrorLogDiagnostics.maximumEvents + 1).map {
            (statusCode: $0.errorStatusCode, domain: $0.errorDomain)
        }
        let snapshot = PlayerErrorLogDiagnostics.snapshot(boundedEvents)
        finalPlayerErrorLog = snapshot.observations
        finalPlayerErrorLogTruncated = snapshot.truncated || events.count > boundedEvents.count
    }

    private func cleanup() {
        player?.pause()
        if let item {
            item.remove(output)
        }
        if let endObserver {
            NotificationCenter.default.removeObserver(endObserver)
        }
        player?.replaceCurrentItem(with: nil)
        item = nil
        player = nil
    }
}

private func monotonicClockSeconds() throws -> Double {
    var value = timespec()
    guard clock_gettime(CLOCK_MONOTONIC, &value) == 0,
        value.tv_sec >= 0, (0..<1_000_000_000).contains(value.tv_nsec)
    else {
        throw ProbeFailure(phase: "video_size_expectations", domain: "NativeMonotonicClock", code: Int(errno))
    }
    let seconds = Double(value.tv_sec) + Double(value.tv_nsec) / 1_000_000_000
    guard seconds.isFinite, seconds > 0 else {
        throw ProbeFailure(phase: "video_size_expectations", domain: "NativeMonotonicClock", code: 1)
    }
    return seconds
}

@main
private struct LANPlaybackProbe {
    @MainActor
    static func main() async {
        var exitCode: Int32 = 64
        var payload: [String: Any] = [:]
        do {
            let arguments = Array(CommandLine.arguments.dropFirst())
            if arguments == ["--self-test"] {
                exitCode = 0
                payload = try ProbeOptions.runSelfTest()
            } else {
                let options = try ProbeOptions.parse(arguments)
                if options.validateOnly {
                    exitCode = 0
                    let expectationTracker = VideoSizeExpectationTracker(expectedSizes: options.expectedVideoSizes)
                    payload = [
                        "schema_version": 1,
                        "status": "validated",
                        "requested_duration_seconds": options.duration,
                        "overall_deadline_seconds": options.deadline,
                        "decoded_frame_continuity": DecodedFrameContinuity().json,
                        "decoded_video_sizes": ["observations": [[String: Any]](), "truncated": false],
                        "video_size_expectations": [
                            "enabled": !options.expectedVideoSizes.isEmpty,
                            "complete": NSNull(),
                            "semantics": "ordered_decoded_size_subsequence",
                            "minimum_distinct_display_timestamps_per_size": 3,
                            "entries": expectationTracker.progress,
                        ],
                        "access_log_observations": ["events": [[String: Any]](), "truncated": false],
                    ]
                    if options.recoverySnapshotPath != nil {
                        payload["diagnostic_optional_recovery_high"] = true
                        payload["recovered_high_distinct_frame_count"] = 0
                        payload["recovered_high_missing"] = true
                    }
                } else {
                    (exitCode, payload) = await PlaybackProbe(options: options).run()
                }
            }
        } catch let failure as ProbeFailure {
            exitCode = CommandLine.arguments.dropFirst().first == "--self-test" ? 1 : 64
            payload = [
                "schema_version": 1,
                "status": "failed",
                "elapsed_seconds": 0,
                "error": ["phase": failure.phase, "domain": failure.safeDomain, "code": failure.code],
            ]
        } catch {
            exitCode = 64
            let failure = ProbeFailure(phase: "cli", error: error)
            payload = [
                "schema_version": 1,
                "status": "failed",
                "elapsed_seconds": 0,
                "error": ["phase": failure.phase, "domain": failure.safeDomain, "code": failure.code],
            ]
        }
        do {
            let data = try JSONSerialization.data(withJSONObject: payload, options: [.sortedKeys, .fragmentsAllowed])
            FileHandle.standardOutput.write(data)
            FileHandle.standardOutput.write(Data([0x0A]))
        } catch {
            FileHandle.standardError.write(
                Data(
                    "{\"status\":\"failed\",\"error\":{\"phase\":\"json\",\"domain\":\"Foundation\",\"code\":1}}\n".utf8
                ))
            exit(EXIT_FAILURE)
        }
        exit(exitCode)
    }
}
