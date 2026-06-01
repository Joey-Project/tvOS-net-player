import AVFoundation
import Combine
import Foundation

@MainActor
final class PlayerViewModel: ObservableObject {
    @Published var streamURLText: String
    @Published private(set) var loadedURL: URL?
    @Published private(set) var player: AVPlayer?
    @Published private(set) var statusMessage: String

    init(defaultStreamURLText: String = "") {
        streamURLText = defaultStreamURLText
        statusMessage = "Ready for an HTTP or HTTPS stream on your network."
    }

    func loadDefaultIfAvailable() {
        guard player == nil, !streamURLText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            return
        }
        load()
    }

    func load() {
        guard let url = Self.normalizedHTTPURL(from: streamURLText) else {
            player = nil
            loadedURL = nil
            statusMessage = "Use an HTTP or HTTPS URL, for example http://192.168.1.10:8080/video.mp4."
            return
        }

        let nextPlayer = AVPlayer(url: url)
        player = nextPlayer
        loadedURL = url
        statusMessage = "Loaded \(url.absoluteString)"
        nextPlayer.play()
    }

    nonisolated static func normalizedHTTPURL(from text: String) -> URL? {
        StreamURLNormalizer.normalizedHTTPURL(from: text)
    }
}
