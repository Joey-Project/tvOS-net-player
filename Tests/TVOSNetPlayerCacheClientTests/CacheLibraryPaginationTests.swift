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

    func testGeneratedBilibiliResolutionV2CapabilityMatchesPublicConstant() {
        XCTAssertEqual(
            String(describing: TvosNetPlayer_V1_ServerCapability.bilibiliResolutionV2),
            CacheServerCapability.bilibiliResolutionV2
        )
    }

    func testGeneratedBilibiliExecutionV2CapabilityMatchesPublicConstant() {
        XCTAssertEqual(
            String(describing: TvosNetPlayer_V1_ServerCapability.bilibiliExecutionV2),
            CacheServerCapability.bilibiliExecutionV2
        )

        let summary = CacheServerSummary(
            id: "server-1",
            name: "Cache server",
            version: "1.0.0",
            mediaBaseURIs: [],
            capabilities: [CacheServerCapability.bilibiliExecutionV2]
        )
        XCTAssertTrue(summary.supportsBilibiliExecutionV2)
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

    func testBilibiliResolutionPageMapsSessionCandidatesIdentityAndPagination() {
        var proto = TvosNetPlayer_V1_BilibiliResolutionPage()
        proto.session.id = "resolution-session-1"
        proto.session.source = "BV1resolution"
        proto.session.title = "Resolved collection"
        proto.session.sourceKind = "collection"
        proto.session.createdAt.seconds = 100
        proto.session.createdAt.nanos = 250_000_000
        proto.session.expiresAt.seconds = 1_000
        proto.session.defaultCandidateToken = "candidate-default"
        proto.session.context.apiMode = .web
        proto.session.context.credentialProfileID = "restricted-bangumi"

        var candidate = TvosNetPlayer_V1_BilibiliResolutionCandidate()
        candidate.candidateToken = "candidate-2"
        candidate.title = "Episode 2"
        candidate.subtitle = "Season episode"
        candidate.sourceKind = "season_episode"
        candidate.identity.kind = .seasonEpisode
        candidate.identity.aid = 42
        candidate.identity.bvid = "BV1identity"
        candidate.identity.cid = 84
        candidate.identity.epid = 126
        candidate.index = 2
        candidate.durationSeconds = 1_800
        proto.candidates = [candidate]
        proto.pageInfo.totalSize = 75
        proto.pageInfo.nextPageToken = "opaque-page-2"
        proto.pageInfo.snapshotID = "snapshot-1"

        let page = BilibiliResolutionPage(proto)

        XCTAssertEqual(page.session.id, "resolution-session-1")
        XCTAssertEqual(page.session.source, "BV1resolution")
        XCTAssertEqual(page.session.createdAt, Date(timeIntervalSince1970: 100.25))
        XCTAssertEqual(page.session.expiresAt, Date(timeIntervalSince1970: 1_000))
        XCTAssertEqual(page.session.defaultCandidateToken, "candidate-default")
        XCTAssertEqual(
            page.session.context,
            BilibiliRequestContext(apiMode: .web, credentialProfileID: "restricted-bangumi")
        )
        XCTAssertEqual(page.candidates.first?.id, "candidate-2")
        XCTAssertEqual(
            page.candidates.first?.identity,
            BilibiliContentIdentity(
                kind: .seasonEpisode,
                aid: 42,
                bvid: "BV1identity",
                cid: 84,
                epid: 126
            )
        )
        XCTAssertEqual(page.candidates.first?.index, 2)
        XCTAssertEqual(page.candidates.first?.durationSeconds, 1_800)
        XCTAssertEqual(page.totalSize, 75)
        XCTAssertEqual(page.nextPageToken, "opaque-page-2")
        XCTAssertEqual(page.snapshotID, "snapshot-1")
        XCTAssertTrue(page.hasMoreCandidates)
    }

    func testBilibiliResolutionPagePreservesUnknownContentKind() {
        var proto = TvosNetPlayer_V1_BilibiliResolutionPage()
        var candidate = TvosNetPlayer_V1_BilibiliResolutionCandidate()
        candidate.identity.kind = .UNRECOGNIZED(99)
        proto.candidates = [candidate]

        let page = BilibiliResolutionPage(proto)

        XCTAssertEqual(page.candidates.first?.identity.kind, .unknown(99))
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
        result.subject.provider = "bilibili"
        result.subject.kind = "season_episode"
        result.subject.id = "ep-664928"
        result.subject.index = 3
        result.providerDetails.bilibili.identity.kind = .seasonEpisode
        result.providerDetails.bilibili.identity.aid = 42
        result.providerDetails.bilibili.identity.bvid = "BV1details"
        result.providerDetails.bilibili.identity.cid = 84
        result.providerDetails.bilibili.identity.epid = 664_928
        result.providerDetails.bilibili.playbackSession.id = "playback-session-1"
        result.providerDetails.bilibili.playbackSession.title = "Episode 3"

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
        artifact.libraryItemID = "library-artifact-1"
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
        XCTAssertEqual(mappedArtifact?.libraryItemID, "library-artifact-1")
        XCTAssertEqual(
            mapped?.subject,
            CacheTaskResultSubject(
                provider: "bilibili",
                kind: "season_episode",
                id: "ep-664928",
                index: 3
            )
        )
        guard case .bilibili(let providerDetails)? = mapped?.providerDetails else {
            return XCTFail("Expected Bilibili provider details.")
        }
        XCTAssertEqual(
            providerDetails.identity,
            BilibiliContentIdentity(
                kind: .seasonEpisode,
                aid: 42,
                bvid: "BV1details",
                cid: 84,
                epid: 664_928
            )
        )
        XCTAssertEqual(providerDetails.playbackSession?.id, "playback-session-1")
        XCTAssertEqual(providerDetails.playbackSession?.title, "Episode 3")
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
        result.identity.kind = .videoPage
        result.identity.aid = 100
        result.identity.bvid = "BV1schema"
        result.identity.cid = 200
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
        XCTAssertEqual(
            task.resultItems.first?.identity,
            BilibiliContentIdentity(
                kind: .videoPage,
                aid: 100,
                bvid: "BV1schema",
                cid: 200,
                epid: 0
            )
        )
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

    func testStartBilibiliResolutionRequestNormalizesHumanInputAndPageSize() throws {
        let options = BilibiliPlaybackTaskOptions(
            qualityPreference: "1080p",
            encodingPreference: "h264",
            audioLanguagePreference: "ja-jp",
            preferTVAPI: true
        )

        let request = try GRPCCacheControlClient.startBilibiliResolutionRequest(
            urlOrID: "  BV1resolution  \n",
            options: options,
            pageSize: 500
        )

        XCTAssertEqual(request.urlOrID, "BV1resolution")
        XCTAssertEqual(request.options.qualityPreference, "1080p")
        XCTAssertEqual(request.options.encodingPreference, "h264")
        XCTAssertEqual(request.options.audioLanguage, "ja-jp")
        XCTAssertTrue(request.options.preferTvApi)
        XCTAssertEqual(request.page.pageSize, 200)
        XCTAssertEqual(request.page.pageToken, "")
        XCTAssertFalse(request.hasContext)
    }

    func testStartBilibiliResolutionRequestMapsExecutionContext() throws {
        let request = try GRPCCacheControlClient.startBilibiliResolutionRequest(
            urlOrID: "BV1context",
            options: BilibiliPlaybackTaskOptions(),
            context: BilibiliRequestContext(
                apiMode: .app,
                credentialProfileID: "restricted-bangumi"
            ),
            pageSize: 25
        )

        XCTAssertTrue(request.hasContext)
        XCTAssertEqual(request.context.apiMode, .app)
        XCTAssertEqual(request.context.credentialProfileID, "restricted-bangumi")
        XCTAssertEqual(
            GRPCCacheControlClient.requiredCapabilitiesForBilibiliResolution(
                options: BilibiliPlaybackTaskOptions(),
                context: .default
            ),
            []
        )
        XCTAssertEqual(
            GRPCCacheControlClient.requiredCapabilitiesForBilibiliResolution(
                options: BilibiliPlaybackTaskOptions(
                    playbackPolicy: BilibiliPlaybackPolicy(transcodingPreference: .force)
                ),
                context: BilibiliRequestContext(apiMode: .web)
            ),
            [
                CacheServerCapability.bilibiliPlaybackPolicy,
                CacheServerCapability.bilibiliExecutionV2,
            ]
        )
    }

    func testBilibiliResolutionPageRequestPreservesOpaqueValuesAndServerDefaultSize() throws {
        let request = try GRPCCacheControlClient.listBilibiliResolutionCandidatesRequest(
            sessionID: " session-token ",
            pageToken: " page-token ",
            pageSize: -1
        )

        XCTAssertEqual(request.sessionID, " session-token ")
        XCTAssertEqual(request.page.pageToken, " page-token ")
        XCTAssertEqual(request.page.pageSize, 0)
    }

    func testCreateBilibiliPlaybackTaskV2RequestMapsEveryTokenSelectionMode() throws {
        let single = try GRPCCacheControlClient.createBilibiliPlaybackTaskV2Request(
            sessionID: "session-1",
            selection: .single(candidateToken: " candidate-1 ")
        )
        XCTAssertEqual(single.sessionID, "session-1")
        XCTAssertEqual(single.selection.mode, .single)
        XCTAssertEqual(single.selection.candidateTokens, [" candidate-1 "])
        XCTAssertEqual(single.selection.rangeStartCandidateToken, "")
        XCTAssertEqual(single.selection.rangeEndCandidateToken, "")

        let multiple = try GRPCCacheControlClient.createBilibiliPlaybackTaskV2Request(
            sessionID: "session-1",
            selection: .multiple(candidateTokens: ["candidate-2", "candidate-1"])
        )
        XCTAssertEqual(multiple.selection.mode, .multiple)
        XCTAssertEqual(multiple.selection.candidateTokens, ["candidate-2", "candidate-1"])

        let range = try GRPCCacheControlClient.createBilibiliPlaybackTaskV2Request(
            sessionID: "session-1",
            selection: .range(
                startCandidateToken: "candidate-2",
                endCandidateToken: "candidate-9"
            )
        )
        XCTAssertEqual(range.selection.mode, .range)
        XCTAssertTrue(range.selection.candidateTokens.isEmpty)
        XCTAssertEqual(range.selection.rangeStartCandidateToken, "candidate-2")
        XCTAssertEqual(range.selection.rangeEndCandidateToken, "candidate-9")

        let all = try GRPCCacheControlClient.createBilibiliPlaybackTaskV2Request(
            sessionID: "session-1",
            selection: .all
        )
        XCTAssertEqual(all.selection.mode, .all)
        XCTAssertTrue(all.selection.candidateTokens.isEmpty)
        XCTAssertEqual(all.selection.rangeStartCandidateToken, "")
        XCTAssertEqual(all.selection.rangeEndCandidateToken, "")
    }

    func testCreateBilibiliTaskV2RequestMapsTypedPlaybackExecution() throws {
        let request = try GRPCCacheControlClient.createBilibiliTaskV2Request(
            sessionID: "session-playback",
            selection: .single(candidateToken: "candidate-1"),
            execution: .playback(
                BilibiliPlaybackSpec(
                    qualityQN: 120,
                    codec: .hevc,
                    audioLanguage: "ja-JP",
                    policy: BilibiliPlaybackPolicy(
                        transcodingPreference: .force,
                        compatibleVariantPreference: .preferRequested,
                        weakNetworkPreference: .holdDowngrade
                    )
                )
            )
        )

        XCTAssertEqual(request.sessionID, "session-playback")
        XCTAssertEqual(request.selection.mode, .single)
        guard case .playback(let playback)? = request.execution else {
            return XCTFail("Expected playback execution.")
        }
        XCTAssertEqual(playback.qualityQn, 120)
        XCTAssertEqual(playback.codec, .hevc)
        XCTAssertEqual(playback.audioLanguage, "ja-JP")
        XCTAssertEqual(playback.policy.transcodingPreference, .force)
        XCTAssertEqual(playback.policy.compatibleVariantPreference, .preferRequested)
        XCTAssertEqual(playback.policy.weakNetworkPreference, .holdDowngrade)
    }

    func testCreateBilibiliTaskV2RequestMapsTypedDownloadExecution() throws {
        let request = try GRPCCacheControlClient.createBilibiliTaskV2Request(
            sessionID: "session-download",
            selection: .all,
            execution: .download(
                BilibiliDownloadSpec(
                    qualityQN: 80,
                    audioLanguage: "zh-CN",
                    mode: .subtitleOnly,
                    downloadSubtitles: true,
                    subtitleAIPolicy: .preferNonAI,
                    downloadDanmaku: true,
                    danmakuFormats: [.xml, .ass],
                    downloadCover: true
                )
            )
        )

        XCTAssertEqual(request.sessionID, "session-download")
        XCTAssertEqual(request.selection.mode, .all)
        guard case .download(let download)? = request.execution else {
            return XCTFail("Expected download execution.")
        }
        XCTAssertEqual(download.qualityQn, 80)
        XCTAssertEqual(download.audioLanguage, "zh-CN")
        XCTAssertEqual(download.mode, .subtitleOnly)
        XCTAssertTrue(download.downloadSubtitles)
        XCTAssertEqual(download.subtitleAiPolicy, .preferNonAi)
        XCTAssertTrue(download.downloadDanmaku)
        XCTAssertEqual(download.danmakuFormats, [.xml, .ass])
        XCTAssertTrue(download.downloadCover)
    }

    func testCreateBilibiliTaskV2RequestDefaultsPreserveWireDefaults() throws {
        let playback = try GRPCCacheControlClient.createBilibiliTaskV2Request(
            sessionID: "session-playback-default",
            selection: .all,
            execution: .playback(BilibiliPlaybackSpec())
        ).playback
        XCTAssertEqual(playback.qualityQn, 0)
        XCTAssertEqual(playback.codec, .unspecified)
        XCTAssertTrue(playback.audioLanguage.isEmpty)
        XCTAssertEqual(playback.policy.transcodingPreference, .auto)

        let download = try GRPCCacheControlClient.createBilibiliTaskV2Request(
            sessionID: "session-download-default",
            selection: .all,
            execution: .download(BilibiliDownloadSpec())
        ).download
        XCTAssertEqual(download.qualityQn, 0)
        XCTAssertEqual(download.mode, .unspecified)
        XCTAssertFalse(download.downloadSubtitles)
        XCTAssertFalse(download.downloadDanmaku)
        XCTAssertFalse(download.downloadCover)
        XCTAssertEqual(download.subtitleAiPolicy, .unspecified)
        XCTAssertTrue(download.danmakuFormats.isEmpty)
    }

    func testBilibiliResolutionRequestsRejectStructurallyEmptyInputs() {
        XCTAssertThrowsError(
            try GRPCCacheControlClient.startBilibiliResolutionRequest(
                urlOrID: " \n ",
                options: BilibiliPlaybackTaskOptions(),
                pageSize: 50
            )
        ) { error in
            XCTAssertEqual(
                error as? CacheControlClientInvalidRequest,
                .bilibiliResolutionInputRequired
            )
        }

        XCTAssertThrowsError(
            try GRPCCacheControlClient.listBilibiliResolutionCandidatesRequest(
                sessionID: "  ",
                pageToken: "",
                pageSize: 50
            )
        ) { error in
            XCTAssertEqual(
                error as? CacheControlClientInvalidRequest,
                .bilibiliResolutionSessionIDRequired
            )
        }

        let invalidSelections: [BilibiliResolutionSelection] = [
            .single(candidateToken: ""),
            .multiple(candidateTokens: []),
            .multiple(candidateTokens: ["candidate-1", "  "]),
            .range(startCandidateToken: "candidate-1", endCandidateToken: ""),
        ]
        for selection in invalidSelections {
            XCTAssertThrowsError(
                try GRPCCacheControlClient.createBilibiliPlaybackTaskV2Request(
                    sessionID: "session-1",
                    selection: selection
                )
            ) { error in
                XCTAssertEqual(
                    error as? CacheControlClientInvalidRequest,
                    .invalidBilibiliResolutionSelection
                )
            }
        }
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
            danmakuFormats: [.xml, .ass],
            downloadMode: .audioOnly
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
        XCTAssertEqual(proto.downloadMode, .audioOnly)
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
        XCTAssertEqual(proto.downloadMode, .unspecified)
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
                CacheServerCapability.bilibiliResolutionV2,
                CacheServerCapability.bilibiliExecutionV2,
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
        XCTAssertTrue(supported.supportsBilibiliResolutionV2)
        XCTAssertTrue(supported.supportsBilibiliExecutionV2)
        XCTAssertTrue(supported.supportsBilibiliTaskSelection)
        XCTAssertTrue(supported.supportsLanTranscoding)
        XCTAssertFalse(unsupported.supportsBilibiliCredentialStatus)
        XCTAssertFalse(unsupported.supportsBilibiliCredentialProfiles)
        XCTAssertFalse(unsupported.supportsBilibiliLoginSessions)
        XCTAssertFalse(unsupported.supportsBilibiliPlaybackPolicy)
        XCTAssertFalse(unsupported.supportsBilibiliResolve)
        XCTAssertFalse(unsupported.supportsBilibiliResolutionV2)
        XCTAssertFalse(unsupported.supportsBilibiliExecutionV2)
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

    func testLegacyConformerReportsBilibiliResolutionV2AsUnsupported() async {
        let client: any CacheControlClient = LegacyBilibiliPlaybackCacheControlClient()

        do {
            _ = try await client.startBilibiliResolution(urlOrID: "BV1legacy")
            XCTFail("Expected StartBilibiliResolution to be unsupported.")
        } catch {
            XCTAssertEqual(error as? CacheControlClientUnsupportedFeature, .bilibiliResolutionV2)
        }

        do {
            _ = try await client.listBilibiliResolutionCandidates(sessionID: "session-1")
            XCTFail("Expected ListBilibiliResolutionCandidates to be unsupported.")
        } catch {
            XCTAssertEqual(error as? CacheControlClientUnsupportedFeature, .bilibiliResolutionV2)
        }

        do {
            _ = try await client.createBilibiliPlaybackTaskV2(
                sessionID: "session-1",
                selection: .all
            )
            XCTFail("Expected CreateBilibiliPlaybackTaskV2 to be unsupported.")
        } catch {
            XCTAssertEqual(error as? CacheControlClientUnsupportedFeature, .bilibiliResolutionV2)
        }
    }

    func testLegacyConformerReportsBilibiliExecutionV2AsUnsupported() async {
        let client: any CacheControlClient = LegacyBilibiliPlaybackCacheControlClient()

        do {
            _ = try await client.startBilibiliResolution(
                urlOrID: "BV1legacy",
                options: BilibiliPlaybackTaskOptions(),
                context: BilibiliRequestContext(apiMode: .web),
                pageSize: 50
            )
            XCTFail("Expected contextual resolution to be unsupported.")
        } catch {
            XCTAssertEqual(error as? CacheControlClientUnsupportedFeature, .bilibiliExecutionV2)
        }

        do {
            _ = try await client.createBilibiliTaskV2(
                sessionID: "session-1",
                selection: .all,
                execution: .playback(BilibiliPlaybackSpec())
            )
            XCTFail("Expected CreateBilibiliTaskV2 to be unsupported.")
        } catch {
            XCTAssertEqual(error as? CacheControlClientUnsupportedFeature, .bilibiliExecutionV2)
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
