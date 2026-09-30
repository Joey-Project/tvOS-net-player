import Combine
import Foundation
import TVOSNetPlayerCacheClient

public enum BilibiliLoginStatus: Equatable, Sendable {
    case disconnected
    case checking
    case credentialStatusUnavailable
    case loginSessionsUnavailable
    case credentialPathMissing
    case authenticated
    case loginRequired
    case sessionPending
    case sessionExpired
    case failed
}

private enum BilibiliLoginViewModelError: Error {
    case timedOut
}

@MainActor
public final class BilibiliLoginViewModel: ObservableObject {
    @Published public private(set) var status: BilibiliLoginStatus = .disconnected
    @Published public private(set) var statusMessage = "Connect to a cache server to check Bilibili login."
    @Published public private(set) var verificationQRPayload: String?
    @Published public private(set) var isStartingLogin = false

    private let clientFactory: @Sendable (CacheServerEndpoint) -> any CacheControlClient
    private let operationTimeout: Duration
    private let pollInterval: Duration
    private let maximumPollingDuration: Duration
    private var serverAddressText = ""
    private var active = false
    private var operationSequence = 0
    private var pollingGeneration = 0
    private var pollingTask: Task<Void, Never>?
    private var activeSession: BilibiliLoginSession?
    private var activeProfileID = ""
    private var supportsLoginSessions = false
    private var credentialAllowsLogin = false

    public init(
        operationTimeout: Duration = .seconds(10),
        clientFactory: @escaping @Sendable (CacheServerEndpoint) -> any CacheControlClient = {
            GRPCCacheControlClient(endpoint: $0)
        }
    ) {
        self.operationTimeout = operationTimeout
        pollInterval = .seconds(2)
        maximumPollingDuration = .seconds(180)
        self.clientFactory = clientFactory
    }

    init(
        operationTimeout: Duration = .seconds(10),
        pollInterval: Duration,
        maximumPollingDuration: Duration = .seconds(180),
        clientFactory: @escaping @Sendable (CacheServerEndpoint) -> any CacheControlClient
    ) {
        self.operationTimeout = operationTimeout
        self.pollInterval = pollInterval
        self.maximumPollingDuration = maximumPollingDuration
        self.clientFactory = clientFactory
    }

    deinit {
        pollingTask?.cancel()
    }

    public var canStartLogin: Bool {
        active
            && (status == .loginRequired || status == .sessionExpired || status == .failed)
            && supportsLoginSessions
            && credentialAllowsLogin
            && !isStartingLogin
    }

    public func activate(serverAddressText: String) async {
        let previousEndpoint = CacheServerEndpoint.normalized(from: self.serverAddressText)
        let nextEndpoint = CacheServerEndpoint.normalized(from: serverAddressText)
        let changedAddress =
            previousEndpoint != nextEndpoint
            || (previousEndpoint == nil && self.serverAddressText != serverAddressText)
        self.serverAddressText = serverAddressText
        active = true
        if changedAddress {
            stopPolling()
            activeSession = nil
            verificationQRPayload = nil
        } else if let session = activeSession, status == .sessionPending {
            if session.expiresAt.map({ $0 > Date() }) ?? true {
                startPolling()
                return
            }
            status = .sessionExpired
            statusMessage = "The Bilibili login QR code expired. Start a new login session."
            activeSession = nil
            verificationQRPayload = nil
        }
        await refreshCredentialStatus()
        if activeSession != nil {
            startPolling()
        }
    }

    public func deactivate() {
        active = false
        stopPolling()
    }

    public func startLogin() async {
        guard canStartLogin,
            let endpoint = CacheServerEndpoint.normalized(from: serverAddressText)
        else {
            return
        }

        operationSequence += 1
        let requestSequence = operationSequence
        isStartingLogin = true
        statusMessage = "Starting Bilibili Web QR login on the cache server."
        let client = clientFactory(endpoint)
        let profileID = activeProfileID

        do {
            let session = try await Self.withTimeout(operationTimeout) {
                try await client.startBilibiliLoginSession(
                    profileID: profileID,
                    method: .webQR
                )
            }
            guard isCurrent(requestSequence, endpoint: endpoint) else {
                return
            }
            isStartingLogin = false
            activeSession = session
            apply(session)
            if status == .sessionPending {
                startPolling()
            }
        } catch {
            guard isCurrent(requestSequence, endpoint: endpoint) else {
                return
            }
            isStartingLogin = false
            status = .failed
            statusMessage = "Could not start Bilibili login. Check the cache server and try again."
        }
    }

    private func refreshCredentialStatus() async {
        operationSequence += 1
        let requestSequence = operationSequence
        guard let endpoint = CacheServerEndpoint.normalized(from: serverAddressText) else {
            status = .disconnected
            statusMessage = "Enter a valid cache server address to check Bilibili login."
            supportsLoginSessions = false
            activeProfileID = ""
            credentialAllowsLogin = false
            return
        }

        status = .checking
        statusMessage = "Checking Bilibili login status."
        verificationQRPayload = nil
        activeSession = nil
        supportsLoginSessions = false
        activeProfileID = ""
        credentialAllowsLogin = false
        let client = clientFactory(endpoint)

        do {
            let serverInfo = try await Self.withTimeout(operationTimeout) {
                try await client.getServerInfo()
            }
            guard isCurrent(requestSequence, endpoint: endpoint) else {
                return
            }
            supportsLoginSessions = serverInfo.supportsBilibiliLoginSessions
            guard serverInfo.supportsBilibiliCredentialStatus else {
                status = .credentialStatusUnavailable
                statusMessage = "This cache server cannot report Bilibili credential status."
                return
            }

            let credentials = try await Self.withTimeout(operationTimeout) {
                try await client.getBilibiliCredentialStatus()
            }
            guard isCurrent(requestSequence, endpoint: endpoint) else {
                return
            }
            activeProfileID = credentials.activeProfileID
            guard credentials.credentialPathConfigured else {
                status = .credentialPathMissing
                statusMessage = "Configure the credential file path on the Mac mini before signing in."
                return
            }
            guard !credentials.hasWebCookie else {
                status = .authenticated
                statusMessage = "Bilibili Web login is configured on the cache server."
                return
            }
            credentialAllowsLogin = true
            status = supportsLoginSessions ? .loginRequired : .loginSessionsUnavailable
            statusMessage =
                supportsLoginSessions
                ? "Bilibili Web login is not configured on the cache server."
                : "This cache server does not support Bilibili Web QR login."
        } catch CacheControlClientUnsupportedFeature.bilibiliCredentialStatus {
            guard isCurrent(requestSequence, endpoint: endpoint) else {
                return
            }
            status = .credentialStatusUnavailable
            statusMessage = "This cache server cannot report Bilibili credential status."
        } catch {
            guard isCurrent(requestSequence, endpoint: endpoint) else {
                return
            }
            status = .failed
            statusMessage = "Could not check Bilibili login status. Check the cache server connection."
        }
    }

    private func startPolling() {
        guard active, activeSession != nil, pollingTask == nil else {
            return
        }
        pollingGeneration += 1
        let generation = pollingGeneration
        pollingTask = Task { [weak self] in
            await self?.pollActiveSession(generation: generation)
        }
    }

    private func stopPolling() {
        pollingGeneration += 1
        pollingTask?.cancel()
        pollingTask = nil
    }

    private func pollActiveSession(generation: Int) async {
        defer {
            if generation == pollingGeneration {
                pollingTask = nil
            }
        }
        let deadline = ContinuousClock.now + maximumPollingDuration

        while !Task.isCancelled, active, ContinuousClock.now < deadline,
            let session = activeSession,
            let endpoint = CacheServerEndpoint.normalized(from: serverAddressText)
        {
            if let expiresAt = session.expiresAt, expiresAt <= Date() {
                status = .sessionExpired
                statusMessage = "The Bilibili login QR code expired. Start a new login session."
                verificationQRPayload = nil
                activeSession = nil
                return
            }

            do {
                let client = clientFactory(endpoint)
                let updated = try await Self.withTimeout(operationTimeout) {
                    try await client.getBilibiliLoginSession(id: session.id)
                }
                guard active, !Task.isCancelled,
                    CacheServerEndpoint.normalized(from: serverAddressText) == endpoint
                else {
                    return
                }
                activeSession = updated
                apply(updated)
                if status != .sessionPending {
                    if status == .authenticated {
                        await refreshCredentialStatus()
                    } else {
                        activeSession = nil
                    }
                    return
                }
            } catch {
                guard active, !Task.isCancelled else {
                    return
                }
                status = .failed
                statusMessage = "Could not refresh the Bilibili login session."
                verificationQRPayload = nil
                activeSession = nil
                return
            }

            do {
                try await Task.sleep(for: pollInterval)
            } catch {
                return
            }
        }

        if active, status == .sessionPending {
            status = .sessionExpired
            statusMessage = "The Bilibili login session timed out. Start a new login session."
            verificationQRPayload = nil
            activeSession = nil
        }
    }

    private func apply(_ session: BilibiliLoginSession) {
        switch session.state.lowercased() {
        case "pending":
            guard let payload = Self.safeVerificationPayload(session.verificationURI) else {
                status = .failed
                statusMessage = "The cache server returned an invalid Bilibili login QR code."
                verificationQRPayload = nil
                return
            }
            status = .sessionPending
            statusMessage = "Scan the QR code with the Bilibili app to sign in."
            verificationQRPayload = payload
        case "ready":
            status = .authenticated
            statusMessage = "Bilibili login completed. Refreshing credential status."
            verificationQRPayload = nil
        case "expired":
            status = .sessionExpired
            statusMessage = "The Bilibili login QR code expired. Start a new login session."
            verificationQRPayload = nil
        case "unsupported":
            status = .loginSessionsUnavailable
            statusMessage = "This cache server does not support Bilibili Web QR login."
            verificationQRPayload = nil
        default:
            status = .failed
            statusMessage = "Bilibili login did not complete. Start a new login session."
            verificationQRPayload = nil
        }
    }

    private static func safeVerificationPayload(_ value: String) -> String? {
        guard let components = URLComponents(string: value),
            components.scheme?.lowercased() == "https",
            let host = components.host?.lowercased(),
            host == "bilibili.com" || host.hasSuffix(".bilibili.com"),
            let url = components.url
        else {
            return nil
        }
        return url.absoluteString
    }

    private func isCurrent(_ sequence: Int, endpoint: CacheServerEndpoint) -> Bool {
        sequence == operationSequence
            && active
            && CacheServerEndpoint.normalized(from: serverAddressText) == endpoint
    }

    private static func withTimeout<T: Sendable>(
        _ duration: Duration,
        operation: @escaping @Sendable () async throws -> T
    ) async throws -> T {
        try await withThrowingTaskGroup(of: T.self) { group in
            group.addTask { try await operation() }
            group.addTask {
                try await Task.sleep(for: duration)
                throw BilibiliLoginViewModelError.timedOut
            }
            defer { group.cancelAll() }
            guard let result = try await group.next() else {
                throw BilibiliLoginViewModelError.timedOut
            }
            return result
        }
    }
}
