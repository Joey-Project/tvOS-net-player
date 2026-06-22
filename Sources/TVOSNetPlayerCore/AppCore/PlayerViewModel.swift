import AVFoundation
import Combine
import Foundation

public enum PlayerPlaybackSpeed: Float, CaseIterable, Hashable, Identifiable, Sendable {
    case threeQuarter = 0.75
    case normal = 1.0
    case oneAndQuarter = 1.25
    case oneAndHalf = 1.5
    case double = 2.0

    public var id: Float {
        rawValue
    }

    public var rate: Float {
        rawValue
    }

    public var displayTitle: String {
        switch self {
        case .threeQuarter:
            return "0.75x"
        case .normal:
            return "1x"
        case .oneAndQuarter:
            return "1.25x"
        case .oneAndHalf:
            return "1.5x"
        case .double:
            return "2x"
        }
    }
}

@MainActor
public final class PlayerViewModel: ObservableObject {
    public static let lastStreamURLDefaultsKey = "LastStreamURLText"
    public static let defaultSkipInterval: TimeInterval = 10

    @Published public var streamURLText: String {
        didSet {
            syncStateAfterInputChange()
        }
    }
    @Published public private(set) var loadedURL: URL?
    @Published public private(set) var player: AVPlayer?
    @Published public private(set) var statusMessage: String
    @Published public private(set) var validationMessage: String?
    @Published public private(set) var playbackSpeed: PlayerPlaybackSpeed = .normal
    public private(set) var manualInteractionSequence = 0

    private let defaults: UserDefaults
    private let autoplay: Bool

    public var canClear: Bool {
        !streamURLText.isEmpty || player != nil || validationMessage != nil
    }

    public var canUsePlaybackControls: Bool {
        player != nil
    }

    public init(defaultStreamURLText: String? = nil, defaults: UserDefaults = .standard, autoplay: Bool = true) {
        self.defaults = defaults
        self.autoplay = autoplay

        let initialStreamURLText = defaultStreamURLText ?? defaults.string(forKey: Self.lastStreamURLDefaultsKey) ?? ""
        streamURLText = initialStreamURLText
        statusMessage =
            initialStreamURLText.isEmpty
            ? "Ready for an HTTP or HTTPS stream on your network."
            : "Ready to replay \(initialStreamURLText)."
    }

    public func load() {
        markManualInteraction()
        guard let url = Self.normalizedHTTPURL(from: streamURLText) else {
            validationMessage = "Use an HTTP or HTTPS URL."
            statusMessage = "Cannot load this stream."
            return
        }

        load(url: url, persist: true)
    }

    public func load(streamURLText: String) {
        self.streamURLText = streamURLText
        load()
    }

    @discardableResult
    public func loadTransient(streamURLText: String) -> Bool {
        guard let url = Self.normalizedHTTPURL(from: streamURLText) else {
            validationMessage = "Use an HTTP or HTTPS URL."
            statusMessage = "Cannot load this stream."
            return false
        }

        markManualInteraction()
        load(url: url, persist: false)
        return true
    }

    @discardableResult
    public func loadTransient(streamURLText: String, ifManualInteractionSequenceMatches expectedSequence: Int) -> Bool {
        guard manualInteractionSequence == expectedSequence else {
            return false
        }

        return loadTransient(streamURLText: streamURLText)
    }

    private func load(url: URL, persist: Bool) {
        let nextPlayer = AVPlayer(url: url)
        nextPlayer.defaultRate = playbackSpeed.rate
        player = nextPlayer
        loadedURL = url
        validationMessage = nil
        if persist {
            streamURLText = url.absoluteString
            defaults.set(url.absoluteString, forKey: Self.lastStreamURLDefaultsKey)
        }
        statusMessage = "Playing \(url.absoluteString)"

        if autoplay {
            nextPlayer.play()
        }
    }

    public func skipBackward() {
        seek(by: -Self.defaultSkipInterval)
    }

    public func skipForward() {
        seek(by: Self.defaultSkipInterval)
    }

    public func seek(by offset: TimeInterval) {
        markManualInteraction()
        guard let player else {
            statusMessage = "No stream loaded."
            return
        }

        let currentSeconds = player.currentTime().seconds
        let baseSeconds = currentSeconds.isFinite ? currentSeconds : 0
        let targetSeconds = max(0, baseSeconds + offset)
        let target = CMTime(seconds: targetSeconds, preferredTimescale: 600)
        player.seek(to: target, toleranceBefore: .zero, toleranceAfter: .zero)

        let label = Self.formattedSeekInterval(abs(offset))
        statusMessage =
            offset < 0
            ? "Skipped back \(label)."
            : "Skipped forward \(label)."
    }

    public func setPlaybackSpeed(_ speed: PlayerPlaybackSpeed) {
        markManualInteraction()
        playbackSpeed = speed
        guard let player else {
            statusMessage = "Playback speed \(speed.displayTitle) selected."
            return
        }

        player.defaultRate = speed.rate
        if player.rate != 0 {
            player.rate = speed.rate
        }
        statusMessage = "Playback speed set to \(speed.displayTitle)."
    }

    public func stop() {
        markManualInteraction()
        player?.pause()
        player = nil
        loadedURL = nil
        statusMessage = "Stopped."
    }

    public func clear() {
        stop()
        streamURLText = ""
        validationMessage = nil
        defaults.removeObject(forKey: Self.lastStreamURLDefaultsKey)
        statusMessage = "Ready for an HTTP or HTTPS stream on your network."
    }

    private func syncStateAfterInputChange() {
        markManualInteraction()

        if let url = Self.normalizedHTTPURL(from: streamURLText) {
            if validationMessage != nil {
                validationMessage = nil
                statusMessage = "Ready to play \(url.absoluteString)."
            }
            return
        }

        guard streamURLText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            return
        }

        validationMessage = nil
        defaults.removeObject(forKey: Self.lastStreamURLDefaultsKey)

        if player == nil {
            loadedURL = nil
            statusMessage = "Ready for an HTTP or HTTPS stream on your network."
        }
    }

    private func markManualInteraction() {
        manualInteractionSequence += 1
    }

    public nonisolated static func normalizedHTTPURL(from text: String) -> URL? {
        StreamURLNormalizer.normalizedHTTPURL(from: text)
    }

    private nonisolated static func formattedSeekInterval(_ interval: TimeInterval) -> String {
        let roundedSeconds = Int(interval.rounded())
        if roundedSeconds % 60 == 0, roundedSeconds >= 60 {
            return "\(roundedSeconds / 60) minute\(roundedSeconds == 60 ? "" : "s")"
        }

        return "\(roundedSeconds) seconds"
    }
}
