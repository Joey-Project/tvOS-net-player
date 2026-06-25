import TVOSNetPlayerCacheClient
@testable import TVOSNetPlayerCore
import XCTest

@MainActor
final class CacheServerDiagnosticsViewModelTests: XCTestCase {
    func testRefreshBuildsOperatorDiagnosticsSnapshot() async {
        let client = DiagnosticsFakeCacheControlClient(
            serverInfo: CacheServerSummary(
                id: "server-1",
                name: "Mac mini cache",
                version: "0.5.0",
                mediaBaseURIs: ["http://mac-mini.local:8080"],
                capabilities: [
                    CacheServerCapability.bilibiliResolve,
                    CacheServerCapability.bilibiliTaskSelection,
                    CacheServerCapability.bilibiliCredentialStatus,
                    CacheServerCapability.lanTranscoding,
                    CacheServerCapability.libraryItemDelete,
                ]
            ),
            healthStatus: CacheHealthStatus(
                state: "HEALTH_STATE_SERVING",
                message: "Ready",
                checkedAt: nil
            ),
            credentialStatus: BilibiliCredentialStatus(
                state: "BILIBILI_CREDENTIAL_STATE_READY",
                message: "Credentials loaded.",
                credentialPathConfigured: true,
                credentialFileLoaded: true,
                hasWebCookie: true,
                hasAccessKey: true,
                hasTVAccessKey: false,
                restrictedArea: "th",
                restrictedPlayURLProxyCount: 2,
                restrictedAPIProxyCount: 1,
                checkedAt: nil
            ),
            cacheRoots: [
                CacheRoot(
                    id: "default",
                    label: "Local Cache",
                    kind: "CACHE_ROOT_KIND_LOCAL_DIRECTORY",
                    path: "/Users/joey/.cache/tvos-net-player",
                    writable: true,
                    freeBytes: 64_000_000,
                    totalBytes: 128_000_000
                )
            ],
            hlsCacheStatus: HLSCacheStatus(
                evictionEnabled: true,
                maxBytes: 100_000_000,
                highWatermarkPercent: 90,
                lowWatermarkPercent: 70,
                highWatermarkBytes: 90_000_000,
                lowWatermarkBytes: 70_000_000,
                usedBytes: 42_000_000,
                completedSessionCount: 3,
                lastEviction: nil,
                weakNetwork: HLSWeakNetworkStatus(
                    state: "HLS_WEAK_NETWORK_STATE_NORMAL",
                    message: "",
                    degradedSessionCount: 0,
                    unhealthyVariantCount: 0,
                    retryingVariantCount: 0,
                    cacheOnlySessionCount: 0,
                    lastChangedAt: nil
                ),
                transcoding: LanTranscodingStatus(
                    enabled: true,
                    state: "LAN_TRANSCODING_RUNTIME_STATE_IDLE",
                    message: "",
                    profileID: "apple-tv-h264",
                    targetContainer: "fmp4",
                    targetVideoCodec: "h264",
                    targetAudioCodec: "aac",
                    maxConcurrentJobs: 1,
                    activeJobCount: 0
                )
            )
        )
        let model = CacheServerDiagnosticsViewModel(
            defaultServerAddressText: "mac-mini.local",
            clientFactory: { _ in client }
        )

        let result = await model.refresh()

        XCTAssertEqual(result, .succeeded)
        XCTAssertEqual(model.serverAddressText, "mac-mini.local:50051")
        XCTAssertEqual(model.snapshot?.serverDisplayName, "Mac mini cache")
        XCTAssertEqual(model.row(id: "health")?.severity, .ready)
        XCTAssertEqual(model.row(id: "credentials")?.value, "Ready")
        XCTAssertTrue(model.row(id: "credentials")?.detail?.contains("Web cookie ready") == true)
        XCTAssertTrue(model.row(id: "credentials")?.detail?.contains("2 playurl proxies") == true)
        XCTAssertEqual(model.row(id: "liveValidation")?.value, "Restricted-ready")
        XCTAssertEqual(model.row(id: "cacheRoots")?.severity, .ready)
        XCTAssertFalse(model.row(id: "cacheRoots")?.detail?.contains("/Users/joey") == true)
        XCTAssertEqual(model.row(id: "hlsCache")?.severity, .ready)
        XCTAssertEqual(model.row(id: "transcoding")?.value, "Ready")
        XCTAssertNil(model.errorMessage)
    }

    func testRefreshKeepsOptionalDiagnosticFailuresLocalToRows() async {
        let client = DiagnosticsFakeCacheControlClient(
            serverInfo: CacheServerSummary(
                id: "server-1",
                name: "Legacy cache",
                version: "0.1.0",
                mediaBaseURIs: [],
                capabilities: []
            ),
            healthError: CacheControlClientUnsupportedFeature.healthCheck,
            cacheRootsError: DiagnosticsFakeCacheError.unavailable,
            hlsCacheError: CacheControlClientUnsupportedFeature.hlsCacheStatus
        )
        let model = CacheServerDiagnosticsViewModel(
            defaultServerAddressText: "legacy.local:50051",
            clientFactory: { _ in client }
        )

        let result = await model.refresh()

        XCTAssertEqual(result, .succeeded)
        XCTAssertEqual(model.snapshot?.serverDisplayName, "Legacy cache")
        XCTAssertEqual(model.row(id: "health")?.severity, .unknown)
        XCTAssertEqual(model.row(id: "credentials")?.value, "Not reported")
        XCTAssertEqual(model.row(id: "liveValidation")?.value, "Unavailable")
        XCTAssertEqual(model.row(id: "cacheRoots")?.value, "Unavailable")
        XCTAssertEqual(model.row(id: "hlsCache")?.severity, .unknown)
        XCTAssertEqual(model.snapshot?.issueCount, 5)
        XCTAssertEqual(model.statusMessage, "Diagnostics checked for Legacy cache with 5 issue(s).")
        XCTAssertNil(model.errorMessage)
    }

    func testLiveValidationDoesNotTreatAccessKeyOnlyAsAuthenticatedReady() async {
        let client = DiagnosticsFakeCacheControlClient(
            serverInfo: CacheServerSummary(
                id: "server-1",
                name: "Mac mini cache",
                version: "0.5.0",
                mediaBaseURIs: [],
                capabilities: [
                    CacheServerCapability.bilibiliResolve,
                    CacheServerCapability.bilibiliCredentialStatus,
                ]
            ),
            credentialStatus: BilibiliCredentialStatus(
                state: "BILIBILI_CREDENTIAL_STATE_READY",
                message: "Credentials loaded.",
                credentialPathConfigured: true,
                credentialFileLoaded: true,
                hasWebCookie: false,
                hasAccessKey: true,
                hasTVAccessKey: false,
                restrictedArea: "th",
                restrictedPlayURLProxyCount: 1,
                restrictedAPIProxyCount: 1,
                checkedAt: nil
            )
        )
        let model = CacheServerDiagnosticsViewModel(
            defaultServerAddressText: "mac-mini.local",
            clientFactory: { _ in client }
        )

        let result = await model.refresh()

        XCTAssertEqual(result, .succeeded)
        XCTAssertEqual(model.row(id: "liveValidation")?.value, "Public only")
        XCTAssertEqual(model.row(id: "liveValidation")?.severity, .info)
        XCTAssertTrue(model.row(id: "liveValidation")?.detail?.contains("web cookie") == true)
    }

    func testTranscodingUnspecifiedRuntimeStateIsNotReady() async {
        let client = DiagnosticsFakeCacheControlClient(
            serverInfo: CacheServerSummary(
                id: "server-1",
                name: "Mac mini cache",
                version: "0.5.0",
                mediaBaseURIs: [],
                capabilities: [CacheServerCapability.lanTranscoding]
            ),
            hlsCacheStatus: HLSCacheStatus(
                evictionEnabled: true,
                maxBytes: 100_000_000,
                highWatermarkPercent: 90,
                lowWatermarkPercent: 70,
                highWatermarkBytes: 90_000_000,
                lowWatermarkBytes: 70_000_000,
                usedBytes: 42_000_000,
                completedSessionCount: 3,
                lastEviction: nil,
                weakNetwork: nil,
                transcoding: LanTranscodingStatus(
                    enabled: true,
                    state: "LAN_TRANSCODING_RUNTIME_STATE_UNSPECIFIED",
                    message: "",
                    profileID: "",
                    targetContainer: "",
                    targetVideoCodec: "",
                    targetAudioCodec: "",
                    maxConcurrentJobs: 0,
                    activeJobCount: 0
                )
            )
        )
        let model = CacheServerDiagnosticsViewModel(
            defaultServerAddressText: "mac-mini.local",
            clientFactory: { _ in client }
        )

        let result = await model.refresh()

        XCTAssertEqual(result, .succeeded)
        XCTAssertEqual(model.row(id: "transcoding")?.value, "Unknown")
        XCTAssertEqual(model.row(id: "transcoding")?.severity, .unknown)
        XCTAssertTrue(model.row(id: "transcoding")?.detail?.contains("UNSPECIFIED") == true)
    }

    func testRefreshRejectsInvalidAddress() async {
        let model = CacheServerDiagnosticsViewModel(defaultServerAddressText: "https://192.168.1.10:50051")

        let result = await model.refresh()

        XCTAssertEqual(result, .failed)
        XCTAssertEqual(model.statusMessage, "Cache server address is invalid.")
        XCTAssertNotNil(model.errorMessage)
        XCTAssertNil(model.snapshot)
    }

    func testClearingAddressAfterInvalidRefreshClearsDiagnosticError() async {
        let model = CacheServerDiagnosticsViewModel(defaultServerAddressText: "https://192.168.1.10:50051")

        let result = await model.refresh()
        model.useServerAddressText("")

        XCTAssertEqual(result, .failed)
        XCTAssertEqual(model.statusMessage, "Set a cache server address to run diagnostics.")
        XCTAssertNil(model.errorMessage)
        XCTAssertFalse(model.isRefreshing)
        XCTAssertNil(model.snapshot)
    }

    func testRefreshDoesNotPublishSupersededServerResultAfterAddressChanges() async {
        let client = DiagnosticsFakeCacheControlClient(
            serverInfo: CacheServerSummary(
                id: "old-server",
                name: "Old cache",
                version: "0.5.0",
                mediaBaseURIs: [],
                capabilities: []
            ),
            suspendServerInfo: true
        )
        let model = CacheServerDiagnosticsViewModel(
            defaultServerAddressText: "old.local:50051",
            clientFactory: { _ in client }
        )

        let refreshTask = Task {
            await model.refresh()
        }
        await client.waitForServerInfoRequest()

        model.useServerAddressText("new.local:50051")
        await client.resumeServerInfo()
        let result = await refreshTask.value

        XCTAssertEqual(result, .superseded)
        XCTAssertEqual(model.serverAddressText, "new.local:50051")
        XCTAssertEqual(model.statusMessage, "Diagnostics not loaded.")
        XCTAssertFalse(model.isRefreshing)
        XCTAssertNil(model.snapshot)
    }
}

private extension CacheServerDiagnosticsViewModel {
    func row(id: String) -> CacheServerDiagnosticRow? {
        rows.first { $0.id == id }
    }
}

private enum DiagnosticsFakeCacheError: LocalizedError {
    case unavailable
    case unexpected

    var errorDescription: String? {
        switch self {
        case .unavailable:
            return "Diagnostics fixture is unavailable."
        case .unexpected:
            return "Unexpected diagnostics fixture call."
        }
    }
}

private actor DiagnosticsFakeCacheControlClient: CacheControlClient {
    let serverInfo: CacheServerSummary
    let healthStatus: CacheHealthStatus?
    let healthError: Error?
    let credentialStatus: BilibiliCredentialStatus?
    let credentialError: Error?
    let cacheRoots: [CacheRoot]
    let cacheRootsError: Error?
    let hlsCacheStatus: HLSCacheStatus?
    let hlsCacheError: Error?
    let suspendServerInfo: Bool
    var didRequestServerInfo = false
    var serverInfoRequestWaiters: [CheckedContinuation<Void, Never>] = []
    var serverInfoWaiter: CheckedContinuation<Void, Never>?

    init(
        serverInfo: CacheServerSummary,
        healthStatus: CacheHealthStatus? = nil,
        healthError: Error? = nil,
        credentialStatus: BilibiliCredentialStatus? = nil,
        credentialError: Error? = nil,
        cacheRoots: [CacheRoot] = [],
        cacheRootsError: Error? = nil,
        hlsCacheStatus: HLSCacheStatus? = nil,
        hlsCacheError: Error? = nil,
        suspendServerInfo: Bool = false
    ) {
        self.serverInfo = serverInfo
        self.healthStatus = healthStatus
        self.healthError = healthError
        self.credentialStatus = credentialStatus
        self.credentialError = credentialError
        self.cacheRoots = cacheRoots
        self.cacheRootsError = cacheRootsError
        self.hlsCacheStatus = hlsCacheStatus
        self.hlsCacheError = hlsCacheError
        self.suspendServerInfo = suspendServerInfo
    }

    func getServerInfo() async throws -> CacheServerSummary {
        if suspendServerInfo {
            await suspendUntilServerInfoResume()
        }
        return serverInfo
    }

    func waitForServerInfoRequest() async {
        guard !didRequestServerInfo else {
            return
        }
        await withCheckedContinuation { continuation in
            serverInfoRequestWaiters.append(continuation)
        }
    }

    func resumeServerInfo() {
        serverInfoWaiter?.resume()
        serverInfoWaiter = nil
    }

    private func suspendUntilServerInfoResume() async {
        didRequestServerInfo = true
        let waiters = serverInfoRequestWaiters
        serverInfoRequestWaiters.removeAll()
        waiters.forEach { $0.resume() }
        await withCheckedContinuation { continuation in
            serverInfoWaiter = continuation
        }
    }

    func checkHealth() async throws -> CacheHealthStatus {
        if let healthError {
            throw healthError
        }
        guard let healthStatus else {
            throw CacheControlClientUnsupportedFeature.healthCheck
        }
        return healthStatus
    }

    func getBilibiliCredentialStatus() async throws -> BilibiliCredentialStatus {
        if let credentialError {
            throw credentialError
        }
        guard let credentialStatus else {
            throw CacheControlClientUnsupportedFeature.bilibiliCredentialStatus
        }
        return credentialStatus
    }

    func listCacheRoots() async throws -> [CacheRoot] {
        if let cacheRootsError {
            throw cacheRootsError
        }
        return cacheRoots
    }

    func getHLSCacheStatus() async throws -> HLSCacheStatus {
        if let hlsCacheError {
            throw hlsCacheError
        }
        guard let hlsCacheStatus else {
            throw CacheControlClientUnsupportedFeature.hlsCacheStatus
        }
        return hlsCacheStatus
    }

    func listLibraryItemsPage(
        pageToken: String,
        pageSize: Int,
        searchText: String?
    ) async throws -> CacheLibraryItemsPage {
        throw DiagnosticsFakeCacheError.unexpected
    }

    func getPlaybackSource(itemID: String, variantID: String) async throws -> CachePlaybackSource {
        throw DiagnosticsFakeCacheError.unexpected
    }

    func deleteLibraryItem(id: String) async throws -> Bool {
        throw DiagnosticsFakeCacheError.unexpected
    }

    func getTask(id: String) async throws -> CacheTask {
        throw DiagnosticsFakeCacheError.unexpected
    }

    func watchTasks(ids: [String]) async -> AsyncThrowingStream<CacheTask, Error> {
        AsyncThrowingStream { continuation in
            continuation.finish()
        }
    }

    func cancelTask(id: String) async throws -> CacheTask {
        throw DiagnosticsFakeCacheError.unexpected
    }

    func createBilibiliPlaybackTask(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask {
        throw DiagnosticsFakeCacheError.unexpected
    }

    func createBilibiliPlaybackTask(
        urlOrID: String,
        selectionID: String?,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask {
        throw DiagnosticsFakeCacheError.unexpected
    }

    func createBilibiliPlaybackTask(
        urlOrID: String,
        selection: BilibiliTaskSelection?,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask {
        throw DiagnosticsFakeCacheError.unexpected
    }
}
