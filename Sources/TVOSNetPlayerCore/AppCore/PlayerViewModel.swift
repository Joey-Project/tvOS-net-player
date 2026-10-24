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

public enum PlayerPlaybackState: String, Sendable {
    case paused
    case buffering
    case playing
    case finished

    init(timeControlStatus: AVPlayer.TimeControlStatus, isFinished: Bool = false) {
        if isFinished {
            self = .finished
            return
        }

        switch timeControlStatus {
        case .paused:
            self = .paused
        case .waitingToPlayAtSpecifiedRate:
            self = .buffering
        case .playing:
            self = .playing
        @unknown default:
            self = .paused
        }
    }

    public var canPause: Bool {
        self == .buffering || self == .playing
    }

    public var displayTitle: String {
        switch self {
        case .paused:
            return "Paused"
        case .buffering:
            return "Buffering"
        case .playing:
            return "Playing"
        case .finished:
            return "Finished"
        }
    }
}

@MainActor
protocol PlayerPlaybackControlling: AnyObject {
    var currentItem: AVPlayerItem? { get }
    var itemDuration: CMTime { get }
    var readinessStatus: AVPlayerItem.Status { get }
    var timeControlStatus: AVPlayer.TimeControlStatus { get }
    var defaultRate: Float { get set }
    func currentTime() -> CMTime
    func play()
    func pause()
    func seek(to time: CMTime, toleranceBefore: CMTime, toleranceAfter: CMTime)
    func seekForPlayback(to time: CMTime, completion: @escaping @MainActor @Sendable (Bool) -> Void)
    func observeReadiness(
        _ observer: @escaping @MainActor @Sendable (AVPlayerItem.Status) -> Void
    ) -> AnyCancellable
}

extension PlayerPlaybackControlling {
    var itemDuration: CMTime { currentItem?.duration ?? .invalid }
}

extension AVPlayer: PlayerPlaybackControlling {
    var readinessStatus: AVPlayerItem.Status {
        currentItem?.status ?? .unknown
    }

    func seekForPlayback(to time: CMTime, completion: @escaping @MainActor @Sendable (Bool) -> Void) {
        seek(to: time, toleranceBefore: .zero, toleranceAfter: .zero) { finished in
            Task { @MainActor in completion(finished) }
        }
    }

    func observeReadiness(
        _ observer: @escaping @MainActor @Sendable (AVPlayerItem.Status) -> Void
    ) -> AnyCancellable {
        guard let item = currentItem else {
            observer(.failed)
            return AnyCancellable {}
        }

        return item.publisher(for: \.status, options: [.initial, .new]).sink { [weak item] _ in
            Task { @MainActor in
                observer(item?.status ?? .unknown)
            }
        }
    }
}

public struct PlayerPlaybackProgressContext: Equatable, Sendable {
    public let endpoint: CacheServerEndpoint
    public let playbackURI: String
    public let libraryItemID: String
    public let variantID: String
    public let resumptionIdentity: ClientPlaybackIdentity?

    public init(
        endpoint: CacheServerEndpoint,
        playbackURI: String = "",
        libraryItemID: String = "",
        variantID: String = "",
        resumptionIdentity: ClientPlaybackIdentity? = nil
    ) {
        self.endpoint = endpoint
        self.playbackURI = playbackURI
        self.libraryItemID = libraryItemID
        self.variantID = variantID
        self.resumptionIdentity = resumptionIdentity
    }

    func withPlaybackURI(_ playbackURI: String) -> Self {
        Self(
            endpoint: endpoint,
            playbackURI: playbackURI,
            libraryItemID: libraryItemID,
            variantID: variantID,
            resumptionIdentity: resumptionIdentity
        )
    }
}

@MainActor
public final class PlayerViewModel: ObservableObject {
    public static let lastStreamURLDefaultsKey = "LastStreamURLText"
    public static let defaultSkipInterval: TimeInterval = 10
    private static let resumeSeekEndSafetyMarginSeconds = 1.0
    private static let resumeSeekPositionToleranceSeconds = 1.0  // Accept up to one second of seek-position drift.

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
    @Published public private(set) var playbackProgressStatusRefreshRequestID = 0
    public private(set) var manualInteractionSequence = 0

    private let defaults: UserDefaults
    private let autoplay: Bool
    private let resumptionStore: ClientResumptionStore?
    private let cacheClientFactory: @Sendable (CacheServerEndpoint) -> any CacheControlClient
    private let playbackProgressReportInterval: Duration?
    private var playbackProgressContext: PlayerPlaybackProgressContext?
    private var playbackController: (any PlayerPlaybackControlling)?
    private var playbackProgressReportTask: Task<Void, Never>?
    private var playbackProgressReportChain: Task<PlaybackProgressReportResult?, Never>?
    private var playbackEndObserver: AnyCancellable?
    private var playbackEndObservationID = UUID()
    private var playbackResumeObserver: AnyCancellable?
    private var playbackTimeJumpObserver: AnyCancellable?
    private weak var playbackProgressEndedItem: AVPlayerItem?
    private var playbackProgressEndedTime: CMTime?
    private var pendingPlaybackResume: PendingPlaybackResume?
    private var pendingPlaybackResumeObserver: AnyCancellable?
    private var pendingPlaybackResumeTimeout: Task<Void, Never>?
    private var resumePositionUnrestoredItem: AVPlayerItem?

    var playbackControllerFactory: @MainActor (URL) -> (AVPlayer, any PlayerPlaybackControlling) = {
        let player = AVPlayer(url: $0)
        return (player, player)
    }
    var resumptionReadinessTimeout: Duration = .seconds(20)
    var resumptionSeekTimeout: Duration = .seconds(15)

    private struct PendingReplay {
        let player: AVPlayer
        let item: AVPlayerItem
        let controller: any PlayerPlaybackControlling
        let interactionSequence: Int
    }
    private var pendingReplay: PendingReplay?

    private struct PendingPlaybackResume {
        enum Phase: Equatable {
            case waitingForReadiness
            case seeking
        }

        let id: UUID
        let player: AVPlayer
        let item: AVPlayerItem
        let controller: any PlayerPlaybackControlling
        let identity: ClientPlaybackIdentity
        let checkpoint: ClientPlaybackCheckpoint
        let interactionSequence: Int
        var phase: Phase
        var targetSeconds: Double?
    }

    public var canClear: Bool {
        !streamURLText.isEmpty || player != nil || validationMessage != nil
    }

    public var canUsePlaybackControls: Bool {
        player != nil
    }

    public var playbackState: PlayerPlaybackState {
        let item = player?.currentItem
        if let pendingPlaybackResume, pendingPlaybackResume.player === player,
            pendingPlaybackResume.item === item
        {
            return .buffering
        }
        if let pendingReplay, pendingReplay.player === player, pendingReplay.item === item {
            return .buffering
        }
        return PlayerPlaybackState(
            timeControlStatus: player?.timeControlStatus ?? .paused,
            isFinished: item != nil
                && (playbackProgressEndedItem === item
                    || player.map {
                        $0.timeControlStatus == .paused
                            && Self.isAtPlaybackEnd(currentTime: $0.currentTime(), duration: $0.itemDuration)
                    } == true)
        )
    }

    public init(
        defaultStreamURLText: String? = nil,
        defaults: UserDefaults = .standard,
        autoplay: Bool = true,
        playbackProgressReportInterval: Duration? = .seconds(15),
        resumptionStore: ClientResumptionStore? = nil,
        cacheClientFactory: @escaping @Sendable (CacheServerEndpoint) -> any CacheControlClient = {
            GRPCCacheControlClient(endpoint: $0)
        }
    ) {
        self.defaults = defaults
        self.autoplay = autoplay
        self.playbackProgressReportInterval = playbackProgressReportInterval
        self.resumptionStore = resumptionStore
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
        let replacingPendingResume = isCurrentResumePositionUnrestored
        markManualInteraction()
        guard let url = Self.normalizedHTTPURL(from: streamURLText) else {
            validationMessage = "Use an HTTP or HTTPS URL."
            statusMessage = "Cannot load this stream."
            return
        }

        load(url: url, persist: true, progressContext: nil, suppressCurrentPosition: replacingPendingResume)
    }

    public func load(streamURLText: String) {
        self.streamURLText = streamURLText
        load()
    }

    @discardableResult
    public func loadTransient(
        streamURLText: String,
        progressContext: PlayerPlaybackProgressContext? = nil,
        resumeCheckpoint: ClientPlaybackCheckpoint? = nil
    ) -> Bool {
        guard let url = Self.normalizedHTTPURL(from: streamURLText) else {
            validationMessage = "Use an HTTP or HTTPS URL."
            statusMessage = "Cannot load this stream."
            return false
        }

        let replacingPendingResume = isCurrentResumePositionUnrestored
        markManualInteraction()
        load(
            url: url,
            persist: false,
            progressContext: progressContext,
            resumeCheckpoint: resumeCheckpoint,
            suppressCurrentPosition: replacingPendingResume
        )
        return true
    }

    @discardableResult
    public func loadTransient(
        streamURLText: String,
        progressContext: PlayerPlaybackProgressContext? = nil,
        resumeCheckpoint: ClientPlaybackCheckpoint? = nil,
        ifManualInteractionSequenceMatches expectedSequence: Int
    ) -> Bool {
        guard manualInteractionSequence == expectedSequence else {
            return false
        }

        return loadTransient(
            streamURLText: streamURLText,
            progressContext: progressContext,
            resumeCheckpoint: resumeCheckpoint
        )
    }

    private func load(
        url: URL,
        persist: Bool,
        progressContext: PlayerPlaybackProgressContext?,
        resumeCheckpoint: ClientPlaybackCheckpoint? = nil,
        suppressCurrentPosition: Bool = false
    ) {
        let queuedStoppedReport =
            suppressCurrentPosition
            ? false : queueCurrentPlaybackProgressReport(intent: .stopped)
        stopPlaybackProgressReporting()
        requestPlaybackProgressStatusRefreshIfNeeded(queuedStoppedReport)

        let (nextPlayer, nextController) = playbackControllerFactory(url)
        nextPlayer.defaultRate = playbackSpeed.rate
        player = nextPlayer
        playbackController = nextController
        loadedURL = url
        playbackProgressContext = progressContext?.withPlaybackURI(url.absoluteString)
        playbackProgressReportingMessage = nil
        validationMessage = nil
        resumePositionUnrestoredItem = nil
        if persist {
            streamURLText = url.absoluteString
            defaults.set(url.absoluteString, forKey: Self.lastStreamURLDefaultsKey)
        }

        let matchingCheckpoint: ClientPlaybackCheckpoint?
        if let identity = playbackProgressContext?.resumptionIdentity,
            let resumeCheckpoint,
            resumeCheckpoint.identity == identity
        {
            matchingCheckpoint = resumeCheckpoint
            resumptionStore?.beginPlayback(identity, checkpoint: resumeCheckpoint)
        } else {
            matchingCheckpoint = nil
            if let identity = playbackProgressContext?.resumptionIdentity {
                resumptionStore?.beginPlayback(identity)
            }
        }

        if let matchingCheckpoint, let identity = playbackProgressContext?.resumptionIdentity,
            let item = nextController.currentItem
        {
            statusMessage = "Restoring saved playback position."
            nextController.pause()
            resumePositionUnrestoredItem = item
            beginPlaybackResume(
                player: nextPlayer,
                item: item,
                controller: nextController,
                identity: identity,
                checkpoint: matchingCheckpoint
            )
        } else if matchingCheckpoint != nil {
            validationMessage = "Could not restore the saved playback position. Try again."
            statusMessage = validationMessage ?? "Could not restore playback."
        } else {
            statusMessage = "Playing \(url.absoluteString)"
            if autoplay {
                nextController.play()
            }
            queueCurrentPlaybackProgressReport(intent: .started)
            startPlaybackProgressReporting()
        }
        observePlaybackEnd(for: nextPlayer.currentItem)
        observePlaybackResume(for: nextPlayer)
        observePlaybackTimeJumps(for: nextPlayer.currentItem)
    }

    public func skipBackward() {
        seek(by: -Self.defaultSkipInterval)
    }

    public func togglePlayback() {
        togglePlayback(using: player)
    }

    func togglePlayback(using controller: (any PlayerPlaybackControlling)?) {
        handlePlaybackTimeJump(using: controller)
        let wasRestarting =
            pendingReplay != nil
            && pendingReplay?.player === player
            && pendingReplay?.item === controller?.currentItem
        markManualInteraction()
        guard let controller else { return }
        if wasRestarting {
            controller.pause()
            return
        }
        guard !PlayerPlaybackState(timeControlStatus: controller.timeControlStatus).canPause,
            let player,
            let item = controller.currentItem,
            player.currentItem === item,
            playbackProgressEndedItem === item
                || Self.isAtPlaybackEnd(currentTime: controller.currentTime(), duration: controller.itemDuration)
        else {
            Self.togglePlayback(player: controller, speed: playbackSpeed)
            return
        }

        if playbackProgressEndedItem !== item {
            finishPlaybackProgressReporting(
                for: item, terminalTime: controller.currentTime(),
                durationSeconds: Self.playbackDurationSeconds(controller.itemDuration))
        }
        let sequence = manualInteractionSequence
        pendingReplay = PendingReplay(player: player, item: item, controller: controller, interactionSequence: sequence)
        controller.seekForPlayback(to: .zero) { [weak self] finished in
            self?.completeReplaySeek(finished: finished, interactionSequence: sequence)
        }
    }

    private func completeReplaySeek(finished: Bool, interactionSequence: Int) {
        guard let request = pendingReplay, request.interactionSequence == interactionSequence else { return }
        pendingReplay = nil
        guard finished,
            manualInteractionSequence == interactionSequence,
            player === request.player,
            player?.currentItem === request.item,
            request.controller.currentItem === request.item,
            playbackProgressEndedItem === request.item
        else {
            handlePlaybackTimeJump(using: request.controller)
            return
        }
        if CMTimeCompare(request.controller.currentTime(), .zero) != 0,
            hasMovedAwayFromPlaybackEnd(using: request.controller)
        {
            handlePlaybackTimeJump(using: request.controller)
            return
        }

        let durationSeconds = Self.playbackDurationSeconds(request.controller.itemDuration)
        queuePlaybackProgressReport(positionSeconds: 0, durationSeconds: durationSeconds, intent: .seek)
        restartPlaybackProgressReportingAfterEndIfNeeded(
            immediateIntent: .playing, positionSeconds: 0, durationSeconds: durationSeconds)
        statusMessage = loadedURL.map { "Playing \($0.absoluteString)" } ?? "Playing."
        request.controller.defaultRate = playbackSpeed.rate
        request.controller.play()
    }

    static func togglePlayback(player: any PlayerPlaybackControlling, speed: PlayerPlaybackSpeed) {
        if PlayerPlaybackState(timeControlStatus: player.timeControlStatus).canPause {
            player.pause()
        } else {
            player.defaultRate = speed.rate
            player.play()
        }
    }

    public func skipForward() {
        seek(by: Self.defaultSkipInterval)
    }

    public func seek(by offset: TimeInterval) {
        seek(by: offset, using: player)
    }

    func seek(by offset: TimeInterval, using controller: (any PlayerPlaybackControlling)?) {
        handlePlaybackTimeJump(using: controller)
        markManualInteraction()
        guard let controller else {
            statusMessage = "No stream loaded."
            return
        }
        guard offset.isFinite, abs(offset) < Double(Int.max) else { return }

        let currentTime = controller.currentTime()
        let currentSeconds = currentTime.seconds
        let baseSeconds = currentSeconds.isFinite ? max(0, currentSeconds) : 0
        let duration = controller.itemDuration
        let durationSeconds = Self.playbackDurationSeconds(duration)
        let targetSeconds = Self.seekTargetSeconds(
            currentSeconds: baseSeconds, durationSeconds: durationSeconds, offset: offset)
        let isEnded =
            controller.currentItem != nil
            && (playbackProgressEndedItem === controller.currentItem
                || Self.isAtPlaybackEnd(currentTime: currentTime, duration: duration))
        if isEnded,
            offset >= 0 || targetSeconds >= baseSeconds || durationSeconds.map({ targetSeconds >= $0 }) == true
        {
            if let item = controller.currentItem, playbackProgressEndedItem !== item {
                finishPlaybackProgressReporting(
                    for: item, terminalTime: currentTime, durationSeconds: durationSeconds)
            }
            return
        }

        let target =
            durationSeconds.map { targetSeconds >= $0 } == true
            ? duration : CMTime(seconds: targetSeconds, preferredTimescale: 600)
        if Self.isAtPlaybackEnd(currentTime: target, duration: duration),
            let player, let item = controller.currentItem, player.currentItem === item
        {
            let sequence = manualInteractionSequence
            controller.seekForPlayback(to: target) { [weak self, weak player, weak item, weak controller] finished in
                guard let self, let player, let item, let controller,
                    finished, self.manualInteractionSequence == sequence,
                    self.player === player, player.currentItem === item, controller.currentItem === item,
                    Self.isAtPlaybackEnd(currentTime: controller.currentTime(), duration: controller.itemDuration),
                    self.playbackProgressEndedItem !== item
                else { return }
                let position = controller.currentTime()
                let duration = Self.playbackDurationSeconds(controller.itemDuration)
                self.resumePositionUnrestoredItem = nil
                self.queuePlaybackProgressReport(
                    positionSeconds: position.seconds, durationSeconds: duration, intent: .seek)
                self.finishPlaybackProgressReporting(for: item, terminalTime: position, durationSeconds: duration)
            }
        } else {
            controller.seek(to: target, toleranceBefore: .zero, toleranceAfter: .zero)
            if controller.currentItem === resumePositionUnrestoredItem {
                resumePositionUnrestoredItem = nil
            }
            queuePlaybackProgressReport(
                positionSeconds: target.seconds, durationSeconds: durationSeconds, intent: .seek)
            restartPlaybackProgressReportingAfterEndIfNeeded(immediateIntent: nil)
        }

        let label = Self.formattedSeekInterval(abs(offset))
        statusMessage =
            offset < 0
            ? "Skipped back \(label)."
            : "Skipped forward \(label)."
    }

    static func seekTargetSeconds(currentSeconds: Double, durationSeconds: Double?, offset: TimeInterval) -> Double {
        let baseSeconds = currentSeconds.isFinite ? max(0, currentSeconds) : 0
        let duration = durationSeconds.flatMap { $0.isFinite && $0 >= 0 ? $0 : nil }
        let upperBound = min(duration ?? .infinity, Double(Int64.max / 600))
        return min(max(0, baseSeconds + offset), upperBound)
    }

    static func isAtPlaybackEnd(currentTime: CMTime, duration: CMTime) -> Bool {
        currentTime.isNumeric && duration.isNumeric && CMTimeCompare(duration, .zero) > 0
            && CMTimeCompare(currentTime, duration) >= 0
    }

    private static func playbackDurationSeconds(_ duration: CMTime) -> Double? {
        let seconds = duration.seconds
        return seconds.isFinite && seconds > 0 ? seconds : nil
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
        handlePlaybackTimeJump(using: playbackController)
        let suppressCurrentPosition = isCurrentResumePositionUnrestored
        markManualInteraction()
        let queuedStoppedReport =
            suppressCurrentPosition
            ? false : queueCurrentPlaybackProgressReport(intent: .stopped)
        stopPlaybackProgressReporting()
        player?.pause()
        player = nil
        playbackController = nil
        loadedURL = nil
        playbackProgressContext = nil
        resumePositionUnrestoredItem = nil
        statusMessage = "Stopped."
        requestPlaybackProgressStatusRefreshIfNeeded(queuedStoppedReport)
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

    public func checkpointPlayback() {
        handlePlaybackTimeJump(using: playbackController)
        guard !isCurrentResumePositionUnrestored,
            let controller = playbackController,
            let item = controller.currentItem,
            player?.currentItem === item,
            let identity = playbackProgressContext?.resumptionIdentity,
            let resumptionStore
        else {
            return
        }

        let position = controller.currentTime().seconds
        guard position.isFinite, position >= 0 else { return }
        let duration = Self.playbackDurationSeconds(controller.itemDuration)
        let completed =
            resumptionStore.record?.playback?.identity == identity
            ? (resumptionStore.record?.playback?.completed ?? false) : false
        resumptionStore.updatePlayback(
            identity: identity,
            positionSeconds: position,
            durationSeconds: duration,
            completed: completed
        )
    }

    @discardableResult
    public func reportCurrentPlaybackProgress(
        intent: PlaybackProgressIntent = .playing
    ) async -> PlaybackProgressReportResult? {
        handlePlaybackTimeJump(using: player)
        guard !isCurrentResumePositionUnrestored,
            let report = currentPlaybackProgressReport(intent: intent),
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

    public func flushPlaybackProgressReports() async {
        _ = await playbackProgressReportChain?.value
    }

    private func markManualInteraction() {
        manualInteractionSequence += 1
        pendingReplay = nil
        cancelPendingPlaybackResume()
    }

    private var isCurrentResumePositionUnrestored: Bool {
        guard let item = player?.currentItem else { return false }
        return resumePositionUnrestoredItem === item
    }

    private func beginPlaybackResume(
        player: AVPlayer,
        item: AVPlayerItem,
        controller: any PlayerPlaybackControlling,
        identity: ClientPlaybackIdentity,
        checkpoint: ClientPlaybackCheckpoint
    ) {
        let request = PendingPlaybackResume(
            id: UUID(),
            player: player,
            item: item,
            controller: controller,
            identity: identity,
            checkpoint: checkpoint,
            interactionSequence: manualInteractionSequence,
            phase: .waitingForReadiness,
            targetSeconds: nil
        )
        pendingPlaybackResume = request

        switch controller.readinessStatus {
        case .readyToPlay:
            beginPlaybackResumeSeek(requestID: request.id)
        case .failed:
            failPlaybackResume(requestID: request.id)
        default:
            startPlaybackResumeTimeout(requestID: request.id, phase: .waitingForReadiness)
            let observer = controller.observeReadiness { [weak self] status in
                self?.handlePlaybackReadiness(status, requestID: request.id)
            }
            if pendingPlaybackResume?.id == request.id,
                pendingPlaybackResume?.phase == .waitingForReadiness
            {
                pendingPlaybackResumeObserver = observer
            } else {
                observer.cancel()
            }
        }
    }

    private func handlePlaybackReadiness(_ status: AVPlayerItem.Status, requestID: UUID) {
        guard let request = pendingPlaybackResume, request.id == requestID,
            request.phase == .waitingForReadiness
        else {
            return
        }
        guard isCurrentPlaybackResume(request) else {
            cancelPendingPlaybackResume()
            return
        }

        switch status {
        case .readyToPlay:
            pendingPlaybackResumeObserver?.cancel()
            pendingPlaybackResumeObserver = nil
            pendingPlaybackResumeTimeout?.cancel()
            pendingPlaybackResumeTimeout = nil
            beginPlaybackResumeSeek(requestID: requestID)
        case .failed:
            failPlaybackResume(requestID: requestID)
        default:
            break
        }
    }

    private func beginPlaybackResumeSeek(requestID: UUID) {
        guard var request = pendingPlaybackResume, request.id == requestID,
            request.phase == .waitingForReadiness,
            isCurrentPlaybackResume(request)
        else {
            return
        }

        let savedPosition = request.checkpoint.completed ? 0 : request.checkpoint.positionSeconds
        let currentDuration = Self.playbackDurationSeconds(request.controller.itemDuration)
        let upperBound =
            currentDuration.map {
                max(0, $0 - Self.resumeSeekEndSafetyMarginSeconds)
            } ?? ClientResumptionStore.maximumPlaybackSeconds
        let targetSeconds = min(max(0, savedPosition), upperBound)
        request.phase = .seeking
        request.targetSeconds = targetSeconds
        pendingPlaybackResume = request
        let target = CMTime(seconds: targetSeconds, preferredTimescale: 600)

        startPlaybackResumeTimeout(requestID: requestID, phase: .seeking)
        request.controller.seekForPlayback(to: target) { [weak self] finished in
            self?.completePlaybackResumeSeek(finished: finished, requestID: requestID)
        }
    }

    private func completePlaybackResumeSeek(finished: Bool, requestID: UUID) {
        guard let request = pendingPlaybackResume, request.id == requestID,
            request.phase == .seeking
        else {
            return
        }
        guard isCurrentPlaybackResume(request) else {
            cancelPendingPlaybackResume()
            return
        }
        guard finished else {
            failPlaybackResume(requestID: requestID)
            return
        }

        let position = request.controller.currentTime().seconds
        guard let targetSeconds = request.targetSeconds,
            position.isFinite,
            position >= 0,
            abs(position - targetSeconds) <= Self.resumeSeekPositionToleranceSeconds
        else {
            failPlaybackResume(requestID: requestID)
            return
        }

        cancelPendingPlaybackResume()
        resumePositionUnrestoredItem = nil
        request.controller.defaultRate = playbackSpeed.rate
        statusMessage =
            autoplay
            ? "Playing \(loadedURL?.absoluteString ?? "stream")."
            : "Playback ready at the saved position."
        validationMessage = nil
        if autoplay {
            request.controller.play()
        }
        queuePlaybackProgressReport(
            positionSeconds: position,
            durationSeconds: Self.playbackDurationSeconds(request.controller.itemDuration),
            intent: .started
        )
        startPlaybackProgressReporting()
    }

    private func isCurrentPlaybackResume(_ request: PendingPlaybackResume) -> Bool {
        request.interactionSequence == manualInteractionSequence
            && player === request.player
            && player?.currentItem === request.item
            && request.controller.currentItem === request.item
    }

    private func startPlaybackResumeTimeout(
        requestID: UUID,
        phase: PendingPlaybackResume.Phase
    ) {
        pendingPlaybackResumeTimeout?.cancel()
        let timeout = phase == .waitingForReadiness ? resumptionReadinessTimeout : resumptionSeekTimeout
        pendingPlaybackResumeTimeout = Task { [weak self] in
            do {
                try await Task.sleep(for: timeout)
            } catch {
                return
            }
            guard let self, let request = self.pendingPlaybackResume,
                request.id == requestID, request.phase == phase
            else {
                return
            }
            self.failPlaybackResume(requestID: requestID)
        }
    }

    private func failPlaybackResume(requestID: UUID) {
        guard pendingPlaybackResume?.id == requestID else { return }
        cancelPendingPlaybackResume()
        validationMessage = "Could not restore the saved playback position. Try again."
        statusMessage = validationMessage ?? "Could not restore playback."
    }

    private func cancelPendingPlaybackResume() {
        pendingPlaybackResume = nil
        pendingPlaybackResumeObserver?.cancel()
        pendingPlaybackResumeObserver = nil
        pendingPlaybackResumeTimeout?.cancel()
        pendingPlaybackResumeTimeout = nil
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
        pendingReplay = nil
        stopPlaybackProgressHeartbeat()
        playbackEndObservationID = UUID()
        playbackEndObserver?.cancel()
        playbackEndObserver = nil
        playbackResumeObserver?.cancel()
        playbackResumeObserver = nil
        playbackTimeJumpObserver?.cancel()
        playbackTimeJumpObserver = nil
        playbackProgressEndedItem = nil
        playbackProgressEndedTime = nil
    }

    private func stopPlaybackProgressHeartbeat() {
        playbackProgressReportTask?.cancel()
        playbackProgressReportTask = nil
    }

    private func observePlaybackEnd(for item: AVPlayerItem?) {
        let observationID = UUID()
        playbackEndObservationID = observationID
        guard let item else {
            return
        }

        playbackEndObserver = NotificationCenter.default
            .publisher(for: .AVPlayerItemDidPlayToEndTime, object: item)
            .sink { [weak self] notification in
                let endedItem = notification.object as? AVPlayerItem
                let observedEndTime = endedItem?.currentTime() ?? .invalid
                Task { @MainActor [weak self, endedItem] in
                    guard let self, let endedItem, let player = self.player,
                        player.currentItem === endedItem, self.playbackEndObservationID == observationID,
                        !Self.hasMovedAwayFromEnd(
                            currentTime: player.currentTime(), endTime: observedEndTime, duration: player.itemDuration)
                    else {
                        return
                    }
                    self.finishPlaybackProgressReporting(for: endedItem, completed: true)
                }
            }
    }

    private func observePlaybackResume(for player: AVPlayer) {
        playbackResumeObserver = player.publisher(for: \.timeControlStatus)
            .sink { [weak self, weak player] status in
                guard status == .playing else {
                    return
                }

                Task { @MainActor [weak self, weak player] in
                    guard let self, let player, self.player === player, player.timeControlStatus == .playing else {
                        return
                    }
                    self.handlePlaybackTimeJump(using: player)
                }
            }
    }

    private func observePlaybackTimeJumps(for item: AVPlayerItem?) {
        guard let item else { return }
        playbackTimeJumpObserver = NotificationCenter.default
            .publisher(for: .AVPlayerItemTimeJumped, object: item)
            .sink { [weak self, weak item] _ in
                Task { @MainActor [weak self, weak item] in
                    guard let self, let item, self.player?.currentItem === item else { return }
                    self.handlePlaybackTimeJump(using: self.player)
                }
            }
    }

    private func hasMovedAwayFromPlaybackEnd(using controller: any PlayerPlaybackControlling) -> Bool {
        guard let item = controller.currentItem, playbackProgressEndedItem === item,
            let endTime = playbackProgressEndedTime
        else { return false }
        return Self.hasMovedAwayFromEnd(
            currentTime: controller.currentTime(), endTime: endTime, duration: controller.itemDuration)
    }

    static func hasMovedAwayFromEnd(currentTime: CMTime, endTime: CMTime, duration: CMTime) -> Bool {
        endTime.isNumeric && CMTimeCompare(endTime, .zero) > 0
            && currentTime.isNumeric && CMTimeCompare(currentTime, .zero) >= 0
            && CMTimeCompare(currentTime, endTime) < 0
            && !Self.isAtPlaybackEnd(currentTime: currentTime, duration: duration)
    }

    func handlePlaybackTimeJump(using controller: (any PlayerPlaybackControlling)?) {
        guard let controller, let player, let item = controller.currentItem,
            player.currentItem === item, hasMovedAwayFromPlaybackEnd(using: controller)
        else { return }
        if let pendingReplay, pendingReplay.player === player, pendingReplay.item === item,
            CMTimeCompare(controller.currentTime(), .zero) == 0
        {
            return
        }

        let position = controller.currentTime().seconds
        let duration = Self.playbackDurationSeconds(controller.itemDuration)
        let intent: PlaybackProgressIntent = controller.timeControlStatus == .playing ? .playing : .paused
        markManualInteraction()
        queuePlaybackProgressReport(positionSeconds: position, durationSeconds: duration, intent: .seek)
        restartPlaybackProgressReportingAfterEndIfNeeded(
            immediateIntent: intent, positionSeconds: position, durationSeconds: duration)
        statusMessage = intent == .playing ? "Playing \(loadedURL?.absoluteString ?? "stream")." : "Playback paused."
    }

    private func finishPlaybackProgressReporting(
        for item: AVPlayerItem,
        terminalTime: CMTime? = nil,
        durationSeconds: Double? = nil,
        completed: Bool? = nil
    ) {
        guard player?.currentItem === item, playbackProgressEndedItem !== item else {
            return
        }
        let queuedStoppedReport: Bool
        if let terminalTime {
            queuePlaybackProgressReport(
                positionSeconds: terminalTime.seconds,
                durationSeconds: durationSeconds,
                intent: .stopped,
                completed: completed
            )
            queuedStoppedReport = playbackProgressContext != nil
        } else {
            queuedStoppedReport = queueCurrentPlaybackProgressReport(intent: .stopped, completed: completed)
        }
        playbackProgressEndedItem = item
        let endTime = terminalTime ?? player?.currentTime() ?? .invalid
        playbackProgressEndedTime = endTime.isNumeric && CMTimeCompare(endTime, .zero) > 0 ? endTime : nil
        stopPlaybackProgressHeartbeat()
        playbackEndObservationID = UUID()
        playbackEndObserver?.cancel()
        playbackEndObserver = nil
        statusMessage = "Playback finished."
        requestPlaybackProgressStatusRefreshIfNeeded(queuedStoppedReport)
    }

    private func restartPlaybackProgressReportingAfterEndIfNeeded(
        immediateIntent: PlaybackProgressIntent?, positionSeconds: Double? = nil, durationSeconds: Double? = nil
    ) {
        guard let player,
            let currentItem = player.currentItem,
            playbackProgressEndedItem === currentItem
        else {
            return
        }

        playbackProgressEndedItem = nil
        playbackProgressEndedTime = nil
        observePlaybackEnd(for: currentItem)
        if let immediateIntent {
            if let positionSeconds {
                queuePlaybackProgressReport(
                    positionSeconds: positionSeconds,
                    durationSeconds: durationSeconds ?? currentPlaybackDurationSeconds(),
                    intent: immediateIntent
                )
            } else {
                queueCurrentPlaybackProgressReport(intent: immediateIntent)
            }
        }
        startPlaybackProgressReporting()
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

    @discardableResult
    private func queueCurrentPlaybackProgressReport(
        intent: PlaybackProgressIntent,
        completed: Bool? = nil
    ) -> Bool {
        guard let report = currentPlaybackProgressReport(intent: intent),
            let endpoint = playbackProgressContext?.endpoint
        else {
            return false
        }

        queuePlaybackProgressReport(report, endpoint: endpoint, completed: completed)
        return true
    }

    private func requestPlaybackProgressStatusRefreshIfNeeded(_ shouldRefresh: Bool) {
        guard shouldRefresh else {
            return
        }
        playbackProgressStatusRefreshRequestID += 1
    }

    private func queuePlaybackProgressReport(
        positionSeconds: Double,
        durationSeconds: Double?,
        intent: PlaybackProgressIntent,
        completed: Bool? = nil
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
        queuePlaybackProgressReport(report, endpoint: context.endpoint, completed: completed)
    }

    @discardableResult
    private func queuePlaybackProgressReport(
        _ report: PlaybackProgressReport,
        endpoint: CacheServerEndpoint,
        updatesReportingMessage: Bool = false,
        completed: Bool? = nil
    ) -> Task<PlaybackProgressReportResult?, Never> {
        saveLocalPlaybackCheckpoint(from: report, completed: completed)
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

    private func saveLocalPlaybackCheckpoint(
        from report: PlaybackProgressReport,
        completed explicitCompletion: Bool?
    ) {
        guard !isCurrentResumePositionUnrestored,
            let identity = playbackProgressContext?.resumptionIdentity,
            let resumptionStore
        else {
            return
        }

        let completed: Bool
        if let explicitCompletion {
            completed = explicitCompletion
        } else if report.intent == .started || report.intent == .playing || report.intent == .seek {
            completed = false
        } else {
            completed =
                resumptionStore.record?.playback?.identity == identity
                ? (resumptionStore.record?.playback?.completed ?? false) : false
        }
        resumptionStore.updatePlayback(
            identity: identity,
            positionSeconds: report.positionSeconds,
            durationSeconds: report.durationSeconds,
            completed: completed
        )
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
        guard let controller = playbackController else {
            return 0
        }

        let seconds = controller.currentTime().seconds
        return seconds.isFinite && seconds > 0 ? seconds : 0
    }

    private func currentPlaybackDurationSeconds() -> Double? {
        guard let controller = playbackController else {
            return nil
        }

        return Self.playbackDurationSeconds(controller.itemDuration)
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
