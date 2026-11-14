import AVKit
import Darwin
import Foundation
import SwiftUI

struct NativeNavigationProbeConfiguration: Equatable {
    let url: URL
    let runID: String
}

enum NativeNavigationProbeLaunch: Equatable {
    case production
    case diagnostic(NativeNavigationProbeConfiguration)
    case invalid(String)

    static func parse(arguments: [String], isSimulator: Bool) -> Self {
        let args = Array(arguments.dropFirst())
        let modeFlag = "--native-navigation-probe"
        let urlFlag = "--native-navigation-url"
        let runIDFlag = "--native-navigation-run-id"
        let hasDiagnosticArgument = args.contains { $0.hasPrefix("--native-navigation-") }
        guard hasDiagnosticArgument else { return .production }
        guard args.filter({ $0 == modeFlag }).count == 1 else {
            return .invalid("Diagnostic launch requires exactly one --native-navigation-probe flag.")
        }

        var values: [String: String] = [:]
        var index = 0
        while index < args.count {
            let key = args[index]
            guard key == modeFlag || key == urlFlag || key == runIDFlag else {
                return .invalid("Unsupported argument in native-navigation diagnostic launch.")
            }
            if key == modeFlag {
                index += 1
                continue
            }
            guard values[key] == nil, index + 1 < args.count else {
                return .invalid("Missing or duplicate value for \(key).")
            }
            values[key] = args[index + 1]
            index += 2
        }

        guard let rawURL = values[urlFlag], let url = validatedFixtureURL(rawURL) else {
            return .invalid(
                "Native-navigation URL must be an unauthenticated http://127.0.0.1:<port>/master.m3u8 URL without query or fragment."
            )
        }
        guard let runID = values[runIDFlag], isValidRunID(runID) else {
            return .invalid(
                "Native-navigation run ID must contain 1-64 ASCII letters, digits, underscores, or hyphens.")
        }
        guard isSimulator else {
            return .invalid("Native-navigation diagnostic mode is available only in a tvOS Simulator build.")
        }
        return .diagnostic(NativeNavigationProbeConfiguration(url: url, runID: runID))
    }

    private static func validatedFixtureURL(_ rawValue: String) -> URL? {
        guard let components = URLComponents(string: rawValue),
            components.scheme == "http",
            components.host == "127.0.0.1",
            let port = components.port, (1...65_535).contains(port),
            components.user == nil,
            components.password == nil,
            components.percentEncodedPath == "/master.m3u8",
            components.query == nil,
            components.fragment == nil,
            let url = components.url
        else {
            return nil
        }
        return url
    }

    private static func isValidRunID(_ value: String) -> Bool {
        !value.isEmpty && value.utf8.count <= 64
            && value.utf8.allSatisfy {
                (0x30...0x39).contains($0) || (0x41...0x5A).contains($0) || (0x61...0x7A).contains($0) || $0 == 0x5F
                    || $0 == 0x2D
            }
    }
}

enum NativeNavigationClockSnapshot: Equatable {
    case value(UInt64)
    case unavailable
}

enum NativeNavigationClock {
    static func capture() -> NativeNavigationClockSnapshot {
        var value = timespec()
        guard clock_gettime(CLOCK_MONOTONIC_RAW, &value) == 0,
            let nanoseconds = nanoseconds(
                seconds: Int64(value.tv_sec),
                subsecondNanoseconds: Int64(value.tv_nsec)
            )
        else {
            return .unavailable
        }
        return .value(nanoseconds)
    }

    static func nanoseconds(seconds: Int64, subsecondNanoseconds: Int64) -> UInt64? {
        guard seconds > 0, (0..<1_000_000_000).contains(subsecondNanoseconds) else { return nil }
        let (wholeSeconds, multiplicationOverflow) = UInt64(seconds).multipliedReportingOverflow(by: 1_000_000_000)
        guard !multiplicationOverflow else { return nil }
        let (result, additionOverflow) = wholeSeconds.addingReportingOverflow(UInt64(subsecondNanoseconds))
        guard !additionOverflow, result > 0 else { return nil }
        return result
    }
}

private struct NativeNavigationEvent: Encodable {
    let schema_version = 1
    let kind: String
    let clock_ns: UInt64
    let clock_domain = "darwin:CLOCK_MONOTONIC_RAW:nanoseconds"
    let run_id: String
    let action: String?
    let source_position_seconds: Double?
    let target_position_seconds: Double?
    let playback_state: String?
    let observation_point: String?
}

@MainActor
final class NativeNavigationProbe: ObservableObject {
    static let maximumEvents = 4_096
    static let maximumTraceBytes = 8 * 1_024 * 1_024
    static let maximumLifetimeSeconds: TimeInterval = 600

    static func safeTraceFilenameComponent(runID: String) -> String {
        String(
            decoding: runID.utf8.prefix(64).map { byte in
                (0x30...0x39).contains(byte) || (0x41...0x5A).contains(byte)
                    || (0x61...0x7A).contains(byte) || byte == 0x5F || byte == 0x2D ? byte : 0x5F
            },
            as: UTF8.self
        )
    }

    @Published private(set) var status = "Preparing diagnostic trace"
    @Published private(set) var tracePath: String?

    let player: AVPlayer?

    private let configuration: NativeNavigationProbeConfiguration
    private let traceHandle: FileHandle?
    private var traceBytes = 0
    private var eventCount = 0
    private var timeJumpObserver: NSObjectProtocol?
    private var playbackObservation: NSKeyValueObservation?
    private var lifetimeTimer: DispatchWorkItem?
    private var isFinished = false

    init(configuration: NativeNavigationProbeConfiguration) {
        self.configuration = configuration
        let trace = Self.createTrace(runID: configuration.runID)
        tracePath = trace.path
        traceHandle = trace.handle
        if let error = trace.error {
            status = "Trace creation failed: \(error)"
            player = nil
            return
        }

        let item = AVPlayerItem(url: configuration.url)
        let player = AVPlayer(playerItem: item)
        player.isMuted = true
        self.player = player
        status = "Diagnostic ready; player muted"
        append(kind: "start")
        print("native-navigation-ready run_id=\(configuration.runID) trace=\(trace.path ?? "unavailable")")

        timeJumpObserver = NotificationCenter.default.addObserver(
            forName: AVPlayerItem.timeJumpedNotification,
            object: item,
            queue: .main
        ) { [weak self] _ in
            let clock = NativeNavigationClock.capture()
            let currentTime = item.currentTime()
            Task { @MainActor in
                self?.append(
                    kind: "time_jump",
                    clock: clock,
                    sourcePosition: currentTime,
                    observationPoint: "main_notification_delivery"
                )
            }
        }
        playbackObservation = player.observe(\.timeControlStatus, options: [.initial, .new]) {
            [weak self] observedPlayer, _ in
            let clock = NativeNavigationClock.capture()
            let currentTime = observedPlayer.currentTime()
            let state: String
            switch observedPlayer.timeControlStatus {
            case .paused: state = "paused"
            case .waitingToPlayAtSpecifiedRate: state = "waiting"
            case .playing: state = "playing"
            @unknown default: state = "unknown"
            }
            Task { @MainActor in
                self?.append(
                    kind: "playback_state",
                    clock: clock,
                    sourcePosition: currentTime,
                    playbackState: state,
                    observationPoint: "kvo_callback_entry"
                )
            }
        }

        let timer = DispatchWorkItem { [weak self] in
            Task { @MainActor in
                self?.finish(reason: "lifetime_limit")
            }
        }
        lifetimeTimer = timer
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.maximumLifetimeSeconds, execute: timer)
    }

    func startPlayback() {
        guard !isFinished, let player else { return }
        player.play()
    }

    func recordNavigation(
        action: String,
        from source: CMTime,
        to target: CMTime,
        clock: NativeNavigationClockSnapshot
    ) {
        append(
            kind: "input",
            clock: clock,
            action: action,
            sourcePosition: source,
            targetPosition: target,
            observationPoint: "delegate_entry"
        )
    }

    func stop() {
        finish(reason: "view_disappeared")
    }

    private func finish(reason: String) {
        guard !isFinished else { return }
        isFinished = true
        append(kind: "finish", action: reason)
        cleanup()
        status = "Diagnostic finished (\(reason))"
    }

    private func cleanup() {
        lifetimeTimer?.cancel()
        lifetimeTimer = nil
        if let timeJumpObserver {
            NotificationCenter.default.removeObserver(timeJumpObserver)
            self.timeJumpObserver = nil
        }
        playbackObservation?.invalidate()
        playbackObservation = nil
        player?.pause()
        try? traceHandle?.close()
    }

    private func append(
        kind: String,
        clock: NativeNavigationClockSnapshot = NativeNavigationClock.capture(),
        action: String? = nil,
        sourcePosition: CMTime? = nil,
        targetPosition: CMTime? = nil,
        playbackState: String? = nil,
        observationPoint: String? = nil
    ) {
        guard !isFinished || kind == "finish" else { return }
        guard case let .value(clockNS) = clock else {
            status = "Clock unavailable or invalid; diagnostic stopped without recording this event"
            isFinished = true
            cleanup()
            return
        }
        guard let traceHandle else {
            status = "Trace unavailable; diagnostic events are not being recorded"
            return
        }
        guard eventCount < Self.maximumEvents else {
            status = "Trace event limit reached; diagnostic stopped recording"
            finish(reason: "event_limit")
            return
        }

        let event = NativeNavigationEvent(
            kind: kind,
            clock_ns: clockNS,
            run_id: configuration.runID,
            action: action,
            source_position_seconds: Self.finiteSeconds(sourcePosition),
            target_position_seconds: Self.finiteSeconds(targetPosition),
            playback_state: playbackState,
            observation_point: observationPoint
        )
        do {
            var line = try JSONEncoder().encode(event)
            line.append(0x0A)
            guard traceBytes + line.count <= Self.maximumTraceBytes else {
                status = "Trace byte limit reached; diagnostic stopped recording"
                finish(reason: "byte_limit")
                return
            }
            try traceHandle.write(contentsOf: line)
            traceBytes += line.count
            eventCount += 1
        } catch {
            status = "Trace write failed: \(error.localizedDescription)"
            finish(reason: "trace_error")
        }
    }

    private static func finiteSeconds(_ time: CMTime?) -> Double? {
        guard let time else { return nil }
        let seconds = CMTimeGetSeconds(time)
        return seconds.isFinite ? seconds : nil
    }

    private static func createTrace(runID: String) -> (path: String?, handle: FileHandle?, error: String?) {
        do {
            let cacheDirectory = try FileManager.default.url(
                for: .cachesDirectory,
                in: .userDomainMask,
                appropriateFor: nil,
                create: true
            ).appendingPathComponent("NativeNavigationProbe", isDirectory: true)
            try FileManager.default.createDirectory(at: cacheDirectory, withIntermediateDirectories: true)
            let safeRunID = safeTraceFilenameComponent(runID: runID)
            let url = cacheDirectory.appendingPathComponent("\(safeRunID)-\(UUID().uuidString).jsonl")
            guard FileManager.default.createFile(atPath: url.path, contents: nil),
                let handle = try? FileHandle(forWritingTo: url)
            else {
                return (nil, nil, "could not create trace file")
            }
            return (url.path, handle, nil)
        } catch {
            return (nil, nil, error.localizedDescription)
        }
    }
}

struct NativeNavigationProbeRoot: View {
    @StateObject private var probe: NativeNavigationProbe

    init(configuration: NativeNavigationProbeConfiguration) {
        _probe = StateObject(wrappedValue: NativeNavigationProbe(configuration: configuration))
    }

    var body: some View {
        ZStack(alignment: .topLeading) {
            NativeAVPlayerController(probe: probe)
                .ignoresSafeArea()
            VStack(alignment: .leading, spacing: 8) {
                Text("Native Navigation Diagnostic")
                    .font(.headline)
                Text(probe.status)
                if let tracePath = probe.tracePath {
                    Text(tracePath)
                        .font(.caption)
                        .lineLimit(2)
                }
            }
            .padding(24)
            .background(.black.opacity(0.78))
        }
        .onAppear(perform: probe.startPlayback)
        .onDisappear(perform: probe.stop)
    }
}

struct NativeAVPlayerController: UIViewControllerRepresentable {
    let probe: NativeNavigationProbe

    func makeCoordinator() -> Coordinator {
        Coordinator(probe: probe)
    }

    func makeUIViewController(context: Context) -> AVPlayerViewController {
        let controller = AVPlayerViewController()
        controller.player = probe.player
        controller.showsPlaybackControls = true
        controller.delegate = context.coordinator
        return controller
    }

    func updateUIViewController(_ controller: AVPlayerViewController, context: Context) {}

    final class Coordinator: NSObject, AVPlayerViewControllerDelegate {
        private weak var probe: NativeNavigationProbe?

        init(probe: NativeNavigationProbe) {
            self.probe = probe
        }

        func playerViewController(
            _ playerViewController: AVPlayerViewController,
            timeToSeekAfterUserNavigatedFrom oldTime: CMTime,
            to targetTime: CMTime
        ) -> CMTime {
            let clock = NativeNavigationClock.capture()
            let probe = probe
            Task { @MainActor in
                probe?.recordNavigation(action: "seek_target", from: oldTime, to: targetTime, clock: clock)
            }
            return targetTime
        }

        func playerViewController(
            _ playerViewController: AVPlayerViewController,
            willResumePlaybackAfterUserNavigatedFrom oldTime: CMTime,
            to targetTime: CMTime
        ) {
            let clock = NativeNavigationClock.capture()
            let probe = probe
            Task { @MainActor in
                probe?.recordNavigation(
                    action: "resume_after_navigation",
                    from: oldTime,
                    to: targetTime,
                    clock: clock
                )
            }
        }
    }
}

struct NativeNavigationProbeLaunchError: View {
    let message: String

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Native Navigation Diagnostic Unavailable")
                .font(.title2)
            Text(message)
        }
        .padding(48)
    }
}
