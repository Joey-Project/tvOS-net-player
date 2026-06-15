import AVFoundation
import Combine
import Foundation

@MainActor
public final class PlayerViewModel: ObservableObject {
    public static let lastStreamURLDefaultsKey = "LastStreamURLText"

    @Published public var streamURLText: String {
        didSet {
            syncStateAfterInputChange()
        }
    }
    @Published public private(set) var loadedURL: URL?
    @Published public private(set) var player: AVPlayer?
    @Published public private(set) var statusMessage: String
    @Published public private(set) var validationMessage: String?
    public private(set) var manualInteractionSequence = 0

    private let defaults: UserDefaults
    private let autoplay: Bool

    public var canClear: Bool {
        !streamURLText.isEmpty || player != nil || validationMessage != nil
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
}
