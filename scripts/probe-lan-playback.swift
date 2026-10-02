import AVFoundation
import CoreVideo
import Darwin
import Foundation
import QuartzCore

private struct ProbeOptions {
    let url: URL
    let duration: TimeInterval
    let deadline: TimeInterval
    let validateOnly: Bool

    static func parse(_ arguments: [String]) throws -> ProbeOptions {
        var urlValue: String?
        var duration = 60.0
        var deadline = 120.0
        var validateOnly = false
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

        return ProbeOptions(url: url, duration: duration, deadline: deadline, validateOnly: validateOnly)
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

        passed.append(contentsOf: try PlaybackPolicy.runSelfTest())
        return ["schema_version": 1, "status": "complete", "passed": passed.count, "cases": passed]
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
    private let start = DispatchTime.now().uptimeNanoseconds
    private var end: UInt64 { start + UInt64(options.deadline * 1_000_000_000) }
    private let output = AVPlayerItemVideoOutput(pixelBufferAttributes: [
        kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA
    ])
    private var player: AVPlayer?
    private var item: AVPlayerItem?
    private var decodedFrames = 0
    private var uniqueDisplayTimes = Set<Int64>()
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
    private var endedEarly = false
    private var endObserver: NSObjectProtocol?

    init(options: ProbeOptions) {
        self.options = options
    }

    func run() async -> (Int32, [String: Any]) {
        do {
            let status = try await execute()
            cleanup()
            return (status == "completed" ? 0 : 3, result(status: status, error: nil))
        } catch let failure as ProbeFailure {
            cleanup()
            return (2, result(status: status(for: failure), error: failure))
        } catch {
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
        avPlayer.automaticallyWaitsToMinimizeStalling = false
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
        avPlayer.play()
        try await activePhase("play", seconds: min(8, options.duration * 0.2))

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

        avPlayer.play()
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

        avPlayer.rate = 1.25
        try await activePhase("rate_1_25", seconds: 3, expectedRate: 1.25)
        avPlayer.rate = 1

        let elapsed = TimeInterval(DispatchTime.now().uptimeNanoseconds - playbackStart) / 1_000_000_000
        if elapsed < options.duration {
            avPlayer.play()
            try await activePhase("duration_remainder", seconds: options.duration - elapsed)
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
        player.play()
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
        let displaySeconds = CMTimeGetSeconds(displayTime)
        if displaySeconds.isFinite {
            uniqueDisplayTimes.insert(Int64((displaySeconds * 1_000).rounded()))
        }
        if frameSamples.count < 12 {
            frameSamples.append([
                "phase": phase,
                "display_time_seconds": displaySeconds.isFinite ? displaySeconds : 0,
                "width": CVPixelBufferGetWidth(pixelBuffer),
                "height": CVPixelBufferGetHeight(pixelBuffer),
            ])
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
            "elapsed_seconds": TimeInterval(DispatchTime.now().uptimeNanoseconds - start) / 1_000_000_000,
            "decoded_frame_count": decodedFrames,
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
            "steps": steps,
        ]
        if let error {
            value["error"] = ["phase": error.phase, "domain": error.safeDomain, "code": error.code]
        }
        return value
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
                    payload = [
                        "schema_version": 1,
                        "status": "validated",
                        "requested_duration_seconds": options.duration,
                        "overall_deadline_seconds": options.deadline,
                    ]
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
