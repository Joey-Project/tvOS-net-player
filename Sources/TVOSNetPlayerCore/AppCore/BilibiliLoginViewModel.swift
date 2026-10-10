import Combine
import Foundation
import TVOSNetPlayerCacheClient

public enum BilibiliLoginStatus: Equatable, Sendable {
    case disconnected
    case checking
    case credentialStatusUnavailable
    case loginSessionsUnavailable
    case credentialPathMissing
    case credentialUnknown
    case credentialUnavailable
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
    @Published public private(set) var verificationLink: String?
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
    private var effectiveProfileID = ""
    private var serverInfo: CacheServerSummary?
    private var supportsLoginSessions = false
    private var supportsAccessKeyLogin = false
    private var supportsCredentialReadiness = false
    private var webLoginAllowed = false
    private var accessKeyLoginAllowed = false

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
        canStartLoginSession && webLoginAllowed
    }

    public var canStartAccessKeyLogin: Bool {
        canStartLoginSession
            && supportsAccessKeyLogin
            && accessKeyLoginAllowed
    }

    private var canStartLoginSession: Bool {
        let statusAllowsLogin =
            status == .loginRequired || status == .authenticated || status == .sessionExpired
            || status == .failed
        return active
            && CacheServerEndpoint.normalized(from: serverAddressText) != nil
            && statusAllowsLogin
            && supportsLoginSessions
            && !isStartingLogin
    }

    public var canCheckReadiness: Bool {
        active
            && supportsCredentialReadiness
            && !isStartingLogin
            && status != .sessionPending
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
            invalidateSession()
        } else if isStartingLogin {
            return
        } else if let session = activeSession, status == .sessionPending {
            if session.expiresAt.map({ $0 > Date() }) ?? true {
                startPolling()
                return
            }
            status = .sessionExpired
            statusMessage = "The Bilibili login session expired. Start a new login session."
            clearPresentation()
            activeSession = nil
        }
        await refreshCredentialStatus()
        if activeSession != nil {
            startPolling()
        }
    }

    public func deactivate() {
        active = false
        operationSequence += 1
        isStartingLogin = false
        stopPolling()
    }

    public func refreshReadiness() async {
        guard active, supportsCredentialReadiness else { return }
        await refreshCredentialStatus()
    }

    public func startLogin() async {
        await startLogin(method: .webQR)
    }

    public func startAccessKeyLogin() async {
        await startLogin(method: .accessKeyBrowser)
    }

    private func startLogin(method: BilibiliLoginMethod) async {
        let allowed = method == .webQR ? canStartLogin : canStartAccessKeyLogin
        guard allowed, let endpoint = CacheServerEndpoint.normalized(from: serverAddressText) else {
            return
        }

        operationSequence += 1
        let requestSequence = operationSequence
        isStartingLogin = true
        statusMessage =
            method == .webQR
            ? "Starting Bilibili Web login on the cache server."
            : "Starting Bilibili access-key login on the cache server."
        let client = clientFactory(endpoint)
        let profileID = activeProfileID

        do {
            let session = try await Self.withTimeout(max(operationTimeout, .seconds(20))) {
                try await client.startBilibiliLoginSession(profileID: profileID, method: method)
            }
            guard isCurrent(requestSequence, endpoint: endpoint),
                session.profileID == effectiveProfileID,
                session.method.lowercased() == method.rawValue.lowercased()
            else {
                if isCurrent(requestSequence, endpoint: endpoint) {
                    isStartingLogin = false
                    status = .failed
                    statusMessage = "The cache server returned a login session for a different profile or method."
                }
                return
            }
            isStartingLogin = false
            activeSession = session
            apply(session)
            if status == .authenticated {
                await refreshCredentialStatus()
            } else if status == .sessionPending {
                startPolling()
            }
        } catch {
            guard isCurrent(requestSequence, endpoint: endpoint) else { return }
            isStartingLogin = false
            status = .failed
            statusMessage = "Could not start Bilibili login. Check the cache server and try again."
        }
    }

    private func refreshCredentialStatus() async {
        operationSequence += 1
        isStartingLogin = false
        let requestSequence = operationSequence
        guard let endpoint = CacheServerEndpoint.normalized(from: serverAddressText) else {
            status = .disconnected
            statusMessage = "Enter a valid cache server address to check Bilibili login."
            resetServerState()
            return
        }

        status = .checking
        statusMessage = "Checking Bilibili login status."
        clearPresentation()
        activeSession = nil
        resetCredentialState()
        let client = clientFactory(endpoint)

        do {
            let info = try await Self.withTimeout(operationTimeout) {
                try await client.getServerInfo()
            }
            guard isCurrent(requestSequence, endpoint: endpoint) else { return }
            serverInfo = info
            supportsLoginSessions = info.supportsBilibiliLoginSessions
            supportsAccessKeyLogin = info.supportsBilibiliAccessKeyLogin
            supportsCredentialReadiness = info.supportsBilibiliCredentialReadiness
            guard info.supportsBilibiliCredentialStatus else {
                status = .credentialStatusUnavailable
                statusMessage = "This cache server cannot report Bilibili credential status."
                return
            }

            let credentials = try await Self.withTimeout(operationTimeout) {
                try await client.getBilibiliCredentialStatus()
            }
            guard isCurrent(requestSequence, endpoint: endpoint) else { return }
            apply(credentials, usingReadiness: supportsCredentialReadiness)
        } catch CacheControlClientUnsupportedFeature.bilibiliCredentialStatus {
            guard isCurrent(requestSequence, endpoint: endpoint) else { return }
            status = .credentialStatusUnavailable
            statusMessage = "This cache server cannot report Bilibili credential status."
        } catch {
            guard isCurrent(requestSequence, endpoint: endpoint) else { return }
            status = .failed
            statusMessage = "Could not check Bilibili login status. Check the cache server connection."
        }
    }

    private func apply(_ credentials: BilibiliCredentialStatus, usingReadiness: Bool) {
        activeProfileID = credentials.activeProfileID
        effectiveProfileID =
            credentials.activeProfileID.isEmpty
            ? credentials.defaultProfileID
            : credentials.activeProfileID
        guard credentials.credentialPathConfigured else {
            status = .credentialPathMissing
            statusMessage = "Configure the credential file path on the Mac mini before signing in."
            return
        }
        guard !credentials.state.localizedCaseInsensitiveContains("error") else {
            status = .credentialUnavailable
            statusMessage = "Bilibili credential status is unavailable on the cache server."
            return
        }

        guard usingReadiness else {
            if credentials.credentialFileLoaded,
                (credentials.hasWebCookie || credentials.hasAccessKey),
                credentials.state.localizedCaseInsensitiveContains("ready")
            {
                status = .authenticated
                statusMessage = "Bilibili credentials are configured on the cache server."
            } else if credentials.hasWebCookie || credentials.hasAccessKey {
                status = .credentialUnknown
                statusMessage = "Bilibili credential health is not verified by this cache server."
            } else {
                webLoginAllowed = true
                accessKeyLoginAllowed = supportsAccessKeyLogin
                setLoginRequired(message: "Bilibili Web login is not configured on the cache server.")
            }
            return
        }

        webLoginAllowed =
            credentials.webCookieReadiness == .missing
            || credentials.webCookieReadiness == .loginRequired
        accessKeyLoginAllowed =
            credentials.accessKeyReadiness == .missing
            || credentials.accessKeyReadiness == .loginRequired

        if credentials.webCookieReadiness == .ready || credentials.accessKeyReadiness == .ready {
            status = .authenticated
            if credentials.webCookieReadiness == .ready,
                credentials.accessKeyReadiness == .ready
            {
                statusMessage = "Bilibili Web and access-key credentials are ready on the cache server."
            } else if credentials.webCookieReadiness == .ready {
                let accessKeyMessage = Self.readinessMessage(
                    for: "access-key",
                    readiness: credentials.accessKeyReadiness,
                    loginAvailable: supportsLoginSessions && supportsAccessKeyLogin
                        && accessKeyLoginAllowed
                )
                statusMessage = "Bilibili Web credentials are ready; \(accessKeyMessage)"
            } else {
                let webMessage = Self.readinessMessage(
                    for: "Web",
                    readiness: credentials.webCookieReadiness,
                    loginAvailable: supportsLoginSessions && webLoginAllowed
                )
                statusMessage = "Bilibili access-key credentials are ready; \(webMessage)"
            }
        } else if credentials.webCookieReadiness == .checking || credentials.accessKeyReadiness == .checking {
            status = .checking
            statusMessage = "The cache server is checking Bilibili credentials. Check again shortly."
        } else if credentials.webCookieReadiness == .unavailable && credentials.accessKeyReadiness == .unavailable {
            status = .credentialUnavailable
            statusMessage = "Bilibili credential readiness is unavailable on the cache server."
        } else if webLoginAllowed || accessKeyLoginAllowed {
            setLoginRequired(message: "Bilibili login is required on the cache server.")
        } else {
            status = .credentialUnknown
            statusMessage = "Bilibili credential readiness is unknown. Check again or update the cache server."
        }
    }

    private func setLoginRequired(message: String) {
        status = supportsLoginSessions ? .loginRequired : .loginSessionsUnavailable
        statusMessage =
            supportsLoginSessions
            ? message
            : "This cache server does not support Bilibili login sessions."
    }

    private func startPolling() {
        guard active, activeSession != nil, pollingTask == nil else { return }
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
            if generation == pollingGeneration { pollingTask = nil }
        }
        let deadline = ContinuousClock.now + maximumPollingDuration

        while !Task.isCancelled, active, ContinuousClock.now < deadline,
            let session = activeSession,
            let endpoint = CacheServerEndpoint.normalized(from: serverAddressText)
        {
            if let expiresAt = session.expiresAt, expiresAt <= Date() {
                status = .sessionExpired
                statusMessage = "The Bilibili login session expired. Start a new login session."
                clearPresentation()
                activeSession = nil
                return
            }

            do {
                let client = clientFactory(endpoint)
                let updated = try await Self.withTimeout(operationTimeout) {
                    try await client.getBilibiliLoginSession(id: session.id)
                }
                guard active, !Task.isCancelled,
                    CacheServerEndpoint.normalized(from: serverAddressText) == endpoint,
                    updated.id == session.id,
                    updated.profileID == effectiveProfileID,
                    updated.method.lowercased() == session.method.lowercased()
                else { return }
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
                guard active, !Task.isCancelled else { return }
                status = .failed
                statusMessage = "Could not refresh the Bilibili login session."
                clearPresentation()
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
            clearPresentation()
            activeSession = nil
        }
    }

    private func apply(_ session: BilibiliLoginSession) {
        switch session.state.lowercased() {
        case "pending":
            if session.method.lowercased() == BilibiliLoginMethod.webQR.rawValue.lowercased() {
                guard let payload = Self.safeWebQRPayload(session.verificationURI) else {
                    failInvalidSession()
                    return
                }
                verificationQRPayload = payload
                verificationLink = nil
                statusMessage = "Scan the QR code with the Bilibili app to sign in."
            } else if session.method.lowercased() == BilibiliLoginMethod.accessKeyBrowser.rawValue.lowercased() {
                guard
                    let link = Self.safeServerLoginLink(
                        session.verificationURI,
                        sessionID: session.id,
                        mediaBaseURIs: serverInfo?.mediaBaseURIs ?? []
                    )
                else {
                    failInvalidSession()
                    return
                }
                verificationLink = link
                verificationQRPayload = link
                statusMessage = "Open the cache server login page to continue in your browser."
            } else {
                failInvalidSession()
                return
            }
            status = .sessionPending
        case "ready":
            if session.method.lowercased() == BilibiliLoginMethod.webQR.rawValue.lowercased() {
                webLoginAllowed = false
            } else if session.method.lowercased()
                == BilibiliLoginMethod.accessKeyBrowser.rawValue.lowercased()
            {
                accessKeyLoginAllowed = false
            }
            status = .authenticated
            statusMessage = "Bilibili login completed. Refreshing credential status."
            clearPresentation()
        case "expired":
            status = .sessionExpired
            statusMessage = "The Bilibili login session expired. Start a new login session."
            clearPresentation()
        case "unsupported":
            status = .loginSessionsUnavailable
            statusMessage = "This cache server does not support this Bilibili login method."
            clearPresentation()
        default:
            status = .failed
            statusMessage = "Bilibili login did not complete. Start a new login session."
            clearPresentation()
        }
    }

    private func failInvalidSession() {
        status = .failed
        statusMessage = "The cache server returned an invalid Bilibili login link."
        clearPresentation()
    }

    private func invalidateSession() {
        operationSequence += 1
        isStartingLogin = false
        stopPolling()
        activeSession = nil
        clearPresentation()
        serverInfo = nil
    }

    private func resetServerState() {
        supportsLoginSessions = false
        supportsAccessKeyLogin = false
        supportsCredentialReadiness = false
        serverInfo = nil
        activeProfileID = ""
        effectiveProfileID = ""
        resetCredentialState()
    }

    private func resetCredentialState() {
        activeProfileID = ""
        effectiveProfileID = ""
        webLoginAllowed = false
        accessKeyLoginAllowed = false
    }

    private func clearPresentation() {
        verificationQRPayload = nil
        verificationLink = nil
    }

    private static func safeWebQRPayload(_ value: String) -> String? {
        guard value.utf8.count <= 2_048,
            !value.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains),
            let components = URLComponents(string: value),
            components.scheme?.lowercased() == "https",
            components.user == nil,
            components.password == nil,
            let host = components.host?.lowercased(),
            host == "bilibili.com" || host.hasSuffix(".bilibili.com"),
            let url = components.url
        else { return nil }
        return url.absoluteString
    }

    private static func safeServerLoginLink(
        _ value: String,
        sessionID: String,
        mediaBaseURIs: [String]
    ) -> String? {
        guard value.utf8.count <= 4_096,
            !value.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains),
            let sessionUUID = UUID(uuidString: sessionID),
            sessionUUID.uuidString.lowercased() == sessionID.lowercased(),
            let candidate = URLComponents(string: value),
            let scheme = candidate.scheme?.lowercased(),
            scheme == "http" || scheme == "https",
            candidate.user == nil,
            candidate.password == nil,
            candidate.query == nil,
            let host = candidate.host?.lowercased(),
            let candidatePath = canonicalPathComponents(candidate),
            let fragment = candidate.percentEncodedFragment,
            fragment.utf8.count == 64,
            fragment.utf8.allSatisfy({
                ($0 >= UInt8(ascii: "0") && $0 <= UInt8(ascii: "9"))
                    || ($0 >= UInt8(ascii: "a") && $0 <= UInt8(ascii: "f"))
            }),
            let url = candidate.url
        else { return nil }

        for baseValue in mediaBaseURIs {
            guard baseValue.utf8.count <= 4_096,
                let base = URLComponents(string: baseValue),
                let baseScheme = base.scheme?.lowercased(),
                baseScheme == "http" || baseScheme == "https",
                base.user == nil,
                base.password == nil,
                base.query == nil,
                base.fragment == nil,
                let baseHost = base.host?.lowercased(),
                let basePath = canonicalPathComponents(base),
                let basePort = effectivePort(base),
                let candidatePort = effectivePort(candidate),
                scheme == baseScheme,
                host == baseHost,
                candidatePort == basePort
            else { continue }

            let requiredPath = basePath + ["login", "bilibili", sessionID]
            guard candidatePath == requiredPath else { continue }
            return url.absoluteString
        }
        return nil
    }

    private static func readinessMessage(
        for credentialKind: String,
        readiness: BilibiliCredentialReadiness,
        loginAvailable: Bool
    ) -> String {
        if readiness == .missing || readiness == .loginRequired {
            return loginAvailable
                ? "\(credentialKind) login is available on the cache server."
                : "\(credentialKind) login is required but unavailable on the cache server."
        }
        if readiness == .checking {
            return "\(credentialKind) credential status is being checked on the cache server."
        }
        if readiness == .unavailable {
            return "\(credentialKind) credential readiness is unavailable on the cache server."
        }
        return "\(credentialKind) credential readiness is unknown on the cache server."
    }

    private static func canonicalPathComponents(_ components: URLComponents) -> [String]? {
        let encodedPath = components.percentEncodedPath
        guard encodedPath.isEmpty || encodedPath.hasPrefix("/") else { return nil }

        var path = encodedPath
        if path.hasPrefix("/") {
            path.removeFirst()
        }
        guard !path.hasPrefix("/") else { return nil }
        if path.hasSuffix("/") {
            path.removeLast()
        }
        guard !path.hasSuffix("/"), !path.contains("//") else { return nil }
        guard !path.isEmpty else { return [] }

        var canonicalComponents: [String] = []
        for component in path.split(separator: "/", omittingEmptySubsequences: false) {
            guard let canonicalComponent = canonicalPathComponent(String(component)) else {
                return nil
            }
            canonicalComponents.append(canonicalComponent)
        }
        return canonicalComponents
    }

    private static func canonicalPathComponent(_ encodedComponent: String) -> String? {
        var component = encodedComponent
        for _ in 0..<9 {
            guard let decoded = component.removingPercentEncoding,
                decoded != ".",
                decoded != "..",
                !decoded.contains("/"),
                !decoded.contains("\\"),
                !decoded.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains)
            else { return nil }

            if decoded == component {
                return decoded
            }
            component = decoded
        }
        return nil
    }

    private static func effectivePort(_ components: URLComponents) -> Int? {
        if let port = components.port { return port }
        switch components.scheme?.lowercased() {
        case "http": return 80
        case "https": return 443
        default: return nil
        }
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
