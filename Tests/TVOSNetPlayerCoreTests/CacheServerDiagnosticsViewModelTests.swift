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
                    CacheServerCapability.bilibiliTaskSelection,
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

    func testLiveValidationRequiresTaskSelectionCapability() async {
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
                hasWebCookie: true,
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
        XCTAssertEqual(model.row(id: "liveValidation")?.value, "Selection unavailable")
        XCTAssertEqual(model.row(id: "liveValidation")?.severity, .unknown)
        XCTAssertTrue(model.row(id: "liveValidation")?.detail?.contains("task selection") == true)
    }

    func testLiveValidationRequiresRestrictedAPIProxyForRestrictedReady() async {
        let client = DiagnosticsFakeCacheControlClient(
            serverInfo: CacheServerSummary(
                id: "server-1",
                name: "Mac mini cache",
                version: "0.5.0",
                mediaBaseURIs: [],
                capabilities: [
                    CacheServerCapability.bilibiliResolve,
                    CacheServerCapability.bilibiliTaskSelection,
                    CacheServerCapability.bilibiliCredentialStatus,
                ]
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
                restrictedAPIProxyCount: 0,
                checkedAt: nil
            )
        )
        let model = CacheServerDiagnosticsViewModel(
            defaultServerAddressText: "mac-mini.local",
            clientFactory: { _ in client }
        )

        let result = await model.refresh()

        XCTAssertEqual(result, .succeeded)
        XCTAssertEqual(model.row(id: "liveValidation")?.value, "Authenticated-ready")
        XCTAssertEqual(model.row(id: "liveValidation")?.severity, .warning)
        XCTAssertTrue(model.row(id: "liveValidation")?.detail?.contains("API proxy") == true)
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

    func testDisabledTranscodingWithoutCapabilityDoesNotAddRow() async {
        let client = DiagnosticsFakeCacheControlClient(
            serverInfo: CacheServerSummary(
                id: "server-1",
                name: "Mac mini cache",
                version: "0.5.0",
                mediaBaseURIs: [],
                capabilities: []
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
                    enabled: false,
                    state: "LAN_TRANSCODING_RUNTIME_STATE_DISABLED",
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
        XCTAssertNil(model.row(id: "transcoding"))
    }

    func testDisabledTranscodingCapabilityIsInformational() async {
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
                    enabled: false,
                    state: "LAN_TRANSCODING_RUNTIME_STATE_DISABLED",
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
        XCTAssertEqual(model.row(id: "transcoding")?.value, "Disabled")
        XCTAssertEqual(model.row(id: "transcoding")?.severity, .info)
    }

    func testWeakNetworkDiagnosticsClassifiesUpstreamFailure() async {
        let client = DiagnosticsFakeCacheControlClient(
            serverInfo: CacheServerSummary(
                id: "server-1",
                name: "Mac mini cache",
                version: "0.5.0",
                mediaBaseURIs: [],
                capabilities: []
            ),
            hlsCacheStatus: .diagnosticsFixture(
                weakNetwork: HLSWeakNetworkStatus(
                    state: "HLS_WEAK_NETWORK_STATE_UPSTREAM_FAILED",
                    message: "HLS upstream failed; playback may continue from cache when available.",
                    degradedSessionCount: 1,
                    unhealthyVariantCount: 1,
                    retryingVariantCount: 1,
                    cacheOnlySessionCount: 0,
                    lastChangedAt: nil
                )
            )
        )
        let model = CacheServerDiagnosticsViewModel(
            defaultServerAddressText: "mac-mini.local",
            clientFactory: { _ in client }
        )

        let result = await model.refresh()

        XCTAssertEqual(result, .succeeded)
        XCTAssertEqual(model.row(id: "weakNetwork")?.value, "Upstream failed")
        XCTAssertEqual(model.row(id: "weakNetwork")?.severity, .error)
        XCTAssertTrue(model.row(id: "weakNetwork")?.detail?.contains("1 retrying variant") == true)
    }

    func testWeakNetworkDiagnosticsCountsRetryingUpstreamAsIssue() async {
        let client = DiagnosticsFakeCacheControlClient(
            serverInfo: CacheServerSummary(
                id: "server-1",
                name: "Mac mini cache",
                version: "0.5.0",
                mediaBaseURIs: [],
                capabilities: [
                    CacheServerCapability.bilibiliResolve,
                    CacheServerCapability.bilibiliTaskSelection,
                    CacheServerCapability.bilibiliCredentialStatus,
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
            hlsCacheStatus: .diagnosticsFixture(
                weakNetwork: HLSWeakNetworkStatus(
                    state: "HLS_WEAK_NETWORK_STATE_RETRYING",
                    message: "Retrying HLS upstream requests via backup URLs.",
                    degradedSessionCount: 0,
                    unhealthyVariantCount: 0,
                    retryingVariantCount: 1,
                    cacheOnlySessionCount: 0,
                    lastChangedAt: nil
                )
            )
        )
        let model = CacheServerDiagnosticsViewModel(
            defaultServerAddressText: "mac-mini.local",
            clientFactory: { _ in client }
        )

        let result = await model.refresh()

        XCTAssertEqual(result, .succeeded)
        XCTAssertEqual(model.row(id: "weakNetwork")?.value, "Retrying upstream")
        XCTAssertEqual(model.row(id: "weakNetwork")?.severity, .warning)
        XCTAssertTrue(model.row(id: "weakNetwork")?.detail?.contains("1 retrying variant") == true)
        XCTAssertEqual(model.snapshot?.issueCount, 1)
        XCTAssertEqual(model.statusMessage, "Diagnostics checked for Mac mini cache with 1 issue(s).")
    }

    func testHLSCacheDiagnosticsCountsQuotaBlockedAsError() async {
        let client = DiagnosticsFakeCacheControlClient(
            serverInfo: CacheServerSummary(
                id: "server-1",
                name: "Mac mini cache",
                version: "0.5.0",
                mediaBaseURIs: [],
                capabilities: [
                    CacheServerCapability.bilibiliResolve,
                    CacheServerCapability.bilibiliTaskSelection,
                    CacheServerCapability.bilibiliCredentialStatus,
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
            hlsCacheStatus: .diagnosticsFixture(
                maxBytes: 100_000_000,
                highWatermarkBytes: 90_000_000,
                lowWatermarkBytes: 70_000_000,
                usedBytes: 95_000_000,
                lastEviction: .diagnosticsFixture(
                    targetReached: false,
                    targetUsedBytes: 80_000_000,
                    evictedBytes: 5_000_000
                )
            )
        )
        let model = CacheServerDiagnosticsViewModel(
            defaultServerAddressText: "mac-mini.local",
            clientFactory: { _ in client }
        )

        let result = await model.refresh()

        XCTAssertEqual(result, .succeeded)
        XCTAssertEqual(model.row(id: "hlsCache")?.severity, .error)
        XCTAssertEqual(model.row(id: "hlsCache")?.systemImage, "externaldrive.badge.xmark")
        XCTAssertTrue(model.row(id: "hlsCache")?.detail?.contains("Cleanup could not trim HLS cache") == true)
        XCTAssertEqual(model.snapshot?.issueCount, 1)
        XCTAssertEqual(model.statusMessage, "Diagnostics checked for Mac mini cache with 1 issue(s).")
    }

    func testHLSCacheDiagnosticsIncludesProjectedBytesForQuotaBlockedEviction() async {
        let client = DiagnosticsFakeCacheControlClient(
            serverInfo: CacheServerSummary(
                id: "server-1",
                name: "Mac mini cache",
                version: "0.5.0",
                mediaBaseURIs: [],
                capabilities: [
                    CacheServerCapability.bilibiliResolve,
                    CacheServerCapability.bilibiliTaskSelection,
                    CacheServerCapability.bilibiliCredentialStatus,
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
            hlsCacheStatus: .diagnosticsFixture(
                maxBytes: 100_000_000,
                highWatermarkBytes: 90_000_000,
                lowWatermarkBytes: 70_000_000,
                usedBytes: 20_000_000,
                lastEviction: .diagnosticsFixture(
                    targetReached: false,
                    targetUsedBytes: 80_000_000,
                    projectedAddedBytes: 75_000_000,
                    evictedBytes: 0
                )
            )
        )
        let model = CacheServerDiagnosticsViewModel(
            defaultServerAddressText: "mac-mini.local",
            clientFactory: { _ in client }
        )

        let result = await model.refresh()

        XCTAssertEqual(result, .succeeded)
        XCTAssertEqual(model.row(id: "hlsCache")?.severity, .error)
        XCTAssertEqual(model.row(id: "hlsCache")?.systemImage, "externaldrive.badge.xmark")
        XCTAssertTrue(model.row(id: "hlsCache")?.detail?.contains("Cleanup could not trim HLS cache") == true)
        XCTAssertEqual(model.snapshot?.issueCount, 1)
    }

    func testWeakNetworkDiagnosticsKeepsNormalStateReadyWhenLastChangedAtIsPresent() async {
        let client = DiagnosticsFakeCacheControlClient(
            serverInfo: CacheServerSummary(
                id: "server-1",
                name: "Mac mini cache",
                version: "0.5.0",
                mediaBaseURIs: [],
                capabilities: []
            ),
            hlsCacheStatus: .diagnosticsFixture(
                weakNetwork: HLSWeakNetworkStatus(
                    state: "HLS_WEAK_NETWORK_STATE_NORMAL",
                    message: "",
                    degradedSessionCount: 0,
                    unhealthyVariantCount: 0,
                    retryingVariantCount: 0,
                    cacheOnlySessionCount: 0,
                    lastChangedAt: Date(timeIntervalSince1970: 100)
                )
            )
        )
        let model = CacheServerDiagnosticsViewModel(
            defaultServerAddressText: "mac-mini.local",
            clientFactory: { _ in client }
        )

        let result = await model.refresh()

        XCTAssertEqual(result, .succeeded)
        XCTAssertEqual(model.row(id: "weakNetwork")?.value, "Normal")
        XCTAssertEqual(model.row(id: "weakNetwork")?.severity, .ready)
    }

    func testWeakNetworkDiagnosticsKeepsUnknownActiveStateVisible() async {
        let client = DiagnosticsFakeCacheControlClient(
            serverInfo: CacheServerSummary(
                id: "server-1",
                name: "Mac mini cache",
                version: "0.5.0",
                mediaBaseURIs: [],
                capabilities: []
            ),
            hlsCacheStatus: .diagnosticsFixture(
                weakNetwork: HLSWeakNetworkStatus(
                    state: "HLS_WEAK_NETWORK_STATE_CAPTIVE_PORTAL",
                    message: "",
                    degradedSessionCount: 0,
                    unhealthyVariantCount: 0,
                    retryingVariantCount: 0,
                    cacheOnlySessionCount: 0,
                    lastChangedAt: nil
                )
            )
        )
        let model = CacheServerDiagnosticsViewModel(
            defaultServerAddressText: "mac-mini.local",
            clientFactory: { _ in client }
        )

        let result = await model.refresh()

        XCTAssertEqual(result, .succeeded)
        XCTAssertEqual(model.row(id: "weakNetwork")?.value, "Weak network active")
        XCTAssertEqual(model.row(id: "weakNetwork")?.severity, .warning)
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

private extension HLSCacheStatus {
    static func diagnosticsFixture(
        maxBytes: Int64 = 100_000_000,
        highWatermarkBytes: Int64 = 90_000_000,
        lowWatermarkBytes: Int64 = 70_000_000,
        usedBytes: Int64 = 42_000_000,
        lastEviction: HLSCacheEvictionSummary? = nil,
        weakNetwork: HLSWeakNetworkStatus? = nil,
        transcoding: LanTranscodingStatus? = nil
    ) -> Self {
        Self(
            evictionEnabled: true,
            maxBytes: maxBytes,
            highWatermarkPercent: 90,
            lowWatermarkPercent: 70,
            highWatermarkBytes: highWatermarkBytes,
            lowWatermarkBytes: lowWatermarkBytes,
            usedBytes: usedBytes,
            completedSessionCount: 3,
            lastEviction: lastEviction,
            weakNetwork: weakNetwork,
            transcoding: transcoding
        )
    }
}

private extension HLSCacheEvictionSummary {
    static func diagnosticsFixture(
        targetReached: Bool = true,
        targetUsedBytes: Int64 = 0,
        projectedAddedBytes: Int64 = 0,
        evictedBytes: Int64 = 0
    ) -> Self {
        Self(
            reason: "highWatermark",
            startedUsedBytes: targetUsedBytes + evictedBytes,
            finishedUsedBytes: targetUsedBytes + max(0, evictedBytes / 2),
            targetUsedBytes: targetUsedBytes,
            projectedAddedBytes: projectedAddedBytes,
            evictedBytes: evictedBytes,
            evictedSessionIDs: [],
            targetReached: targetReached,
            completedAt: nil
        )
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
