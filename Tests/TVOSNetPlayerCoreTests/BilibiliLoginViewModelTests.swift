import XCTest
import TVOSNetPlayerCacheClient
@testable import TVOSNetPlayerCore

final class BilibiliLoginViewModelTests: XCTestCase {
    @MainActor
    func testMissingCredentialPathDoesNotOfferLogin() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(credentialPathConfigured: false)
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")

        XCTAssertEqual(model.status, .credentialPathMissing)
        XCTAssertFalse(model.canStartLogin)
        let loginStartCount = await client.loginStartCount
        XCTAssertEqual(loginStartCount, 0)
        XCTAssertFalse(model.statusMessage.localizedCaseInsensitiveContains("/"))
    }

    @MainActor
    func testUnsupportedCredentialStatusIsReportedWithoutStartingLogin() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [CacheServerCapability.bilibiliLoginSessions]),
            credentialStatus: .fixture()
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")

        XCTAssertEqual(model.status, .credentialStatusUnavailable)
        XCTAssertFalse(model.canStartLogin)
        let credentialStatusCallCount = await client.credentialStatusCallCount
        XCTAssertEqual(credentialStatusCallCount, 0)
    }

    @MainActor
    func testCredentialStatusFailureDoesNotOfferLogin() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(),
            credentialStatusFails: true
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")

        XCTAssertEqual(model.status, .failed)
        XCTAssertFalse(model.canStartLogin)
    }

    @MainActor
    func testHealthyWebCookieCannotBeOverwrittenByClientLogin() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(hasWebCookie: true)
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")
        await model.startLogin()

        XCTAssertEqual(model.status, .authenticated)
        XCTAssertFalse(model.canStartLogin)
        let loginStartCount = await client.loginStartCount
        XCTAssertEqual(loginStartCount, 0)
    }

    @MainActor
    func testPendingSessionExposesOnlySafeBilibiliQRPayload() async {
        let session = BilibiliLoginSession(
            id: "session-1",
            profileID: "profile-1",
            method: "webQR",
            state: "pending",
            message: "private message that must not be shown",
            verificationURI: "https://passport.bilibili.com/qrcode/h5?token=short-lived",
            createdAt: Date(),
            expiresAt: Date().addingTimeInterval(30)
        )
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(),
            newSession: session
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")
        await model.startLogin()

        XCTAssertEqual(model.status, .sessionPending)
        XCTAssertEqual(model.verificationQRPayload, session.verificationURI)
        XCTAssertFalse(model.statusMessage.contains("private message"))
        XCTAssertFalse(model.statusMessage.contains("token="))
        model.deactivate()
    }

    @MainActor
    func testEmptyActiveProfileUsesServerDefaultProfile() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(credentialFileLoaded: false, activeProfileID: ""),
            newSession: .fixture(state: "pending")
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")
        XCTAssertTrue(model.canStartLogin)
        await model.startLogin()

        let requestedProfileID = await client.requestedProfileID
        XCTAssertEqual(requestedProfileID, "")
        XCTAssertEqual(model.status, .sessionPending)
        model.deactivate()
    }

    @MainActor
    func testExpiredSessionCanBeRetriedWithoutReconnect() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(),
            newSession: .fixture(state: "expired")
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")
        await model.startLogin()
        XCTAssertEqual(model.status, .sessionExpired)
        XCTAssertTrue(model.canStartLogin)

        await model.startLogin()

        let loginStartCount = await client.loginStartCount
        XCTAssertEqual(loginStartCount, 2)
        XCTAssertEqual(model.status, .sessionExpired)
    }

    @MainActor
    func testLoginStartCanOutlastOrdinaryStatusTimeout() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(),
            startDelay: .milliseconds(75)
        )
        let model = BilibiliLoginViewModel(
            operationTimeout: .milliseconds(20),
            pollInterval: .seconds(2),
            clientFactory: { _ in client }
        )

        await model.activate(serverAddressText: "mac-mini.local")
        await model.startLogin()

        XCTAssertEqual(model.status, .sessionPending)
        XCTAssertNotNil(model.verificationQRPayload)
        model.deactivate()
    }

    @MainActor
    func testReactivatingSameServerResumesLiveSessionWithoutClearingQR() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(),
            newSession: .fixture(state: "pending")
        )
        let model = BilibiliLoginViewModel(
            pollInterval: .milliseconds(10),
            clientFactory: { _ in client }
        )

        await model.activate(serverAddressText: "mac-mini.local")
        await model.startLogin()
        let payload = model.verificationQRPayload
        let initialCredentialStatusCallCount = await client.credentialStatusCallCount
        model.deactivate()

        await model.activate(serverAddressText: "mac-mini.local:50051")
        for _ in 0..<50 {
            if await client.loginPollCount > 0 {
                break
            }
            try? await Task.sleep(for: .milliseconds(10))
        }

        let resumedCredentialStatusCallCount = await client.credentialStatusCallCount
        let loginPollCount = await client.loginPollCount
        XCTAssertEqual(model.status, .sessionPending)
        XCTAssertEqual(model.verificationQRPayload, payload)
        XCTAssertEqual(resumedCredentialStatusCallCount, initialCredentialStatusCallCount)
        XCTAssertGreaterThan(loginPollCount, 0)
        model.deactivate()
    }

    @MainActor
    func testReadySessionRefreshesRedactedCredentialStatus() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(),
            credentialStatusAfterLogin: .fixture(hasWebCookie: true),
            newSession: .fixture(state: "pending"),
            polledSession: .fixture(state: "ready")
        )
        let model = BilibiliLoginViewModel(
            pollInterval: .milliseconds(10),
            clientFactory: { _ in client }
        )

        await model.activate(serverAddressText: "mac-mini.local")
        await model.startLogin()
        for _ in 0..<50 {
            if model.status == .authenticated {
                break
            }
            try? await Task.sleep(for: .milliseconds(10))
        }

        let credentialStatusCallCount = await client.credentialStatusCallCount
        XCTAssertEqual(model.status, .authenticated)
        XCTAssertEqual(model.statusMessage, "Bilibili Web login is configured on the cache server.")
        XCTAssertEqual(credentialStatusCallCount, 2)
        model.deactivate()
    }

    @MainActor
    func testNonBilibiliVerificationURLIsRejected() async {
        let session = BilibiliLoginSession(
            id: "session-1",
            profileID: "profile-1",
            method: "webQR",
            state: "pending",
            message: "",
            verificationURI: "https://example.com/login?token=secret",
            createdAt: Date(),
            expiresAt: Date().addingTimeInterval(30)
        )
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(),
            newSession: session
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")
        await model.startLogin()

        XCTAssertEqual(model.status, .failed)
        XCTAssertNil(model.verificationQRPayload)
        XCTAssertFalse(model.statusMessage.contains("secret"))
        model.deactivate()
    }

    @MainActor
    func testChangingServerWhileStartIsPendingAllowsRetry() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(),
            startDelay: .milliseconds(100)
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "first.local")
        let oldStart = Task { await model.startLogin() }
        for _ in 0..<50 {
            if await client.loginStartCount > 0 { break }
            try? await Task.sleep(for: .milliseconds(2))
        }
        XCTAssertTrue(model.isStartingLogin)

        await model.activate(serverAddressText: "second.local")
        XCTAssertEqual(model.status, .loginRequired)
        XCTAssertFalse(model.isStartingLogin)
        XCTAssertTrue(model.canStartLogin)

        await oldStart.value
        XCTAssertEqual(model.status, .loginRequired)
        XCTAssertTrue(model.canStartLogin)
        model.deactivate()
    }

    @MainActor
    func testLeavingViewWhileStartIsPendingAllowsRetryOnReturn() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(),
            startDelay: .milliseconds(100)
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")
        let oldStart = Task { await model.startLogin() }
        for _ in 0..<50 {
            if await client.loginStartCount > 0 { break }
            try? await Task.sleep(for: .milliseconds(2))
        }
        XCTAssertTrue(model.isStartingLogin)

        model.deactivate()
        XCTAssertFalse(model.isStartingLogin)
        await model.activate(serverAddressText: "mac-mini.local")
        XCTAssertEqual(model.status, .loginRequired)
        XCTAssertTrue(model.canStartLogin)

        await oldStart.value
        XCTAssertEqual(model.status, .loginRequired)
        XCTAssertTrue(model.canStartLogin)
        model.deactivate()
    }
}

private actor LoginClient: CacheControlClient {
    let serverInfo: CacheServerSummary
    let credentialStatus: BilibiliCredentialStatus
    let credentialStatusAfterLogin: BilibiliCredentialStatus?
    let credentialStatusFails: Bool
    let newSession: BilibiliLoginSession
    let polledSession: BilibiliLoginSession?
    let startDelay: Duration
    private(set) var credentialStatusCallCount = 0
    private(set) var loginStartCount = 0
    private(set) var loginPollCount = 0
    private(set) var requestedProfileID = ""

    init(
        serverInfo: CacheServerSummary,
        credentialStatus: BilibiliCredentialStatus,
        credentialStatusAfterLogin: BilibiliCredentialStatus? = nil,
        credentialStatusFails: Bool = false,
        newSession: BilibiliLoginSession = .fixture(state: "pending"),
        polledSession: BilibiliLoginSession? = nil,
        startDelay: Duration = .zero
    ) {
        self.serverInfo = serverInfo
        self.credentialStatus = credentialStatus
        self.credentialStatusAfterLogin = credentialStatusAfterLogin
        self.credentialStatusFails = credentialStatusFails
        self.newSession = newSession
        self.polledSession = polledSession
        self.startDelay = startDelay
    }

    func getServerInfo() async throws -> CacheServerSummary {
        serverInfo
    }

    func getBilibiliCredentialStatus() async throws -> BilibiliCredentialStatus {
        credentialStatusCallCount += 1
        if credentialStatusFails {
            throw LoginClientError.unused
        }
        if credentialStatusCallCount > 1, let credentialStatusAfterLogin {
            return credentialStatusAfterLogin
        }
        return credentialStatus
    }

    func startBilibiliLoginSession(
        profileID: String,
        method: BilibiliLoginMethod
    ) async throws -> BilibiliLoginSession {
        loginStartCount += 1
        requestedProfileID = profileID
        if startDelay > .zero {
            try await Task.sleep(for: startDelay)
        }
        return newSession
    }

    func getBilibiliLoginSession(id: String) async throws -> BilibiliLoginSession {
        loginPollCount += 1
        return polledSession ?? newSession
    }

    func listCacheRoots() async throws -> [CacheRoot] {
        throw LoginClientError.unused
    }

    func listLibraryItemsPage(
        pageToken: String,
        pageSize: Int,
        searchText: String?
    ) async throws -> CacheLibraryItemsPage {
        throw LoginClientError.unused
    }

    func getPlaybackSource(itemID: String, variantID: String) async throws -> CachePlaybackSource {
        throw LoginClientError.unused
    }

    func deleteLibraryItem(id: String) async throws -> Bool {
        throw LoginClientError.unused
    }

    func getTask(id: String) async throws -> CacheTask {
        throw LoginClientError.unused
    }

    func watchTasks(ids: [String]) async -> AsyncThrowingStream<CacheTask, Error> {
        AsyncThrowingStream { $0.finish() }
    }

    func cancelTask(id: String) async throws -> CacheTask {
        throw LoginClientError.unused
    }

    func createBilibiliPlaybackTask(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask {
        throw LoginClientError.unused
    }
}

private enum LoginClientError: Error {
    case unused
}

private extension CacheServerSummary {
    static func fixture(capabilities: [String]) -> CacheServerSummary {
        CacheServerSummary(
            id: "server-1",
            name: "Cache server",
            version: "1.0.0",
            mediaBaseURIs: [],
            capabilities: capabilities
        )
    }
}

private extension BilibiliCredentialStatus {
    static func fixture(
        credentialPathConfigured: Bool = true,
        credentialFileLoaded: Bool = true,
        hasWebCookie: Bool = false,
        activeProfileID: String = "profile-1"
    ) -> BilibiliCredentialStatus {
        BilibiliCredentialStatus(
            state: "ready",
            message: "sensitive path must remain hidden",
            credentialPathConfigured: credentialPathConfigured,
            credentialFileLoaded: credentialFileLoaded,
            hasWebCookie: hasWebCookie,
            hasAccessKey: false,
            hasTVAccessKey: false,
            restrictedArea: "",
            restrictedPlayURLProxyCount: 0,
            restrictedAPIProxyCount: 0,
            checkedAt: Date(),
            activeProfileID: activeProfileID,
            defaultProfileID: "profile-1",
            profileCount: 1
        )
    }
}

private extension BilibiliLoginSession {
    static func fixture(state: String) -> BilibiliLoginSession {
        BilibiliLoginSession(
            id: "session-1",
            profileID: "profile-1",
            method: "webQR",
            state: state,
            message: "",
            verificationURI: "https://passport.bilibili.com/qrcode/h5",
            createdAt: Date(),
            expiresAt: Date().addingTimeInterval(30)
        )
    }
}
