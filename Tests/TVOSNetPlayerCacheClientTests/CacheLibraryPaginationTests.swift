import XCTest
@testable import TVOSNetPlayerCacheClient

final class CacheLibraryPaginationTests: XCTestCase {
    func testGeneratedBilibiliResolveCapabilityMatchesPublicConstant() {
        XCTAssertEqual(
            String(describing: TvosNetPlayer_V1_ServerCapability.bilibiliResolve),
            CacheServerCapability.bilibiliResolve
        )
    }

    func testGeneratedBilibiliTaskSelectionCapabilityMatchesPublicConstant() {
        XCTAssertEqual(
            String(describing: TvosNetPlayer_V1_ServerCapability.bilibiliTaskSelection),
            CacheServerCapability.bilibiliTaskSelection
        )
    }

    func testGeneratedBilibiliCredentialStatusCapabilityMatchesPublicConstant() {
        XCTAssertEqual(
            String(describing: TvosNetPlayer_V1_ServerCapability.bilibiliCredentialStatus),
            CacheServerCapability.bilibiliCredentialStatus
        )
    }

    func testGeneratedBilibiliCredentialProfilesCapabilityMatchesPublicConstant() {
        XCTAssertEqual(
            String(describing: TvosNetPlayer_V1_ServerCapability.bilibiliCredentialProfiles),
            CacheServerCapability.bilibiliCredentialProfiles
        )
    }

    func testGeneratedBilibiliLoginSessionsCapabilityMatchesPublicConstant() {
        XCTAssertEqual(
            String(describing: TvosNetPlayer_V1_ServerCapability.bilibiliLoginSessions),
            CacheServerCapability.bilibiliLoginSessions
        )
    }

    func testGeneratedBilibiliPlaybackPolicyCapabilityMatchesPublicConstant() {
        XCTAssertEqual(
            String(describing: TvosNetPlayer_V1_ServerCapability.bilibiliPlaybackPolicy),
            CacheServerCapability.bilibiliPlaybackPolicy
        )
    }

    func testGeneratedLanTranscodingCapabilityMatchesPublicConstant() {
        XCTAssertEqual(
            String(describing: TvosNetPlayer_V1_ServerCapability.lanTranscoding),
            CacheServerCapability.lanTranscoding
        )
    }

    func testGeneratedTaskOutputV2CapabilityMatchesPublicConstant() {
        XCTAssertEqual(
            String(describing: TvosNetPlayer_V1_ServerCapability.taskOutputV2),
            CacheServerCapability.taskOutputV2
        )

        let summary = CacheServerSummary(
            id: "server-1",
            name: "Cache server",
            version: "1.0.0",
            mediaBaseURIs: [],
            capabilities: [CacheServerCapability.taskOutputV2]
        )
        XCTAssertTrue(summary.supportsTaskOutputV2)
    }

    func testTaskOutputSummaryMapsToPublicModel() {
        var proto = TvosNetPlayer_V1_Task()
        proto.id = "task-output-v2"
        proto.outputSummary.revision = 7
        proto.outputSummary.resultCount = 3
        proto.outputSummary.terminalResultCount = 2
        proto.outputSummary.successfulResultCount = 1
        proto.outputSummary.failedResultCount = 1
        proto.outputSummary.cancelledResultCount = 0
        proto.outputSummary.availableArtifactCount = 4
        proto.outputSummary.primaryResultID = "result-1"

        let task = CacheTask(proto)

        XCTAssertEqual(task.outputSummary?.revision, 7)
        XCTAssertEqual(task.outputSummary?.resultCount, 3)
        XCTAssertEqual(task.outputSummary?.terminalResultCount, 2)
        XCTAssertEqual(task.outputSummary?.successfulResultCount, 1)
        XCTAssertEqual(task.outputSummary?.failedResultCount, 1)
        XCTAssertEqual(task.outputSummary?.cancelledResultCount, 0)
        XCTAssertEqual(task.outputSummary?.availableArtifactCount, 4)
        XCTAssertEqual(task.outputSummary?.primaryResultID, "result-1")
    }

    func testTaskOutputV2DefaultsRemainCompatibleWithLegacyTasks() {
        let task = CacheTask(TvosNetPlayer_V1_Task())
        let page = CacheTaskResultsPage(TvosNetPlayer_V1_ListTaskResultsResponse())

        XCTAssertNil(task.outputSummary)
        XCTAssertTrue(page.results.isEmpty)
        XCTAssertEqual(page.totalSize, 0)
        XCTAssertEqual(page.nextPageToken, "")
        XCTAssertEqual(page.snapshotID, "")
        XCTAssertEqual(page.outputRevision, 0)
    }

    func testTaskResultsPageMapsArtifactsProblemsAndResourceMetadata() {
        var proto = TvosNetPlayer_V1_ListTaskResultsResponse()
        proto.pageInfo.totalSize = 2
        proto.pageInfo.nextPageToken = "opaque-page-token"
        proto.pageInfo.snapshotID = "snapshot-7"
        proto.outputRevision = 7

        var result = TvosNetPlayer_V1_TaskResult()
        result.id = "result-1"
        result.state = .failed
        result.title = "Episode 1"
        result.subtitle = "Page 1"
        result.progress.fraction = 0.5
        result.progress.completedBytes = 512
        result.progress.totalBytes = 1_024
        result.progress.totalBytesKnown = true
        result.progress.phase = "downloading"
        result.problem.category = .authentication
        result.problem.code = "bilibili.credential_required"
        result.problem.message = "A credential profile is required."
        result.problem.retryable = true
        result.libraryItemID = "library-1"
        result.playbackSource.itemID = "library-1"
        result.playbackSource.variantID = "h264"
        result.playbackSource.`protocol` = .hls
        result.playbackSource.uri = "/v1/library/library-1/master.m3u8"
        result.createdAt.seconds = 100
        result.updatedAt.seconds = 200

        var artifact = TvosNetPlayer_V1_TaskArtifact()
        artifact.id = "subtitle-ja"
        artifact.kind = .subtitle
        artifact.state = .available
        artifact.title = "Japanese"
        artifact.format = "ass"
        artifact.languageTag = "ja-JP"
        artifact.isAiGenerated = false
        artifact.resource.id = "resource-1"
        artifact.resource.uri = "/v1/resources/resource-1"
        artifact.resource.contentType = "text/x-ass"
        artifact.resource.sizeBytes = 4_096
        artifact.resource.sizeKnown = true
        artifact.resource.supportsByteRanges = true
        artifact.resource.etag = "resource-etag"
        artifact.resource.expiresAt.seconds = 300
        result.artifacts = [artifact]
        proto.results = [result]

        let page = CacheTaskResultsPage(proto)
        let mapped = page.results.first
        let mappedArtifact = mapped?.artifacts.first

        XCTAssertEqual(page.totalSize, 2)
        XCTAssertEqual(page.nextPageToken, "opaque-page-token")
        XCTAssertEqual(page.snapshotID, "snapshot-7")
        XCTAssertTrue(page.hasMoreResults)
        XCTAssertTrue(page.pageInfo.hasMoreItems)
        XCTAssertEqual(page.outputRevision, 7)
        XCTAssertEqual(mapped?.progress?.completedBytes, 512)
        XCTAssertEqual(mapped?.problem?.category, "authentication")
        XCTAssertEqual(mapped?.problem?.code, "bilibili.credential_required")
        XCTAssertEqual(mapped?.playbackSource?.uri, "/v1/library/library-1/master.m3u8")
        XCTAssertEqual(mapped?.createdAt, Date(timeIntervalSince1970: 100))
        XCTAssertEqual(mappedArtifact?.kind, "subtitle")
        XCTAssertEqual(mappedArtifact?.state, "available")
        XCTAssertEqual(mappedArtifact?.languageTag, "ja-JP")
        XCTAssertEqual(mappedArtifact?.resource?.contentType, "text/x-ass")
        XCTAssertEqual(mappedArtifact?.resource?.sizeBytes, 4_096)
        XCTAssertEqual(mappedArtifact?.resource?.expiresAt, Date(timeIntervalSince1970: 300))
    }

    func testBilibiliTaskSchemaMapsSelectionAndResultItems() {
        var proto = TvosNetPlayer_V1_Task()
        proto.id = "task-1"
        proto.kind = .bilibiliProgressivePlayback
        proto.state = .completed
        proto.source = "BV1schema"
        proto.title = "Schema video"
        proto.message = "Completed."
        proto.libraryItemID = "bilibili.hls.task-1"
        proto.playbackSession.id = "task-1"
        proto.playbackSession.title = "Schema video"
        proto.playbackSession.contentID = "cid-1"
        proto.playbackSession.selectedVariantID = "h264"
        proto.playbackSession.transcodingPlan.state = .notRequired
        proto.playbackSession.transcodingPlan.profileID = "avplayer-h264-aac-hls-v1"
        proto.playbackSession.transcodingPlan.reason = "Already compatible."
        proto.playbackSession.transcodingPlan.sourceVariantID = "h264"
        proto.playbackSession.transcodingPlan.targetContainer = "hls/fmp4"
        proto.playbackSession.transcodingPlan.targetVideoCodec = "h264"
        proto.playbackSession.transcodingPlan.targetAudioCodec = "aac"
        proto.playbackSession.transcodingPlan.outputProtocol = .hls
        proto.playbackSession.effectivePolicy.transcodingPreference = .force
        proto.playbackSession.effectivePolicy.compatibleVariantPreference = .preferRequested
        proto.playbackSession.effectivePolicy.weakNetworkPreference = .holdDowngrade
        proto.bilibiliSelection.mode = .range
        proto.bilibiliSelection.selectionIds = ["page:1", "page:2"]
        proto.bilibiliSelection.rangeStartIndex = 1
        proto.bilibiliSelection.rangeEndIndex = 2

        var result = TvosNetPlayer_V1_BilibiliTaskResultItem()
        result.id = "result-1"
        result.selectionID = "page:1"
        result.title = "Part 1"
        result.subtitle = "Page 1"
        result.sourceKind = "video_page"
        result.contentID = "BV1schema:cid1"
        result.index = 1
        result.state = .completed
        result.message = "Cached."
        result.libraryItemID = "bilibili.hls.result-1"
        result.playbackSource.itemID = "bilibili.hls.result-1"
        result.playbackSource.variantID = "h264"
        result.playbackSource.`protocol` = .hls
        result.playbackSource.uri = "http://mac-mini.local:8080/hls/result-1/master.m3u8"
        proto.resultItems = [result]

        let task = CacheTask(proto)
        let expectedSelectionMode = String(
            describing: TvosNetPlayer_V1_BilibiliTaskSelectionMode.range
        )
        let expectedResultState = String(describing: TvosNetPlayer_V1_TaskState.completed)
        let expectedPlaybackProtocol = String(describing: TvosNetPlayer_V1_PlaybackProtocol.hls)
        let expectedTranscodingState = String(
            describing: TvosNetPlayer_V1_LanTranscodingPlanState.notRequired
        )

        XCTAssertEqual(task.bilibiliSelection?.mode, expectedSelectionMode)
        XCTAssertEqual(task.bilibiliSelection?.selectionIDs, ["page:1", "page:2"])
        XCTAssertEqual(task.bilibiliSelection?.rangeStartIndex, 1)
        XCTAssertEqual(task.bilibiliSelection?.rangeEndIndex, 2)
        XCTAssertEqual(task.resultItems.map(\.selectionID), ["page:1"])
        XCTAssertEqual(task.resultItems.first?.state, expectedResultState)
        XCTAssertEqual(task.resultItems.first?.playbackSource?.playbackProtocol, expectedPlaybackProtocol)
        XCTAssertEqual(task.playbackSession?.transcodingPlan?.state, expectedTranscodingState)
        XCTAssertEqual(task.playbackSession?.transcodingPlan?.outputProtocol, expectedPlaybackProtocol)
        XCTAssertEqual(
            task.playbackSession?.effectivePolicy,
            BilibiliPlaybackPolicy(
                transcodingPreference: .force,
                compatibleVariantPreference: .preferRequested,
                weakNetworkPreference: .holdDowngrade
            )
        )
    }

    func testBilibiliPlaybackTaskOptionsMapPolicyToGeneratedSchema() {
        let options = BilibiliPlaybackTaskOptions(
            qualityPreference: "1080p",
            encodingPreference: "h264",
            audioLanguagePreference: "ja-jp",
            preferTVAPI: true,
            playbackPolicy: BilibiliPlaybackPolicy(
                transcodingPreference: .never,
                compatibleVariantPreference: .preferRequested,
                weakNetworkPreference: .avPlayerManaged
            )
        )

        let proto = TvosNetPlayer_V1_BilibiliPlaybackOptions(options)

        XCTAssertEqual(proto.qualityPreference, "1080p")
        XCTAssertEqual(proto.encodingPreference, "h264")
        XCTAssertEqual(proto.audioLanguage, "ja-jp")
        XCTAssertTrue(proto.preferTvApi)
        XCTAssertEqual(proto.playbackPolicy.transcodingPreference, .never)
        XCTAssertEqual(proto.playbackPolicy.compatibleVariantPreference, .preferRequested)
        XCTAssertEqual(proto.playbackPolicy.weakNetworkPreference, .avplayerManaged)
    }

    func testBilibiliPlaybackTaskOptionsDefaultsMapConservativePolicy() {
        let options = BilibiliPlaybackTaskOptions()
        let proto = TvosNetPlayer_V1_BilibiliPlaybackOptions(options)

        XCTAssertEqual(options.playbackPolicy, .default)
        XCTAssertEqual(proto.playbackPolicy.transcodingPreference, .auto)
        XCTAssertEqual(proto.playbackPolicy.compatibleVariantPreference, .preferCompatible)
        XCTAssertEqual(proto.playbackPolicy.weakNetworkPreference, .adaptive)
    }

    func testBilibiliDownloadTaskOptionsMapToGeneratedSchema() {
        let options = BilibiliDownloadTaskOptions(
            qualityPreference: "1080p",
            encodingPreference: "h264",
            audioLanguagePreference: "ja-jp",
            preferTVAPI: true,
            downloadSubtitles: true,
            downloadDanmaku: true,
            downloadCover: true,
            subtitleAIPolicy: .excludeAI,
            danmakuFormats: [.xml, .ass]
        )

        let proto = TvosNetPlayer_V1_BilibiliDownloadOptions(options)

        XCTAssertEqual(proto.qualityPreference, "1080p")
        XCTAssertEqual(proto.encodingPreference, "h264")
        XCTAssertEqual(proto.audioLanguage, "ja-jp")
        XCTAssertTrue(proto.preferTvApi)
        XCTAssertTrue(proto.downloadSubtitles)
        XCTAssertTrue(proto.downloadDanmaku)
        XCTAssertTrue(proto.downloadCover)
        XCTAssertEqual(proto.subtitleAiPolicy, .excludeAi)
        XCTAssertEqual(proto.danmakuFormats, [.xml, .ass])
    }

    func testBilibiliDownloadTaskOptionsDefaultsPreserveWireDefaults() {
        let proto = TvosNetPlayer_V1_BilibiliDownloadOptions(BilibiliDownloadTaskOptions())

        XCTAssertTrue(proto.qualityPreference.isEmpty)
        XCTAssertTrue(proto.encodingPreference.isEmpty)
        XCTAssertTrue(proto.audioLanguage.isEmpty)
        XCTAssertFalse(proto.preferTvApi)
        XCTAssertFalse(proto.downloadSubtitles)
        XCTAssertFalse(proto.downloadDanmaku)
        XCTAssertFalse(proto.downloadCover)
        XCTAssertEqual(proto.subtitleAiPolicy, .unspecified)
        XCTAssertTrue(proto.danmakuFormats.isEmpty)
    }

    func testCacheServerSummaryExposesBilibiliSupport() {
        let supported = CacheServerSummary(
            id: "server-1",
            name: "Test cache",
            version: "0.1.0",
            mediaBaseURIs: [],
            capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliCredentialProfiles,
                CacheServerCapability.bilibiliLoginSessions,
                CacheServerCapability.bilibiliPlaybackPolicy,
                CacheServerCapability.bilibiliResolve,
                CacheServerCapability.bilibiliTaskSelection,
                CacheServerCapability.lanTranscoding,
            ]
        )
        let unsupported = CacheServerSummary(
            id: "server-2",
            name: "Old cache",
            version: "0.1.0",
            mediaBaseURIs: [],
            capabilities: []
        )

        XCTAssertTrue(supported.supportsBilibiliCredentialStatus)
        XCTAssertTrue(supported.supportsBilibiliCredentialProfiles)
        XCTAssertTrue(supported.supportsBilibiliLoginSessions)
        XCTAssertTrue(supported.supportsBilibiliPlaybackPolicy)
        XCTAssertTrue(supported.supportsBilibiliResolve)
        XCTAssertTrue(supported.supportsBilibiliTaskSelection)
        XCTAssertTrue(supported.supportsLanTranscoding)
        XCTAssertFalse(unsupported.supportsBilibiliCredentialStatus)
        XCTAssertFalse(unsupported.supportsBilibiliCredentialProfiles)
        XCTAssertFalse(unsupported.supportsBilibiliLoginSessions)
        XCTAssertFalse(unsupported.supportsBilibiliPlaybackPolicy)
        XCTAssertFalse(unsupported.supportsBilibiliResolve)
        XCTAssertFalse(unsupported.supportsBilibiliTaskSelection)
        XCTAssertFalse(unsupported.supportsLanTranscoding)
    }

    func testLegacyBilibiliPlaybackConformerUsesDefaultSelectionFallback() async throws {
        let client: any CacheControlClient = LegacyBilibiliPlaybackCacheControlClient()

        let task = try await client.createBilibiliPlaybackTask(
            urlOrID: "BV1legacy",
            selectionID: nil,
            options: BilibiliPlaybackTaskOptions()
        )
        XCTAssertEqual(task.source, "BV1legacy")

        do {
            _ = try await client.createBilibiliPlaybackTask(
                urlOrID: "BV1legacy",
                selectionID: "page:2",
                options: BilibiliPlaybackTaskOptions()
            )
            XCTFail("selected playback should require a client implementation")
        } catch {
            XCTAssertEqual(error as? CacheControlClientUnsupportedFeature, .bilibiliResolve)
        }
    }

    func testGRPCBilibiliPlaybackCapabilityGateKeepsLegacySelectionIDCompatible() {
        XCTAssertNil(
            GRPCCacheControlClient.requiredCapabilityForBilibiliPlaybackTask(
                selectionID: "  ",
                selection: nil
            )
        )
        XCTAssertEqual(
            GRPCCacheControlClient.requiredCapabilityForBilibiliPlaybackTask(
                selectionID: " page:2 ",
                selection: nil
            ),
            CacheServerCapability.bilibiliResolve
        )
        XCTAssertEqual(
            GRPCCacheControlClient.requiredCapabilityForBilibiliPlaybackTask(
                selectionID: "page:2",
                selection: BilibiliTaskSelection(mode: "single", selectionIDs: ["page:2"])
            ),
            CacheServerCapability.bilibiliTaskSelection
        )
        XCTAssertNil(
            GRPCCacheControlClient.requiredCapabilityForBilibiliPlaybackPolicy(
                options: BilibiliPlaybackTaskOptions()
            )
        )
        let policyOptions = BilibiliPlaybackTaskOptions(
            playbackPolicy: BilibiliPlaybackPolicy(transcodingPreference: .force)
        )
        XCTAssertEqual(
            GRPCCacheControlClient.requiredCapabilityForBilibiliPlaybackPolicy(options: policyOptions),
            CacheServerCapability.bilibiliPlaybackPolicy
        )
        XCTAssertEqual(
            GRPCCacheControlClient.requiredCapabilitiesForBilibiliPlaybackTask(
                selectionID: "page:2",
                selection: BilibiliTaskSelection(mode: "single", selectionIDs: ["page:2"]),
                options: policyOptions
            ),
            [
                CacheServerCapability.bilibiliTaskSelection,
                CacheServerCapability.bilibiliPlaybackPolicy,
            ]
        )
        XCTAssertEqual(
            GRPCCacheControlClient.requiredCapabilitiesForBilibiliPlaybackTask(
                selectionID: "",
                selection: nil,
                options: BilibiliPlaybackTaskOptions()
            ),
            []
        )
    }

    func testGeneratedDeleteCapabilityMatchesPublicConstant() {
        XCTAssertEqual(
            String(describing: TvosNetPlayer_V1_ServerCapability.libraryItemDelete),
            CacheServerCapability.libraryItemDelete
        )
    }

    func testCacheControlClientPageContractExposesNextTokenAndSearch() async throws {
        let client: any CacheControlClient = FakePagedCacheControlClient()

        let page = try await client.listLibraryItemsPage(
            pageToken: "page-1",
            pageSize: 25,
            searchText: "cached clip"
        )

        XCTAssertEqual(page.items.map(\.id), ["item-1"])
        XCTAssertEqual(page.nextPageToken, "page-2")
        XCTAssertTrue(page.hasMoreItems)
    }

    func testDefaultHLSCacheStatusImplementationReportsUnsupportedFeature() async {
        let client: any CacheControlClient = FakePagedCacheControlClient()

        do {
            _ = try await client.getHLSCacheStatus()
            XCTFail("Expected unsupported feature error.")
        } catch CacheControlClientUnsupportedFeature.hlsCacheStatus {
            return
        } catch {
            XCTFail("Unexpected error: \(error)")
        }
    }

    func testCollectsItemsAcrossAllPages() async throws {
        var requestedPageTokens: [String] = []
        let pages = [
            "": CacheLibraryItemsPage(items: [.fixture(id: "item-1")], nextPageToken: "page-2"),
            "page-2": CacheLibraryItemsPage(items: [.fixture(id: "item-2")], nextPageToken: "page-3"),
            "page-3": CacheLibraryItemsPage(items: [.fixture(id: "item-3")], nextPageToken: ""),
        ]

        let items = try await collectCacheLibraryItems { pageToken in
            requestedPageTokens.append(pageToken)
            return try XCTUnwrap(pages[pageToken])
        }

        XCTAssertEqual(requestedPageTokens, ["", "page-2", "page-3"])
        XCTAssertEqual(items.map(\.id), ["item-1", "item-2", "item-3"])
    }

    func testThrowsWhenServerRepeatsPageToken() async {
        do {
            _ = try await collectCacheLibraryItems { _ in
                CacheLibraryItemsPage(items: [.fixture(id: "item-1")], nextPageToken: "same-page")
            }
            XCTFail("Expected repeated page token error.")
        } catch CacheLibraryPaginationError.repeatedPageToken("same-page") {
            return
        } catch {
            XCTFail("Unexpected error: \(error)")
        }
    }

    func testThrowsWhenServerReturnsTooManyUniquePages() async {
        var requestedPageTokens: [String] = []
        var pageIndex = 0

        do {
            _ = try await collectCacheLibraryItems(maxPages: 3) { pageToken in
                requestedPageTokens.append(pageToken)
                pageIndex += 1
                return CacheLibraryItemsPage(
                    items: [.fixture(id: "item-\(pageIndex)")],
                    nextPageToken: "page-\(pageIndex)"
                )
            }
            XCTFail("Expected page limit error.")
        } catch CacheLibraryPaginationError.exceededPageLimit(3) {
            XCTAssertEqual(requestedPageTokens, ["", "page-1", "page-2"])
        } catch {
            XCTFail("Unexpected error: \(error)")
        }
    }

    func testReturnsPartialResultsWhenAllowedAtPageLimit() async {
        var requestedPageTokens: [String] = []

        do {
            let items = try await collectCacheLibraryItems(
                maxPages: 1,
                allowPartialResults: true
            ) { pageToken in
                requestedPageTokens.append(pageToken)
                return CacheLibraryItemsPage(
                    items: [.fixture(id: "item-1")],
                    nextPageToken: "page-1"
                )
            }

            XCTAssertEqual(items.map(\.id), ["item-1"])
            XCTAssertEqual(requestedPageTokens, [""])
        } catch {
            XCTFail("Unexpected error: \(error)")
        }
    }

    func testThrowsWhenServerReturnsTooManyItems() async {
        do {
            _ = try await collectCacheLibraryItems(maxItems: 2) { _ in
                CacheLibraryItemsPage(
                    items: [
                        .fixture(id: "item-1"),
                        .fixture(id: "item-2"),
                        .fixture(id: "item-3"),
                    ],
                    nextPageToken: ""
                )
            }
            XCTFail("Expected item limit error.")
        } catch CacheLibraryPaginationError.exceededItemLimit(2) {
            return
        } catch {
            XCTFail("Unexpected error: \(error)")
        }
    }
}

private actor FakePagedCacheControlClient: CacheControlClient {
    func getServerInfo() async throws -> CacheServerSummary {
        CacheServerSummary(
            id: "server-1",
            name: "Test cache",
            version: "0.1.0",
            mediaBaseURIs: [],
            capabilities: []
        )
    }

    func listCacheRoots() async throws -> [CacheRoot] {
        []
    }

    func listLibraryItemsPage(
        pageToken: String,
        pageSize: Int,
        searchText: String?
    ) async throws -> CacheLibraryItemsPage {
        XCTAssertEqual(pageToken, "page-1")
        XCTAssertEqual(pageSize, 25)
        XCTAssertEqual(searchText, "cached clip")
        return CacheLibraryItemsPage(
            items: [.fixture(id: "item-1")],
            nextPageToken: "page-2"
        )
    }

    func getPlaybackSource(itemID: String, variantID: String) async throws -> CachePlaybackSource {
        throw FakePagedCacheControlClientError.notImplemented
    }

    func deleteLibraryItem(id: String) async throws -> Bool {
        throw FakePagedCacheControlClientError.notImplemented
    }

    func getTask(id: String) async throws -> CacheTask {
        throw FakePagedCacheControlClientError.notImplemented
    }

    func watchTasks(ids: [String]) async -> AsyncThrowingStream<CacheTask, Error> {
        AsyncThrowingStream { continuation in
            continuation.finish(throwing: FakePagedCacheControlClientError.notImplemented)
        }
    }

    func cancelTask(id: String) async throws -> CacheTask {
        throw FakePagedCacheControlClientError.notImplemented
    }

    func createBilibiliPlaybackTask(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask {
        throw FakePagedCacheControlClientError.notImplemented
    }

    func createBilibiliPlaybackTask(
        urlOrID: String,
        selectionID: String?,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask {
        throw FakePagedCacheControlClientError.notImplemented
    }
}

private enum FakePagedCacheControlClientError: Error {
    case notImplemented
}

private struct LegacyBilibiliPlaybackCacheControlClient: CacheControlClient {
    func getServerInfo() async throws -> CacheServerSummary {
        throw FakePagedCacheControlClientError.notImplemented
    }

    func listCacheRoots() async throws -> [CacheRoot] {
        throw FakePagedCacheControlClientError.notImplemented
    }

    func listLibraryItemsPage(
        pageToken: String,
        pageSize: Int,
        searchText: String?
    ) async throws -> CacheLibraryItemsPage {
        throw FakePagedCacheControlClientError.notImplemented
    }

    func getPlaybackSource(itemID: String, variantID: String) async throws -> CachePlaybackSource {
        throw FakePagedCacheControlClientError.notImplemented
    }

    func deleteLibraryItem(id: String) async throws -> Bool {
        throw FakePagedCacheControlClientError.notImplemented
    }

    func getTask(id: String) async throws -> CacheTask {
        throw FakePagedCacheControlClientError.notImplemented
    }

    func watchTasks(ids: [String]) async -> AsyncThrowingStream<CacheTask, Error> {
        AsyncThrowingStream { continuation in
            continuation.finish(throwing: FakePagedCacheControlClientError.notImplemented)
        }
    }

    func cancelTask(id: String) async throws -> CacheTask {
        throw FakePagedCacheControlClientError.notImplemented
    }

    func createBilibiliPlaybackTask(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask {
        CacheTask(
            id: "legacy-playback-1",
            kind: "TASK_KIND_BILIBILI_PROGRESSIVE_PLAYBACK",
            state: "TASK_STATE_PREPARING",
            source: urlOrID,
            title: "Legacy playback",
            progress: 0,
            message: "",
            libraryItemID: "",
            playbackSource: nil,
            playbackSession: nil
        )
    }
}

extension CacheLibraryItem {
    fileprivate static func fixture(id: String) -> Self {
        Self(
            id: id,
            title: id,
            subtitle: "",
            source: "localCache",
            sourceID: id,
            posterURI: "",
            variants: []
        )
    }
}
