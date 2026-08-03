import Combine
import Foundation
import TVOSNetPlayerCacheClient

public enum CacheServerDiagnosticsRefreshResult: Equatable, Sendable {
    case succeeded
    case failed
    case superseded
}

public enum CacheServerDiagnosticSeverity: String, Equatable, Sendable {
    case ready
    case info
    case warning
    case error
    case unknown
}

public struct CacheServerDiagnosticRow: Identifiable, Equatable, Sendable {
    public let id: String
    public let title: String
    public let value: String
    public let detail: String?
    public let systemImage: String
    public let severity: CacheServerDiagnosticSeverity

    public init(
        id: String,
        title: String,
        value: String,
        detail: String? = nil,
        systemImage: String,
        severity: CacheServerDiagnosticSeverity
    ) {
        self.id = id
        self.title = title
        self.value = value
        self.detail = detail
        self.systemImage = systemImage
        self.severity = severity
    }
}

public struct CacheServerDiagnosticsSnapshot: Equatable, Sendable {
    public let endpoint: CacheServerEndpoint
    public let serverInfo: CacheServerSummary
    public let healthStatus: CacheHealthStatus?
    public let healthWarning: String?
    public let credentialStatus: BilibiliCredentialStatus?
    public let credentialWarning: String?
    public let cacheRoots: [CacheRoot]
    public let cacheRootsWarning: String?
    public let hlsCacheStatus: HLSCacheStatus?
    public let hlsCacheWarning: String?
    public let checkedAt: Date
    public let rows: [CacheServerDiagnosticRow]

    public init(
        endpoint: CacheServerEndpoint,
        serverInfo: CacheServerSummary,
        healthStatus: CacheHealthStatus?,
        healthWarning: String?,
        credentialStatus: BilibiliCredentialStatus?,
        credentialWarning: String?,
        cacheRoots: [CacheRoot],
        cacheRootsWarning: String?,
        hlsCacheStatus: HLSCacheStatus?,
        hlsCacheWarning: String?,
        checkedAt: Date = Date()
    ) {
        self.endpoint = endpoint
        self.serverInfo = serverInfo
        self.healthStatus = healthStatus
        self.healthWarning = healthWarning
        self.credentialStatus = credentialStatus
        self.credentialWarning = credentialWarning
        self.cacheRoots = cacheRoots
        self.cacheRootsWarning = cacheRootsWarning
        self.hlsCacheStatus = hlsCacheStatus
        self.hlsCacheWarning = hlsCacheWarning
        self.checkedAt = checkedAt
        rows = Self.buildRows(
            endpoint: endpoint,
            serverInfo: serverInfo,
            healthStatus: healthStatus,
            healthWarning: healthWarning,
            credentialStatus: credentialStatus,
            credentialWarning: credentialWarning,
            cacheRoots: cacheRoots,
            cacheRootsWarning: cacheRootsWarning,
            hlsCacheStatus: hlsCacheStatus,
            hlsCacheWarning: hlsCacheWarning
        )
    }

    public var serverDisplayName: String {
        serverInfo.name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            ? endpoint.displayAddress
            : serverInfo.name
    }

    public var issueCount: Int {
        rows.filter(\.countsAsIssue).count
    }

    private static func buildRows(
        endpoint: CacheServerEndpoint,
        serverInfo: CacheServerSummary,
        healthStatus: CacheHealthStatus?,
        healthWarning: String?,
        credentialStatus: BilibiliCredentialStatus?,
        credentialWarning: String?,
        cacheRoots: [CacheRoot],
        cacheRootsWarning: String?,
        hlsCacheStatus: HLSCacheStatus?,
        hlsCacheWarning: String?
    ) -> [CacheServerDiagnosticRow] {
        var rows = [
            serverRow(endpoint: endpoint, serverInfo: serverInfo),
            healthRow(healthStatus, warning: healthWarning),
            capabilityRow(serverInfo),
            credentialRow(credentialStatus, warning: credentialWarning, serverInfo: serverInfo),
            liveValidationRow(credentialStatus, warning: credentialWarning, serverInfo: serverInfo),
            cacheRootsRow(cacheRoots, warning: cacheRootsWarning),
            hlsCacheRow(hlsCacheStatus, warning: hlsCacheWarning),
        ]

        if let weakNetwork = hlsCacheStatus?.weakNetwork {
            rows.append(weakNetworkRow(weakNetwork))
        }
        if serverInfo.supportsLanTranscoding {
            rows.append(transcodingRow(hlsCacheStatus?.transcoding))
        }
        if let playback = hlsCacheStatus?.playback, playback.isActive || playback.isRecentlyStopped {
            rows.append(playbackRow(playback))
        }

        return rows
    }

    private static func serverRow(
        endpoint: CacheServerEndpoint,
        serverInfo: CacheServerSummary
    ) -> CacheServerDiagnosticRow {
        let version = serverInfo.version.trimmingCharacters(in: .whitespacesAndNewlines)
        let mediaBaseCount = serverInfo.mediaBaseURIs.count
        let detailParts = [
            version.isEmpty ? nil : "Version \(version)",
            mediaBaseCount == 1 ? "1 media base URI" : "\(mediaBaseCount) media base URIs",
        ].compactMap(\.self)
        return CacheServerDiagnosticRow(
            id: "server",
            title: "Server",
            value: serverInfo.name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                ? endpoint.displayAddress
                : serverInfo.name,
            detail: detailParts.joined(separator: " · "),
            systemImage: endpoint.usesTLS ? "lock.shield" : "network",
            severity: .ready
        )
    }

    private static func healthRow(
        _ status: CacheHealthStatus?,
        warning: String?
    ) -> CacheServerDiagnosticRow {
        guard let status else {
            return CacheServerDiagnosticRow(
                id: "health",
                title: "Health",
                value: "Unknown",
                detail: warning,
                systemImage: "heart.text.square",
                severity: .unknown
            )
        }

        let message = trimmed(status.message)
        if status.isServing {
            return CacheServerDiagnosticRow(
                id: "health",
                title: "Health",
                value: "Serving",
                detail: message,
                systemImage: "checkmark.circle",
                severity: .ready
            )
        }
        if status.isDegraded {
            return CacheServerDiagnosticRow(
                id: "health",
                title: "Health",
                value: "Degraded",
                detail: message,
                systemImage: "exclamationmark.triangle",
                severity: .warning
            )
        }
        if status.isNotServing {
            return CacheServerDiagnosticRow(
                id: "health",
                title: "Health",
                value: "Not serving",
                detail: message,
                systemImage: "xmark.octagon",
                severity: .error
            )
        }

        return CacheServerDiagnosticRow(
            id: "health",
            title: "Health",
            value: "Unknown",
            detail: message,
            systemImage: "questionmark.circle",
            severity: .unknown
        )
    }

    private static func capabilityRow(_ serverInfo: CacheServerSummary) -> CacheServerDiagnosticRow {
        let highlights = [
            capabilityLabel("Bilibili", isReady: serverInfo.supportsBilibiliResolve),
            capabilityLabel("Selection", isReady: serverInfo.supportsBilibiliTaskSelection),
            capabilityLabel("Credentials", isReady: serverInfo.supportsBilibiliCredentialStatus),
            capabilityLabel("Profiles", isReady: serverInfo.supportsBilibiliCredentialProfiles),
            capabilityLabel("Transcoding", isReady: serverInfo.supportsLanTranscoding),
            capabilityLabel("Delete", isReady: serverInfo.supportsLibraryItemDelete),
        ]

        return CacheServerDiagnosticRow(
            id: "capabilities",
            title: "Capabilities",
            value: "\(serverInfo.capabilities.count) advertised",
            detail: highlights.joined(separator: " · "),
            systemImage: "switch.2",
            severity: .info
        )
    }

    private static func credentialRow(
        _ status: BilibiliCredentialStatus?,
        warning: String?,
        serverInfo: CacheServerSummary
    ) -> CacheServerDiagnosticRow {
        guard serverInfo.supportsBilibiliCredentialStatus else {
            return CacheServerDiagnosticRow(
                id: "credentials",
                title: "Bilibili Credentials",
                value: "Not reported",
                detail: "This server does not advertise credential status.",
                systemImage: "person.badge.key",
                severity: .unknown
            )
        }

        guard let status else {
            return CacheServerDiagnosticRow(
                id: "credentials",
                title: "Bilibili Credentials",
                value: "Unknown",
                detail: warning,
                systemImage: "person.badge.key",
                severity: .unknown
            )
        }

        let normalizedState = status.state.diagnosticsNormalizedToken.removingBilibiliCredentialStatePrefix
        let value: String
        let severity: CacheServerDiagnosticSeverity
        switch normalizedState {
        case "ready":
            value = "Ready"
            severity = .ready
        case "degraded":
            value = "Degraded"
            severity = .warning
        case "error":
            value = "Error"
            severity = .error
        case "notconfigured":
            value = "Not configured"
            severity = .warning
        default:
            value = "Unknown"
            severity = .unknown
        }

        return CacheServerDiagnosticRow(
            id: "credentials",
            title: "Bilibili Credentials",
            value: value,
            detail: credentialDetail(status),
            systemImage: "person.badge.key",
            severity: severity
        )
    }

    private static func cacheRootsRow(
        _ roots: [CacheRoot],
        warning: String?
    ) -> CacheServerDiagnosticRow {
        guard warning == nil else {
            return CacheServerDiagnosticRow(
                id: "cacheRoots",
                title: "Cache Storage",
                value: "Unavailable",
                detail: warning,
                systemImage: "externaldrive.badge.questionmark",
                severity: .unknown
            )
        }
        guard !roots.isEmpty else {
            return CacheServerDiagnosticRow(
                id: "cacheRoots",
                title: "Cache Storage",
                value: "No roots",
                detail: "The server did not report a cache root.",
                systemImage: "externaldrive.badge.xmark",
                severity: .warning
            )
        }

        let writableCount = roots.filter(\.writable).count
        let freeBytes = roots.reduce(Int64(0)) { $0 + max(0, $1.freeBytes) }
        let totalBytes = roots.reduce(Int64(0)) { $0 + max(0, $1.totalBytes) }
        let value = roots.count == 1 ? "1 root" : "\(roots.count) roots"
        let detail: String
        if totalBytes > 0 {
            let free = ByteCountFormatter.string(fromByteCount: freeBytes, countStyle: .file)
            let total = ByteCountFormatter.string(fromByteCount: totalBytes, countStyle: .file)
            detail = "\(free) free of \(total) · \(writableCount) writable"
        } else {
            detail = "\(writableCount) writable"
        }

        return CacheServerDiagnosticRow(
            id: "cacheRoots",
            title: "Cache Storage",
            value: value,
            detail: detail,
            systemImage: writableCount > 0 ? "externaldrive.fill" : "externaldrive.badge.xmark",
            severity: writableCount > 0 ? .ready : .warning
        )
    }

    private static func liveValidationRow(
        _ credentialStatus: BilibiliCredentialStatus?,
        warning: String?,
        serverInfo: CacheServerSummary
    ) -> CacheServerDiagnosticRow {
        guard serverInfo.supportsBilibiliResolve else {
            return CacheServerDiagnosticRow(
                id: "liveValidation",
                title: "Live Validation",
                value: "Unavailable",
                detail: "Bilibili resolve is not advertised by this server.",
                systemImage: "checklist",
                severity: .unknown
            )
        }

        guard serverInfo.supportsBilibiliCredentialStatus else {
            return CacheServerDiagnosticRow(
                id: "liveValidation",
                title: "Live Validation",
                value: "Public only",
                detail: "Default public Bilibili cases can run; authenticated/restricted cases need credential status.",
                systemImage: "checklist",
                severity: .info
            )
        }

        guard serverInfo.supportsBilibiliTaskSelection else {
            return CacheServerDiagnosticRow(
                id: "liveValidation",
                title: "Live Validation",
                value: "Selection unavailable",
                detail: "Live validation needs Bilibili task selection capability.",
                systemImage: "checklist",
                severity: .unknown
            )
        }

        guard let credentialStatus else {
            return CacheServerDiagnosticRow(
                id: "liveValidation",
                title: "Live Validation",
                value: "Unknown",
                detail: warning,
                systemImage: "checklist",
                severity: .unknown
            )
        }

        let hasAuthenticatedCredential =
            credentialStatus.credentialFileLoaded && credentialStatus.hasWebCookie
        let hasRestrictedProxy = credentialStatus.restrictedAPIProxyCount > 0

        if hasAuthenticatedCredential && hasRestrictedProxy {
            return CacheServerDiagnosticRow(
                id: "liveValidation",
                title: "Live Validation",
                value: "Restricted-ready",
                detail: "Authenticated and restricted Bilibili live cases have required local readiness signals.",
                systemImage: "checklist.checked",
                severity: .ready
            )
        }
        if hasAuthenticatedCredential {
            return CacheServerDiagnosticRow(
                id: "liveValidation",
                title: "Live Validation",
                value: "Authenticated-ready",
                detail: "Restricted Bangumi validation still needs a restricted API proxy.",
                systemImage: "checklist",
                severity: .warning
            )
        }

        return CacheServerDiagnosticRow(
            id: "liveValidation",
            title: "Live Validation",
            value: "Public only",
            detail: "Authenticated and restricted live cases need a loaded web cookie.",
            systemImage: "checklist",
            severity: .info
        )
    }

    private static func hlsCacheRow(
        _ status: HLSCacheStatus?,
        warning: String?
    ) -> CacheServerDiagnosticRow {
        guard let status else {
            return CacheServerDiagnosticRow(
                id: "hlsCache",
                title: "HLS Cache",
                value: "Unknown",
                detail: warning,
                systemImage: "film.stack",
                severity: .unknown
            )
        }

        let used = ByteCountFormatter.string(fromByteCount: status.usedBytes, countStyle: .file)
        let value: String
        if status.maxBytes > 0 {
            let maxBytes = ByteCountFormatter.string(fromByteCount: status.maxBytes, countStyle: .file)
            value = "\(used) of \(maxBytes)"
        } else {
            value = used
        }

        var detailParts: [String] = []
        if status.evictionEnabled {
            detailParts.append(
                "Cleanup \(status.highWatermarkPercent)% -> \(status.lowWatermarkPercent)%"
            )
        } else {
            detailParts.append("Automatic cleanup disabled")
        }
        detailParts.append("\(status.completedSessionCount) completed HLS sessions")
        if let eviction = status.lastEviction {
            let evicted = ByteCountFormatter.string(fromByteCount: eviction.evictedBytes, countStyle: .file)
            detailParts.append("Last cleanup freed \(evicted)")
        }
        let storagePressureBadge = HLSCacheStatusPresentation.storagePressureBadge(for: status)
        if let detail = storagePressureBadge?.detail {
            detailParts.append(detail)
        }

        let severity =
            storagePressureBadge.map { HLSCacheStatusPresentation.diagnosticSeverity(for: $0.tone) } ?? .ready

        return CacheServerDiagnosticRow(
            id: "hlsCache",
            title: "HLS Cache",
            value: value,
            detail: detailParts.joined(separator: " · "),
            systemImage: storagePressureBadge?.systemImage ?? "film.stack",
            severity: severity
        )
    }

    private static func weakNetworkRow(_ status: HLSWeakNetworkStatus) -> CacheServerDiagnosticRow {
        HLSCacheStatusPresentation.weakNetworkDiagnosticRow(status)
    }

    private static func transcodingRow(_ status: LanTranscodingStatus?) -> CacheServerDiagnosticRow {
        guard let status else {
            return CacheServerDiagnosticRow(
                id: "transcoding",
                title: "LAN Transcoding",
                value: "Unknown",
                detail: "The server advertises transcoding but did not report runtime state.",
                systemImage: "slider.horizontal.3",
                severity: .unknown
            )
        }

        let normalizedState = status.state.diagnosticsNormalizedToken.removingLanTranscodingRuntimeStatePrefix
        let value: String
        let severity: CacheServerDiagnosticSeverity
        if !status.enabled || normalizedState == "disabled" {
            value = "Disabled"
            severity = .info
        } else if normalizedState == "idle" {
            value = "Ready"
            severity = .ready
        } else if normalizedState == "busy" {
            value = "Busy"
            severity = .info
        } else {
            value = "Unknown"
            severity = .unknown
        }

        let detail = [
            status.profileID.isEmpty ? nil : "Profile \(status.profileID)",
            !normalizedState.isEmpty && normalizedState != "idle" && normalizedState != "busy"
                && normalizedState != "disabled"
                ? "State \(status.state)" : nil,
            "\(status.activeJobCount)/\(status.maxConcurrentJobs) jobs",
            status.targetVideoCodec.isEmpty ? nil : "\(status.targetVideoCodec)/\(status.targetAudioCodec)",
        ].compactMap(\.self).joined(separator: " · ")

        return CacheServerDiagnosticRow(
            id: "transcoding",
            title: "LAN Transcoding",
            value: value,
            detail: detail.isEmpty ? trimmed(status.message) : detail,
            systemImage: "slider.horizontal.3",
            severity: severity
        )
    }

    private static func playbackRow(_ status: HLSPlaybackProgressStatus) -> CacheServerDiagnosticRow {
        CacheServerDiagnosticRow(
            id: "playbackProgress",
            title: "Playback Signal",
            value: status.isActive ? "Active" : "Recently stopped",
            detail: status.positionLabel ?? trimmed(status.message),
            systemImage: status.isActive ? "play.circle" : "pause.circle",
            severity: .info
        )
    }

    private static func credentialDetail(_ status: BilibiliCredentialStatus) -> String {
        var parts: [String] = []
        if !status.activeProfileID.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            parts.append("Profile \(status.activeProfileID)")
        }
        if status.profileCount > 1 {
            parts.append("\(status.profileCount) profiles")
        }
        parts.append(status.credentialFileLoaded ? "Credential file loaded" : "Credential file not loaded")
        parts.append(status.hasWebCookie ? "Web cookie ready" : "Web cookie missing")
        parts.append(status.hasAccessKey ? "Access key ready" : "Access key missing")
        if status.hasTVAccessKey {
            parts.append("TV access key ready")
        }
        let restrictedArea = status.restrictedArea.trimmingCharacters(in: .whitespacesAndNewlines)
        if !restrictedArea.isEmpty {
            parts.append("Restricted area \(restrictedArea)")
        }
        parts.append("\(status.restrictedPlayURLProxyCount) playurl proxies")
        parts.append("\(status.restrictedAPIProxyCount) API proxies")
        if let message = trimmed(status.message) {
            parts.append(message)
        }
        return parts.joined(separator: " · ")
    }

    private static func capabilityLabel(_ name: String, isReady: Bool) -> String {
        "\(name) \(isReady ? "ready" : "missing")"
    }

    private static func trimmed(_ text: String) -> String? {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }
}

private extension CacheServerDiagnosticRow {
    var countsAsIssue: Bool {
        switch severity {
        case .warning, .error:
            return true
        case .unknown:
            return detail?.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty == false
        case .ready, .info:
            return false
        }
    }
}

@MainActor
public final class CacheServerDiagnosticsViewModel: ObservableObject {
    private static let addressGuidance =
        "Use a cache server address or URL, such as mac-mini.local:50051 or https://cache.example.com."

    @Published public private(set) var serverAddressText: String
    @Published public private(set) var statusMessage: String = "Diagnostics not loaded."
    @Published public private(set) var errorMessage: String?
    @Published public private(set) var isRefreshing = false
    @Published public private(set) var snapshot: CacheServerDiagnosticsSnapshot?

    private let clientFactory: @Sendable (CacheServerEndpoint) -> any CacheControlClient
    private let operationTimeout: Duration
    private var refreshSequence = 0

    public init(
        defaultServerAddressText: String = "",
        operationTimeout: Duration = .seconds(10),
        clientFactory: @escaping @Sendable (CacheServerEndpoint) -> any CacheControlClient = {
            GRPCCacheControlClient(endpoint: $0)
        }
    ) {
        serverAddressText = defaultServerAddressText
        self.operationTimeout = operationTimeout
        self.clientFactory = clientFactory
    }

    public var canRefresh: Bool {
        !serverAddressText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && !isRefreshing
    }

    public var rows: [CacheServerDiagnosticRow] {
        snapshot?.rows ?? []
    }

    public func useServerAddressText(_ text: String) {
        let previousAddressText = serverAddressText.trimmingCharacters(in: .whitespacesAndNewlines)
        let nextAddressText = text.trimmingCharacters(in: .whitespacesAndNewlines)
        let previousEndpoint = CacheServerEndpoint.normalized(from: serverAddressText)
        let nextEndpoint = CacheServerEndpoint.normalized(from: text)
        serverAddressText = text
        if previousEndpoint == nextEndpoint,
            nextEndpoint != nil || previousAddressText == nextAddressText
        {
            return
        }

        refreshSequence += 1
        isRefreshing = false
        snapshot = nil
        errorMessage = nil
        statusMessage =
            text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            ? "Set a cache server address to run diagnostics."
            : "Diagnostics not loaded."
    }

    @discardableResult
    public func refresh(serverAddressText: String) async -> CacheServerDiagnosticsRefreshResult {
        useServerAddressText(serverAddressText)
        return await refresh()
    }

    @discardableResult
    public func refresh() async -> CacheServerDiagnosticsRefreshResult {
        refreshSequence += 1
        let requestSequence = refreshSequence
        let requestedAddress = serverAddressText

        guard let endpoint = CacheServerEndpoint.normalized(from: requestedAddress) else {
            snapshot = nil
            errorMessage = Self.addressGuidance
            statusMessage = "Cache server address is invalid."
            return .failed
        }

        isRefreshing = true
        errorMessage = nil
        statusMessage = "Checking diagnostics for \(endpoint.displayAddress)..."

        let client = clientFactory(endpoint)
        let timeout = operationTimeout
        do {
            let serverInfo = try await Self.withOperationTimeout(timeout) {
                try await client.getServerInfo()
            }

            let health = await Self.loadOptionalDiagnostic("Health check") {
                try await Self.withOperationTimeout(timeout) {
                    try await client.checkHealth()
                }
            }
            let credentials = await Self.loadOptionalDiagnostic("Bilibili credential status") {
                guard serverInfo.supportsBilibiliCredentialStatus else {
                    throw CacheControlClientUnsupportedFeature.bilibiliCredentialStatus
                }
                return try await Self.withOperationTimeout(timeout) {
                    try await client.getBilibiliCredentialStatus()
                }
            }
            let cacheRoots = await Self.loadOptionalDiagnostic("Cache roots") {
                try await Self.withOperationTimeout(timeout) {
                    try await client.listCacheRoots()
                }
            }
            let hlsCache = await Self.loadOptionalDiagnostic("HLS cache status") {
                try await Self.withOperationTimeout(timeout) {
                    try await client.getHLSCacheStatus()
                }
            }

            guard isCurrentRefresh(requestSequence, endpoint: endpoint) else {
                return .superseded
            }

            let snapshot = CacheServerDiagnosticsSnapshot(
                endpoint: endpoint,
                serverInfo: serverInfo,
                healthStatus: health.value,
                healthWarning: health.warning,
                credentialStatus: credentials.value,
                credentialWarning: credentials.warning,
                cacheRoots: cacheRoots.value ?? [],
                cacheRootsWarning: cacheRoots.warning,
                hlsCacheStatus: hlsCache.value,
                hlsCacheWarning: hlsCache.warning
            )
            self.snapshot = snapshot
            serverAddressText = endpoint.displayAddress
            let issueCount = snapshot.issueCount
            statusMessage =
                issueCount == 0
                ? "Diagnostics checked for \(snapshot.serverDisplayName)."
                : "Diagnostics checked for \(snapshot.serverDisplayName) with \(issueCount) issue(s)."
            isRefreshing = false
            return .succeeded
        } catch {
            guard isCurrentRefresh(requestSequence, endpoint: endpoint) else {
                return .superseded
            }

            snapshot = nil
            errorMessage = error.localizedDescription
            statusMessage = "Could not check cache server diagnostics."
            isRefreshing = false
            return .failed
        }
    }

    private func isCurrentRefresh(_ requestSequence: Int, endpoint: CacheServerEndpoint) -> Bool {
        requestSequence == refreshSequence && CacheServerEndpoint.normalized(from: serverAddressText) == endpoint
    }

    private static func loadOptionalDiagnostic<Value: Sendable>(
        _ featureName: String,
        operation: @Sendable @escaping () async throws -> Value
    ) async -> OptionalDiagnostic<Value> {
        do {
            return .value(try await operation())
        } catch {
            return .warning("\(featureName): \(error.localizedDescription)")
        }
    }

    private static func withOperationTimeout<Value: Sendable>(
        _ timeout: Duration,
        operation: @Sendable @escaping () async throws -> Value
    ) async throws -> Value {
        try await withCheckedThrowingContinuation { continuation in
            let race = CacheServerDiagnosticsOperationTimeoutRace(continuation: continuation)
            race.start(timeout: timeout, operation: operation)
        }
    }
}

private enum OptionalDiagnostic<Value: Sendable>: Sendable {
    case value(Value)
    case warning(String)

    var value: Value? {
        switch self {
        case .value(let value):
            return value
        case .warning:
            return nil
        }
    }

    var warning: String? {
        switch self {
        case .value:
            return nil
        case .warning(let message):
            return message
        }
    }
}

private final class CacheServerDiagnosticsOperationTimeoutRace<Value: Sendable>: @unchecked Sendable {
    private let lock = NSLock()
    private var continuation: CheckedContinuation<Value, Error>?
    private var operationTask: Task<Void, Never>?
    private var timeoutTask: Task<Void, Never>?

    init(continuation: CheckedContinuation<Value, Error>) {
        self.continuation = continuation
    }

    func start(
        timeout: Duration,
        operation: @Sendable @escaping () async throws -> Value
    ) {
        let operationTask = Task.detached {
            do {
                self.complete(.success(try await operation()))
            } catch {
                self.complete(.failure(error))
            }
        }
        let timeoutTask = Task.detached {
            do {
                try await Task.sleep(for: timeout)
                self.complete(.failure(CacheServerDiagnosticsOperationError.timedOut))
            } catch {
                // The timeout task is expected to be cancelled when the operation wins.
            }
        }

        lock.lock()
        if continuation == nil {
            lock.unlock()
            operationTask.cancel()
            timeoutTask.cancel()
            return
        }

        self.operationTask = operationTask
        self.timeoutTask = timeoutTask
        lock.unlock()
    }

    private func complete(_ result: Result<Value, Error>) {
        lock.lock()
        guard let continuation else {
            lock.unlock()
            return
        }

        self.continuation = nil
        let operationTask = operationTask
        let timeoutTask = timeoutTask
        self.operationTask = nil
        self.timeoutTask = nil
        lock.unlock()

        operationTask?.cancel()
        timeoutTask?.cancel()
        continuation.resume(with: result)
    }
}

private enum CacheServerDiagnosticsOperationError: LocalizedError {
    case timedOut

    var errorDescription: String? {
        "Cache server diagnostics request timed out."
    }
}

private extension String {
    var diagnosticsNormalizedToken: String {
        lowercased().filter(\.isLetter)
    }

    var removingBilibiliCredentialStatePrefix: String {
        removingDiagnosticsPrefix("bilibilicredentialstate")
    }

    var removingLanTranscodingRuntimeStatePrefix: String {
        removingDiagnosticsPrefix("lantranscodingruntimestate")
    }

    func removingDiagnosticsPrefix(_ prefix: String) -> String {
        guard hasPrefix(prefix) else {
            return self
        }

        return String(dropFirst(prefix.count))
    }
}
