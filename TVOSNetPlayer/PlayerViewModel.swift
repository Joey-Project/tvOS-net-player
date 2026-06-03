import AVFoundation
import Combine
import Foundation

@MainActor
final class PlayerViewModel: ObservableObject {
    static let lastStreamURLDefaultsKey = "LastStreamURLText"

    @Published var streamURLText: String {
        didSet {
            syncStateAfterInputChange()
        }
    }
    @Published private(set) var loadedURL: URL?
    @Published private(set) var player: AVPlayer?
    @Published private(set) var statusMessage: String
    @Published private(set) var validationMessage: String?

    private let defaults: UserDefaults
    private let autoplay: Bool

    var canClear: Bool {
        !streamURLText.isEmpty || player != nil || validationMessage != nil
    }

    init(defaultStreamURLText: String? = nil, defaults: UserDefaults = .standard, autoplay: Bool = true) {
        self.defaults = defaults
        self.autoplay = autoplay

        let initialStreamURLText = defaultStreamURLText ?? defaults.string(forKey: Self.lastStreamURLDefaultsKey) ?? ""
        streamURLText = initialStreamURLText
        statusMessage =
            initialStreamURLText.isEmpty
            ? "Ready for an HTTP or HTTPS stream on your network."
            : "Ready to replay \(initialStreamURLText)."
    }

    func load() {
        guard let url = Self.normalizedHTTPURL(from: streamURLText) else {
            validationMessage = "Use an HTTP or HTTPS URL."
            statusMessage = "Cannot load this stream."
            return
        }

        let nextPlayer = AVPlayer(url: url)
        player = nextPlayer
        loadedURL = url
        validationMessage = nil
        streamURLText = url.absoluteString
        defaults.set(url.absoluteString, forKey: Self.lastStreamURLDefaultsKey)
        statusMessage = "Playing \(url.absoluteString)"

        if autoplay {
            nextPlayer.play()
        }
    }

    func stop() {
        player?.pause()
        player = nil
        loadedURL = nil
        statusMessage = "Stopped."
    }

    func clear() {
        stop()
        streamURLText = ""
        validationMessage = nil
        defaults.removeObject(forKey: Self.lastStreamURLDefaultsKey)
        statusMessage = "Ready for an HTTP or HTTPS stream on your network."
    }

    private func syncStateAfterInputChange() {
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

    nonisolated static func normalizedHTTPURL(from text: String) -> URL? {
        StreamURLNormalizer.normalizedHTTPURL(from: text)
    }
}
