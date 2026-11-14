import AppKit
import AVFoundation
import AVKit
import Darwin
import Foundation

private let clockDomain = "darwin:CLOCK_MONOTONIC_RAW:nanoseconds"
private let maxDurationSeconds = 600.0
private let maxEvents = 4096
private let maxOutputBytes = 8 * 1024 * 1024
private let terminalByteReserve = 1024

private enum ProbeFailure: Error, CustomStringConvertible {
    case usage(String)
    case output(String)

    var description: String {
        switch self {
        case .usage(let message), .output(let message):
            return message
        }
    }
}

private enum ProbeMode: String {
    case observe
    case keyboardGate = "keyboard-gate"
}

private struct ProbeOptions {
    let mediaURL: URL
    let eventsURL: URL
    let runID: String
    let duration: Double
    let mode: ProbeMode
    let barrierDelayMilliseconds: Int

    static func parse(_ arguments: [String]) throws -> Invocation {
        if arguments == ["--help"] || arguments == ["-h"] {
            return .help
        }
        if arguments == ["--self-test"] {
            return .selfTest
        }
        if arguments.contains("--self-test") {
            throw ProbeFailure.usage("--self-test must be used by itself")
        }

        var values: [String: String] = [:]
        var index = 0
        while index < arguments.count {
            let name = arguments[index]
            guard ["--url", "--events-output", "--run-id", "--duration", "--mode", "--barrier-delay-ms"].contains(name)
            else {
                throw ProbeFailure.usage("unknown option: \(name)")
            }
            guard index + 1 < arguments.count, !arguments[index + 1].hasPrefix("--") else {
                throw ProbeFailure.usage("missing value for \(name)")
            }
            guard values[name] == nil else {
                throw ProbeFailure.usage("duplicate option: \(name)")
            }
            values[name] = arguments[index + 1]
            index += 2
        }

        guard let rawURL = values["--url"], let mediaURL = validatedFixtureURL(rawURL) else {
            throw ProbeFailure.usage("--url must be http://127.0.0.1:PORT/master.m3u8")
        }
        guard let rawEvents = values["--events-output"], !rawEvents.isEmpty else {
            throw ProbeFailure.usage("--events-output is required")
        }
        let eventsURL = URL(fileURLWithPath: rawEvents).standardizedFileURL
        guard eventsURL.pathExtension.lowercased() == "jsonl" else {
            throw ProbeFailure.usage("--events-output must name a .jsonl file")
        }

        let runID = values["--run-id"] ?? UUID().uuidString.lowercased()
        guard runID.range(of: "^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$", options: .regularExpression) != nil else {
            throw ProbeFailure.usage("--run-id must contain 1 to 64 bounded ASCII identifier characters")
        }

        let duration: Double
        if let rawDuration = values["--duration"] {
            guard let parsedDuration = Double(rawDuration) else {
                throw ProbeFailure.usage("--duration must be a finite number")
            }
            duration = parsedDuration
        } else {
            duration = 60.0
        }
        guard duration.isFinite, duration > 0, duration <= maxDurationSeconds else {
            throw ProbeFailure.usage("--duration must be greater than 0 and no more than 600 seconds")
        }

        let mode = ProbeMode(rawValue: values["--mode"] ?? ProbeMode.observe.rawValue)
        guard let mode else {
            throw ProbeFailure.usage("--mode must be observe or keyboard-gate")
        }
        let delayWasProvided = values["--barrier-delay-ms"] != nil
        guard mode == .keyboardGate || !delayWasProvided else {
            throw ProbeFailure.usage("--barrier-delay-ms is only valid with --mode keyboard-gate")
        }
        let delay = values["--barrier-delay-ms"].flatMap(Int.init) ?? (delayWasProvided ? -1 : 0)
        guard (0...2000).contains(delay) else {
            throw ProbeFailure.usage("--barrier-delay-ms must be from 0 through 2000")
        }

        return .run(
            ProbeOptions(
                mediaURL: mediaURL,
                eventsURL: eventsURL,
                runID: runID,
                duration: duration,
                mode: mode,
                barrierDelayMilliseconds: delay
            ))
    }

    private static func validatedFixtureURL(_ value: String) -> URL? {
        guard let components = URLComponents(string: value),
            components.scheme == "http",
            components.host == "127.0.0.1",
            components.port.map({ (1...65535).contains($0) }) == true,
            components.user == nil,
            components.password == nil,
            components.path == "/master.m3u8",
            components.percentEncodedPath == "/master.m3u8",
            components.query == nil,
            components.fragment == nil
        else {
            return nil
        }
        return components.url
    }
}

private enum Invocation {
    case help
    case selfTest
    case run(ProbeOptions)
}

private enum NavigationKey {
    static func action(keyCode: UInt16, shift: Bool) -> String? {
        switch keyCode {
        case 123: "left"
        case 124: "right"
        case 125: "down"
        case 126: "up"
        case 48: shift ? "shift_tab" : "tab"
        case 36, 76: "enter"
        case 49: "space"
        case 53: "escape"
        default: nil
        }
    }

    static func isGateCandidate(_ action: String) -> Bool {
        ["left", "right", "up", "down", "tab", "shift_tab", "enter"].contains(action)
    }
}

private func rawMonotonicNanoseconds() -> Int64? {
    var value = timespec()
    guard clock_gettime(CLOCK_MONOTONIC_RAW, &value) == 0,
        value.tv_sec >= 0,
        value.tv_nsec >= 0
    else {
        return nil
    }
    let seconds = Int64(value.tv_sec)
    let nanoseconds = Int64(value.tv_nsec)
    let (wholeSeconds, multiplicationOverflow) = seconds.multipliedReportingOverflow(by: 1_000_000_000)
    let (result, additionOverflow) = wholeSeconds.addingReportingOverflow(nanoseconds)
    guard !multiplicationOverflow, !additionOverflow, result > 0 else {
        return nil
    }
    return result
}

private enum EventAppendResult: Equatable {
    case written
    case eventLimit
    case outputLimit
    case clockUnavailable
    case serializationFailure
    case writeFailure
}

private struct EventBudget {
    var eventCount = 0
    var outputBytes = 0

    func rejection(for lineBytes: Int, terminal: Bool) -> EventAppendResult? {
        if eventCount >= maxEvents || (!terminal && eventCount >= maxEvents - 1) {
            return .eventLimit
        }
        let byteLimit = terminal ? maxOutputBytes : maxOutputBytes - terminalByteReserve
        guard lineBytes >= 0, lineBytes <= byteLimit, outputBytes <= byteLimit - lineBytes else {
            return .outputLimit
        }
        return nil
    }

    mutating func account(_ lineBytes: Int) {
        eventCount += 1
        outputBytes += lineBytes
    }
}

private struct PositionSample {
    let seconds: Double?
    let clockNS: Int64
}

@MainActor
private final class BoundedEventWriter {
    private let runID: String
    private var descriptor: Int32
    private var budget = EventBudget()

    init(path: URL, runID: String) throws {
        try FileManager.default.createDirectory(
            at: path.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        let opened = Darwin.open(path.path, O_WRONLY | O_CREAT | O_EXCL, mode_t(S_IRUSR | S_IWUSR))
        guard opened >= 0 else {
            throw ProbeFailure.output("could not create unique events file (errno \(errno))")
        }
        self.descriptor = opened
        self.runID = runID
    }

    func append(kind: String, clockNS: Int64, fields: [String: Any], terminal: Bool = false) -> EventAppendResult {
        guard clockNS > 0 else {
            return .clockUnavailable
        }
        var event: [String: Any] = [
            "schema_version": 1,
            "kind": kind,
            "clock_ns": clockNS,
            "clock_domain": clockDomain,
            "run_id": runID,
        ]
        for (key, value) in fields where event[key] == nil {
            event[key] = value
        }
        guard JSONSerialization.isValidJSONObject(event),
            let encoded = try? JSONSerialization.data(withJSONObject: event, options: [.sortedKeys])
        else {
            return .serializationFailure
        }
        var line = encoded
        line.append(0x0A)
        if let rejection = budget.rejection(for: line.count, terminal: terminal) {
            return rejection
        }
        guard writeAll(line) else {
            return .writeFailure
        }
        budget.account(line.count)
        return .written
    }

    func close() {
        guard descriptor >= 0 else { return }
        _ = Darwin.close(descriptor)
        descriptor = -1
    }

    private func writeAll(_ data: Data) -> Bool {
        guard descriptor >= 0 else { return false }
        return data.withUnsafeBytes { bytes in
            guard let baseAddress = bytes.baseAddress else { return false }
            var offset = 0
            while offset < bytes.count {
                let written = Darwin.write(descriptor, baseAddress.advanced(by: offset), bytes.count - offset)
                if written < 0 {
                    if errno == EINTR { continue }
                    return false
                }
                if written == 0 { return false }
                offset += written
            }
            return true
        }
    }
}

@MainActor
private final class ProbePlayerView: AVPlayerView {
    var onNavigationKey: ((String, Int64) -> Void)?

    override var acceptsFirstResponder: Bool { true }

    override func keyDown(with event: NSEvent) {
        guard let observedAt = rawMonotonicNanoseconds() else {
            onNavigationKey?("clock_unavailable", 0)
            return
        }
        if let action = NavigationKey.action(keyCode: event.keyCode, shift: event.modifierFlags.contains(.shift)) {
            onNavigationKey?(action, observedAt)
        }
        super.keyDown(with: event)
    }
}

@MainActor
private final class ProbeWindow: NSWindow {
    var shouldGateKeyboard = false
    var onGateCandidate: ((NSEvent, String, Int64) -> Void)?

    override func sendEvent(_ event: NSEvent) {
        let observedAt = event.type == .keyDown ? rawMonotonicNanoseconds() : nil
        guard shouldGateKeyboard,
            event.type == .keyDown,
            let action = NavigationKey.action(keyCode: event.keyCode, shift: event.modifierFlags.contains(.shift)),
            NavigationKey.isGateCandidate(action)
        else {
            super.sendEvent(event)
            return
        }
        guard let observedAt else {
            onGateCandidate?(event, "clock_unavailable", 0)
            return
        }
        onGateCandidate?(event, action, observedAt)
    }

    func forwardApprovedKey(_ event: NSEvent) {
        super.sendEvent(event)
    }
}

private final class RejectRedirectsDelegate: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
    func urlSession(
        _ session: URLSession,
        task: URLSessionTask,
        willPerformHTTPRedirection response: HTTPURLResponse,
        newRequest request: URLRequest,
        completionHandler: @escaping (URLRequest?) -> Void
    ) {
        completionHandler(nil)
    }
}

@MainActor
private final class ProbeApplicationDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    private let options: ProbeOptions
    private let writer: BoundedEventWriter
    private let redirectDelegate: RejectRedirectsDelegate
    private let session: URLSession
    private var player: AVPlayer?
    private var item: AVPlayerItem?
    private var playerView: ProbePlayerView?
    private var window: ProbeWindow?
    private var localMonitor: Any?
    private var itemEndObserver: NSObjectProtocol?
    private var timeJumpObserver: NSObjectProtocol?
    private var sampleTimer: Timer?
    private var durationTimer: Timer?
    private var pendingGateTasks: [String: Task<Void, Never>] = [:]
    private var activeGateRequestID: String?
    private var gateGeneration = 0
    private var lastPositionSample: PositionSample?
    private var lastFinitePositionSample: PositionSample?
    private var lastMouseDragClockNS: Int64?
    private var lastPlaybackState: String?
    private var isShuttingDown = false

    init(options: ProbeOptions, writer: BoundedEventWriter) {
        self.options = options
        self.writer = writer
        self.redirectDelegate = RejectRedirectsDelegate()
        let configuration = URLSessionConfiguration.ephemeral
        configuration.connectionProxyDictionary = [:]
        configuration.httpCookieStorage = nil
        configuration.httpShouldSetCookies = false
        configuration.urlCache = nil
        configuration.timeoutIntervalForRequest = 5
        configuration.timeoutIntervalForResource = 7
        configuration.waitsForConnectivity = false
        self.session = URLSession(configuration: configuration, delegate: redirectDelegate, delegateQueue: nil)
        super.init()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        guard let observedAt = rawMonotonicNanoseconds() else {
            stopWithoutClock()
            return
        }

        let newPlayer = AVPlayer()
        newPlayer.isMuted = true
        player = newPlayer
        let newItem = AVPlayerItem(url: options.mediaURL)
        replaceCurrentItem(with: newItem)

        let newView = ProbePlayerView(frame: NSRect(x: 0, y: 0, width: 960, height: 540))
        newView.controlsStyle = .floating
        newView.player = newPlayer
        newView.onNavigationKey = { [weak self] action, clockNS in
            guard action != "clock_unavailable", clockNS > 0 else {
                self?.stopWithoutClock()
                return
            }
            self?.recordWithPosition(
                "input", at: clockNS,
                fields: [
                    "action": action,
                    "stage": "native_player_view",
                ])
        }

        let newWindow = ProbeWindow(
            contentRect: NSRect(x: 0, y: 0, width: 960, height: 540),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        newWindow.title = "Native Navigation Probe"
        newWindow.isReleasedWhenClosed = false
        newWindow.contentView = newView
        newWindow.delegate = self
        newWindow.shouldGateKeyboard = options.mode == .keyboardGate
        newWindow.onGateCandidate = { [weak self] event, action, clockNS in
            guard action != "clock_unavailable", clockNS > 0 else {
                self?.stopWithoutClock()
                return
            }
            self?.handleGateCandidate(event, action: action, observedAt: clockNS)
        }
        playerView = newView
        window = newWindow

        let observedInputTypes: NSEvent.EventTypeMask = [
            .keyDown, .leftMouseDown, .leftMouseUp, .leftMouseDragged,
        ]
        guard
            let monitor = NSEvent.addLocalMonitorForEvents(
                matching: observedInputTypes,
                handler: { [weak self] event in
                    let observedAt = rawMonotonicNanoseconds()
                    MainActor.assumeIsolated {
                        guard let self else { return }
                        guard let observedAt else {
                            self.stopWithoutClock()
                            return
                        }
                        self.observeRawInput(event, at: observedAt)
                    }
                    return event
                })
        else {
            stop(reason: "input_monitor_unavailable", at: observedAt)
            return
        }
        localMonitor = monitor

        itemEndObserver = NotificationCenter.default.addObserver(
            forName: AVPlayerItem.didPlayToEndTimeNotification,
            object: newItem,
            queue: nil
        ) { [weak self] _ in
            let endedAt = rawMonotonicNanoseconds()
            Task { @MainActor [weak self] in
                guard let self else { return }
                guard let endedAt else {
                    self.stopWithoutClock()
                    return
                }
                self.stop(reason: "item_ended", at: endedAt)
            }
        }

        timeJumpObserver = NotificationCenter.default.addObserver(
            forName: AVPlayerItem.timeJumpedNotification,
            object: newItem,
            queue: .main
        ) { [weak self, weak newItem] _ in
            let observedAt = rawMonotonicNanoseconds()
            let positionObservedAt = rawMonotonicNanoseconds()
            let seconds = newItem?.currentTime().seconds
            let position = seconds.flatMap { $0.isFinite && $0 >= 0 ? $0 : nil }
            Task { @MainActor [weak self] in
                guard let self else { return }
                guard !self.isShuttingDown else { return }
                guard let observedAt, observedAt > 0,
                    let positionObservedAt, positionObservedAt > 0
                else {
                    self.stopWithoutClock()
                    return
                }
                self.record(
                    "native_time_jump", at: observedAt,
                    fields: [
                        "source": "AVPlayerItem.timeJumpedNotification",
                        "observation_point": "notification_callback_entry_main_queue",
                        "to_position_seconds": position as Any? ?? NSNull(),
                        "to_position_clock_ns": positionObservedAt,
                        "to_position_offset_ns": positionObservedAt - observedAt,
                        "to_position_timing": "sampled_in_notification_callback_before_actor_hop",
                    ])
            }
        }

        recordWithPosition(
            "start", at: observedAt,
            fields: [
                "mode": options.mode.rawValue,
                "duration_seconds": options.duration,
                "player_muted": true,
                "buffering_policy": "avplayer_default",
                "barrier_delay_ms": options.mode == .keyboardGate
                    ? (options.barrierDelayMilliseconds as Any)
                    : NSNull(),
                "keyboard_gate_coverage": "window_keydown_candidates_only;other_input_paths_unproven",
                "barrier_semantics": "simulated_http_barrier_not_context_close",
            ])
        guard !isShuttingDown else { return }

        newWindow.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        _ = newWindow.makeFirstResponder(newView)
        installTimers()
        newPlayer.play()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }

    func applicationWillTerminate(_ notification: Notification) {
        guard !isShuttingDown else { return }
        guard let observedAt = rawMonotonicNanoseconds() else {
            stopWithoutClock()
            return
        }
        stop(reason: "application_terminated", at: observedAt)
    }

    func windowWillClose(_ notification: Notification) {
        guard !isShuttingDown else { return }
        guard let observedAt = rawMonotonicNanoseconds() else {
            stopWithoutClock()
            return
        }
        stop(reason: "window_closed", at: observedAt)
    }

    private func installTimers() {
        let sampler = Timer(timeInterval: 0.25, repeats: true) { [weak self] _ in
            let observedAt = rawMonotonicNanoseconds()
            MainActor.assumeIsolated {
                guard let self else { return }
                guard let observedAt else {
                    self.stopWithoutClock()
                    return
                }
                self.sample(at: observedAt)
            }
        }
        sampleTimer = sampler
        RunLoop.main.add(sampler, forMode: .common)

        let deadline = Timer(timeInterval: options.duration, repeats: false) { [weak self] _ in
            let observedAt = rawMonotonicNanoseconds()
            MainActor.assumeIsolated {
                guard let self else { return }
                guard let observedAt else {
                    self.stopWithoutClock()
                    return
                }
                self.stop(reason: "duration_elapsed", at: observedAt)
            }
        }
        durationTimer = deadline
        RunLoop.main.add(deadline, forMode: .common)
    }

    private func observeRawInput(_ event: NSEvent, at clockNS: Int64) {
        var action: String
        var fields: [String: Any] = ["stage": "pre_dispatch"]
        switch event.type {
        case .keyDown:
            action =
                NavigationKey.action(keyCode: event.keyCode, shift: event.modifierFlags.contains(.shift))
                ?? "unclassified_key"
            fields["input_kind"] = "keyboard"
            fields["is_repeat"] = event.isARepeat
        case .leftMouseDown:
            action = "mouse_down"
            fields["input_kind"] = "mouse"
        case .leftMouseUp:
            action = "mouse_up"
            fields["input_kind"] = "mouse"
        case .leftMouseDragged:
            if let lastMouseDragClockNS, clockNS - lastMouseDragClockNS < 500_000_000 {
                return
            }
            lastMouseDragClockNS = clockNS
            action = "mouse_drag"
            fields["input_kind"] = "mouse"
        default:
            return
        }
        fields["action"] = action
        recordWithPosition("input", at: clockNS, fields: fields)
    }

    private func handleGateCandidate(_ event: NSEvent, action: String, observedAt: Int64) {
        recordWithPosition(
            "input", at: observedAt,
            fields: [
                "action": action,
                "stage": "window_dispatch",
                "is_repeat": event.isARepeat,
            ])
        guard !isShuttingDown else { return }
        guard activeGateRequestID == nil else {
            recordWithPosition(
                "gate_rejected", at: observedAt,
                fields: [
                    "action": action,
                    "reason": "gate_inflight",
                ])
            return
        }

        let requestID = UUID().uuidString.lowercased()
        var components = URLComponents(url: options.mediaURL, resolvingAgainstBaseURL: false)
        components?.path = "/barrier"
        guard let barrierURL = components?.url else {
            recordWithPosition(
                "gate_rejected", at: observedAt,
                fields: [
                    "action": action,
                    "reason": "barrier_url_invalid",
                ])
            return
        }
        let payload: [String: Any] = [
            "request_id": requestID,
            "delay_ms": options.barrierDelayMilliseconds,
        ]
        guard let body = try? JSONSerialization.data(withJSONObject: payload, options: [.sortedKeys]) else {
            recordWithPosition(
                "gate_rejected", at: observedAt,
                fields: [
                    "action": action,
                    "reason": "request_encoding_failed",
                ])
            return
        }

        var request = URLRequest(url: barrierURL, timeoutInterval: 5)
        request.httpMethod = "POST"
        request.httpBody = body
        request.httpShouldHandleCookies = false
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        activeGateRequestID = requestID
        let generation = gateGeneration
        recordWithPosition(
            "gate_requested", at: observedAt,
            fields: [
                "request_id": requestID,
                "action": action,
                "delay_ms": options.barrierDelayMilliseconds,
            ])
        guard !isShuttingDown else { return }

        let task = Task { [weak self] in
            guard let self else { return }
            do {
                let (data, response) = try await self.session.data(for: request)
                let responseObservedAt = rawMonotonicNanoseconds()
                guard self.isCurrentGate(requestID, generation: generation) else { return }
                guard let responseObservedAt else {
                    self.stopWithoutClock()
                    return
                }
                self.pendingGateTasks.removeValue(forKey: requestID)
                self.activeGateRequestID = nil
                guard let http = response as? HTTPURLResponse, http.statusCode == 200 else {
                    self.rejectGate(requestID, action: action, reason: "barrier_http_status", at: responseObservedAt)
                    return
                }
                guard let body = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                    body["request_id"] as? String == requestID,
                    body["outcome"] as? String == "acknowledged"
                else {
                    self.rejectGate(requestID, action: action, reason: "barrier_ack_mismatch", at: responseObservedAt)
                    return
                }
                self.recordWithPosition(
                    "gate_released", at: responseObservedAt,
                    fields: [
                        "request_id": requestID,
                        "action": action,
                        "outcome": "acknowledged",
                    ])
                guard !self.isShuttingDown else { return }
                self.window?.forwardApprovedKey(event)
            } catch {
                let failureObservedAt = rawMonotonicNanoseconds()
                guard self.isCurrentGate(requestID, generation: generation) else { return }
                guard let failureObservedAt else {
                    self.stopWithoutClock()
                    return
                }
                self.pendingGateTasks.removeValue(forKey: requestID)
                self.activeGateRequestID = nil
                self.rejectGate(requestID, action: action, reason: "barrier_unavailable", at: failureObservedAt)
            }
        }
        pendingGateTasks[requestID] = task
    }

    private func rejectGate(_ requestID: String, action: String, reason: String, at clockNS: Int64) {
        recordWithPosition(
            "gate_rejected", at: clockNS,
            fields: [
                "request_id": requestID,
                "action": action,
                "reason": reason,
            ])
    }

    private func isCurrentGate(_ requestID: String, generation: Int) -> Bool {
        !isShuttingDown && gateGeneration == generation && activeGateRequestID == requestID
    }

    private func replaceCurrentItem(with newItem: AVPlayerItem) {
        invalidatePendingGates()
        item = newItem
        player?.replaceCurrentItem(with: newItem)
    }

    private func invalidatePendingGates() {
        gateGeneration += 1
        for task in pendingGateTasks.values {
            task.cancel()
        }
        pendingGateTasks.removeAll()
        activeGateRequestID = nil
    }

    private func sample(at clockNS: Int64) {
        guard !isShuttingDown, let player else { return }
        guard let positionClockNS = rawMonotonicNanoseconds() else {
            stopWithoutClock()
            return
        }
        let position = finitePlayerPosition(player)
        let currentSample = PositionSample(seconds: position, clockNS: positionClockNS)
        lastPositionSample = currentSample
        if let position, let previousSample = lastFinitePositionSample,
            let previousPosition = previousSample.seconds
        {
            let elapsed = Double(positionClockNS - previousSample.clockNS) / 1_000_000_000
            let delta = position - previousPosition
            if delta < -0.75 || delta > max(2.0, elapsed * 4.0 + 0.75) {
                recordWithPosition(
                    "time_jump", at: clockNS,
                    fields: [
                        "from_position_seconds": previousPosition,
                        "to_position_seconds": position,
                        "delta_seconds": delta,
                        "source": "periodic_position_observation",
                    ])
            }
            lastFinitePositionSample = currentSample
        } else if position != nil {
            lastFinitePositionSample = currentSample
        }
        let state = playbackState(player)
        let itemState = itemStatus(item)
        let combinedState = "\(state):\(itemState)"
        if combinedState != lastPlaybackState {
            lastPlaybackState = combinedState
            recordWithPosition(
                "playback_state", at: clockNS,
                fields: [
                    "time_control": state,
                    "item_status": itemState,
                ])
        }
    }

    private func positionFields(at eventClockNS: Int64) -> [String: Any] {
        guard let sample = lastPositionSample else {
            return [
                "player_position_seconds": NSNull(),
                "player_position_clock_ns": NSNull(),
                "player_position_timing": "not_sampled_yet",
            ]
        }
        let position: Any = sample.seconds.map { $0 as Any } ?? NSNull()
        return [
            "player_position_seconds": position,
            "player_position_clock_ns": sample.clockNS,
            "player_position_offset_ns": sample.clockNS - eventClockNS,
            "player_position_timing": "separate_periodic_sample",
        ]
    }

    private func recordWithPosition(_ kind: String, at clockNS: Int64, fields: [String: Any]) {
        var observedFields = fields
        observedFields.merge(positionFields(at: clockNS)) { _, positionValue in positionValue }
        record(kind, at: clockNS, fields: observedFields)
    }

    private func finitePlayerPosition(_ player: AVPlayer) -> Double? {
        let seconds = player.currentTime().seconds
        return seconds.isFinite && seconds >= 0 ? seconds : nil
    }

    private func playbackState(_ player: AVPlayer) -> String {
        switch player.timeControlStatus {
        case .paused: "paused"
        case .waitingToPlayAtSpecifiedRate: "waiting"
        case .playing: "playing"
        @unknown default: "unknown"
        }
    }

    private func itemStatus(_ item: AVPlayerItem?) -> String {
        guard let item else { return "absent" }
        return switch item.status {
        case .unknown: "unknown"
        case .readyToPlay: "ready"
        case .failed: "failed"
        @unknown default: "unknown"
        }
    }

    private func record(_ kind: String, at clockNS: Int64, fields: [String: Any]) {
        switch writer.append(kind: kind, clockNS: clockNS, fields: fields) {
        case .written:
            break
        case .eventLimit:
            stop(reason: "event_limit", at: clockNS)
        case .outputLimit:
            stop(reason: "output_limit", at: clockNS)
        case .clockUnavailable:
            stopWithoutClock()
        case .serializationFailure, .writeFailure:
            report("event output failed; diagnostic stopped")
            stopWithoutClock()
        }
    }

    private func stop(reason: String, at clockNS: Int64) {
        guard !isShuttingDown else { return }
        isShuttingDown = true
        cleanup()
        var finishFields: [String: Any] = ["reason": reason]
        finishFields.merge(positionFields(at: clockNS)) { _, positionValue in positionValue }
        let result = writer.append(kind: "finish", clockNS: clockNS, fields: finishFields, terminal: true)
        if case .written = result {
        } else {
            report("finish event could not be written")
        }
        writer.close()
        window?.close()
        NSApp.terminate(nil)
    }

    private func stopWithoutClock() {
        guard !isShuttingDown else { return }
        isShuttingDown = true
        report("CLOCK_MONOTONIC_RAW failed or overflowed; diagnostic stopped without a replacement clock")
        cleanup()
        writer.close()
        window?.close()
        NSApp.terminate(nil)
    }

    private func cleanup() {
        sampleTimer?.invalidate()
        durationTimer?.invalidate()
        sampleTimer = nil
        durationTimer = nil
        if let localMonitor {
            NSEvent.removeMonitor(localMonitor)
            self.localMonitor = nil
        }
        if let itemEndObserver {
            NotificationCenter.default.removeObserver(itemEndObserver)
            self.itemEndObserver = nil
        }
        if let timeJumpObserver {
            NotificationCenter.default.removeObserver(timeJumpObserver)
            self.timeJumpObserver = nil
        }
        invalidatePendingGates()
        player?.pause()
        session.invalidateAndCancel()
    }
}

private func report(_ message: String) {
    guard let data = ("native-navigation-probe: \(message)\n").data(using: .utf8) else { return }
    try? FileHandle.standardError.write(contentsOf: data)
}

private func runSelfTests() throws {
    guard let now = rawMonotonicNanoseconds(), now > 0 else {
        throw ProbeFailure.output("CLOCK_MONOTONIC_RAW self-test failed")
    }
    guard NavigationKey.action(keyCode: 123, shift: false) == "left",
        NavigationKey.action(keyCode: 48, shift: true) == "shift_tab",
        NavigationKey.isGateCandidate("enter"),
        !NavigationKey.isGateCandidate("space")
    else {
        throw ProbeFailure.output("navigation policy self-test failed")
    }
    guard
        case .run = try ProbeOptions.parse([
            "--url", "http://127.0.0.1:8123/master.m3u8",
            "--events-output", "trace.jsonl",
        ])
    else {
        throw ProbeFailure.output("default CLI mode self-test failed")
    }
    guard
        case .run(let gateOptions) = try ProbeOptions.parse([
            "--url", "http://127.0.0.1:8123/master.m3u8",
            "--events-output", "trace.jsonl",
            "--run-id", "self-test",
            "--duration", "600",
            "--mode", "keyboard-gate",
            "--barrier-delay-ms", "2000",
        ]),
        gateOptions.mode == .keyboardGate,
        gateOptions.duration == 600,
        gateOptions.barrierDelayMilliseconds == 2000
    else {
        throw ProbeFailure.output("keyboard-gate CLI boundary self-test failed")
    }
    for rejectedURL in [
        "https://127.0.0.1:8123/master.m3u8",
        "http://localhost:8123/master.m3u8",
        "http://127.0.0.1:8123/master.m3u8?token=x",
        "http://user@127.0.0.1:8123/master.m3u8",
    ] {
        if case .run? = try? ProbeOptions.parse([
            "--url", rejectedURL,
            "--events-output", "trace.jsonl",
        ]) {
            throw ProbeFailure.output("loopback URL policy self-test failed")
        }
    }
    let validArguments = [
        "--url", "http://127.0.0.1:8123/master.m3u8",
        "--events-output", "trace.jsonl",
    ]
    let rejectedArguments = [
        validArguments + ["--duration", "601"],
        validArguments + ["--duration", "NaN"],
        validArguments + ["--mode", "keyboard-gate", "--barrier-delay-ms", "2001"],
        validArguments + ["--barrier-delay-ms", "1"],
        ["--url", "http://127.0.0.1:8123/master.m3u8", "--events-output", "trace.txt"],
    ]
    for arguments in rejectedArguments {
        guard case nil = try? ProbeOptions.parse(arguments) else {
            throw ProbeFailure.output("CLI bounds self-test failed")
        }
    }

    var eventBudget = EventBudget()
    for _ in 0..<(maxEvents - 1) {
        guard eventBudget.rejection(for: 1, terminal: false) == nil else {
            throw ProbeFailure.output("event-count budget self-test failed")
        }
        eventBudget.account(1)
    }
    guard case .eventLimit? = eventBudget.rejection(for: 1, terminal: false),
        case nil = eventBudget.rejection(for: 1, terminal: true)
    else {
        throw ProbeFailure.output("terminal event reservation self-test failed")
    }
    let byteBudget = EventBudget(outputBytes: maxOutputBytes - terminalByteReserve)
    guard case .outputLimit? = byteBudget.rejection(for: 1, terminal: false),
        case nil = byteBudget.rejection(for: 1, terminal: true)
    else {
        throw ProbeFailure.output("output-byte budget self-test failed")
    }
    print("native-navigation-probe self-test passed (CLI, CLOCK_MONOTONIC_RAW, bounds, keyboard policy)")
}

private func usage() -> String {
    """
    Usage: native-navigation-probe --url http://127.0.0.1:PORT/master.m3u8 --events-output PATH.jsonl [--run-id ID] [--duration SECONDS] [--mode observe|keyboard-gate] [--barrier-delay-ms 0..2000]
           native-navigation-probe --self-test
    """
}

@main
private struct NativeNavigationProbeMain {
    @MainActor
    static func main() {
        do {
            switch try ProbeOptions.parse(Array(CommandLine.arguments.dropFirst())) {
            case .help:
                print(usage())
            case .selfTest:
                try runSelfTests()
            case .run(let options):
                let writer = try BoundedEventWriter(path: options.eventsURL, runID: options.runID)
                let delegate = ProbeApplicationDelegate(options: options, writer: writer)
                let application = NSApplication.shared
                application.setActivationPolicy(.regular)
                application.delegate = delegate
                withExtendedLifetime(delegate) {
                    application.run()
                }
            }
        } catch {
            report(String(describing: error))
            report(usage())
            exit(2)
        }
    }
}
