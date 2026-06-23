import AVFoundation
import Combine
import Foundation
import TVOSNetPlayerCacheClient

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

public struct PlayerPlaybackProgressContext: Equatable, Sendable {
    public let endpoint: CacheServerEndpoint
    public let playbackURI: String
    public let libraryItemID: String
    public let variantID: String

    public init(
        endpoint: CacheServerEndpoint,
        playbackURI: String = "",
        libraryItemID: String = "",
        variantID: String = ""
    ) {
        self.endpoint = endpoint
        self.playbackURI = playbackURI
        self.libraryItemID = libraryItemID
        self.variantID = variantID
    }

    func withPlaybackURI(_ playbackURI: String) -> Self {
        Self(
            endpoint: endpoint,
            playbackURI: playbackURI,
            libraryItemID: libraryItemID,
            variantID: variantID
        )
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
    @Published public private(set) var playbackProgressReportingMessage: String?
    public private(set) var manualInteractionSequence = 0

    private let defaults: UserDefaults
    private let autoplay: Bool
    private let cacheClientFactory: @Sendable (CacheServerEndpoint) -> any CacheControlClient
    private let playbackProgressReportInterval: Duration?
    private var playbackProgressContext: PlayerPlaybackProgressContext?
    private var playbackProgressReportTask: Task<Void, Never>?
    private var playbackProgressReportChain: Task<PlaybackProgressReportResult?, Never>?
    private var playbackEndObserver: AnyCancellable?

    public var canClear: Bool {
        !streamURLText.isEmpty || player != nil || validationMessage != nil
    }

    public var canUsePlaybackControls: Bool {
        player != nil
    }

    public init(
        defaultStreamURLText: String? = nil,
        defaults: UserDefaults = .standard,
        autoplay: Bool = true,
        playbackProgressReportInterval: Duration? = .seconds(15),
        cacheClientFactory: @escaping @Sendable (CacheServerEndpoint) -> any CacheControlClient = {
            GRPCCacheControlClient(endpoint: $0)
        }
    ) {
        self.defaults = defaults
        self.autoplay = autoplay
        self.playbackProgressReportInterval = playbackProgressReportInterval
        self.cacheClientFactory = cacheClientFactory

        let initialStreamURLText = defaultStreamURLText ?? defaults.string(forKey: Self.lastStreamURLDefaultsKey) ?? ""
        streamURLText = initialStreamURLText
        statusMessage =
            initialStreamURLText.isEmpty
            ? "Ready for an HTTP or HTTPS stream on your network."
            : "Ready to replay \(initialStreamURLText)."
    }

    deinit {
        playbackProgressReportTask?.cancel()
    }

    public func load() {
        markManualInteraction()
        guard let url = Self.normalizedHTTPURL(from: streamURLText) else {
            validationMessage = "Use an HTTP or HTTPS URL."
            statusMessage = "Cannot load this stream."
            return
        }

        load(url: url, persist: true, progressContext: nil)
    }

    public func load(streamURLText: String) {
        self.streamURLText = streamURLText
        load()
    }

    @discardableResult
    public func loadTransient(
        streamURLText: String,
        progressContext: PlayerPlaybackProgressContext? = nil
    ) -> Bool {
        guard let url = Self.normalizedHTTPURL(from: streamURLText) else {
            validationMessage = "Use an HTTP or HTTPS URL."
            statusMessage = "Cannot load this stream."
            return false
        }

        markManualInteraction()
        load(url: url, persist: false, progressContext: progressContext)
        return true
    }

    @discardableResult
    public func loadTransient(
        streamURLText: String,
        progressContext: PlayerPlaybackProgressContext? = nil,
        ifManualInteractionSequenceMatches expectedSequence: Int
    ) -> Bool {
        guard manualInteractionSequence == expectedSequence else {
            return false
        }

        return loadTransient(streamURLText: streamURLText, progressContext: progressContext)
    }

    private func load(url: URL, persist: Bool, progressContext: PlayerPlaybackProgressContext?) {
        queueCurrentPlaybackProgressReport(intent: .stopped)
        stopPlaybackProgressReporting()

        let nextPlayer = AVPlayer(url: url)
        nextPlayer.defaultRate = playbackSpeed.rate
        player = nextPlayer
        loadedURL = url
        playbackProgressContext = progressContext?.withPlaybackURI(url.absoluteString)
        playbackProgressReportingMessage = nil
        validationMessage = nil
        if persist {
            streamURLText = url.absoluteString
            defaults.set(url.absoluteString, forKey: Self.lastStreamURLDefaultsKey)
        }
        statusMessage = "Playing \(url.absoluteString)"

        if autoplay {
            nextPlayer.play()
        }
        observePlaybackEnd(for: nextPlayer.currentItem)
        queueCurrentPlaybackProgressReport(intent: .started)
        startPlaybackProgressReporting()
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
        queuePlaybackProgressReport(
            positionSeconds: targetSeconds,
            durationSeconds: currentPlaybackDurationSeconds(),
            intent: .seek
        )

        let label = Self.formattedSeekInterval(abs(offset))
        statusMessage =
            offset < 0
            ? "Skipped back \(label)."
            : "Skipped forward \(label)."
    }

    public func setPlaybackSpeed(_ speed: PlayerPlaybackSpeed) {
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
        queueCurrentPlaybackProgressReport(intent: .stopped)
        stopPlaybackProgressReporting()
        player?.pause()
        player = nil
        loadedURL = nil
        playbackProgressContext = nil
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

    @discardableResult
    public func reportCurrentPlaybackProgress(
        intent: PlaybackProgressIntent = .playing
    ) async -> PlaybackProgressReportResult? {
        guard let report = currentPlaybackProgressReport(intent: intent),
            let endpoint = playbackProgressContext?.endpoint
        else {
            return nil
        }

        return await queuePlaybackProgressReport(
            report,
            endpoint: endpoint,
            updatesReportingMessage: true
        ).value
    }

    private func markManualInteraction() {
        manualInteractionSequence += 1
    }

    private func startPlaybackProgressReporting() {
        guard playbackProgressContext != nil, let playbackProgressReportInterval else {
            return
        }

        playbackProgressReportTask?.cancel()
        playbackProgressReportTask = Task { [weak self] in
            while !Task.isCancelled {
                do {
                    try await Task.sleep(for: playbackProgressReportInterval)
                } catch {
                    return
                }
                guard !Task.isCancelled else {
                    return
                }
                guard let intent = self?.periodicPlaybackProgressIntent() else {
                    continue
                }
                await self?.reportCurrentPlaybackProgress(intent: intent)
            }
        }
    }

    private func stopPlaybackProgressReporting() {
        playbackProgressReportTask?.cancel()
        playbackProgressReportTask = nil
        playbackEndObserver?.cancel()
        playbackEndObserver = nil
    }

    private func observePlaybackEnd(for item: AVPlayerItem?) {
        guard let item else {
            return
        }

        playbackEndObserver = NotificationCenter.default
            .publisher(for: .AVPlayerItemDidPlayToEndTime, object: item)
            .sink { [weak self] notification in
                let endedItem = notification.object as? AVPlayerItem
                Task { @MainActor [weak self, endedItem] in
                    guard let endedItem, self?.player?.currentItem === endedItem else {
                        return
                    }
                    self?.finishPlaybackProgressReporting(for: endedItem)
                }
            }
    }

    private func finishPlaybackProgressReporting(for item: AVPlayerItem) {
        guard player?.currentItem === item else {
            return
        }
        queueCurrentPlaybackProgressReport(intent: .stopped)
        stopPlaybackProgressReporting()
        statusMessage = "Playback finished."
    }

    private func periodicPlaybackProgressIntent() -> PlaybackProgressIntent? {
        guard let player else {
            return nil
        }

        if player.rate != 0 || player.timeControlStatus == .playing {
            return .playing
        }

        guard player.currentItem != nil else {
            return nil
        }
        return .paused
    }

    private func queueCurrentPlaybackProgressReport(intent: PlaybackProgressIntent) {
        guard let report = currentPlaybackProgressReport(intent: intent),
            let endpoint = playbackProgressContext?.endpoint
        else {
            return
        }

        queuePlaybackProgressReport(report, endpoint: endpoint)
    }

    private func queuePlaybackProgressReport(
        positionSeconds: Double,
        durationSeconds: Double?,
        intent: PlaybackProgressIntent
    ) {
        guard let context = playbackProgressContext else {
            return
        }

        let report = PlaybackProgressReport(
            playbackURI: context.playbackURI,
            libraryItemID: context.libraryItemID,
            variantID: context.variantID,
            positionSeconds: positionSeconds,
            durationSeconds: durationSeconds,
            intent: intent
        )
        queuePlaybackProgressReport(report, endpoint: context.endpoint)
    }

    @discardableResult
    private func queuePlaybackProgressReport(
        _ report: PlaybackProgressReport,
        endpoint: CacheServerEndpoint,
        updatesReportingMessage: Bool = false
    ) -> Task<PlaybackProgressReportResult?, Never> {
        playbackProgressReportingMessage = nil
        let previousReportTask = playbackProgressReportChain
        let cacheClientFactory = cacheClientFactory
        let reportTask = Task<PlaybackProgressReportResult?, Never> { @MainActor in
            _ = await previousReportTask?.value
            do {
                let result = try await cacheClientFactory(endpoint).reportPlaybackProgress(report)
                if updatesReportingMessage {
                    self.playbackProgressReportingMessage = result.accepted ? nil : result.message
                }
                return result
            } catch let error as CacheControlClientUnsupportedFeature where error == .playbackProgressReporting {
                if updatesReportingMessage {
                    self.playbackProgressReportingMessage = nil
                }
            } catch {
                if updatesReportingMessage {
                    self.playbackProgressReportingMessage = "Could not report playback position."
                }
            }

            return nil
        }
        playbackProgressReportChain = reportTask
        return reportTask
    }

    private func currentPlaybackProgressReport(intent: PlaybackProgressIntent) -> PlaybackProgressReport? {
        guard let context = playbackProgressContext else {
            return nil
        }

        return PlaybackProgressReport(
            playbackURI: context.playbackURI,
            libraryItemID: context.libraryItemID,
            variantID: context.variantID,
            positionSeconds: currentPlaybackPositionSeconds(),
            durationSeconds: currentPlaybackDurationSeconds(),
            intent: intent
        )
    }

    private func currentPlaybackPositionSeconds() -> Double {
        guard let player else {
            return 0
        }

        let seconds = player.currentTime().seconds
        return seconds.isFinite && seconds > 0 ? seconds : 0
    }

    private func currentPlaybackDurationSeconds() -> Double? {
        guard let player else {
            return nil
        }

        let seconds = player.currentItem?.duration.seconds ?? 0
        return seconds.isFinite && seconds > 0 ? seconds : nil
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
