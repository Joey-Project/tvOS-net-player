import XCTest
import TVOSNetPlayerCacheClient
@testable import TVOSNetPlayerCore

final class ResolverSettingsViewModelTests: XCTestCase {
    @MainActor
    func testUnsupportedServerAndReadFailuresExposeUnavailableState() async {
        let unsupported = ResolverSettingsClient(serverInfo: .fixture())
        let unsupportedModel = ResolverSettingsViewModel(client: unsupported)

        await unsupportedModel.load()

        XCTAssertFalse(unsupportedModel.isAvailable)
        XCTAssertNil(unsupportedModel.snapshot)
        XCTAssertEqual(unsupportedModel.errorMessage, "This cache server does not support resolver settings.")
        XCTAssertFalse(unsupportedModel.isLoading)

        let serverError = ResolverSettingsClient(serverError: .serverFailure)
        let serverErrorModel = ResolverSettingsViewModel(client: serverError)
        await serverErrorModel.load()
        XCTAssertEqual(serverErrorModel.errorMessage, "server failure")
        XCTAssertFalse(serverErrorModel.isAvailable)

        let readError = ResolverSettingsClient(settingsResults: [.failure(.settingsFailure)])
        let readErrorModel = ResolverSettingsViewModel(client: readError)
        await readErrorModel.load()
        XCTAssertEqual(readErrorModel.errorMessage, "settings failure")
        XCTAssertFalse(readErrorModel.isAvailable)
    }

    @MainActor
    func testEditsSaveExactPayloadAndAdvanceRevision() async throws {
        let initial = resolverSnapshot(revision: 12)
        let client = ResolverSettingsClient(settingsResults: [.success(initial)])
        let model = ResolverSettingsViewModel(client: client)
        await model.load()

        XCTAssertTrue(model.isAvailable)
        XCTAssertEqual(model.builtinEndpoints, initial.builtin)
        XCTAssertFalse(model.hasChanges)

        model.setBuiltin(initial.builtin[0], enabled: false)
        try model.updateCustom(
            at: 0,
            name: " Updated ",
            origin: "https://Updated.Example:443",
            regions: [.hk, .tw],
            enabled: false
        )
        try model.addCustom(name: "Added", origin: "https://added.example/", regions: [.cn])
        XCTAssertTrue(model.hasChanges)

        await model.save()

        let expected = [
            ResolverCustomEndpoint(
                name: "Updated",
                origin: "https://updated.example/",
                regions: [.hk, .tw],
                enabled: false
            ),
            ResolverCustomEndpoint(
                name: "Added",
                origin: "https://added.example/",
                regions: [.cn],
                enabled: true
            ),
        ]
        let requests = await client.updateRequests
        XCTAssertEqual(
            requests,
            [
                UpdateResolverSettingsRequest(
                    disabledBuiltinHostIDs: ["builtin.example"],
                    custom: expected,
                    expectedRevision: 12
                )
            ])
        XCTAssertEqual(model.snapshot?.revision, 13)
        XCTAssertEqual(model.customEndpoints, expected)
        XCTAssertEqual(model.disabledBuiltinHostIDs, ["builtin.example"])
        XCTAssertFalse(model.hasChanges)
        XCTAssertEqual(model.statusMessage, "Resolver settings saved.")
        XCTAssertNil(model.errorMessage)
    }

    @MainActor
    func testConflictReloadDiscardsSubmittedDraftAndReportsReloadFailure() async throws {
        let initial = resolverSnapshot(revision: 4)
        let latest = resolverSnapshot(
            revision: 5,
            disabled: ["builtin.example"],
            customName: "Server value"
        )
        let client = ResolverSettingsClient(
            settingsResults: [.success(initial), .success(latest)],
            updateResult: .conflict
        )
        let model = ResolverSettingsViewModel(client: client)
        await model.load()
        try model.addCustom(name: "Local draft", origin: "https://draft.example", regions: [.all])

        await model.save()

        XCTAssertEqual(model.snapshot, latest)
        XCTAssertEqual(model.customEndpoints, latest.custom)
        XCTAssertTrue(model.disabledBuiltinHostIDs.contains("builtin.example"))
        XCTAssertFalse(model.hasChanges)
        XCTAssertTrue(model.errorMessage?.contains("unsaved edits were discarded") == true)

        let failingClient = ResolverSettingsClient(
            settingsResults: [.success(initial), .failure(.settingsFailure)],
            updateResult: .conflict
        )
        let failingModel = ResolverSettingsViewModel(client: failingClient)
        await failingModel.load()
        try failingModel.addCustom(name: "Keep this draft", origin: "https://keep.example", regions: [.hk])
        let draft = failingModel.customEndpoints

        await failingModel.save()

        XCTAssertEqual(failingModel.snapshot, initial)
        XCTAssertEqual(failingModel.customEndpoints, draft)
        XCTAssertTrue(failingModel.hasChanges)
        XCTAssertTrue(failingModel.errorMessage?.contains("latest values could not be loaded") == true)
        XCTAssertTrue(failingModel.errorMessage?.contains("settings failure") == true)
    }

    @MainActor
    func testCustomEndpointValidationNormalizationDuplicatesAndLimit() async throws {
        let model = ResolverSettingsViewModel(
            client: ResolverSettingsClient(settingsResults: [.success(resolverSnapshot(revision: 1))])
        )
        await model.load()

        try model.addCustom(name: "  Custom name  ", origin: " HTTPS://MiXeD.Example:443/ ", regions: [.tw])
        XCTAssertEqual(model.customEndpoints.last?.name, "Custom name")
        XCTAssertEqual(model.customEndpoints.last?.origin, "https://mixed.example/")

        XCTAssertThrowsError(try model.addCustom(name: "", origin: "https://empty-name.example", regions: [.all]))
        XCTAssertThrowsError(
            try model.addCustom(name: "Bad\u{0001}name", origin: "https://control.example", regions: [.all]))
        XCTAssertThrowsError(try model.addCustom(name: "Plain", origin: "http://plain.example", regions: [.all]))
        XCTAssertThrowsError(try model.addCustom(name: "Path", origin: "https://path.example/api", regions: [.all]))
        XCTAssertThrowsError(
            try model.addCustom(name: "Credentials", origin: "https://u:p@auth.example", regions: [.all]))
        XCTAssertThrowsError(try model.addCustom(name: "Query", origin: "https://query.example/?x=1", regions: [.all]))
        XCTAssertThrowsError(
            try model.addCustom(name: "Bad port", origin: "https://port.example:65536", regions: [.all]))
        XCTAssertThrowsError(try model.addCustom(name: "IP", origin: "https://192.0.2.1", regions: [.all]))
        XCTAssertThrowsError(try model.addCustom(name: "No region", origin: "https://region.example", regions: []))
        XCTAssertThrowsError(
            try model.addCustom(name: "Repeated region", origin: "https://region.example", regions: [.hk, .hk]))
        XCTAssertThrowsError(
            try model.addCustom(name: "Host collision", origin: "https://MIXED.example", regions: [.all]))
        XCTAssertThrowsError(
            try model.addCustom(name: "Builtin collision", origin: "https://builtin.example", regions: [.all]))

        for index in 1..<31 {
            try model.addCustom(name: "Entry \(index)", origin: "https://entry\(index).example", regions: [.all])
        }
        XCTAssertEqual(model.customEndpoints.count, 32)
        XCTAssertThrowsError(try model.addCustom(name: "Over limit", origin: "https://over.example", regions: [.all]))

        try model.updateCustom(
            at: 0, name: "Disabled", origin: "https://existing.example", regions: [.cn], enabled: false)
        XCTAssertFalse(model.customEndpoints[0].enabled)
        XCTAssertThrowsError(
            try model.updateCustom(
                at: 0, name: "Collision", origin: "https://entry1.example", regions: [.all], enabled: true))
        XCTAssertThrowsError(
            try model.updateCustom(
                at: 0, name: "Collision", origin: "https://builtin.example", regions: [.all], enabled: true))
        XCTAssertThrowsError(
            try model.updateCustom(
                at: 99, name: "Missing", origin: "https://missing.example", regions: [.all], enabled: true))
        model.removeCustom(at: 0)
        model.removeCustom(at: 500)
        XCTAssertEqual(model.customEndpoints.count, 31)
    }

    @MainActor
    func testCustomEndpointIdentityIncludesNonDefaultPortAndCanonicalizes443() async throws {
        let model = ResolverSettingsViewModel(
            client: ResolverSettingsClient(settingsResults: [.success(resolverSnapshot(revision: 1))])
        )
        await model.load()

        try model.addCustom(name: "8443", origin: "https://builtin.example:8443", regions: [.all])
        try model.addCustom(name: "9443", origin: "https://builtin.example:9443", regions: [.all])
        XCTAssertThrowsError(
            try model.addCustom(name: "Duplicate 8443", origin: "https://BUILTIN.example:8443/", regions: [.all])
        ) { error in
            XCTAssertEqual(error as? ResolverSettingsValidationError, .duplicateHost)
        }

        try model.addCustom(name: "Default port", origin: "https://default-port.example:443", regions: [.all])
        XCTAssertEqual(model.customEndpoints.last?.origin, "https://default-port.example/")
        XCTAssertThrowsError(
            try model.addCustom(name: "Duplicate default port", origin: "https://default-port.example", regions: [.all])
        ) { error in
            XCTAssertEqual(error as? ResolverSettingsValidationError, .duplicateHost)
        }
    }

    @MainActor
    func testPersistedPortedBuiltinHostnameCanBeEditedAndToggled() async throws {
        let initial = ResolverSettingsSnapshot(
            builtin: resolverBuiltinFixture,
            disabledBuiltinHostIDs: [],
            custom: [
                ResolverCustomEndpoint(
                    name: "Ported custom",
                    origin: "https://builtin.example:8443/",
                    regions: [.all],
                    enabled: true
                )
            ],
            revision: 1
        )
        let model = ResolverSettingsViewModel(client: ResolverSettingsClient(settingsResults: [.success(initial)]))
        await model.load()

        try model.updateCustom(
            at: 0,
            name: "Updated ported custom",
            origin: "https://builtin.example:8443",
            regions: [.hk],
            enabled: false
        )

        XCTAssertEqual(model.customEndpoints.first?.name, "Updated ported custom")
        XCTAssertEqual(model.customEndpoints.first?.origin, "https://builtin.example:8443/")
        XCTAssertFalse(model.customEndpoints.first?.enabled ?? true)
    }

    @MainActor
    func testSecurityWarningNamesUnauthenticatedControlAndSharedKeyDisclosure() {
        let warning = ResolverSettingsViewModel.securityWarning.lowercased()
        XCTAssertTrue(warning.contains("no authentication"))
        XCTAssertTrue(warning.contains("shared access_key"))
        XCTAssertTrue(warning.contains("third-party resolver"))
        XCTAssertTrue(warning.contains("built-ins and custom resolvers"))
    }

    @MainActor
    func testOlderOverlappingLoadCannotOverwriteNewerLoad() async {
        let first = resolverSnapshot(revision: 1, customName: "Old response")
        let second = resolverSnapshot(revision: 2, customName: "New response")
        let client = ResolverSettingsClient(
            settingsResults: [.success(first), .success(second)],
            suspendedSettingsCalls: [1]
        )
        let model = ResolverSettingsViewModel(client: client)

        let olderLoad = Task { await model.load() }
        await client.waitForSettingsCall(1)
        let newerLoad = Task { await model.load() }
        await client.waitForSettingsCall(2)
        await newerLoad.value
        await client.releaseSettingsCall(1)
        await olderLoad.value

        XCTAssertEqual(model.snapshot, second)
        XCTAssertEqual(model.customEndpoints.first?.name, "New response")
        XCTAssertFalse(model.isLoading)
    }

    @MainActor
    func testLoadAndSaveResponsesPreserveEditsMadeWhileRequestsAreInFlight() async throws {
        let original = resolverSnapshot(revision: 7)
        let fromLoad = resolverSnapshot(revision: 8, customName: "Loaded")
        let loadClient = ResolverSettingsClient(
            settingsResults: [.success(original), .success(fromLoad)],
            suspendedSettingsCalls: [2]
        )
        let loadModel = ResolverSettingsViewModel(client: loadClient)
        await loadModel.load()
        let loadTask = Task { await loadModel.load() }
        await loadClient.waitForSettingsCall(2)
        try loadModel.updateCustom(
            at: 0, name: "Edit while loading", origin: "https://edited.example", regions: [.hk], enabled: true)
        await loadClient.releaseSettingsCall(2)
        await loadTask.value
        XCTAssertEqual(loadModel.snapshot, fromLoad)
        XCTAssertEqual(loadModel.customEndpoints.first?.name, "Edit while loading")
        XCTAssertTrue(loadModel.hasChanges)

        let saveClient = ResolverSettingsClient(
            settingsResults: [.success(original)],
            suspendUpdate: true
        )
        let saveModel = ResolverSettingsViewModel(client: saveClient)
        await saveModel.load()
        try saveModel.updateCustom(
            at: 0, name: "Submitted", origin: "https://submitted.example", regions: [.tw], enabled: true)
        let saveTask = Task { await saveModel.save() }
        await saveClient.waitForUpdateCall()
        try saveModel.updateCustom(
            at: 0, name: "Newer edit", origin: "https://newer.example", regions: [.cn], enabled: false)
        await saveClient.releaseUpdate()
        await saveTask.value

        XCTAssertEqual(saveModel.snapshot?.revision, 8)
        XCTAssertEqual(saveModel.customEndpoints.first?.name, "Newer edit")
        XCTAssertTrue(saveModel.hasChanges)
        await saveModel.save()
        let requests = await saveClient.updateRequests
        XCTAssertEqual(requests.map(\.expectedRevision), [7, 8])
        XCTAssertEqual(requests.last?.custom.first?.name, "Newer edit")
        XCTAssertEqual(saveModel.snapshot?.revision, 9)
        XCTAssertFalse(saveModel.hasChanges)
    }

    @MainActor
    func testConflictReloadDoesNotDiscardEditsMadeDuringReload() async throws {
        let initial = resolverSnapshot(revision: 2)
        let latest = resolverSnapshot(revision: 3, customName: "Remote")
        let client = ResolverSettingsClient(
            settingsResults: [.success(initial), .success(latest)],
            updateResult: .conflict,
            suspendedSettingsCalls: [2]
        )
        let model = ResolverSettingsViewModel(client: client)
        await model.load()
        try model.addCustom(name: "Submitted draft", origin: "https://draft.example", regions: [.all])

        let saveTask = Task { await model.save() }
        await client.waitForSettingsCall(2)
        try model.updateCustom(
            at: 0, name: "Newer draft", origin: "https://newer.example", regions: [.hk], enabled: false)
        await client.releaseSettingsCall(2)
        await saveTask.value

        XCTAssertEqual(model.snapshot, latest)
        XCTAssertEqual(model.customEndpoints.first?.name, "Newer draft")
        XCTAssertTrue(model.hasChanges)
        XCTAssertTrue(model.errorMessage?.contains("newer edits made while saving were preserved") == true)
    }

    private func resolverSnapshot(
        revision: UInt64,
        disabled: [String] = [],
        customName: String = "Existing"
    ) -> ResolverSettingsSnapshot {
        ResolverSettingsSnapshot(
            builtin: [
                ResolverEndpoint(
                    hostID: "builtin.example",
                    name: "Bundled",
                    origin: "https://builtin.example/",
                    regions: [.all]
                )
            ],
            disabledBuiltinHostIDs: disabled,
            custom: [
                ResolverCustomEndpoint(
                    name: customName,
                    origin: "https://existing.example/",
                    regions: [ResolverRegion(rawValue: "future-region")],
                    enabled: true
                )
            ],
            revision: revision
        )
    }

    private var resolverBuiltinFixture: [ResolverEndpoint] {
        [
            ResolverEndpoint(
                hostID: "builtin.example", name: "Bundled", origin: "https://builtin.example/", regions: [.all])
        ]
    }
}

private actor ResolverSettingsClient: CacheControlClient {
    private let serverInfo: CacheServerSummary
    private let serverError: ResolverSettingsClientError?
    private let settingsResults: [Result<ResolverSettingsSnapshot, ResolverSettingsClientError>]
    private let updateResult: UpdateResult
    private let suspendedSettingsCalls: Set<Int>
    private let suspendUpdate: Bool
    private var settingsCallCount = 0
    private var settingsCallWaiters: [(Int, CheckedContinuation<Void, Never>)] = []
    private var settingsContinuations: [Int: CheckedContinuation<Void, Never>] = [:]
    private var updateStarted: CheckedContinuation<Void, Never>?
    private var updateContinuation: CheckedContinuation<Void, Never>?
    private var updateStartedWaiters: [CheckedContinuation<Void, Never>] = []
    private(set) var updateRequests: [UpdateResolverSettingsRequest] = []

    init(
        serverInfo: CacheServerSummary = .fixture(capabilities: [CacheServerCapability.resolverSettingsWrite]),
        serverError: ResolverSettingsClientError? = nil,
        settingsResults: [Result<ResolverSettingsSnapshot, ResolverSettingsClientError>] = [],
        updateResult: UpdateResult = .success,
        suspendedSettingsCalls: Set<Int> = [],
        suspendUpdate: Bool = false
    ) {
        self.serverInfo = serverInfo
        self.serverError = serverError
        self.settingsResults = settingsResults
        self.updateResult = updateResult
        self.suspendedSettingsCalls = suspendedSettingsCalls
        self.suspendUpdate = suspendUpdate
    }

    func getServerInfo() async throws -> CacheServerSummary {
        if let serverError { throw serverError }
        return serverInfo
    }

    func getResolverSettings() async throws -> ResolverSettingsSnapshot {
        settingsCallCount += 1
        let call = settingsCallCount
        let ready = settingsCallWaiters.filter { $0.0 <= call }
        settingsCallWaiters.removeAll { $0.0 <= call }
        ready.forEach { $0.1.resume() }
        if suspendedSettingsCalls.contains(call) {
            await withCheckedContinuation { settingsContinuations[call] = $0 }
        }
        guard settingsResults.indices.contains(call - 1) else { throw ResolverSettingsClientError.noResponse }
        return try settingsResults[call - 1].get()
    }

    func updateResolverSettings(_ request: UpdateResolverSettingsRequest) async throws -> ResolverSettingsSnapshot {
        updateRequests.append(request)
        updateStartedWaiters.forEach { $0.resume() }
        updateStartedWaiters.removeAll()
        if suspendUpdate, updateRequests.count == 1 {
            await withCheckedContinuation { continuation in
                updateContinuation = continuation
                if let updateStarted {
                    self.updateStarted = nil
                    updateStarted.resume()
                }
            }
        }
        if case .conflict = updateResult { throw CacheControlClientRevisionConflict() }
        let nextRevision = request.expectedRevision + 1
        return ResolverSettingsSnapshot(
            builtin: resolverBuiltinFixture,
            disabledBuiltinHostIDs: request.disabledBuiltinHostIDs,
            custom: request.custom,
            revision: nextRevision
        )
    }

    func waitForSettingsCall(_ count: Int) async {
        guard settingsCallCount < count else { return }
        await withCheckedContinuation { settingsCallWaiters.append((count, $0)) }
    }

    func releaseSettingsCall(_ count: Int) {
        settingsContinuations.removeValue(forKey: count)?.resume()
    }

    func waitForUpdateCall() async {
        guard updateRequests.isEmpty else { return }
        await withCheckedContinuation { updateStartedWaiters.append($0) }
        if suspendUpdate, updateContinuation == nil {
            await withCheckedContinuation { updateStarted = $0 }
        }
    }

    func releaseUpdate() {
        updateContinuation?.resume()
        updateContinuation = nil
    }

    private var resolverBuiltinFixture: [ResolverEndpoint] {
        [
            ResolverEndpoint(
                hostID: "builtin.example", name: "Bundled", origin: "https://builtin.example/", regions: [.all])
        ]
    }

    func checkHealth() async throws -> CacheHealthStatus { fatalError() }
    func getBilibiliCredentialStatus() async throws -> BilibiliCredentialStatus { fatalError() }
    func listBilibiliCredentialProfiles() async throws -> BilibiliCredentialProfilesSummary { fatalError() }
    func startBilibiliLoginSession(profileID: String, method: BilibiliLoginMethod) async throws -> BilibiliLoginSession
    { fatalError() }
    func getBilibiliLoginSession(id: String) async throws -> BilibiliLoginSession { fatalError() }
    func listCacheRoots() async throws -> [CacheRoot] { fatalError() }
    func getHLSCacheStatus() async throws -> HLSCacheStatus { fatalError() }
    func reportPlaybackProgress(_ report: PlaybackProgressReport) async throws -> PlaybackProgressReportResult {
        fatalError()
    }
    func listLibraryItemsPage(pageToken: String, pageSize: Int, searchText: String?) async throws
        -> CacheLibraryItemsPage
    { fatalError() }
    func getPlaybackSource(itemID: String, variantID: String) async throws -> CachePlaybackSource { fatalError() }
    func deleteLibraryItem(id: String) async throws -> Bool { fatalError() }
    func getTask(id: String) async throws -> CacheTask { fatalError() }
    func listTaskResults(taskID: String, pageToken: String, pageSize: Int) async throws -> CacheTaskResultsPage {
        fatalError()
    }
    func watchTasks(ids: [String]) async -> AsyncThrowingStream<CacheTask, Error> { fatalError() }
    func cancelTask(id: String) async throws -> CacheTask { fatalError() }
    func resolveBilibiliInput(urlOrID: String, options: BilibiliPlaybackTaskOptions) async throws
        -> BilibiliResolveResult
    { fatalError() }
    func startBilibiliResolution(urlOrID: String, options: BilibiliPlaybackTaskOptions, pageSize: Int) async throws
        -> BilibiliResolutionPage
    { fatalError() }
    func startBilibiliResolution(
        urlOrID: String, options: BilibiliPlaybackTaskOptions, context: BilibiliRequestContext, pageSize: Int
    ) async throws -> BilibiliResolutionPage { fatalError() }
    func listBilibiliResolutionCandidates(sessionID: String, pageToken: String, pageSize: Int) async throws
        -> BilibiliResolutionPage
    { fatalError() }
    func createBilibiliTask(urlOrID: String, options: BilibiliDownloadTaskOptions) async throws -> CacheTask {
        fatalError()
    }
    func createBilibiliPlaybackTask(urlOrID: String, options: BilibiliPlaybackTaskOptions) async throws -> CacheTask {
        fatalError()
    }
    func createBilibiliPlaybackTask(urlOrID: String, selectionID: String?, options: BilibiliPlaybackTaskOptions)
        async throws -> CacheTask
    { fatalError() }
    func createBilibiliPlaybackTask(
        urlOrID: String, selection: BilibiliTaskSelection?, options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask { fatalError() }
    func createBilibiliPlaybackTaskV2(sessionID: String, selection: BilibiliResolutionSelection) async throws
        -> CacheTask
    { fatalError() }
    func createBilibiliTaskV2(
        sessionID: String, selection: BilibiliResolutionSelection, execution: BilibiliTaskExecution
    ) async throws -> CacheTask { fatalError() }
}

private enum UpdateResult: Sendable {
    case success
    case conflict
}

private enum ResolverSettingsClientError: Error, LocalizedError, Sendable {
    case serverFailure
    case settingsFailure
    case noResponse

    var errorDescription: String? {
        switch self {
        case .serverFailure: "server failure"
        case .settingsFailure: "settings failure"
        case .noResponse: "no response"
        }
    }
}

private extension CacheServerSummary {
    static func fixture(capabilities: [String] = []) -> CacheServerSummary {
        CacheServerSummary(
            id: "server", name: "Resolver server", version: "1", mediaBaseURIs: [], capabilities: capabilities)
    }
}
