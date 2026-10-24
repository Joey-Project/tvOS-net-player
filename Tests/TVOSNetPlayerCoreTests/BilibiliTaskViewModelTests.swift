import Combine
import TVOSNetPlayerCacheClient
import XCTest
@testable import TVOSNetPlayerCore

@MainActor
final class BilibiliTaskViewModelTests: XCTestCase {
    func testPlaybackPolicyDefaultsLoadAndSave() throws {
        let suiteName = "BilibiliTaskViewModelTests-\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defaults.removePersistentDomain(forName: suiteName)
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let model = BilibiliTaskViewModel(defaults: defaults)

        XCTAssertEqual(model.playbackTranscodingPreference, .auto)
        XCTAssertEqual(model.playbackCompatibleVariantPreference, .preferCompatible)
        XCTAssertEqual(model.playbackWeakNetworkPreference, .adaptive)

        model.playbackTranscodingPreference = .force
        model.playbackCompatibleVariantPreference = .preferRequested
        model.playbackWeakNetworkPreference = .holdDowngrade

        XCTAssertEqual(
            defaults.string(forKey: BilibiliTaskViewModel.playbackTranscodingPreferenceDefaultsKey),
            "force"
        )
        XCTAssertEqual(
            defaults.string(forKey: BilibiliTaskViewModel.playbackCompatibleVariantPreferenceDefaultsKey),
            "preferRequested"
        )
        XCTAssertEqual(
            defaults.string(forKey: BilibiliTaskViewModel.playbackWeakNetworkPreferenceDefaultsKey),
            "holdDowngrade"
        )

        let restored = BilibiliTaskViewModel(defaults: defaults)
        XCTAssertEqual(restored.playbackTranscodingPreference, .force)
        XCTAssertEqual(restored.playbackCompatibleVariantPreference, .preferRequested)
        XCTAssertEqual(restored.playbackWeakNetworkPreference, .holdDowngrade)

        restored.playbackTranscodingPreference = .auto
        restored.playbackCompatibleVariantPreference = .preferCompatible
        restored.playbackWeakNetworkPreference = .adaptive

        XCTAssertNil(defaults.string(forKey: BilibiliTaskViewModel.playbackTranscodingPreferenceDefaultsKey))
        XCTAssertNil(defaults.string(forKey: BilibiliTaskViewModel.playbackCompatibleVariantPreferenceDefaultsKey))
        XCTAssertNil(defaults.string(forKey: BilibiliTaskViewModel.playbackWeakNetworkPreferenceDefaultsKey))
    }

    func testSubmitCreatesPlaybackTaskWithOptionsAndStartsWatching() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(state: "TASK_STATE_PREPARING", message: "Preparing playback."))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1test",
            qualityPreference: "1080p",
            encodingPreference: "h264",
            audioLanguagePreference: "ja-jp",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        let requests = await client.createdRequestsSnapshot()
        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(requests.first?.urlOrID, "BV1test")
        XCTAssertEqual(requests.first?.selectionID, "page:1")
        XCTAssertEqual(requests.first?.selection?.mode, "single")
        XCTAssertEqual(requests.first?.selection?.selectionIDs, ["page:1"])
        XCTAssertEqual(requests.first?.options.qualityPreference, "1080p")
        XCTAssertEqual(requests.first?.options.encodingPreference, "h264")
        XCTAssertEqual(requests.first?.options.audioLanguagePreference, "ja-jp")
        let v2Requests = await client.v2TaskRequestsSnapshot()
        XCTAssertEqual(v2Requests.count, 1)
        XCTAssertEqual(v2Requests.first?.selection, .single(candidateToken: "page:1"))
        if case let .playback(spec) = v2Requests.first?.execution {
            XCTAssertEqual(spec.qualityQN, 80)
            XCTAssertEqual(spec.codec, .h264)
            XCTAssertEqual(spec.audioLanguage, "ja-jp")
        } else {
            XCTFail("Expected typed playback execution.")
        }
        let legacyCalls = await client.legacyRPCCallCountsSnapshot()
        XCTAssertEqual(legacyCalls.resolve, 0)
        XCTAssertEqual(legacyCalls.create, 0)
        let resolvedRequests = await client.resolvedRequestsSnapshot()
        XCTAssertEqual(resolvedRequests.count, 1)
        XCTAssertEqual(resolvedRequests.first?.urlOrID, "BV1test")
        XCTAssertEqual(resolvedRequests.first?.options.audioLanguagePreference, "ja-jp")
        XCTAssertEqual(model.currentTask?.id, "bilibili-playback-1")
        XCTAssertTrue(model.isWatching)

        await client.waitForWatchSubscription()
        model.clearTask()
        await client.waitForWatchTermination()
    }

    func testSubmitPassesPlaybackPolicyToResolveAndCreate() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(state: "TASK_STATE_PREPARING", message: "Preparing playback."))
        ])
        let policy = BilibiliPlaybackPolicy(
            transcodingPreference: .force,
            compatibleVariantPreference: .preferRequested,
            weakNetworkPreference: .avPlayerManaged
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1policy",
            playbackPolicy: policy,
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        let resolvedRequests = await client.resolvedRequestsSnapshot()
        XCTAssertEqual(resolvedRequests.first?.options.playbackPolicy, policy)
        let requests = await client.createdRequestsSnapshot()
        XCTAssertEqual(requests.first?.options.playbackPolicy, policy)

        await client.waitForWatchSubscription()
        model.clearTask()
        await client.waitForWatchTermination()
    }

    func testActivePlaybackPolicySummaryUsesEffectivePolicyAndTranscodingPlan() async {
        let policy = BilibiliPlaybackPolicy(
            transcodingPreference: .never,
            compatibleVariantPreference: .preferRequested,
            weakNetworkPreference: .holdDowngrade
        )
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(
                .fixture(
                    state: "TASK_STATE_PLAYABLE",
                    playbackSession: CacheBilibiliPlaybackSession(
                        id: "bilibili-playback-1",
                        title: "Ready video",
                        contentID: "BV1ready-cid1",
                        selectedVariantID: "h264",
                        selectedVariant: nil,
                        variants: [],
                        transcodingPlan: LanTranscodingPlan(
                            state: "LAN_TRANSCODING_PLAN_STATE_REQUIRED",
                            profileID: "avplayer-h264-aac-hls-v1",
                            reason: "Requested policy requires transcoding.",
                            sourceVariantID: "dolby",
                            targetContainer: "hls/fmp4",
                            targetVideoCodec: "h264",
                            targetAudioCodec: "aac",
                            outputProtocol: "PLAYBACK_PROTOCOL_HLS"
                        ),
                        effectivePolicy: policy
                    )
                ))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1summary",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertEqual(
            model.activePlaybackPolicySummary,
            "Policy: never transcode, prefer requested, hold downgrade · Transcoding: Requested policy requires transcoding."
        )
        model.clearTask()
    }

    func testSubmitCreatesDownloadTaskWithSidecarOptions() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(id: "bilibili-download-1", state: "TASK_STATE_QUEUED"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1download",
            qualityPreference: "1080p",
            encodingPreference: "h264",
            audioLanguagePreference: "ja-jp",
            clientFactory: { _ in client }
        )
        model.submissionMode = .download
        model.downloadSubtitles = true
        model.downloadDanmaku = true
        model.downloadCover = true
        model.subtitleAIPolicy = .preferNonAI
        model.setDanmakuFormat(.xml, selected: true)
        model.setDanmakuFormat(.ass, selected: true)

        await model.submit(serverAddressText: "mac-mini.local:50051")

        let downloadRequests = await client.downloadRequestsSnapshot()
        XCTAssertEqual(downloadRequests.count, 1)
        XCTAssertEqual(downloadRequests.first?.urlOrID, "BV1download")
        XCTAssertEqual(downloadRequests.first?.options.qualityPreference, "1080p")
        XCTAssertEqual(downloadRequests.first?.options.encodingPreference, "")
        XCTAssertEqual(downloadRequests.first?.options.audioLanguagePreference, "ja-jp")
        XCTAssertTrue(downloadRequests.first?.options.downloadSubtitles == true)
        XCTAssertTrue(downloadRequests.first?.options.downloadDanmaku == true)
        XCTAssertTrue(downloadRequests.first?.options.downloadCover == true)
        XCTAssertEqual(downloadRequests.first?.options.subtitleAIPolicy, .preferNonAI)
        XCTAssertEqual(downloadRequests.first?.options.danmakuFormats, [.xml, .ass])
        let resolvedRequests = await client.resolvedRequestsSnapshot()
        let playbackRequests = await client.createdRequestsSnapshot()
        XCTAssertEqual(resolvedRequests.count, 1)
        XCTAssertTrue(playbackRequests.isEmpty)
        let v2Requests = await client.v2TaskRequestsSnapshot()
        XCTAssertEqual(v2Requests.count, 1)
        XCTAssertEqual(v2Requests.first?.selection, .single(candidateToken: "page:1"))
        if case let .download(spec) = v2Requests.first?.execution {
            XCTAssertEqual(spec.qualityQN, 80)
            XCTAssertEqual(spec.audioLanguage, "ja-jp")
            XCTAssertTrue(spec.downloadSubtitles)
            XCTAssertTrue(spec.downloadDanmaku)
            XCTAssertTrue(spec.downloadCover)
            XCTAssertEqual(spec.subtitleAIPolicy, .preferNonAI)
            XCTAssertEqual(spec.danmakuFormats, [.xml, .ass])
        } else {
            XCTFail("Expected typed download execution.")
        }
        let legacyCalls = await client.legacyRPCCallCountsSnapshot()
        XCTAssertEqual(legacyCalls.resolve, 0)
        XCTAssertEqual(legacyCalls.create, 0)
        XCTAssertEqual(model.currentTask?.id, "bilibili-download-1")

        model.clearTask()
    }

    func testSubmitDownloadIgnoresInvalidPlaybackCodec() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(id: "bilibili-download-vp9", state: "TASK_STATE_QUEUED"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1download-vp9",
            encodingPreference: "vp9",
            clientFactory: { _ in client }
        )
        model.submissionMode = .download

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(model.currentTask?.id, "bilibili-download-vp9")
        let requests = await client.v2TaskRequestsSnapshot()
        XCTAssertEqual(requests.count, 1)
        let resolvedRequests = await client.resolvedRequestsSnapshot()
        XCTAssertEqual(resolvedRequests.first?.options.encodingPreference, "")
        if case .download = requests.first?.execution {
            // Download execution does not use the playback codec preference.
        } else {
            XCTFail("Expected typed download execution.")
        }
        model.clearTask()
    }

    func testSubmitUsesV2SelectionWhenLegacySelectionIsUnsupported() async {
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1legacy-selection", state: "TASK_STATE_PREPARING"))
            ],
            supportsTaskSelection: false
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1legacy-selection",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        let requests = await client.createdRequestsSnapshot()
        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(requests.first?.urlOrID, "BV1legacy-selection")
        XCTAssertEqual(requests.first?.selectionID, "page:1")
        XCTAssertEqual(requests.first?.selection?.mode, "single")
        let legacyCalls = await client.legacyRPCCallCountsSnapshot()
        XCTAssertEqual(legacyCalls.resolve, 0)
        XCTAssertEqual(legacyCalls.create, 0)

        model.clearTask()
    }

    func testSubmitDoesNotFallBackToLegacyCreateWhenV2ResolutionFails() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .failure(CacheControlClientUnsupportedFeature.bilibiliResolve)
            ],
            createResponses: [
                .success(.fixture(source: "BV1legacy", state: "TASK_STATE_PREPARING"))
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1legacy",
            qualityPreference: "720p",
            encodingPreference: "h265",
            audioLanguagePreference: "en-US",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        let resolvedRequests = await client.resolvedRequestsSnapshot()
        XCTAssertEqual(resolvedRequests.count, 1)
        let requests = await client.createdRequestsSnapshot()
        XCTAssertTrue(requests.isEmpty)
        XCTAssertNil(model.currentTask)
        XCTAssertNotNil(model.errorMessage)
        XCTAssertFalse(model.isResolving)
        XCTAssertFalse(model.isSubmitting)
        let legacyCalls = await client.legacyRPCCallCountsSnapshot()
        XCTAssertEqual(legacyCalls.resolve, 0)
        XCTAssertEqual(legacyCalls.create, 0)

        model.clearTask()
    }

    func testSubmitRequiresTaskOutputV2BeforeStartingResolution() async {
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1no-output", state: "TASK_STATE_PREPARING"))
            ],
            supportsTaskOutputV2: false
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1no-output",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertTrue(model.errorMessage?.contains("Paginated Bilibili task results are unavailable") == true)
        XCTAssertNil(model.currentTask)
        XCTAssertFalse(model.isResolving)
        XCTAssertFalse(model.isSubmitting)
        let resolutions = await client.resolvedRequestsSnapshot()
        let creates = await client.v2TaskRequestsSnapshot()
        let legacyCalls = await client.legacyRPCCallCountsSnapshot()
        XCTAssertTrue(resolutions.isEmpty)
        XCTAssertTrue(creates.isEmpty)
        XCTAssertEqual(legacyCalls.resolve, 0)
        XCTAssertEqual(legacyCalls.create, 0)
        model.clearTask()
    }

    func testSubmitRequiresResolutionAndExecutionV2WithoutLegacyFallback() async {
        for capabilities in [(false, true), (true, false)] {
            let client = FakeBilibiliCacheControlClient(
                createResponses: [
                    .success(.fixture(source: "BV1upgrade", state: "TASK_STATE_PREPARING"))
                ],
                supportsBilibiliResolutionV2: capabilities.0,
                supportsBilibiliExecutionV2: capabilities.1
            )
            let model = BilibiliTaskViewModel(
                sourceText: "BV1upgrade",
                clientFactory: { _ in client }
            )

            await model.submit(serverAddressText: "mac-mini.local:50051")

            XCTAssertTrue(model.errorMessage?.contains("Upgrade the cache server") == true)
            XCTAssertNil(model.currentTask)
            XCTAssertFalse(model.isResolving)
            XCTAssertFalse(model.isSubmitting)
            let resolutions = await client.resolvedRequestsSnapshot()
            let creates = await client.v2TaskRequestsSnapshot()
            let legacyCalls = await client.legacyRPCCallCountsSnapshot()
            XCTAssertTrue(resolutions.isEmpty)
            XCTAssertTrue(creates.isEmpty)
            XCTAssertEqual(legacyCalls.resolve, 0)
            XCTAssertEqual(legacyCalls.create, 0)
            model.clearTask()
        }
    }

    func testUnknownQualityAndCodecPreferencesAreRejectedBeforeResolution() async {
        for preferences in [("ultra-quality", "h264"), ("1080p", "vp9")] {
            let client = FakeBilibiliCacheControlClient(createResponses: [])
            let model = BilibiliTaskViewModel(
                sourceText: "BV1invalid-preference",
                qualityPreference: preferences.0,
                encodingPreference: preferences.1,
                clientFactory: { _ in client }
            )

            await model.submit(serverAddressText: "mac-mini.local:50051")

            XCTAssertTrue(model.errorMessage?.contains("not recognized") == true)
            let resolutions = await client.resolvedRequestsSnapshot()
            let creates = await client.v2TaskRequestsSnapshot()
            let legacyCalls = await client.legacyRPCCallCountsSnapshot()
            XCTAssertTrue(resolutions.isEmpty)
            XCTAssertTrue(creates.isEmpty)
            XCTAssertEqual(legacyCalls.resolve, 0)
            XCTAssertEqual(legacyCalls.create, 0)
            model.clearTask()
        }
    }

    func testSubmitResolvesCollectionAndFeedInputsBeforeSelection() async {
        let sources = [
            "history",
            "watch-later",
            "following",
            "recommendations",
            "mid123",
            "fav456",
            "collection789",
            "series101",
            "https://space.bilibili.com/123",
            "https://space.bilibili.com/123/favlist?fid=456",
            "https://www.bilibili.com/account/history",
            "https://www.bilibili.com/list/ml1103407912",
            "https://www.bilibili.com/medialist/detail/ml1103407912",
            "https://t.bilibili.com/",
        ]

        for source in sources {
            let client = FakeBilibiliCacheControlClient(
                resolveResponses: [
                    .success(
                        .fixture(
                            source: source,
                            title: "Resolved collection",
                            candidates: [
                                .fixture(selectionID: "item:1", title: "Item 1", index: 1),
                                .fixture(selectionID: "item:2", title: "Item 2", index: 2),
                            ],
                            defaultSelectionID: ""
                        ))
                ],
                createResponses: [
                    .success(.fixture(source: source, state: "TASK_STATE_PREPARING"))
                ]
            )
            let model = BilibiliTaskViewModel(
                sourceText: source,
                clientFactory: { _ in client }
            )

            await model.submit(serverAddressText: "mac-mini.local:50051")

            let resolvedRequests = await client.resolvedRequestsSnapshot()
            XCTAssertEqual(resolvedRequests.count, 1, source)
            XCTAssertEqual(resolvedRequests.first?.urlOrID, source)
            var requests = await client.createdRequestsSnapshot()
            XCTAssertTrue(requests.isEmpty, source)
            XCTAssertTrue(model.isWaitingForCandidateSelection, source)
            XCTAssertTrue(model.availableCandidateSelectionModes.contains(.all), source)

            model.candidateSelectionMode = .all
            await model.submit(serverAddressText: "mac-mini.local:50051")

            requests = await client.createdRequestsSnapshot()
            XCTAssertEqual(requests.count, 1, source)
            XCTAssertEqual(requests.first?.urlOrID, source)
            XCTAssertNil(requests.first?.selectionID, source)
            XCTAssertEqual(requests.first?.selection?.mode, "all", source)

            model.clearTask()
        }
    }

    func testSubmitStillResolvesBilibiliRootEpisodeQuery() async {
        let source = "https://www.bilibili.com/?ep_id=123"
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(source: source, state: "TASK_STATE_PREPARING"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: source,
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        let resolvedRequests = await client.resolvedRequestsSnapshot()
        XCTAssertEqual(resolvedRequests.count, 1)
        XCTAssertEqual(resolvedRequests.first?.urlOrID, source)
        let requests = await client.createdRequestsSnapshot()
        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(requests.first?.selectionID, "page:1")

        model.clearTask()
    }

    func testSubmitStopsForMultiCandidateSelectionThenCreatesSelectedPlaybackTask() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1multi",
                        title: "Multi page video",
                        candidates: [
                            .fixture(selectionID: "page:1", title: "Part 1", subtitle: "Page 1", index: 1),
                            .fixture(selectionID: "page:2", title: "Part 2", subtitle: "Page 2", index: 2),
                        ],
                        defaultSelectionID: ""
                    ))
            ],
            createResponses: [
                .success(.fixture(source: "BV1multi", state: "TASK_STATE_PREPARING"))
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1multi",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertTrue(model.isWaitingForCandidateSelection)
        XCTAssertEqual(model.statusMessage, "Select a Bilibili item to play.")
        XCTAssertEqual(model.resolvedCandidates.map(\.selectionID), ["page:1", "page:2"])
        XCTAssertEqual(model.selectedCandidateID, "page:1")
        XCTAssertNil(model.currentTask)
        XCTAssertTrue(model.canClear)
        var requests = await client.createdRequestsSnapshot()
        XCTAssertTrue(requests.isEmpty)

        model.selectedCandidateID = "page:2"
        await model.submit(serverAddressText: "mac-mini.local:50051")

        requests = await client.createdRequestsSnapshot()
        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(requests.first?.selectionID, "page:2")
        XCTAssertEqual(model.currentTask?.source, "BV1multi")

        model.clearTask()
        XCTAssertFalse(model.canClear)
    }

    func testSubmitCreatesMultipleSelectionTask() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1multi",
                        title: "Multi page video",
                        candidates: [
                            .fixture(selectionID: "page:1", title: "Part 1", index: 1),
                            .fixture(selectionID: "page:2", title: "Part 2", index: 2),
                            .fixture(selectionID: "page:3", title: "Part 3", index: 3),
                        ],
                        defaultSelectionID: ""
                    ))
            ],
            createResponses: [
                .success(.fixture(source: "BV1multi", state: "TASK_STATE_PREPARING"))
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1multi",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        model.candidateSelectionMode = .multiple
        model.chooseCandidate(model.resolvedCandidates[1])
        XCTAssertEqual(model.selectedCandidateCount, 2)
        XCTAssertEqual(model.candidateSelectionSummary, "2 Bilibili items selected.")

        await model.submit(serverAddressText: "mac-mini.local:50051")

        let requests = await client.createdRequestsSnapshot()
        XCTAssertEqual(requests.count, 1)
        XCTAssertNil(requests.first?.selectionID)
        XCTAssertEqual(requests.first?.selection?.mode, "multiple")
        XCTAssertEqual(requests.first?.selection?.selectionIDs, ["page:1", "page:2"])

        model.clearTask()
    }

    func testMultipleSelectionCanClearLastCandidate() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1multi",
                        title: "Multi page video",
                        candidates: [
                            .fixture(selectionID: "page:1", title: "Part 1", index: 1),
                            .fixture(selectionID: "page:2", title: "Part 2", index: 2),
                        ],
                        defaultSelectionID: "page:1"
                    ))
            ],
            createResponses: []
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1multi",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        model.candidateSelectionMode = .multiple
        XCTAssertEqual(model.selectedCandidateIDs, Set(["page:1"]))
        XCTAssertTrue(model.canSubmit)

        model.chooseCandidate(model.resolvedCandidates[0])

        XCTAssertEqual(model.selectedCandidateIDs, Set<String>())
        XCTAssertFalse(model.isCandidateSelected(model.resolvedCandidates[0]))
        XCTAssertEqual(model.selectedCandidateCount, 0)
        XCTAssertEqual(model.candidateSelectionSummary, "0 Bilibili items selected.")
        XCTAssertFalse(model.canSubmit)

        model.clearTask()
    }

    func testMultipleSelectionSwitchesBackToCurrentSingleCandidate() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1multi",
                        title: "Multi page video",
                        candidates: [
                            .fixture(selectionID: "page:1", title: "Part 1", index: 1),
                            .fixture(selectionID: "page:2", title: "Part 2", index: 2),
                        ],
                        defaultSelectionID: "page:1"
                    ))
            ],
            createResponses: [
                .success(.fixture(source: "BV1multi", state: "TASK_STATE_PREPARING"))
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1multi",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        model.candidateSelectionMode = .multiple
        model.chooseCandidate(model.resolvedCandidates[0])
        model.chooseCandidate(model.resolvedCandidates[1])
        XCTAssertEqual(model.selectedCandidateID, "page:2")

        model.candidateSelectionMode = .single
        XCTAssertEqual(model.selectedCandidateID, "page:2")
        XCTAssertEqual(model.selectedCandidateIDs, Set(["page:2"]))

        await model.submit(serverAddressText: "mac-mini.local:50051")

        let requests = await client.createdRequestsSnapshot()
        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(requests.first?.selectionID, "page:2")
        XCTAssertEqual(requests.first?.selection?.mode, "single")
        XCTAssertEqual(requests.first?.selection?.selectionIDs, ["page:2"])

        model.clearTask()
    }

    func testSubmitCreatesRangeAndAllSelectionTasks() async {
        let candidates: [BilibiliResolvedCandidate] = [
            .fixture(selectionID: "page:1", title: "Part 1", index: 1),
            .fixture(selectionID: "page:2", title: "Part 2", index: 2),
            .fixture(selectionID: "page:3", title: "Part 3", index: 3),
        ]
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(.fixture(source: "BV1range", candidates: candidates, defaultSelectionID: "")),
                .success(.fixture(source: "BV1all", candidates: candidates, defaultSelectionID: "")),
            ],
            createResponses: [
                .success(.fixture(source: "BV1range", state: "TASK_STATE_PREPARING")),
                .success(.fixture(source: "BV1all", state: "TASK_STATE_PREPARING")),
            ]
        )

        let rangeModel = BilibiliTaskViewModel(
            sourceText: "BV1range",
            clientFactory: { _ in client }
        )
        await rangeModel.submit(serverAddressText: "mac-mini.local:50051")
        rangeModel.candidateSelectionMode = .range
        rangeModel.chooseCandidate(rangeModel.resolvedCandidates[1])
        XCTAssertEqual(rangeModel.rangeStartCandidateID, "page:2")
        XCTAssertEqual(rangeModel.rangeEndCandidateID, "page:2")
        XCTAssertEqual(rangeModel.selectedCandidateCount, 1)
        rangeModel.chooseCandidate(rangeModel.resolvedCandidates[2])
        XCTAssertEqual(rangeModel.rangeStartCandidateID, "page:2")
        XCTAssertEqual(rangeModel.rangeEndCandidateID, "page:3")
        XCTAssertEqual(rangeModel.selectedCandidateCount, 2)

        await rangeModel.submit(serverAddressText: "mac-mini.local:50051")

        let allModel = BilibiliTaskViewModel(
            sourceText: "BV1all",
            clientFactory: { _ in client }
        )
        await allModel.submit(serverAddressText: "mac-mini.local:50051")
        allModel.candidateSelectionMode = .all
        XCTAssertEqual(allModel.selectedCandidateCount, 3)

        await allModel.submit(serverAddressText: "mac-mini.local:50051")

        let requests = await client.createdRequestsSnapshot()
        XCTAssertEqual(requests.count, 2)
        XCTAssertEqual(requests[0].selection?.mode, "range")
        XCTAssertEqual(requests[0].selection?.rangeStartIndex, 2)
        XCTAssertEqual(requests[0].selection?.rangeEndIndex, 3)
        XCTAssertEqual(requests[1].selection?.mode, "all")
        XCTAssertEqual(requests[1].selection?.selectionIDs, [])
    }

    func testV2BatchSelectionDoesNotDependOnLegacySelectionCapability() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1unsupported",
                        candidates: [
                            .fixture(selectionID: "page:1", title: "Part 1", index: 1),
                            .fixture(selectionID: "page:2", title: "Part 2", index: 2),
                        ],
                        defaultSelectionID: ""
                    ))
            ],
            createResponses: [
                .success(.fixture(source: "BV1unsupported", state: "TASK_STATE_PREPARING"))
            ],
            supportsTaskSelection: false
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1unsupported",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        model.candidateSelectionMode = .multiple
        model.chooseCandidate(model.resolvedCandidates[1])

        await model.submit(serverAddressText: "mac-mini.local:50051")

        let requests = await client.createdRequestsSnapshot()
        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(requests.first?.selection?.mode, "multiple")
        XCTAssertEqual(requests.first?.selection?.selectionIDs, ["page:1", "page:2"])
        XCTAssertNotNil(model.currentTask)
        let legacyCalls = await client.legacyRPCCallCountsSnapshot()
        XCTAssertEqual(legacyCalls.resolve, 0)
        XCTAssertEqual(legacyCalls.create, 0)
    }

    func testAllSelectionIsUnavailableAboveExecutionLimitWithoutLoadingAllCandidates() async {
        let candidates = makeCandidates(count: 101, prefix: "fav")
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "fav123",
                        candidates: candidates,
                        defaultSelectionID: "fav-token-1"
                    ))
            ],
            createResponses: [
                .success(.fixture(source: "fav123", state: "TASK_STATE_PREPARING"))
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "fav123",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        XCTAssertEqual(model.resolvedCandidates.count, 50)
        XCTAssertTrue(model.hasMoreResolvedCandidates)
        XCTAssertFalse(model.availableCandidateSelectionModes.contains(.all))
        model.candidateSelectionMode = .all

        XCTAssertFalse(model.canSubmit)
        XCTAssertEqual(model.selectedCandidateCount, 0)
        XCTAssertEqual(
            model.candidateSelectionSummary,
            "All selection is available only for lists of up to 100 items."
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        let requests = await client.createdRequestsSnapshot()
        XCTAssertTrue(requests.isEmpty)
        let pageRequests = await client.resolutionPageRequestsSnapshot()
        XCTAssertTrue(pageRequests.isEmpty)
    }

    func testPagedResolutionShowsLoadMoreNotice() async {
        let candidates = makeCandidates(count: 60, prefix: "item")
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "fav123",
                        sourceKind: "favorite",
                        candidates: candidates,
                        defaultSelectionID: "item-token-1"
                    ))
            ],
            createResponses: []
        )
        let model = BilibiliTaskViewModel(
            sourceText: "fav123",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertEqual(model.fetchNotice?.title, "More items available")
        XCTAssertEqual(model.fetchNotice?.tone, .info)
        XCTAssertEqual(model.fetchNotice?.actionTitle, "Load More")
        XCTAssertTrue(model.availableCandidateSelectionModes.contains(.all))
        XCTAssertEqual(model.resolvedCandidates.count, 50)
        XCTAssertTrue(model.hasMoreResolvedCandidates)
        XCTAssertTrue(model.canReResolve)
    }

    func testResolvedFetchNoticeClearsAfterPlaybackTaskSubmission() async {
        let candidates = makeCandidates(count: 60, prefix: "item")
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "fav123",
                        sourceKind: "favorite",
                        candidates: candidates,
                        defaultSelectionID: "item-token-1"
                    ))
            ],
            createResponses: [
                .success(.fixture(source: "fav123", state: "TASK_STATE_PREPARING"))
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "fav123",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        XCTAssertEqual(model.fetchNotice?.title, "More items available")

        model.selectedCandidateID = "item-token-1"
        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertEqual(model.currentTask?.source, "fav123")
        XCTAssertNil(model.fetchNotice)
        XCTAssertFalse(model.canReResolve)
    }

    func testAllSelectionUsesServerSideSelectionWithoutLoadingEveryCandidatePage() async {
        let candidates = makeCandidates(count: 60, prefix: "all")
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1all-paged",
                        candidates: candidates,
                        defaultSelectionID: "all-token-1"
                    ))
            ],
            createResponses: [
                .success(.fixture(source: "BV1all-paged", state: "TASK_STATE_PREPARING"))
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1all-paged",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        XCTAssertEqual(model.resolvedCandidates.count, 50)
        XCTAssertTrue(model.hasMoreResolvedCandidates)
        XCTAssertTrue(model.availableCandidateSelectionModes.contains(.all))
        model.candidateSelectionMode = .all
        XCTAssertEqual(model.selectedCandidateCount, 60)

        await model.submit(serverAddressText: "mac-mini.local:50051")

        let requests = await client.v2TaskRequestsSnapshot()
        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(requests.first?.selection, .all)
        let pageRequests = await client.resolutionPageRequestsSnapshot()
        XCTAssertTrue(pageRequests.isEmpty)
        model.clearTask()
    }

    func testPagedLargeDownloadCanSubmitRangeOfAtMost100Candidates() async {
        let candidates = makeCandidates(count: 101, prefix: "download")
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1download-large",
                        candidates: candidates,
                        defaultSelectionID: "download-token-1"
                    ))
            ],
            createResponses: [
                .success(
                    .fixture(id: "bilibili-download-large", source: "BV1download-large", state: "TASK_STATE_QUEUED"))
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1download-large",
            clientFactory: { _ in client }
        )
        model.submissionMode = .download

        await model.submit(serverAddressText: "mac-mini.local:50051")
        XCTAssertTrue(model.isWaitingForCandidateSelection)
        XCTAssertEqual(model.resolvedCandidates.count, 50)
        XCTAssertTrue(model.hasMoreResolvedCandidates)
        XCTAssertFalse(model.availableCandidateSelectionModes.contains(.all))
        let requestsBeforeSelection = await client.v2TaskRequestsSnapshot()
        XCTAssertTrue(requestsBeforeSelection.isEmpty)

        await model.loadMoreResolvedCandidates(serverAddressText: "mac-mini.local:50051")
        XCTAssertEqual(model.resolvedCandidates.count, 100)
        XCTAssertTrue(model.hasMoreResolvedCandidates)
        model.candidateSelectionMode = .range
        model.chooseCandidate(model.resolvedCandidates[0])
        model.chooseCandidate(model.resolvedCandidates[99])
        XCTAssertEqual(model.selectedCandidateCount, 100)
        XCTAssertTrue(model.canSubmit)

        await model.submit(serverAddressText: "mac-mini.local:50051")

        let requests = await client.v2TaskRequestsSnapshot()
        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(
            requests.first?.selection,
            .range(startCandidateToken: "download-token-1", endCandidateToken: "download-token-100")
        )
        if let execution = requests.first?.execution, case let .download(spec) = execution {
            XCTAssertEqual(spec.qualityQN, 0)
        } else {
            XCTFail("Expected typed download execution.")
        }
        model.clearTask()
    }

    func testCandidateNearEndAutomaticallyLoadsPagesOnceAndKeepsSnapshotBoundary() async {
        let candidates = makeCandidates(count: 120, prefix: "auto-page")
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1auto-page",
                        candidates: candidates,
                        defaultSelectionID: "auto-page-token-1"
                    ))
            ],
            createResponses: []
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1auto-page",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        async let firstAppearance: Void = model.loadMoreResolvedCandidatesIfNeeded(
            appearingCandidateID: "auto-page-token-49",
            serverAddressText: "mac-mini.local:50051"
        )
        async let repeatedAppearance: Void = model.loadMoreResolvedCandidatesIfNeeded(
            appearingCandidateID: "auto-page-token-49",
            serverAddressText: "mac-mini.local:50051"
        )
        _ = await (firstAppearance, repeatedAppearance)

        XCTAssertEqual(model.resolvedCandidates.count, 100)
        XCTAssertTrue(model.hasMoreResolvedCandidates)
        let firstPageRequests = await client.resolutionPageRequestsSnapshot()
        XCTAssertEqual(firstPageRequests.count, 1)

        await model.loadMoreResolvedCandidatesIfNeeded(
            appearingCandidateID: "auto-page-token-100",
            serverAddressText: "mac-mini.local:50051"
        )

        XCTAssertEqual(model.resolvedCandidates.count, 120)
        XCTAssertFalse(model.hasMoreResolvedCandidates)
        let allPageRequests = await client.resolutionPageRequestsSnapshot()
        XCTAssertEqual(allPageRequests.count, 2)
        model.clearTask()
    }

    func testDelayedResolutionOutlivesOrdinaryOperationTimeout() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [],
            createResponses: [],
            suspendsResolveResponses: true
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1slow-resolution",
            operationTimeout: .milliseconds(20),
            resolutionTimeout: .seconds(1),
            clientFactory: { _ in client }
        )
        let submitTask = Task { @MainActor in
            await model.submit(serverAddressText: "mac-mini.local:50051")
        }

        await client.waitForResolveRequestCount(1)
        try? await Task.sleep(nanoseconds: 60_000_000)
        await client.completeNextResolve(
            with: .success(
                .fixture(
                    source: "BV1slow-resolution",
                    candidates: [
                        .fixture(selectionID: "slow-1", title: "Slow result 1", index: 1),
                        .fixture(selectionID: "slow-2", title: "Slow result 2", index: 2),
                    ],
                    defaultSelectionID: "slow-1"
                ))
        )
        await submitTask.value

        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(model.resolvedCandidates.count, 2)
        XCTAssertTrue(model.isWaitingForCandidateSelection)
        model.clearTask()
    }

    func testResolutionStillTimesOutAtItsDedicatedBudget() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [],
            createResponses: [],
            suspendsResolveResponses: true
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1resolution-timeout",
            operationTimeout: .seconds(1),
            resolutionTimeout: .milliseconds(50),
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertTrue(model.errorMessage?.contains("timed out") == true)
        XCTAssertEqual(model.statusMessage, "Could not resolve Bilibili input.")
        await client.completeNextResolve(
            with: .success(.fixture(source: "BV1resolution-timeout"))
        )
        model.clearTask()
    }

    func testAutomaticCandidatePageFailureWaitsForExplicitRetryAndRetainsWindowMarker() async {
        let candidates = makeCandidates(count: 60, prefix: "bounded")
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1bounded",
                        candidates: candidates,
                        defaultSelectionID: "bounded-token-1",
                        candidatesTruncated: true
                    ))
            ],
            createResponses: []
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1bounded",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        XCTAssertTrue(model.isResolvedCandidateWindowTruncated)
        XCTAssertEqual(model.fetchNotice?.title, "Limited list")
        await client.setCandidatePageFailureNext("Temporary page failure.")

        await model.loadMoreResolvedCandidatesIfNeeded(
            appearingCandidateID: "bounded-token-49",
            serverAddressText: "mac-mini.local:50051"
        )
        XCTAssertTrue(model.canRetryResolvedCandidatePage)
        XCTAssertEqual(model.resolvedCandidates.count, 50)
        await model.loadMoreResolvedCandidatesIfNeeded(
            appearingCandidateID: "bounded-token-50",
            serverAddressText: "mac-mini.local:50051"
        )
        let requestsAfterRepeatedAppearance = await client.resolutionPageRequestsSnapshot()
        XCTAssertEqual(requestsAfterRepeatedAppearance.count, 1)

        await model.loadMoreResolvedCandidates(serverAddressText: "mac-mini.local:50051")

        XCTAssertEqual(model.resolvedCandidates.count, 60)
        XCTAssertFalse(model.hasMoreResolvedCandidates)
        XCTAssertTrue(model.isResolvedCandidateWindowTruncated)
        XCTAssertTrue(model.resolvedInput?.candidatesTruncated == true)
        XCTAssertEqual(model.fetchNotice?.title, "Limited list")
        model.candidateSelectionMode = .all
        XCTAssertEqual(model.candidateSelectionSummary, "All 60 items in this limited list are selected.")
        XCTAssertEqual(model.submitButtonTitle, "Submit Window")
        model.candidateSelectionMode = .range
        model.chooseCandidate(model.resolvedCandidates[0])
        model.chooseCandidate(model.resolvedCandidates[59])
        XCTAssertTrue(model.candidateSelectionSummary?.contains("in this limited list") == true)
        let requestsAfterRetry = await client.resolutionPageRequestsSnapshot()
        XCTAssertEqual(requestsAfterRetry.count, 2)
        model.clearTask()
    }

    func testCancelledAutomaticCandidatePageRetriesWhenSameTokenReappears() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1cancelled-auto-page",
                        candidates: makeCandidates(count: 60, prefix: "cancelled-auto-page"),
                        defaultSelectionID: "cancelled-auto-page-token-1"
                    ))
            ],
            createResponses: []
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1cancelled-auto-page",
            clientFactory: { _ in client }
        )
        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.setSuspendsCandidatePageResponses(true)

        let cancelledTask = Task { @MainActor in
            await model.loadMoreResolvedCandidatesIfNeeded(
                appearingCandidateID: "cancelled-auto-page-token-49",
                serverAddressText: "mac-mini.local:50051"
            )
        }
        await client.waitForCandidatePageRequestCount(1)
        cancelledTask.cancel()
        await client.completeNextCandidatePage()
        await cancelledTask.value

        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(model.resolvedCandidates.count, 50)
        XCTAssertTrue(model.hasMoreResolvedCandidates)
        XCTAssertFalse(model.isLoadingMoreCandidates)

        await client.setSuspendsCandidatePageResponses(false)
        await model.loadMoreResolvedCandidatesIfNeeded(
            appearingCandidateID: "cancelled-auto-page-token-49",
            serverAddressText: "mac-mini.local:50051"
        )

        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(model.resolvedCandidates.count, 60)
        XCTAssertFalse(model.isLoadingMoreCandidates)
        let requests = await client.resolutionPageRequestsSnapshot()
        XCTAssertEqual(requests.count, 2)
        if requests.count == 2 {
            XCTAssertEqual(requests[0].pageToken, requests[1].pageToken)
        }
        model.clearTask()
    }

    func testClientCancellationOfAutomaticCandidatePageCanRetry() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1client-cancelled-page",
                        candidates: makeCandidates(count: 60, prefix: "client-cancelled-page"),
                        defaultSelectionID: "client-cancelled-page-token-1"
                    ))
            ],
            createResponses: []
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1client-cancelled-page",
            clientFactory: { _ in client }
        )
        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.setCandidatePageCancellationNext()

        await model.loadMoreResolvedCandidatesIfNeeded(
            appearingCandidateID: "client-cancelled-page-token-49",
            serverAddressText: "mac-mini.local:50051"
        )

        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(model.resolvedCandidates.count, 50)
        XCTAssertFalse(model.isLoadingMoreCandidates)
        await model.loadMoreResolvedCandidatesIfNeeded(
            appearingCandidateID: "client-cancelled-page-token-49",
            serverAddressText: "mac-mini.local:50051"
        )

        XCTAssertEqual(model.resolvedCandidates.count, 60)
        XCTAssertFalse(model.isLoadingMoreCandidates)
        let requests = await client.resolutionPageRequestsSnapshot()
        XCTAssertEqual(requests.count, 2)
        if requests.count == 2 {
            XCTAssertEqual(requests[0].pageToken, requests[1].pageToken)
        }
        model.clearTask()
    }

    func testCancelledAndStaleAutomaticCandidateLoadsDoNotReplaceSessionCandidates() async {
        let candidates = makeCandidates(count: 60, prefix: "stale-page")
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1stale-page",
                        candidates: candidates,
                        defaultSelectionID: "stale-page-token-1"
                    ))
            ],
            createResponses: []
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1stale-page",
            clientFactory: { _ in client }
        )
        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.setSuspendsCandidatePageResponses(true)

        let cancelledTask = Task { @MainActor in
            await model.loadMoreResolvedCandidates(
                serverAddressText: "mac-mini.local:50051"
            )
        }
        await client.waitForCandidatePageRequestCount(1)
        model.clearTask()
        cancelledTask.cancel()
        await client.completeNextCandidatePage()
        await cancelledTask.value
        XCTAssertTrue(model.resolvedCandidates.isEmpty)
        XCTAssertNil(model.resolutionSession)
        XCTAssertFalse(model.hasMoreResolvedCandidates)
        XCTAssertFalse(model.isLoadingMoreCandidates)
        let requestsAfterStaleCancellation = await client.resolutionPageRequestsSnapshot()
        XCTAssertEqual(requestsAfterStaleCancellation.count, 1)
    }

    func testCandidatePageSnapshotMismatchAndExpiryInvalidateResolution() async {
        let candidates = makeCandidates(count: 60, prefix: "expiry")
        for mismatch in [true, false] {
            let client = FakeBilibiliCacheControlClient(
                resolveResponses: [
                    .success(
                        .fixture(
                            source: "BV1candidate-expiry",
                            candidates: candidates,
                            defaultSelectionID: "expiry-token-1"
                        ))
                ],
                createResponses: []
            )
            let model = BilibiliTaskViewModel(
                sourceText: "BV1candidate-expiry",
                clientFactory: { _ in client }
            )
            await model.submit(serverAddressText: "mac-mini.local:50051")
            if mismatch {
                await client.setCandidatePageSnapshotMismatchNext()
            } else {
                await client.setCandidatePageFailureNext("Bilibili resolution page token is invalid or expired.")
            }

            await model.loadMoreResolvedCandidates(serverAddressText: "mac-mini.local:50051")

            XCTAssertNil(model.resolutionSession)
            XCTAssertFalse(model.hasMoreResolvedCandidates)
            XCTAssertTrue(model.errorMessage?.contains("resolution expired") == true)
            XCTAssertEqual(model.statusMessage, "Re-resolve the Bilibili input before submitting.")
            model.clearTask()
        }
    }

    func testCandidatePageCannotChangeFrozenSessionTruncationMarker() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1truncation-marker",
                        candidates: makeCandidates(count: 60, prefix: "marker"),
                        defaultSelectionID: "marker-token-1",
                        candidatesTruncated: true
                    ))
            ],
            createResponses: []
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1truncation-marker",
            clientFactory: { _ in client }
        )
        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.setCandidatePageSessionTruncationMismatchNext()

        await model.loadMoreResolvedCandidates(serverAddressText: "mac-mini.local:50051")

        XCTAssertNil(model.resolutionSession)
        XCTAssertFalse(model.hasMoreResolvedCandidates)
        XCTAssertTrue(model.errorMessage?.contains("resolution expired") == true)
        model.clearTask()
    }

    func testDynamicFeedResolveShowsVolatilityNoticeForServerSourceKinds() async {
        for (source, sourceKind) in [
            ("recommendations", "recommendation"),
            ("https://t.bilibili.com/", "space_dynamic"),
        ] {
            let client = FakeBilibiliCacheControlClient(
                resolveResponses: [
                    .success(
                        .fixture(
                            source: source,
                            sourceKind: sourceKind,
                            candidates: [
                                .fixture(selectionID: "item:1", title: "Feed Item 1", index: 1),
                                .fixture(selectionID: "item:2", title: "Feed Item 2", index: 2),
                            ],
                            defaultSelectionID: ""
                        ))
                ],
                createResponses: []
            )
            let model = BilibiliTaskViewModel(
                sourceText: source,
                clientFactory: { _ in client }
            )

            await model.submit(serverAddressText: "mac-mini.local:50051")

            XCTAssertEqual(model.fetchNotice?.title, "List may change", sourceKind)
            XCTAssertEqual(model.fetchNotice?.tone, .info, sourceKind)
            XCTAssertEqual(
                model.fetchNotice?.message,
                "This Bilibili list or feed can reorder between refreshes. Selections use candidate tokens from this immutable resolution.",
                sourceKind
            )
            XCTAssertTrue(model.canReResolve, sourceKind)
        }
    }

    func testEmptyResolveShowsEmptyListNotice() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(.fixture(source: "history", sourceKind: "history", candidates: [], defaultSelectionID: ""))
            ],
            createResponses: []
        )
        let model = BilibiliTaskViewModel(
            sourceText: "history",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertEqual(model.statusMessage, "No selectable Bilibili item was found.")
        XCTAssertEqual(model.fetchNotice?.title, "No items found")
        XCTAssertEqual(model.fetchNotice?.tone, .warning)
        XCTAssertEqual(model.fetchNotice?.actionTitle, "Re-resolve")
    }

    func testSubmitAfterEmptyResolveReResolvesInput() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(.fixture(source: "history", sourceKind: "history", candidates: [], defaultSelectionID: "")),
                .success(
                    .fixture(
                        source: "history",
                        sourceKind: "history",
                        candidates: [
                            .fixture(selectionID: "history:1", title: "Recovered Item", index: 1)
                        ],
                        defaultSelectionID: "history:1"
                    )),
            ],
            createResponses: [
                .success(.fixture(source: "history", state: "TASK_STATE_PREPARING"))
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "history",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await model.submit(serverAddressText: "mac-mini.local:50051")

        let resolvedRequests = await client.resolvedRequestsSnapshot()
        XCTAssertEqual(resolvedRequests.map(\.urlOrID), ["history", "history"])
        let requests = await client.createdRequestsSnapshot()
        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(requests.first?.selectionID, "history:1")

        model.clearTask()
    }

    func testCredentialResolveFailureShowsCredentialNotice() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .failure(FakeLocalizedError(message: "Login required: missing web cookie."))
            ],
            createResponses: []
        )
        let model = BilibiliTaskViewModel(
            sourceText: "https://www.bilibili.com/account/history",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertEqual(model.statusMessage, "Could not resolve Bilibili input.")
        XCTAssertEqual(model.fetchNotice?.title, "Credentials required")
        XCTAssertEqual(model.fetchNotice?.tone, .warning)
        XCTAssertEqual(model.fetchNotice?.actionTitle, "Retry")
        XCTAssertTrue(model.canRetry)
    }

    func testBilibiliCredentialFailurePatternsShowCredentialNotice() async {
        for message in [
            "API returned code -101.",
            "account \u{672a}\u{767b}\u{5f55}",
            "not logged in",
            "missing SESSDATA",
            "csrf token is invalid",
        ] {
            let client = FakeBilibiliCacheControlClient(
                resolveResponses: [
                    .failure(FakeLocalizedError(message: message))
                ],
                createResponses: []
            )
            let model = BilibiliTaskViewModel(
                sourceText: "https://t.bilibili.com/",
                clientFactory: { _ in client }
            )

            await model.submit(serverAddressText: "mac-mini.local:50051")

            XCTAssertEqual(model.fetchNotice?.title, "Credentials required", message)
            XCTAssertEqual(model.fetchNotice?.actionTitle, "Retry", message)
        }
    }

    func testNonCredentialAuthorityFailureDoesNotShowCredentialNotice() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .failure(FakeLocalizedError(message: "Invalid URL authority for Bilibili input."))
            ],
            createResponses: []
        )
        let model = BilibiliTaskViewModel(
            sourceText: "not a url",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertNotEqual(model.fetchNotice?.title, "Credentials required")
    }

    func testRetryableTaskFailureShowsRetryNotice() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(source: "BV1rate", state: "TASK_STATE_FAILED", message: "API returned code -352."))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1rate",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertTrue(model.canRetry)
        XCTAssertEqual(model.fetchNotice?.title, "Retry available")
        XCTAssertEqual(model.fetchNotice?.tone, .error)
        XCTAssertEqual(model.fetchNotice?.actionTitle, "Retry")
    }

    func testClearResolvedCandidateSelectionDisablesSelectionSubmit() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1multi",
                        candidates: [
                            .fixture(selectionID: "page:1", title: "Part 1", index: 1),
                            .fixture(selectionID: "page:2", title: "Part 2", index: 2),
                        ],
                        defaultSelectionID: ""
                    ))
            ],
            createResponses: []
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1multi",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        XCTAssertEqual(model.selectedCandidateCount, 1)
        XCTAssertTrue(model.canClearCandidateSelection)

        model.clearResolvedCandidateSelection()

        XCTAssertEqual(model.candidateSelectionMode, .multiple)
        XCTAssertEqual(model.selectedCandidateCount, 0)
        XCTAssertFalse(model.canSubmit)
        XCTAssertFalse(model.canClearCandidateSelection)
    }

    func testClearResolvedCandidateSelectionClearsStaleSubmitError() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1multi",
                        candidates: [
                            .fixture(selectionID: "page:1", title: "Part 1", index: 1),
                            .fixture(selectionID: "page:2", title: "Part 2", index: 2),
                        ],
                        defaultSelectionID: "page:1"
                    ))
            ],
            createResponses: [
                .failure(FakeLocalizedError(message: "Upstream timed out."))
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1multi",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        XCTAssertTrue(model.canSubmit)

        await model.submit(serverAddressText: "mac-mini.local:50051")
        XCTAssertEqual(model.errorMessage, "Upstream timed out.")
        XCTAssertTrue(model.canRetry)
        XCTAssertTrue(model.canClearCandidateSelection)

        model.clearResolvedCandidateSelection()

        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(model.statusMessage, "Select a Bilibili item to play.")
        XCTAssertFalse(model.canRetry)
        XCTAssertFalse(model.canSubmit)
    }

    func testClearResolvedCandidateSelectionIsDisabledDuringSubmission() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1multi",
                        candidates: [
                            .fixture(selectionID: "page:1", title: "Part 1", index: 1),
                            .fixture(selectionID: "page:2", title: "Part 2", index: 2),
                        ],
                        defaultSelectionID: "page:1"
                    ))
            ],
            createResponses: [
                .success(.fixture(id: "task-selected", source: "BV1multi", state: "TASK_STATE_PREPARING"))
            ],
            suspendsCreateResponses: true
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1multi",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        XCTAssertTrue(model.canClearCandidateSelection)

        let submitTask = Task {
            await model.submit(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForCreateRequestCount(1)

        XCTAssertTrue(model.isSubmitting)
        XCTAssertFalse(model.canClearCandidateSelection)

        model.clearResolvedCandidateSelection()

        XCTAssertEqual(model.selectedCandidateID, "page:1")
        XCTAssertEqual(model.selectedCandidateCount, 1)
        XCTAssertFalse(model.canSubmit)

        await client.completeNextCreate(
            with: .success(.fixture(id: "task-selected", source: "BV1multi", state: "TASK_STATE_PREPARING")))
        await submitTask.value
    }

    func testReResolveRefreshesCandidatesWithoutCreatingTask() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1multi",
                        title: "First result",
                        candidates: [
                            .fixture(selectionID: "page:1", title: "Old 1", index: 1),
                            .fixture(selectionID: "page:2", title: "Old 2", index: 2),
                        ],
                        defaultSelectionID: ""
                    )),
                .success(
                    .fixture(
                        source: "BV1multi",
                        title: "Second result",
                        candidates: [
                            .fixture(selectionID: "page:3", title: "New 3", index: 3),
                            .fixture(selectionID: "page:4", title: "New 4", index: 4),
                        ],
                        defaultSelectionID: "page:4"
                    )),
            ],
            createResponses: []
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1multi",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        model.selectedCandidateID = "page:2"

        await model.reResolve(serverAddressText: "mac-mini.local:50051")

        let resolvedRequests = await client.resolvedRequestsSnapshot()
        XCTAssertEqual(resolvedRequests.map(\.urlOrID), ["BV1multi", "BV1multi"])
        XCTAssertEqual(model.resolvedInput?.title, "Second result")
        XCTAssertEqual(model.resolvedCandidates.map(\.selectionID), ["page:3", "page:4"])
        XCTAssertEqual(model.selectedCandidateID, "page:4")
        let requests = await client.createdRequestsSnapshot()
        XCTAssertTrue(requests.isEmpty)
    }

    func testReResolveWithSingleCandidateDoesNotCreatePlaybackTask() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1multi",
                        title: "Multi result",
                        candidates: [
                            .fixture(selectionID: "page:1", title: "Part 1", index: 1),
                            .fixture(selectionID: "page:2", title: "Part 2", index: 2),
                        ],
                        defaultSelectionID: "page:1"
                    )),
                .success(
                    .fixture(
                        source: "BV1multi",
                        title: "Single result",
                        candidates: [
                            .fixture(selectionID: "page:solo", title: "Only Part", index: 1)
                        ],
                        defaultSelectionID: "page:solo"
                    )),
            ],
            createResponses: []
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1multi",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        await model.reResolve(serverAddressText: "mac-mini.local:50051")

        let resolvedRequests = await client.resolvedRequestsSnapshot()
        XCTAssertEqual(resolvedRequests.map(\.urlOrID), ["BV1multi", "BV1multi"])
        XCTAssertEqual(model.resolvedInput?.title, "Single result")
        XCTAssertEqual(model.resolvedCandidates.map(\.selectionID), ["page:solo"])
        XCTAssertEqual(model.selectedCandidateID, "page:solo")
        XCTAssertEqual(model.statusMessage, "Bilibili input resolved.")
        XCTAssertNil(model.currentTask)
        XCTAssertFalse(model.isSubmitting)
        XCTAssertFalse(model.isResolving)
        XCTAssertTrue(model.canSubmit)

        let requests = await client.createdRequestsSnapshot()
        XCTAssertTrue(requests.isEmpty)
    }

    func testReResolveDownloadIgnoresInvalidPlaybackCodec() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1download-reresolve-vp9",
                        candidates: [
                            .fixture(selectionID: "page:1", title: "Part 1", index: 1),
                            .fixture(selectionID: "page:2", title: "Part 2", index: 2),
                        ],
                        defaultSelectionID: "page:1"
                    )),
                .success(
                    .fixture(
                        source: "BV1download-reresolve-vp9",
                        candidates: [
                            .fixture(selectionID: "page:1", title: "Part 1 refreshed", index: 1),
                            .fixture(selectionID: "page:2", title: "Part 2 refreshed", index: 2),
                        ],
                        defaultSelectionID: "page:1"
                    )),
            ],
            createResponses: []
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1download-reresolve-vp9",
            encodingPreference: "vp9",
            clientFactory: { _ in client }
        )
        model.submissionMode = .download

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await model.reResolve(serverAddressText: "mac-mini.local:50051")

        let resolvedRequests = await client.resolvedRequestsSnapshot()
        XCTAssertEqual(resolvedRequests.count, 2)
        XCTAssertEqual(resolvedRequests.map(\.options.encodingPreference), ["", ""])
        XCTAssertEqual(model.resolvedCandidates.map(\.title), ["Part 1 refreshed", "Part 2 refreshed"])
        XCTAssertNil(model.errorMessage)
        XCTAssertFalse(model.isResolving)
        let taskRequests = await client.v2TaskRequestsSnapshot()
        XCTAssertTrue(taskRequests.isEmpty)
    }

    func testSubmitUsesSingleCandidateFromReResolveWithoutResolvingAgain() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1multi",
                        title: "Multi result",
                        candidates: [
                            .fixture(selectionID: "page:1", title: "Part 1", index: 1),
                            .fixture(selectionID: "page:2", title: "Part 2", index: 2),
                        ],
                        defaultSelectionID: "page:1"
                    )),
                .success(
                    .fixture(
                        source: "BV1multi",
                        title: "Single result",
                        candidates: [
                            .fixture(selectionID: "page:solo", title: "Only Part", index: 1)
                        ],
                        defaultSelectionID: "page:solo"
                    )),
            ],
            createResponses: [
                .success(.fixture(source: "BV1multi", state: "TASK_STATE_PREPARING"))
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1multi",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await model.reResolve(serverAddressText: "mac-mini.local:50051")
        await model.submit(serverAddressText: "mac-mini.local:50051")

        let resolvedRequests = await client.resolvedRequestsSnapshot()
        XCTAssertEqual(resolvedRequests.map(\.urlOrID), ["BV1multi", "BV1multi"])
        let requests = await client.createdRequestsSnapshot()
        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(requests.first?.selectionID, "page:solo")
        XCTAssertEqual(requests.first?.selection?.legacySingleSelectionID, "page:solo")

        model.clearTask()
    }

    func testSubmitWithExistingTaskReResolvesInsteadOfReusingCachedSelection() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1dynamic",
                        candidates: [
                            .fixture(selectionID: "page:old", title: "Old Item", index: 1)
                        ],
                        defaultSelectionID: "page:old"
                    )),
                .success(
                    .fixture(
                        source: "BV1dynamic",
                        candidates: [
                            .fixture(selectionID: "page:new", title: "New Item", index: 1)
                        ],
                        defaultSelectionID: "page:new"
                    )),
            ],
            createResponses: [
                .success(.fixture(source: "BV1dynamic", state: "TASK_STATE_PREPARING")),
                .success(.fixture(id: "bilibili-playback-2", source: "BV1dynamic", state: "TASK_STATE_PREPARING")),
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1dynamic",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        XCTAssertNotNil(model.currentTask)

        await model.submit(serverAddressText: "mac-mini.local:50051")

        let resolvedRequests = await client.resolvedRequestsSnapshot()
        XCTAssertEqual(resolvedRequests.map(\.urlOrID), ["BV1dynamic", "BV1dynamic"])
        let requests = await client.createdRequestsSnapshot()
        XCTAssertEqual(requests.count, 2)
        XCTAssertEqual(requests.map(\.selectionID), ["page:old", "page:new"])
        XCTAssertEqual(model.currentTask?.id, "bilibili-playback-2")

        model.clearTask()
    }

    func testReResolveWithInvalidEndpointKeepsExistingCandidates() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1multi",
                        title: "Resolved result",
                        candidates: [
                            .fixture(selectionID: "page:1", title: "Part 1", index: 1),
                            .fixture(selectionID: "page:2", title: "Part 2", index: 2),
                        ],
                        defaultSelectionID: "page:1"
                    )),
                .success(
                    .fixture(
                        source: "BV1multi",
                        title: "Refreshed result",
                        candidates: [
                            .fixture(selectionID: "page:3", title: "Part 3", index: 3),
                            .fixture(selectionID: "page:4", title: "Part 4", index: 4),
                        ],
                        defaultSelectionID: "page:3"
                    )),
            ],
            createResponses: []
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1multi",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        model.selectedCandidateID = "page:2"

        await model.reResolve(serverAddressText: "not a valid endpoint")

        XCTAssertEqual(
            model.errorMessage,
            "Use a cache server address or URL, such as mac-mini.local:50051 or https://cache.example.com."
        )
        XCTAssertEqual(model.statusMessage, "Cache server address is invalid.")
        XCTAssertEqual(model.resolvedInput?.title, "Resolved result")
        XCTAssertEqual(model.resolvedCandidates.map(\.selectionID), ["page:1", "page:2"])
        XCTAssertEqual(model.selectedCandidateID, "page:2")
        XCTAssertTrue(model.canSubmit)
        XCTAssertTrue(model.canRetry)

        await model.retry(serverAddressText: "mac-mini.local:50051")

        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(model.statusMessage, "Select a Bilibili item to play.")
        XCTAssertEqual(model.resolvedInput?.title, "Refreshed result")
        XCTAssertEqual(model.resolvedCandidates.map(\.selectionID), ["page:3", "page:4"])
        XCTAssertEqual(model.selectedCandidateID, "page:3")

        let resolvedRequests = await client.resolvedRequestsSnapshot()
        XCTAssertEqual(resolvedRequests.map(\.urlOrID), ["BV1multi", "BV1multi"])
        let requests = await client.createdRequestsSnapshot()
        XCTAssertTrue(requests.isEmpty)
    }

    func testReResolveFailureKeepsExistingCandidatesAndSelection() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1multi",
                        title: "Resolved result",
                        candidates: [
                            .fixture(selectionID: "page:1", title: "Part 1", index: 1),
                            .fixture(selectionID: "page:2", title: "Part 2", index: 2),
                        ],
                        defaultSelectionID: "page:1"
                    )),
                .failure(FakeLocalizedError(message: "Upstream timed out.")),
                .success(
                    .fixture(
                        source: "BV1multi",
                        title: "Refreshed result",
                        candidates: [
                            .fixture(selectionID: "page:3", title: "Part 3", index: 3),
                            .fixture(selectionID: "page:4", title: "Part 4", index: 4),
                        ],
                        defaultSelectionID: "page:3"
                    )),
            ],
            createResponses: []
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1multi",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        model.selectedCandidateID = "page:2"

        await model.reResolve(serverAddressText: "mac-mini.local:50051")

        XCTAssertEqual(model.errorMessage, "Upstream timed out.")
        XCTAssertEqual(model.statusMessage, "Could not resolve Bilibili input.")
        XCTAssertEqual(model.resolvedInput?.title, "Resolved result")
        XCTAssertEqual(model.resolvedCandidates.map(\.selectionID), ["page:1", "page:2"])
        XCTAssertEqual(model.selectedCandidateID, "page:2")
        XCTAssertTrue(model.canSubmit)
        XCTAssertTrue(model.canReResolve)
        XCTAssertFalse(model.isResolving)

        await model.retry(serverAddressText: "mac-mini.local:50051")

        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(model.statusMessage, "Select a Bilibili item to play.")
        XCTAssertEqual(model.resolvedInput?.title, "Refreshed result")
        XCTAssertEqual(model.resolvedCandidates.map(\.selectionID), ["page:3", "page:4"])
        XCTAssertEqual(model.selectedCandidateID, "page:3")

        let resolvedRequests = await client.resolvedRequestsSnapshot()
        XCTAssertEqual(resolvedRequests.map(\.urlOrID), ["BV1multi", "BV1multi", "BV1multi"])
        let requests = await client.createdRequestsSnapshot()
        XCTAssertTrue(requests.isEmpty)
    }

    func testReResolveFailureIsIgnoredWhenInputChangesBeforeCompletion() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1old",
                        title: "Resolved result",
                        candidates: [
                            .fixture(selectionID: "page:1", title: "Part 1", index: 1),
                            .fixture(selectionID: "page:2", title: "Part 2", index: 2),
                        ],
                        defaultSelectionID: "page:1"
                    ))
            ],
            createResponses: []
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1old",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.setSuspendsResolveResponses(true)
        let reResolveTask = Task {
            await model.reResolve(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForResolveRequestCount(2)
        XCTAssertTrue(model.isResolving)

        model.sourceText = "BV1new"
        await client.completeNextResolve(with: .failure(FakeLocalizedError(message: "Old upstream timeout.")))
        await reResolveTask.value

        XCTAssertFalse(model.isResolving)
        XCTAssertFalse(model.isSubmitting)
        XCTAssertNil(model.resolvedInput)
        XCTAssertNil(model.currentTask)
        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(model.statusMessage, "Bilibili input changed before resolve completed.")
        let requests = await client.createdRequestsSnapshot()
        XCTAssertTrue(requests.isEmpty)
    }

    func testSubmitReResolvesWhenEndpointChangesAfterCandidateSelection() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1multi",
                        title: "Server A result",
                        candidates: [
                            .fixture(selectionID: "page:a1", title: "A1", index: 1),
                            .fixture(selectionID: "page:a2", title: "A2", index: 2),
                        ],
                        defaultSelectionID: ""
                    )),
                .success(
                    .fixture(
                        source: "BV1multi",
                        title: "Server B result",
                        candidates: [
                            .fixture(selectionID: "page:b1", title: "B1", index: 1)
                        ],
                        defaultSelectionID: "page:b1"
                    )),
            ],
            createResponses: [
                .success(.fixture(source: "BV1multi", state: "TASK_STATE_PREPARING"))
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1multi",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "server-a.local:50051")
        XCTAssertTrue(model.isWaitingForCandidateSelection)
        model.selectedCandidateID = "page:a2"

        await model.submit(serverAddressText: "server-b.local:50051")

        let resolvedRequests = await client.resolvedRequestsSnapshot()
        XCTAssertEqual(resolvedRequests.map(\.urlOrID), ["BV1multi", "BV1multi"])
        let requests = await client.createdRequestsSnapshot()
        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(requests.first?.selectionID, "page:b1")

        model.clearTask()
    }

    func testSubmitReResolvesWhenOptionsChangeAfterCandidateSelection() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1multi",
                        title: "Default quality result",
                        candidates: [
                            .fixture(selectionID: "page:default1", title: "Default 1", index: 1),
                            .fixture(selectionID: "page:default2", title: "Default 2", index: 2),
                        ],
                        defaultSelectionID: ""
                    )),
                .success(
                    .fixture(
                        source: "BV1multi",
                        title: "1080p result",
                        candidates: [
                            .fixture(selectionID: "page:1080p1", title: "1080p 1", index: 1)
                        ],
                        defaultSelectionID: "page:1080p1"
                    )),
            ],
            createResponses: [
                .success(.fixture(source: "BV1multi", state: "TASK_STATE_PREPARING"))
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1multi",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        XCTAssertTrue(model.isWaitingForCandidateSelection)
        model.selectedCandidateID = "page:default2"
        model.qualityPreference = "1080p"

        await model.submit(serverAddressText: "mac-mini.local:50051")

        let resolvedRequests = await client.resolvedRequestsSnapshot()
        XCTAssertEqual(resolvedRequests.map(\.options.qualityPreference), ["", "1080p"])
        let requests = await client.createdRequestsSnapshot()
        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(requests.first?.selectionID, "page:1080p1")

        model.clearTask()
    }

    func testPlaybackPolicyChangePreservesResolvedCandidateSelection() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "BV1multi",
                        title: "Multi page video",
                        candidates: [
                            .fixture(selectionID: "page:1", title: "Part 1", index: 1),
                            .fixture(selectionID: "page:2", title: "Part 2", index: 2),
                        ],
                        defaultSelectionID: ""
                    ))
            ],
            createResponses: [
                .success(.fixture(source: "BV1multi", state: "TASK_STATE_PREPARING"))
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1multi",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        model.selectedCandidateID = "page:2"
        model.playbackTranscodingPreference = .force
        model.playbackCompatibleVariantPreference = .preferRequested
        model.playbackWeakNetworkPreference = .holdDowngrade

        XCTAssertEqual(model.resolvedCandidates.map(\.selectionID), ["page:1", "page:2"])
        XCTAssertEqual(model.selectedCandidateID, "page:2")
        XCTAssertTrue(model.isWaitingForCandidateSelection)

        await model.submit(serverAddressText: "mac-mini.local:50051")

        let resolvedRequests = await client.resolvedRequestsSnapshot()
        XCTAssertEqual(resolvedRequests.count, 1)
        let requests = await client.v2TaskRequestsSnapshot()
        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(requests.first?.selection, .single(candidateToken: "page:2"))
        if case let .playback(spec) = requests.first?.execution {
            XCTAssertEqual(
                spec.policy,
                BilibiliPlaybackPolicy(
                    transcodingPreference: .force,
                    compatibleVariantPreference: .preferRequested,
                    weakNetworkPreference: .holdDowngrade
                )
            )
        } else {
            XCTFail("Expected typed playback execution.")
        }

        model.clearTask()
    }

    func testSubmitIgnoresResolveResultWhenInputChangesBeforeCompletion() async {
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1old", state: "TASK_STATE_PREPARING"))
            ],
            suspendsResolveResponses: true
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1old",
            clientFactory: { _ in client }
        )

        let submitTask = Task {
            await model.submit(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForResolveRequestCount(1)
        XCTAssertTrue(model.isResolving)

        model.sourceText = "BV1new"
        await client.completeNextResolve(with: .success(.fixture(source: "BV1old")))
        await submitTask.value

        XCTAssertFalse(model.isResolving)
        XCTAssertFalse(model.isSubmitting)
        XCTAssertNil(model.resolvedInput)
        XCTAssertNil(model.currentTask)
        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(model.statusMessage, "Bilibili input changed before resolve completed.")
        let requests = await client.createdRequestsSnapshot()
        XCTAssertTrue(requests.isEmpty)
    }

    func testSubmitIgnoresResolveResultWhenOptionsChangeBeforeCompletion() async {
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1old", state: "TASK_STATE_PREPARING"))
            ],
            suspendsResolveResponses: true
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1old",
            qualityPreference: "720p",
            clientFactory: { _ in client }
        )

        let submitTask = Task {
            await model.submit(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForResolveRequestCount(1)
        XCTAssertTrue(model.isResolving)

        model.qualityPreference = "1080p"
        await client.completeNextResolve(with: .success(.fixture(source: "BV1old")))
        await submitTask.value

        XCTAssertFalse(model.isResolving)
        XCTAssertFalse(model.isSubmitting)
        XCTAssertNil(model.resolvedInput)
        XCTAssertNil(model.currentTask)
        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(model.statusMessage, "Bilibili input changed before resolve completed.")
        let requests = await client.createdRequestsSnapshot()
        XCTAssertTrue(requests.isEmpty)
    }

    func testSubmitIgnoresUnsupportedResolveFallbackWhenInputChangesBeforeCompletion() async {
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1old", state: "TASK_STATE_PREPARING"))
            ],
            suspendsResolveResponses: true
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1old",
            clientFactory: { _ in client }
        )

        let submitTask = Task {
            await model.submit(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForResolveRequestCount(1)
        XCTAssertTrue(model.isResolving)

        model.sourceText = "BV1new"
        await client.completeNextResolve(with: .failure(CacheControlClientUnsupportedFeature.bilibiliResolve))
        await submitTask.value

        XCTAssertFalse(model.isResolving)
        XCTAssertFalse(model.isSubmitting)
        XCTAssertNil(model.resolvedInput)
        XCTAssertNil(model.currentTask)
        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(model.statusMessage, "Bilibili input changed before resolve completed.")
        let requests = await client.createdRequestsSnapshot()
        XCTAssertTrue(requests.isEmpty)
    }

    func testSubmitIgnoresUnsupportedResolveFallbackWhenOptionsChangeBeforeCompletion() async {
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1old", state: "TASK_STATE_PREPARING"))
            ],
            suspendsResolveResponses: true
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1old",
            qualityPreference: "720p",
            clientFactory: { _ in client }
        )

        let submitTask = Task {
            await model.submit(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForResolveRequestCount(1)
        XCTAssertTrue(model.isResolving)

        model.qualityPreference = "1080p"
        await client.completeNextResolve(with: .failure(CacheControlClientUnsupportedFeature.bilibiliResolve))
        await submitTask.value

        XCTAssertFalse(model.isResolving)
        XCTAssertFalse(model.isSubmitting)
        XCTAssertNil(model.resolvedInput)
        XCTAssertNil(model.currentTask)
        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(model.statusMessage, "Bilibili input changed before resolve completed.")
        let requests = await client.createdRequestsSnapshot()
        XCTAssertTrue(requests.isEmpty)
    }

    func testTerminalSubmitResponseDoesNotStartWatching() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(source: "BV1fail", state: "TASK_STATE_FAILED", message: "Planning failed."))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1fail",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_FAILED")
        XCTAssertFalse(model.isWatching)
        XCTAssertEqual(model.statusMessage, "Planning failed.")
        XCTAssertEqual(model.errorMessage, "Planning failed.")
        XCTAssertFalse(model.canCancel)
        XCTAssertTrue(model.canRetry)
    }

    func testPreparingTaskShowsPendingOfflineFillBadge() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(state: "TASK_STATE_PREPARING", message: "Preparing playback."))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1pending",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertEqual(model.progressiveCacheStatusBadge?.label, "Pending offline fill")
        XCTAssertEqual(model.progressiveCacheStatusBadge?.systemImage, "clock")

        model.clearTask()
    }

    func testSubmitCleansUpWhenSubmissionModeChangesDuringResolve() async {
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1mode-change", state: "TASK_STATE_PREPARING"))
            ],
            suspendsResolveResponses: true
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1mode-change",
            clientFactory: { _ in client }
        )

        let submitTask = Task {
            await model.submit(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForResolveRequestCount(1)
        XCTAssertTrue(model.isResolving)
        XCTAssertTrue(model.isSubmitting)

        model.submissionMode = .download
        await client.completeNextResolve(with: .success(.fixture(source: "BV1mode-change")))
        await submitTask.value

        XCTAssertFalse(model.isResolving)
        XCTAssertFalse(model.isSubmitting)
        XCTAssertNil(model.resolutionSession)
        XCTAssertNil(model.currentTask)
        XCTAssertEqual(model.statusMessage, "Bilibili submission mode changed before resolve completed.")
        let requests = await client.v2TaskRequestsSnapshot()
        XCTAssertTrue(requests.isEmpty)
        model.clearTask()
    }

    func testPlayablePartialTaskKeepsPlaybackEnabledAndShowsFillProgressBadge() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.playableFixture(downloadedBytes: 256, totalBytes: 1_024))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1partial",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertTrue(model.canPlay)
        XCTAssertEqual(model.progressiveCacheStatusBadge?.label, "Partially cached 25%")
        XCTAssertEqual(model.progressiveCacheStatusBadge?.systemImage, "arrow.down.circle")

        model.clearTask()
    }

    func testPlayablePartialTaskCapsBytePercentAtOverallProgress() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.playableFixture(progress: 0.45, downloadedBytes: 100, totalBytes: 100))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1partial",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertTrue(model.canPlay)
        XCTAssertEqual(model.progressiveCacheStatusBadge?.label, "Partially cached 45%")
        XCTAssertEqual(model.progressiveCacheStatusBadge?.systemImage, "arrow.down.circle")

        model.clearTask()
    }

    func testPlayableTaskShowsQuotaBlockedBadgeWhenOfflineFillHitsWatermark() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.playableFixture(message: "Offline cache fill paused because HLS quota watermark was reached."))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1quota",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertTrue(model.canPlay)
        XCTAssertEqual(model.progressiveCacheStatusBadge?.label, "Quota blocked; playable online")
        XCTAssertEqual(model.progressiveCacheStatusBadge?.systemImage, "externaldrive.badge.xmark")

        model.clearTask()
    }

    func testPlayableTaskShowsRetryingOfflineCacheBadge() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.playableFixture(message: "Retrying offline cache fill with backup URLs."))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1retry",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertTrue(model.canPlay)
        XCTAssertEqual(model.progressiveCacheStatusBadge?.label, "Retrying offline cache")
        XCTAssertEqual(model.progressiveCacheStatusBadge?.systemImage, "arrow.clockwise.circle")

        model.clearTask()
    }

    func testPlayableTaskShowsUpstreamFailedPartialCacheBadge() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.playableFixture(message: "Playable online; offline cache fill failed: upstream failed."))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1upstream",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertTrue(model.canPlay)
        XCTAssertEqual(model.progressiveCacheStatusBadge?.label, "Upstream failed; cache may be partial")
        XCTAssertEqual(model.progressiveCacheStatusBadge?.systemImage, "wifi.slash")

        model.clearTask()
    }

    func testPlayableTaskShowsGenericCacheFailedBadgeWhenOfflineFillFails() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(
                .playableFixture(message: "Playable online; some Bilibili playback results failed to cache offline."))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1failed",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertTrue(model.canPlay)
        XCTAssertEqual(model.progressiveCacheStatusBadge?.label, "Cache failed; playable online")
        XCTAssertEqual(model.progressiveCacheStatusBadge?.systemImage, "exclamationmark.triangle")

        model.clearTask()
    }

    func testTypedFillFailureOverridesLegacyMessageAndKeepsPlaybackAvailable() async {
        let fillStatus = HlsCacheFillStatus(
            state: .sourceUnavailable,
            failureKind: .sourceUnavailable,
            completedBytes: 300,
            totalBytes: 1_000,
            totalBytesKnown: true,
            message: "Fresh playback source is unavailable."
        )
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(
                .playableFixture(
                    message: "Offline cache fill paused because HLS quota watermark was reached.",
                    hlsCacheFillStatus: fillStatus
                ))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1typed-fill",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_PLAYABLE")
        XCTAssertTrue(model.canPlay)
        XCTAssertEqual(model.progressiveCacheStatusBadge?.label, "Source unavailable; playable online")
        XCTAssertEqual(model.progressiveCacheFillStatus?.progressPercentLabel, "30%")

        model.clearTask()
    }

    func testMultiResultFillStatusesRemainPerResultAndSurfaceFailureBadge() async {
        let resultItems = [
            BilibiliTaskResultItem.fixture(
                id: "result-complete",
                title: "Cached part",
                state: "TASK_STATE_COMPLETED",
                hlsCacheFillStatus: HlsCacheFillStatus(state: .completed)
            ),
            BilibiliTaskResultItem.fixture(
                id: "result-quota",
                selectionID: "page:2",
                title: "Playable part",
                index: 2,
                hlsCacheFillStatus: HlsCacheFillStatus(state: .blockedQuota)
            ),
        ]
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(
                .playableFixture(
                    message: "Playing online.",
                    resultItems: resultItems
                ))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1multi-fill",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertTrue(model.canPlay)
        XCTAssertEqual(model.taskResults.compactMap(\.hlsCacheFillStatus?.state), [.completed, .blockedQuota])
        XCTAssertEqual(model.progressiveCacheStatusBadge?.label, "Quota blocked; playable online")

        model.clearTask()
    }

    func testV2PagedFillStatusAppearsInResultPresentation() async {
        let summary = CacheTaskOutputSummary(
            revision: 1,
            resultCount: 1,
            terminalResultCount: 0,
            successfulResultCount: 0,
            failedResultCount: 0,
            cancelledResultCount: 0,
            availableArtifactCount: 0,
            primaryResultID: "page-1"
        )
        let fillStatus = HlsCacheFillStatus(
            state: .filling,
            completedBytes: 200,
            totalBytes: 800,
            totalBytesKnown: true
        )
        let page = CacheTaskResultsPage(
            results: [
                CacheTaskResult.fixture(
                    id: "page-1",
                    title: "Paged result",
                    hlsCacheFillStatus: fillStatus
                )
            ],
            pageInfo: CachePageInfo(totalSize: 1, nextPageToken: "", snapshotID: "fill-snapshot"),
            outputRevision: 1
        )
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1paged-fill", state: "TASK_STATE_PLAYABLE", outputSummary: summary))
            ],
            taskResultPagesByTaskID: ["bilibili-playback-1": [page]]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1paged-fill",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await waitUntil(model.taskResults.count == 1)

        XCTAssertEqual(model.taskResults.first?.hlsCacheFillStatus, fillStatus)
        XCTAssertEqual(model.taskResults.first?.hlsCacheFillStatus?.progressPercentLabel, "25%")
        XCTAssertEqual(model.progressiveCacheStatusBadge?.label, "Filling offline cache")

        model.clearTask()
    }

    func testMultiResultTaskExposesSummaryAndPlayableResultFallback() async {
        let childResultID = "bilibili-playback-1-result-2"
        let resultItems: [BilibiliTaskResultItem] = [
            .fixture(
                id: "bilibili-playback-1",
                selectionID: "page:1",
                title: "Part 1",
                state: "TASK_STATE_FAILED",
                message: "Planning failed."
            ),
            .fixture(
                id: childResultID,
                selectionID: "page:2",
                title: "Part 2",
                index: 2,
                state: "TASK_STATE_PLAYABLE",
                playbackSourceItemID: childResultID
            ),
        ]
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(
                .fixture(
                    state: "TASK_STATE_PLAYABLE",
                    progress: 1,
                    message: "1/2 Bilibili playback result(s) are playable.",
                    resultItems: resultItems
                ))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1multi-result",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertTrue(model.canPlay)
        XCTAssertEqual(
            model.playableURL?.absoluteString,
            "http://mac-mini.local:8080/hls/\(childResultID)/master.m3u8"
        )
        XCTAssertEqual(model.taskResults.map(\.selectionID), ["page:1", "page:2"])
        XCTAssertEqual(model.playableTaskResults.map(\.id), [childResultID])
        XCTAssertEqual(model.taskResultSummary?.totalCount, 2)
        XCTAssertEqual(model.taskResultSummary?.readyCount, 1)
        XCTAssertEqual(model.taskResultSummary?.failedCount, 1)
        XCTAssertEqual(model.taskResultSummary?.progress, 1)
        XCTAssertEqual(model.taskResultSummary?.hasPartialSuccess, true)
        XCTAssertEqual(model.statusMessage, "1 of 2 Bilibili results are ready; 1 failed.")
        XCTAssertEqual(model.progressiveCacheStatusBadge?.label, "Partial result success")

        model.clearTask()
    }

    func testV2TaskResultsRefreshByRevisionAppendPagesAndPresentArtifacts() async {
        let revision1 = CacheTaskOutputSummary(
            revision: 1,
            resultCount: 2,
            terminalResultCount: 1,
            successfulResultCount: 1,
            failedResultCount: 0,
            cancelledResultCount: 0,
            availableArtifactCount: 1,
            primaryResultID: "page-1"
        )
        let revision2 = CacheTaskOutputSummary(
            revision: 2,
            resultCount: 2,
            terminalResultCount: 1,
            successfulResultCount: 1,
            failedResultCount: 0,
            cancelledResultCount: 0,
            availableArtifactCount: 0,
            primaryResultID: "revision-2-page-1"
        )
        let artifact = CacheTaskArtifact.fixture(
            id: "subtitle-en",
            uri: "http://mac-mini.local:8080/resources/subtitle-en",
            libraryItemID: "subtitle-library-item"
        )
        let page1 = CacheTaskResultsPage(
            results: [.fixture(id: "page-1", title: "Page One", artifacts: [artifact])],
            pageInfo: CachePageInfo(totalSize: 2, nextPageToken: "revision-1-next", snapshotID: "snapshot-1"),
            outputRevision: 1
        )
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(
                    .fixture(
                        source: "BV1result-pages",
                        state: "TASK_STATE_PREPARING",
                        resultItems: [.fixture(id: "legacy-inline", title: "Legacy row")],
                        outputSummary: revision1
                    ))
            ],
            taskResultPagesByTaskID: ["bilibili-playback-1": [page1]],
            suspendsTaskResultResponses: true
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1result-pages",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForTaskResultRequestCount(1)
        XCTAssertTrue(model.taskResults.isEmpty)
        XCTAssertTrue(model.isLoadingMoreTaskResults)
        XCTAssertEqual(model.currentTask?.resultItems.map(\.id), ["legacy-inline"])

        await client.completeNextTaskResult(with: .success(page1))
        await waitUntil(model.taskResults.map(\.id) == ["page-1"])
        XCTAssertTrue(model.hasMoreTaskResults)
        guard let presentedArtifact = model.taskResults.first?.artifacts.first else {
            XCTFail("Expected a task result artifact.")
            return
        }
        XCTAssertEqual(presentedArtifact.id, "subtitle-en")
        XCTAssertTrue(presentedArtifact.isAvailable)
        XCTAssertTrue(presentedArtifact.canOpenResource)
        XCTAssertTrue(presentedArtifact.canOpenInLibrary)
        XCTAssertEqual(presentedArtifact.libraryItemID, "subtitle-library-item")
        XCTAssertEqual(
            model.artifactURL(for: presentedArtifact)?.absoluteString,
            "http://mac-mini.local:8080/resources/subtitle-en"
        )

        await client.setSuspendsTaskResultResponses(false)
        let page2 = CacheTaskResultsPage(
            results: [.fixture(id: "page-2", title: "Page Two")],
            pageInfo: CachePageInfo(totalSize: 2, nextPageToken: "", snapshotID: "snapshot-1"),
            outputRevision: 1
        )
        await client.enqueueTaskResultPage(
            taskID: "bilibili-playback-1", pageToken: "revision-1-next", result: .success(page2))
        await model.loadMoreTaskResults(serverAddressText: "mac-mini.local:50051")
        XCTAssertEqual(model.taskResults.map(\.id), ["page-1", "page-2"])
        XCTAssertFalse(model.hasMoreTaskResults)

        await client.setSuspendsTaskResultResponses(true)
        let updatedTask = CacheTask.fixture(
            source: "BV1result-pages",
            state: "TASK_STATE_PREPARING",
            resultItems: [.fixture(id: "legacy-revision-2", title: "Stale inline row")],
            outputSummary: revision2
        )
        await client.yield(updatedTask)
        await waitUntil(model.currentTask?.outputSummary?.revision == 2)
        XCTAssertTrue(model.taskResults.isEmpty)
        await client.waitForTaskResultRequestCount(3)

        let revision2Page1 = CacheTaskResultsPage(
            results: [.fixture(id: "revision-2-page-1", title: "Revision Two")],
            pageInfo: CachePageInfo(totalSize: 2, nextPageToken: "revision-2-next", snapshotID: "snapshot-2"),
            outputRevision: 2
        )
        await client.completeNextTaskResult(with: .success(revision2Page1))
        await waitUntil(model.taskResults.map(\.id) == ["revision-2-page-1"])

        await client.setSuspendsTaskResultResponses(false)
        let revision2Page2 = CacheTaskResultsPage(
            results: [.fixture(id: "revision-2-page-2", title: "Revision Two Continued")],
            pageInfo: CachePageInfo(totalSize: 2, nextPageToken: "", snapshotID: "snapshot-2"),
            outputRevision: 2
        )
        await client.enqueueTaskResultPage(
            taskID: "bilibili-playback-1",
            pageToken: "revision-2-next",
            result: .success(revision2Page2)
        )
        await model.loadMoreTaskResults(serverAddressText: "mac-mini.local:50051")
        XCTAssertEqual(model.taskResults.map(\.id), ["revision-2-page-1", "revision-2-page-2"])
        XCTAssertFalse(model.taskResults.contains { $0.id.hasPrefix("legacy-") })
        model.clearTask()
    }

    func testArtifactURLsAreRestrictedToAdvertisedMediaBases() async {
        let uris = [
            "http://mac-mini.local:8080/resources/valid",
            "https://media.example.test/cache/resources/https-valid",
            "/resources/relative-valid",
            "http://attacker.example/resources/off-origin",
            "http://user:password@mac-mini.local:8080/resources/credentials",
            "http://mac-mini.local:8080/resources/query?download=1",
            "http://mac-mini.local:8080/resources/fragment#part",
            "http://mac-mini.local:8080/resources/id%2Fprivate",
            "http://mac-mini.local:8080/resources/%2e%2e/private",
            "http://mac-mini.local:8081/resources/different-port",
        ]
        let artifacts = uris.enumerated().map { index, uri in
            CacheTaskArtifact.fixture(id: "artifact-\(index)", uri: uri)
        }
        let page = CacheTaskResultsPage(
            results: [.fixture(id: "page-1", title: "Artifacts", artifacts: artifacts)],
            pageInfo: CachePageInfo(totalSize: 1, nextPageToken: "", snapshotID: "artifact-snapshot"),
            outputRevision: 1
        )
        let outputSummary = CacheTaskOutputSummary(
            revision: 1,
            resultCount: 1,
            terminalResultCount: 1,
            successfulResultCount: 1,
            failedResultCount: 0,
            cancelledResultCount: 0,
            availableArtifactCount: UInt64(artifacts.count),
            primaryResultID: "page-1"
        )
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(state: "TASK_STATE_PREPARING", outputSummary: outputSummary))
            ],
            taskResultPagesByTaskID: ["bilibili-playback-1": [page]],
            mediaBaseURIs: [
                "http://mac-mini.local:8080",
                "https://media.example.test/cache/",
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1artifact-allowlist",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForTaskResultRequestCount(1)
        await waitUntil(model.taskResults.count == 1)

        let presentedArtifacts = model.taskResults[0].artifacts
        XCTAssertEqual(model.artifactURL(for: presentedArtifacts[0])?.absoluteString, uris[0])
        XCTAssertEqual(model.artifactURL(for: presentedArtifacts[1])?.absoluteString, uris[1])
        XCTAssertEqual(
            model.artifactURL(for: presentedArtifacts[2])?.absoluteString,
            "http://mac-mini.local:8080/resources/relative-valid"
        )
        for artifact in presentedArtifacts.dropFirst(3) {
            XCTAssertNil(model.artifactURL(for: artifact), "Unexpectedly allowed \(artifact.id)")
        }
        model.clearTask()

        let relativePage = CacheTaskResultsPage(
            results: [
                .fixture(
                    id: "page-1",
                    title: "Relative artifact",
                    artifacts: [
                        .fixture(id: "relative", uri: "/resources/relative-id")
                    ]
                )
            ],
            pageInfo: CachePageInfo(totalSize: 1, nextPageToken: "", snapshotID: "relative-snapshot"),
            outputRevision: 1
        )
        let relativeOutputSummary = CacheTaskOutputSummary(
            revision: 1,
            resultCount: 1,
            terminalResultCount: 1,
            successfulResultCount: 1,
            failedResultCount: 0,
            cancelledResultCount: 0,
            availableArtifactCount: 1,
            primaryResultID: "page-1"
        )
        let noBaseClient = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(state: "TASK_STATE_PREPARING", outputSummary: relativeOutputSummary))
            ],
            taskResultPagesByTaskID: ["bilibili-playback-1": [relativePage]],
            mediaBaseURIs: []
        )
        let noBaseModel = BilibiliTaskViewModel(
            sourceText: "BV1artifact-no-base",
            clientFactory: { _ in noBaseClient }
        )
        await noBaseModel.submit(serverAddressText: "mac-mini.local:50051")
        await noBaseClient.waitForTaskResultRequestCount(1)
        await waitUntil(noBaseModel.taskResults.count == 1)
        XCTAssertNil(noBaseModel.artifactURL(for: noBaseModel.taskResults[0].artifacts[0]))
        noBaseModel.clearTask()
    }

    func testTaskResultsCanRetryTransientFirstPageFailure() async {
        let summary = CacheTaskOutputSummary(
            revision: 4,
            resultCount: 1,
            terminalResultCount: 1,
            successfulResultCount: 1,
            failedResultCount: 0,
            cancelledResultCount: 0,
            availableArtifactCount: 0,
            primaryResultID: "result-after-retry"
        )
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(
                .fixture(
                    source: "BV1retry-results",
                    state: "TASK_STATE_PREPARING",
                    resultItems: [.fixture(id: "legacy-only")],
                    outputSummary: summary
                ))
        ])
        let expectedPage = CacheTaskResultsPage(
            results: [.fixture(id: "result-after-retry", title: "Recovered result")],
            pageInfo: CachePageInfo(totalSize: 1, nextPageToken: "", snapshotID: "snapshot-retry"),
            outputRevision: 4
        )
        await client.enqueueTaskResultPage(
            taskID: "bilibili-playback-1",
            pageToken: "",
            result: .failure(FakeLocalizedError(message: "Temporary result-page failure."))
        )
        await client.enqueueTaskResultPage(
            taskID: "bilibili-playback-1",
            pageToken: "",
            result: .success(expectedPage)
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1retry-results",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await waitUntil(model.taskResultsErrorMessage != nil)
        XCTAssertTrue(model.taskResults.isEmpty)
        XCTAssertFalse(model.isLoadingMoreTaskResults)

        await model.retryTaskResults(serverAddressText: "mac-mini.local:50051")

        XCTAssertEqual(model.taskResults.map(\.id), ["result-after-retry"])
        XCTAssertNil(model.taskResultsErrorMessage)
        let requests = await client.taskResultRequestsSnapshot()
        XCTAssertEqual(requests.map(\.pageToken), ["", ""])
        model.clearTask()
    }

    func testTaskOutputCapabilityLossHasExplicitDiagnosticWithoutLegacyRows() async {
        let summary = CacheTaskOutputSummary(
            revision: 1,
            resultCount: 1,
            terminalResultCount: 0,
            successfulResultCount: 0,
            failedResultCount: 0,
            cancelledResultCount: 0,
            availableArtifactCount: 0,
            primaryResultID: ""
        )
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(
                .fixture(
                    source: "BV1output-dropped",
                    state: "TASK_STATE_PREPARING",
                    resultItems: [.fixture(id: "legacy-output-row")],
                    outputSummary: summary
                ))
        ])
        await client.enqueueTaskResultPage(
            taskID: "bilibili-playback-1",
            pageToken: "",
            result: .failure(CacheControlClientUnsupportedOperation.taskOutputV2)
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1output-dropped",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await waitUntil(model.taskResultsErrorMessage != nil)

        XCTAssertNotNil(model.currentTask)
        XCTAssertTrue(model.taskResults.isEmpty)
        XCTAssertTrue(
            model.taskResultsErrorMessage?.contains("Paginated Bilibili task results are unavailable") == true)
        let legacyCalls = await client.legacyRPCCallCountsSnapshot()
        XCTAssertEqual(legacyCalls.resolve, 0)
        XCTAssertEqual(legacyCalls.create, 0)
        model.clearTask()
    }

    func testPlaybackProgressContextUsesActiveEndpoint() async throws {
        let childResultID = "bilibili-playback-1-result-2"
        let resultItems: [BilibiliTaskResultItem] = [
            .fixture(
                id: childResultID,
                selectionID: "page:2",
                title: "Part 2",
                index: 2,
                state: "TASK_STATE_PLAYABLE",
                playbackSourceItemID: childResultID
            )
        ]
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.playableFixture(resultItems: resultItems))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1active-endpoint",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "server-a.local:50051")

        let expectedEndpoint = CacheServerEndpoint(host: "server-a.local", port: 50_051)
        let taskContext = try XCTUnwrap(
            model.playbackProgressContext(serverAddressText: "server-b.local:50051")
        )
        XCTAssertEqual(taskContext.endpoint, expectedEndpoint)

        let result = try XCTUnwrap(model.playableTaskResults.first)
        let resultContext = try XCTUnwrap(
            model.playbackProgressContext(for: result, serverAddressText: "server-b.local:50051")
        )
        XCTAssertEqual(resultContext.endpoint, expectedEndpoint)

        model.clearTask()
    }

    func testTaskResultPlaybackIsDisabledWhileCancellationIsPending() async {
        let childResultID = "bilibili-playback-1-result-2"
        let resultItems: [BilibiliTaskResultItem] = [
            .fixture(
                id: childResultID,
                selectionID: "page:2",
                title: "Part 2",
                index: 2,
                state: "TASK_STATE_PLAYABLE",
                playbackSourceItemID: childResultID
            )
        ]
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.playableFixture(resultItems: resultItems))
        ])
        await client.setSuspendsCancelResponses(true)
        let model = BilibiliTaskViewModel(
            sourceText: "BV1multi-result",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        let playableResult = model.playableTaskResults[0]
        XCTAssertTrue(model.canPlay(result: playableResult))
        XCTAssertNotNil(model.playableURL(for: playableResult))

        let cancelTask = Task {
            await model.cancel(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForCancelRequestCount(1)

        XCTAssertTrue(model.isCancelling)
        XCTAssertFalse(model.canPlay(result: playableResult))
        XCTAssertNil(model.playableURL(for: playableResult))

        await client.completeNextCancel(with: .success(.fixture(state: "TASK_STATE_CANCELLED")))
        await cancelTask.value
        XCTAssertFalse(model.isCancelling)
    }

    func testTaskResultPlaybackIsDisabledForCancellationPendingTaskUpdate() async {
        let childResultID = "bilibili-playback-1-result-2"
        let resultItems: [BilibiliTaskResultItem] = [
            .fixture(
                id: childResultID,
                selectionID: "page:2",
                title: "Part 2",
                index: 2,
                state: "TASK_STATE_PLAYABLE",
                playbackSourceItemID: childResultID
            )
        ]
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.playableFixture(resultItems: resultItems))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1multi-result",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()

        let playableResult = model.playableTaskResults[0]
        XCTAssertTrue(model.canPlay(result: playableResult))
        XCTAssertNotNil(model.playableURL(for: playableResult))

        await client.yield(
            .playableFixture(
                state: "TASK_STATE_CANCEL_REQUESTED",
                resultItems: resultItems
            ))
        await waitUntil(!model.canPlay(result: playableResult))

        XCTAssertFalse(model.isCancelling)
        XCTAssertFalse(model.canPlay(result: playableResult))
        XCTAssertNil(model.playableURL(for: playableResult))

        model.clearTask()
    }

    func testActiveTaskResultPlaybackClearsForCancellationPendingTaskUpdate() async throws {
        let childResultID = "bilibili-playback-1-result-2"
        let resultItems: [BilibiliTaskResultItem] = [
            .fixture(
                id: childResultID,
                selectionID: "page:2",
                title: "Part 2",
                index: 2,
                state: "TASK_STATE_PLAYABLE",
                playbackSourceItemID: childResultID
            )
        ]
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.playableFixture(resultItems: resultItems))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1multi-result",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()

        let playableResult = try XCTUnwrap(model.playableTaskResults.first)
        model.finishPreparedPlayback(result: playableResult, didStartPlayback: true)
        XCTAssertEqual(model.statusMessage, "Playing Part 2.")

        await client.yield(
            .playableFixture(
                state: "TASK_STATE_CANCEL_REQUESTED",
                message: "Cancelling task.",
                resultItems: resultItems
            ))
        await waitUntil(model.statusMessage == "Cancelling task.")

        XCTAssertEqual(model.statusMessage, "Cancelling task.")
        XCTAssertFalse(model.canPlay(result: playableResult))
        XCTAssertNil(model.playableURL(for: playableResult))

        model.clearTask()
    }

    func testTaskResultPlaybackRevalidatesCurrentTaskMembership() async {
        let childResultID = "bilibili-playback-1-result-2"
        let resultItems: [BilibiliTaskResultItem] = [
            .fixture(
                id: childResultID,
                selectionID: "page:2",
                title: "Part 2",
                index: 2,
                state: "TASK_STATE_PLAYABLE",
                playbackSourceItemID: childResultID
            )
        ]
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.playableFixture(resultItems: resultItems))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1multi-result",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        let playableResult = model.playableTaskResults[0]
        XCTAssertTrue(model.canPlay(result: playableResult))
        XCTAssertEqual(
            model.playableURL(for: playableResult)?.absoluteString,
            "http://mac-mini.local:8080/hls/\(childResultID)/master.m3u8"
        )

        model.clearTask()

        XCTAssertFalse(model.canPlay(result: playableResult))
        XCTAssertNil(model.playableURL(for: playableResult))
    }

    func testFinishPreparedPlaybackForTaskResultTracksCachedLibraryItem() async throws {
        let childResultID = "bilibili-playback-1-result-2"
        let childLibraryItemID = "bilibili.hls.\(childResultID)"
        let resultItems: [BilibiliTaskResultItem] = [
            .fixture(
                id: "bilibili-playback-1",
                selectionID: "page:1",
                title: "Part 1",
                state: "TASK_STATE_FAILED",
                message: "Planning failed."
            ),
            .fixture(
                id: childResultID,
                selectionID: "page:2",
                title: "Part 2",
                index: 2,
                state: "TASK_STATE_COMPLETED",
                libraryItemID: childLibraryItemID
            ),
        ]
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(
                .fixture(
                    state: "TASK_STATE_COMPLETED",
                    progress: 1,
                    libraryItemID: "",
                    resultItems: resultItems
                ))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1multi-result",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        let playableResult = try XCTUnwrap(model.playableTaskResults.first)
        XCTAssertEqual(playableResult.playbackLibraryItemID, childLibraryItemID)
        model.finishPreparedPlayback(result: playableResult, didStartPlayback: true)

        XCTAssertEqual(model.statusMessage, "Playing Part 2.")
        XCTAssertTrue(model.isActivePlaybackLibraryItem(id: childLibraryItemID))
        XCTAssertTrue(model.clearTaskIfCachedLibraryItemDeleted(id: childLibraryItemID))
        XCTAssertNotNil(model.currentTask)
        XCTAssertFalse(model.isActivePlaybackLibraryItem(id: childLibraryItemID))
        XCTAssertFalse(model.canPlay)
        XCTAssertEqual(model.statusMessage, "2 Bilibili results failed.")
        let updatedResult = model.currentTask?.resultItems.first { $0.id == childResultID }
        XCTAssertEqual(updatedResult?.state, "TASK_STATE_FAILED")
        XCTAssertEqual(updatedResult?.message, "Cached Bilibili result was deleted.")
        XCTAssertEqual(updatedResult?.libraryItemID, "")
    }

    func testCompletedTaskShowsOfflineReadyBadge() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(
                .fixture(
                    state: "TASK_STATE_COMPLETED",
                    progress: 1,
                    message: "Bilibili playback session is cached for offline playback.",
                    libraryItemID: "bilibili.hls.bilibili-playback-1"
                ))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1done",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertEqual(model.progressiveCacheStatusBadge?.label, "Offline ready")
        XCTAssertEqual(model.progressiveCacheStatusBadge?.systemImage, "externaldrive.fill.badge.checkmark")
    }

    func testCompletedMultiResultTaskShowsPartialOfflineReadyBadge() async {
        let primaryLibraryItemID = "bilibili.hls.bilibili-playback-1"
        let secondaryResultID = "bilibili-playback-1-result-2"
        let secondaryLibraryItemID = "bilibili.hls.bilibili-playback-1-result-2"
        let resultItems: [BilibiliTaskResultItem] = [
            .fixture(
                id: "bilibili-playback-1",
                selectionID: "page:1",
                title: "Part 1",
                state: "TASK_STATE_COMPLETED",
                libraryItemID: primaryLibraryItemID
            ),
            .fixture(
                id: secondaryResultID,
                selectionID: "page:2",
                title: "Part 2",
                index: 2,
                state: "TASK_STATE_PLAYABLE",
                playbackSourceItemID: secondaryResultID
            ),
        ]
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(
                .fixture(
                    state: "TASK_STATE_COMPLETED",
                    progress: 1,
                    message: "Primary Bilibili playback result is cached.",
                    libraryItemID: primaryLibraryItemID,
                    resultItems: resultItems
                ))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1partial-offline",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertEqual(model.taskResultSummary?.cachedCount, 1)
        XCTAssertEqual(model.taskResultSummary?.totalCount, 2)
        XCTAssertEqual(model.progressiveCacheStatusBadge?.label, "1 of 2 offline ready")
        XCTAssertEqual(model.progressiveCacheStatusBadge?.systemImage, "externaldrive.badge.checkmark")
        XCTAssertTrue(model.isWatching)

        await client.waitForWatchSubscription()
        await client.yield(
            .fixture(
                state: "TASK_STATE_COMPLETED",
                progress: 1,
                message: "All Bilibili playback results are cached.",
                libraryItemID: primaryLibraryItemID,
                resultItems: [
                    resultItems[0],
                    .fixture(
                        id: secondaryResultID,
                        selectionID: "page:2",
                        title: "Part 2",
                        index: 2,
                        state: "TASK_STATE_COMPLETED",
                        libraryItemID: secondaryLibraryItemID
                    ),
                ]
            ))
        await waitUntil(model.taskResultSummary?.cachedCount == 2)
        await waitUntil(!model.isWatching)
        await client.waitForWatchTermination()

        XCTAssertEqual(model.progressiveCacheStatusBadge?.label, "Offline ready")
        XCTAssertEqual(model.progressiveCacheStatusBadge?.systemImage, "externaldrive.fill.badge.checkmark")
        XCTAssertEqual(model.statusMessage, "2 Bilibili results are cached for LAN playback.")

        model.clearTask()
    }

    func testCompletedMultiResultTaskShowsOfflineReadyBadgeWhenSiblingsFailOrCancel() async {
        let resultItems: [BilibiliTaskResultItem] = [
            .fixture(
                id: "bilibili-playback-1",
                selectionID: "page:1",
                title: "Part 1",
                state: "TASK_STATE_COMPLETED",
                libraryItemID: "bilibili.hls.bilibili-playback-1"
            ),
            .fixture(
                id: "bilibili-playback-1-result-2",
                selectionID: "page:2",
                title: "Part 2",
                index: 2,
                state: "TASK_STATE_FAILED",
                message: "Planning failed."
            ),
            .fixture(
                id: "bilibili-playback-1-result-3",
                selectionID: "page:3",
                title: "Part 3",
                index: 3,
                state: "TASK_STATE_CANCELLED",
                message: "Cancelled."
            ),
        ]
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(
                .fixture(
                    state: "TASK_STATE_COMPLETED",
                    progress: 1,
                    message: "Primary Bilibili playback result is cached.",
                    libraryItemID: "bilibili.hls.bilibili-playback-1",
                    resultItems: resultItems
                ))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1partial-offline-failures",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertEqual(model.taskResultSummary?.cachedCount, 1)
        XCTAssertEqual(model.taskResultSummary?.failedCount, 1)
        XCTAssertEqual(model.taskResultSummary?.cancelledCount, 1)
        XCTAssertEqual(model.taskResultSummary?.hasPartialSuccess, true)
        XCTAssertEqual(model.progressiveCacheStatusBadge?.label, "1 of 3 offline ready")
        XCTAssertEqual(model.progressiveCacheStatusBadge?.systemImage, "externaldrive.badge.checkmark")
    }

    func testCompletedMultiResultTaskKeepsFailedCacheFillResultPlayable() async throws {
        let primaryLibraryItemID = "bilibili.hls.bilibili-playback-1"
        let secondaryResultID = "bilibili-playback-1-result-2"
        let resultItems: [BilibiliTaskResultItem] = [
            .fixture(
                id: "bilibili-playback-1",
                selectionID: "page:1",
                title: "Part 1",
                state: "TASK_STATE_COMPLETED",
                libraryItemID: primaryLibraryItemID
            ),
            .fixture(
                id: secondaryResultID,
                selectionID: "page:2",
                title: "Part 2",
                index: 2,
                state: "TASK_STATE_FAILED",
                message: "Playable online; offline cache fill failed: upstream failed",
                playbackSourceItemID: secondaryResultID
            ),
        ]
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(
                .fixture(
                    state: "TASK_STATE_COMPLETED",
                    progress: 1,
                    message: "Completed offline; some Bilibili playback results failed to cache offline.",
                    libraryItemID: primaryLibraryItemID,
                    resultItems: resultItems
                ))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1partial-offline-cache-fill-failure",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        let failedButPlayableResult = try XCTUnwrap(
            model.playableTaskResults.first { $0.id == secondaryResultID }
        )
        XCTAssertTrue(model.canPlay(result: failedButPlayableResult))
        XCTAssertEqual(
            model.playableURL(for: failedButPlayableResult)?.absoluteString,
            "http://mac-mini.local:8080/hls/\(secondaryResultID)/master.m3u8"
        )
        XCTAssertEqual(model.taskResultSummary?.cachedCount, 1)
        XCTAssertEqual(model.taskResultSummary?.failedCount, 1)
        XCTAssertEqual(model.progressiveCacheStatusBadge?.label, "Upstream failed; 1 of 2 offline ready")
        XCTAssertEqual(model.progressiveCacheStatusBadge?.systemImage, "wifi.slash")
        XCTAssertFalse(model.isWatching)
    }

    func testCompletedMultiResultTaskDoesNotTreatPlanningRetryAsOfflineCacheRetry() async {
        let primaryLibraryItemID = "bilibili.hls.bilibili-playback-1"
        let resultItems: [BilibiliTaskResultItem] = [
            .fixture(
                id: "bilibili-playback-1",
                selectionID: "page:1",
                title: "Part 1",
                state: "TASK_STATE_COMPLETED",
                libraryItemID: primaryLibraryItemID
            ),
            .fixture(
                id: "bilibili-playback-1-result-2",
                selectionID: "page:2",
                title: "Part 2",
                index: 2,
                state: "TASK_STATE_FAILED",
                message:
                    "Selected Bilibili item no longer matches the resolved candidate. Resolve the input again and retry."
            ),
        ]
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(
                .fixture(
                    state: "TASK_STATE_COMPLETED",
                    progress: 1,
                    message: "Completed offline; some Bilibili playback results failed.",
                    libraryItemID: primaryLibraryItemID,
                    resultItems: resultItems
                ))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1planning-retry",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertEqual(model.taskResultSummary?.cachedCount, 1)
        XCTAssertEqual(model.taskResultSummary?.failedCount, 1)
        XCTAssertEqual(model.progressiveCacheStatusBadge?.label, "1 of 2 offline ready")
        XCTAssertEqual(model.progressiveCacheStatusBadge?.systemImage, "externaldrive.badge.checkmark")
    }

    func testMultiResultTaskShowsFailureStatusBeforeAnyResultIsReady() async {
        let resultItems: [BilibiliTaskResultItem] = [
            .fixture(
                id: "bilibili-playback-1",
                selectionID: "page:1",
                title: "Part 1",
                state: "TASK_STATE_FAILED",
                message: "Planning failed."
            ),
            .fixture(
                id: "bilibili-playback-1-result-2",
                selectionID: "page:2",
                title: "Part 2",
                index: 2,
                state: "TASK_STATE_RUNNING",
                message: "Preparing."
            ),
        ]
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(
                .fixture(
                    state: "TASK_STATE_RUNNING",
                    progress: 0.5,
                    message: "Preparing Bilibili playback results.",
                    resultItems: resultItems
                ))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1partial-failure",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertEqual(model.taskResultSummary?.readyCount, 0)
        XCTAssertEqual(model.taskResultSummary?.failedCount, 1)
        XCTAssertEqual(model.taskResultSummary?.pendingCount, 1)
        XCTAssertEqual(model.statusMessage, "No Bilibili results are ready; 1 failed; 1 still preparing.")
    }

    func testMultiResultTaskShowsMixedTerminalFailureStatus() async {
        let resultItems: [BilibiliTaskResultItem] = [
            .fixture(
                id: "bilibili-playback-1",
                selectionID: "page:1",
                title: "Part 1",
                state: "TASK_STATE_FAILED",
                message: "Planning failed."
            ),
            .fixture(
                id: "bilibili-playback-1-result-2",
                selectionID: "page:2",
                title: "Part 2",
                index: 2,
                state: "TASK_STATE_CANCELLED",
                message: "Cancelled."
            ),
        ]
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(
                .fixture(
                    state: "TASK_STATE_FAILED",
                    progress: 1,
                    message: "Bilibili playback results finished with errors.",
                    resultItems: resultItems
                ))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1mixed-terminal",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertEqual(model.taskResultSummary?.readyCount, 0)
        XCTAssertEqual(model.taskResultSummary?.failedCount, 1)
        XCTAssertEqual(model.taskResultSummary?.cancelledCount, 1)
        XCTAssertEqual(model.statusMessage, "No Bilibili results are ready; 1 failed; 1 cancelled.")
    }

    func testQuotaFailureShowsQuotaBlockedBadge() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(
                .fixture(
                    source: "BV1quota",
                    state: "TASK_STATE_FAILED",
                    message: "Cache quota blocked offline fill."
                ))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1quota",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertEqual(model.progressiveCacheStatusBadge?.label, "Quota blocked")
        XCTAssertEqual(model.progressiveCacheStatusBadge?.systemImage, "externaldrive.badge.xmark")
        XCTAssertNil(model.fetchNotice)
    }

    func testFailedTaskWithUpstreamFailureShowsUpstreamFailedBadge() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(
                .fixture(
                    source: "BV1upstream",
                    state: "TASK_STATE_FAILED",
                    message: "Offline cache fill failed: upstream failed."
                ))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1upstream",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertEqual(model.progressiveCacheStatusBadge?.label, "Upstream failed")
        XCTAssertEqual(model.progressiveCacheStatusBadge?.systemImage, "wifi.slash")
        XCTAssertEqual(model.fetchNotice?.title, "Retry available")
    }

    func testDuplicateSubmitWhileSubmittingDoesNotInvalidateInFlightSubmission() async {
        let client = FakeBilibiliCacheControlClient(
            createResponses: [],
            suspendsCreateResponses: true
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1slow",
            clientFactory: { _ in client }
        )

        let submitTask = Task {
            await model.submit(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForCreateRequestCount(1)
        XCTAssertTrue(model.isSubmitting)

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.completeNextCreate(with: .success(.fixture(source: "BV1slow", state: "TASK_STATE_PREPARING")))
        await submitTask.value

        XCTAssertFalse(model.isSubmitting)
        XCTAssertEqual(model.currentTask?.source, "BV1slow")
        let requests = await client.createdRequestsSnapshot()
        XCTAssertEqual(requests.map(\.urlOrID), ["BV1slow"])

        model.clearTask()
    }

    func testSubmittingNewTaskDisablesCancellingPreviousTask() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.playableFixture(id: "old-task", source: "BV1old"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1old",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        XCTAssertEqual(model.currentTask?.id, "old-task")
        XCTAssertTrue(model.canPlay)

        await client.setSuspendsCreateResponses(true)
        model.sourceText = "BV1new"
        let submitTask = Task {
            await model.submit(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForCreateRequestCount(2)
        XCTAssertTrue(model.isSubmitting)
        XCTAssertFalse(model.canCancel)
        XCTAssertFalse(model.canPlay)

        await model.cancel(serverAddressText: "mac-mini.local:50051")
        let cancelledIDs = await client.cancelledIDsSnapshot()
        XCTAssertEqual(cancelledIDs, [])

        await client.completeNextCreate(
            with: .success(.fixture(id: "new-task", source: "BV1new", state: "TASK_STATE_PREPARING")))
        await submitTask.value

        XCTAssertEqual(model.currentTask?.id, "new-task")
        XCTAssertFalse(model.isSubmitting)

        model.clearTask()
    }

    func testCompletedPlayableTaskShowsCachedStatus() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(source: "BV1done", state: "TASK_STATE_PREPARING"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1done",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.yield(
            .playableFixture(
                source: "BV1done",
                state: "TASK_STATE_COMPLETED",
                libraryItemID: "cached-bilibili-playback-1",
                playbackSourceItemID: "cached-bilibili-playback-1"
            )
        )
        await waitUntil(model.currentTask?.state == "TASK_STATE_COMPLETED")

        XCTAssertTrue(model.canPlay)
        XCTAssertEqual(model.statusMessage, "Ready video is cached for LAN playback.")

        model.clearTask()
    }

    func testDeletedCachedLibraryItemClearsMatchingCompletedTask() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(source: "BV1done", state: "TASK_STATE_PREPARING"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1done",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.yield(
            .playableFixture(
                source: "BV1done",
                state: "TASK_STATE_COMPLETED",
                libraryItemID: "cached-bilibili-playback-1",
                playbackSourceItemID: "cached-bilibili-playback-1"
            )
        )
        await waitUntil(model.currentTask?.state == "TASK_STATE_COMPLETED")

        XCTAssertTrue(model.canPlay)
        XCTAssertFalse(model.clearTaskIfCachedLibraryItemDeleted(id: "other-item"))
        XCTAssertNotNil(model.currentTask)

        XCTAssertTrue(model.clearTaskIfCachedLibraryItemDeleted(id: "cached-bilibili-playback-1"))
        XCTAssertNil(model.currentTask)
        XCTAssertFalse(model.canPlay)
        XCTAssertEqual(model.statusMessage, "No Bilibili playback task submitted.")
    }

    func testDeletedCachedLibraryItemClearsMatchingResultItem() async {
        let outputSummary = CacheTaskOutputSummary(
            revision: 7,
            resultCount: 2,
            terminalResultCount: 2,
            successfulResultCount: 2,
            failedResultCount: 0,
            cancelledResultCount: 0,
            availableArtifactCount: 3,
            primaryResultID: "bilibili-playback-1"
        )
        let resultItems: [BilibiliTaskResultItem] = [
            .fixture(
                id: "bilibili-playback-1",
                selectionID: "page:1",
                title: "Part 1",
                state: "TASK_STATE_COMPLETED",
                libraryItemID: "bilibili.hls.bilibili-playback-1"
            ),
            .fixture(
                id: "bilibili-playback-1-result-2",
                selectionID: "page:2",
                title: "Part 2",
                index: 2,
                state: "TASK_STATE_COMPLETED",
                libraryItemID: "bilibili.hls.bilibili-playback-1-result-2"
            ),
        ]
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(
                .fixture(
                    source: "BV1done",
                    state: "TASK_STATE_COMPLETED",
                    libraryItemID: "bilibili.hls.bilibili-playback-1",
                    resultItems: resultItems,
                    outputSummary: outputSummary
                ))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1done",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertFalse(model.clearTaskIfCachedLibraryItemDeleted(id: "other-item"))
        XCTAssertNotNil(model.currentTask)
        XCTAssertTrue(model.clearTaskIfCachedLibraryItemDeleted(id: "bilibili.hls.bilibili-playback-1-result-2"))
        XCTAssertNotNil(model.currentTask)
        XCTAssertTrue(model.canPlay)
        XCTAssertEqual(model.currentTask?.libraryItemID, "bilibili.hls.bilibili-playback-1")
        XCTAssertEqual(model.currentTask?.resultItems[0].state, "TASK_STATE_COMPLETED")
        XCTAssertEqual(model.currentTask?.resultItems[1].state, "TASK_STATE_FAILED")
        XCTAssertEqual(model.currentTask?.resultItems[1].message, "Cached Bilibili result was deleted.")
        XCTAssertEqual(model.currentTask?.resultItems[1].libraryItemID, "")
        XCTAssertNil(model.currentTask?.resultItems[1].playbackSource)
        XCTAssertNil(model.currentTask?.resultItems[1].playbackSession)
        XCTAssertEqual(model.currentTask?.outputSummary, outputSummary)
    }

    func testActivePlaybackLibraryItemMatchesCompletedTaskCache() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(
                .playableFixture(
                    source: "BV1done",
                    state: "TASK_STATE_COMPLETED",
                    libraryItemID: "cached-bilibili-playback-1",
                    playbackSourceItemID: "cached-bilibili-playback-1"
                )
            )
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1done",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertFalse(model.isActivePlaybackLibraryItem(id: "cached-bilibili-playback-1"))

        model.finishPreparedPlayback(didStartPlayback: true)

        XCTAssertTrue(model.isActivePlaybackLibraryItem(id: "cached-bilibili-playback-1"))
        XCTAssertFalse(model.isActivePlaybackLibraryItem(id: "other-item"))
        XCTAssertFalse(model.isActivePlaybackLibraryItem(id: ""))

        model.clearPlaybackStatus()

        XCTAssertFalse(model.isActivePlaybackLibraryItem(id: "cached-bilibili-playback-1"))

        model.clearTask()
    }

    func testActivePlaybackLibraryItemDoesNotMatchSiblingResultCache() async {
        let primaryLibraryItemID = "bilibili.hls.bilibili-playback-1"
        let siblingLibraryItemID = "bilibili.hls.bilibili-playback-1-result-2"
        let resultItems: [BilibiliTaskResultItem] = [
            .fixture(
                id: "bilibili-playback-1",
                selectionID: "page:1",
                title: "Part 1",
                state: "TASK_STATE_COMPLETED",
                libraryItemID: primaryLibraryItemID
            ),
            .fixture(
                id: "bilibili-playback-1-result-2",
                selectionID: "page:2",
                title: "Part 2",
                index: 2,
                state: "TASK_STATE_COMPLETED",
                libraryItemID: siblingLibraryItemID
            ),
        ]
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(
                .playableFixture(
                    source: "BV1done",
                    state: "TASK_STATE_COMPLETED",
                    libraryItemID: primaryLibraryItemID,
                    playbackSourceItemID: primaryLibraryItemID,
                    resultItems: resultItems
                )
            )
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1done",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertTrue(model.canPlay)
        model.finishPreparedPlayback(didStartPlayback: true)

        XCTAssertTrue(model.isActivePlaybackLibraryItem(id: primaryLibraryItemID))
        XCTAssertFalse(model.isActivePlaybackLibraryItem(id: siblingLibraryItemID))

        model.clearTask()
    }

    func testPlayableTaskRejectsMismatchedPlaybackSourceOwner() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(state: "TASK_STATE_PREPARING"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1wrong",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.yield(.playableFixture(source: "BV1wrong", playbackSourceItemID: "different-task"))
        await waitUntil(model.currentTask?.state == "TASK_STATE_PLAYABLE")

        XCTAssertNil(model.playableURL)
        XCTAssertFalse(model.canPlay)

        model.clearTask()
    }

    func testCompletedTaskRejectsMismatchedPlaybackSourceOwner() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(state: "TASK_STATE_PREPARING"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1cachedWrong",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.yield(
            .playableFixture(
                source: "BV1cachedWrong",
                state: "TASK_STATE_COMPLETED",
                libraryItemID: "cached-bilibili-playback-1",
                playbackSourceItemID: "different-library-item"
            )
        )
        await waitUntil(model.currentTask?.state == "TASK_STATE_COMPLETED")

        XCTAssertNil(model.playableURL)
        XCTAssertFalse(model.canPlay)
        XCTAssertEqual(model.statusMessage, "Ready video is cached for LAN playback.")

        model.clearTask()
    }

    func testWatchUpdateExposesPlayableURL() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(state: "TASK_STATE_PREPARING"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1ready",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.yield(.playableFixture(source: "BV1ready"))
        await waitUntil(model.currentTask?.state == "TASK_STATE_PLAYABLE")

        XCTAssertEqual(
            model.playableURL?.absoluteString,
            "http://mac-mini.local:8080/hls/bilibili-playback-1/master.m3u8"
        )
        XCTAssertTrue(model.canPlay)

        model.finishPreparedPlayback(didStartPlayback: true)
        XCTAssertEqual(model.statusMessage, "Playing Ready video.")

        model.clearTask()
    }

    func testTerminalWatchUpdateStopsWatching() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(state: "TASK_STATE_PREPARING"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1done",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.yield(.fixture(source: "BV1done", state: "TASK_STATE_COMPLETED"))
        await waitUntil(!model.isWatching)
        await client.waitForWatchTermination()

        XCTAssertFalse(model.isWatching)
        XCTAssertEqual(model.statusMessage, "Ready video is cached for LAN playback.")

        model.clearTask()
    }

    func testDefaultTaskWatchRecoverySleepReconcilesAndResubscribesOriginalTask() async {
        let taskID = "default-sleep-task"
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(id: taskID, source: "BV1default-sleep", state: "TASK_STATE_PREPARING"))
            ],
            getTaskResponses: [
                taskID: [
                    .success(
                        .fixture(id: taskID, source: "BV1default-sleep", state: "TASK_STATE_RUNNING", progress: 0.5))
                ]
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1default-sleep",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        let clock = ContinuousClock()
        let recoveryStarted = clock.now
        await client.failWatching(with: CacheTaskControlError.unavailable)
        await client.waitForGetTaskRequestCount(1)
        await client.waitForWatchSubscription(count: 2)
        let recoveryElapsed = recoveryStarted.duration(to: clock.now)

        let taskIDs = await client.getTaskIDsSnapshot()
        let subscriptionCount = await client.watchSubscriptionCountSnapshot()
        let createdTasks = await client.v2TaskRequestsSnapshot()
        XCTAssertGreaterThanOrEqual(recoveryElapsed, .seconds(1))
        XCTAssertEqual(taskIDs, [taskID])
        XCTAssertEqual(subscriptionCount, 2)
        XCTAssertEqual(createdTasks.count, 1)
        XCTAssertEqual(model.currentTask?.id, taskID)
        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_RUNNING")
        XCTAssertTrue(model.isWatching)

        model.clearTask()
        let cancelledTaskIDs = await client.cancelledIDsSnapshot()
        XCTAssertTrue(cancelledTaskIDs.isEmpty)
    }

    func testTaskWatchRecoveryBackoffIgnoresHeartbeatAndResetsOnProgress() async {
        let taskID = "bilibili-playback-1"
        let sleeper = TaskWatchRecoverySleepRecorder()
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1watch-recovery", state: "TASK_STATE_PREPARING"))
            ],
            getTaskResponses: [
                taskID: [
                    .failure(CacheTaskControlError.unavailable),
                    .success(
                        .fixture(
                            source: "BV1watch-recovery",
                            state: "TASK_STATE_RUNNING",
                            progress: 0.6
                        )),
                    .failure(CacheTaskControlError.notFound),
                ]
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1watch-recovery",
            watchRecoverySleep: { delay in await sleeper.record(delay) },
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.yield(
            .fixture(
                source: "BV1watch-recovery",
                state: "TASK_STATE_PREPARING",
                progress: 0.25,
                message: "Heartbeat only."
            ))
        await waitUntil(model.currentTask?.message == "Heartbeat only.")
        await client.failWatching(with: CacheTaskControlError.unavailable)

        await sleeper.waitForCount(2)
        await client.waitForWatchSubscription(count: 2)
        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_RUNNING")

        await client.failWatching(with: CacheTaskControlError.unavailable)
        await sleeper.waitForCount(3)
        await waitUntil(!model.isWatching)

        let delays = await sleeper.delaysSnapshot()
        XCTAssertEqual(delays, [.seconds(1), .seconds(2), .seconds(1)])
        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_RUNNING")
        XCTAssertNotNil(model.watchErrorMessage)
        XCTAssertTrue(model.canRetryTaskUpdates)

        model.clearTask()
    }

    func testCleanTaskWatchEOFReconcilesAndResubscribesWithoutCreatingTask() async {
        let taskID = "bilibili-playback-1"
        let sleeper = TaskWatchRecoverySleepRecorder()
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1clean-eof", state: "TASK_STATE_PREPARING"))
            ],
            getTaskResponses: [
                taskID: [
                    .success(.fixture(source: "BV1clean-eof", state: "TASK_STATE_RUNNING", progress: 0.4))
                ]
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1clean-eof",
            watchRecoverySleep: { delay in await sleeper.record(delay) },
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.finishWatching()

        await sleeper.waitForCount(1)
        await client.waitForGetTaskRequestCount(1)
        await client.waitForWatchSubscription(count: 2)

        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_RUNNING")
        let taskIDs = await client.getTaskIDsSnapshot()
        let createdTasks = await client.v2TaskRequestsSnapshot()
        XCTAssertEqual(taskIDs, [taskID])
        XCTAssertEqual(createdTasks.count, 1)

        model.clearTask()
    }

    func testRepeatedHeartbeatSnapshotsAndEOFDoNotResetSixAttemptBudget() async {
        let taskID = "bilibili-playback-1"
        let sleeper = TaskWatchRecoverySleepRecorder()
        let reconciledSnapshot = CacheTask.fixture(
            source: "BV1eof-budget",
            state: "TASK_STATE_PREPARING",
            progress: 0.25,
            message: "Reconcile heartbeat."
        )
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1eof-budget", state: "TASK_STATE_PREPARING"))
            ],
            getTaskResponses: [
                taskID: Array(repeating: .success(reconciledSnapshot), count: 6)
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1eof-budget",
            watchRecoverySleep: { delay in await sleeper.record(delay) },
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()

        for attempt in 1...6 {
            await client.yield(
                .fixture(
                    source: "BV1eof-budget",
                    state: "TASK_STATE_PREPARING",
                    progress: 0.25,
                    message: "Watch heartbeat \(attempt)."
                ))
            await client.finishWatching()
            await sleeper.waitForCount(attempt)
            await client.waitForGetTaskRequestCount(attempt)
            await client.waitForWatchSubscription(count: attempt + 1)
        }

        await client.yield(
            .fixture(
                source: "BV1eof-budget",
                state: "TASK_STATE_PREPARING",
                progress: 0.25,
                message: "Final heartbeat."
            ))
        await client.finishWatching()
        await waitUntil(!model.isWatching)

        XCTAssertTrue(model.canRetryTaskUpdates)
        let delays = await sleeper.delaysSnapshot()
        let taskIDs = await client.getTaskIDsSnapshot()
        let createdTasks = await client.v2TaskRequestsSnapshot()
        let subscriptionCount = await client.watchSubscriptionCountSnapshot()
        XCTAssertEqual(delays, [.seconds(1), .seconds(2), .seconds(4), .seconds(8), .seconds(16), .seconds(16)])
        XCTAssertEqual(taskIDs, Array(repeating: taskID, count: 6))
        XCTAssertEqual(createdTasks.count, 1)
        XCTAssertEqual(subscriptionCount, 7)

        model.clearTask()
    }

    func testTaskWatchRecoveryStopsAfterSixAttemptsAndAllowsManualRetry() async {
        let taskID = "bilibili-playback-1"
        let sleeper = TaskWatchRecoverySleepRecorder()
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1watch-budget", state: "TASK_STATE_PREPARING"))
            ],
            getTaskResponses: [
                taskID: Array(repeating: .failure(CacheTaskControlError.unavailable), count: 6)
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1watch-budget",
            watchRecoverySleep: { delay in await sleeper.record(delay) },
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.failWatching(with: CacheTaskControlError.unavailable)
        await sleeper.waitForCount(6)
        await waitUntil(!model.isWatching)

        XCTAssertTrue(model.canRetryTaskUpdates)
        let recoveryDelays = await sleeper.delaysSnapshot()
        let reconciledTaskIDs = await client.getTaskIDsSnapshot()
        XCTAssertEqual(
            recoveryDelays,
            [.seconds(1), .seconds(2), .seconds(4), .seconds(8), .seconds(16), .seconds(16)]
        )
        XCTAssertEqual(reconciledTaskIDs, Array(repeating: taskID, count: 6))

        model.clearTask()
    }

    func testSilentWatchReconciliationAppliesOpenOutputAndTerminalSnapshots() async {
        let taskID = "bilibili-playback-1"
        let sleeper = TaskWatchReconciliationSleepGate()
        let outputSummary = CacheTaskOutputSummary(
            revision: 1,
            resultCount: 1,
            terminalResultCount: 1,
            successfulResultCount: 1,
            failedResultCount: 0,
            cancelledResultCount: 0,
            availableArtifactCount: 0,
            primaryResultID: "silent-result"
        )
        let resultPage = CacheTaskResultsPage(
            results: [.fixture(id: "silent-result", title: "Silent result")],
            pageInfo: CachePageInfo(totalSize: 1, nextPageToken: "", snapshotID: "silent-snapshot"),
            outputRevision: 1
        )
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1silent-output", state: "TASK_STATE_PREPARING"))
            ],
            getTaskResponses: [
                taskID: [
                    .success(
                        .fixture(
                            source: "BV1silent-output",
                            state: "TASK_STATE_RUNNING",
                            progress: 0.7,
                            outputSummary: outputSummary
                        )),
                    .success(
                        .fixture(
                            source: "BV1silent-output",
                            state: "TASK_STATE_COMPLETED",
                            progress: 1,
                            outputSummary: outputSummary
                        )),
                ]
            ],
            taskResultPagesByTaskID: [taskID: [resultPage]]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1silent-output",
            watchReconciliationSleep: { delay in try await sleeper.sleep(delay) },
            watchReconciliationNow: { sleeper.clock.now() },
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await sleeper.waitForSleepCount(1)
        await sleeper.advanceNext()
        await client.waitForGetTaskRequestCount(1)
        await waitUntil(model.currentTask?.state == "TASK_STATE_RUNNING")
        await client.waitForTaskResultRequestCount(1)
        await waitUntil(model.taskResults.map(\.id) == ["silent-result"])

        await sleeper.waitForSleepCount(2)
        await sleeper.advanceNext()
        await client.waitForGetTaskRequestCount(2)
        await waitUntil(!model.isWatching)

        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_COMPLETED")
        XCTAssertEqual(model.currentTask?.outputSummary?.revision, 1)
        XCTAssertEqual(model.taskResults.map(\.id), ["silent-result"])
        XCTAssertEqual(model.statusMessage, "Ready video is cached for LAN playback.")
        XCTAssertNil(model.watchErrorMessage)

        model.clearTask()
    }

    func testSilentWatchReconciliationKeepsHealthyIdenticalSnapshotWithoutResubscribing() async {
        let taskID = "bilibili-playback-1"
        let sleeper = TaskWatchReconciliationSleepGate()
        let startedAt = sleeper.clock.now()
        let initialTask = CacheTask.fixture(source: "BV1silent-healthy", state: "TASK_STATE_PREPARING")
        let client = FakeBilibiliCacheControlClient(
            createResponses: [.success(initialTask)],
            getTaskResponses: [taskID: Array(repeating: .success(initialTask), count: 3)]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1silent-healthy",
            watchReconciliationSleep: { delay in try await sleeper.sleep(delay) },
            watchReconciliationNow: { sleeper.clock.now() },
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()

        for tick in 1...3 {
            await sleeper.waitForSleepCount(tick)
            await sleeper.advanceNext()
            await client.waitForGetTaskRequestCount(tick)
            await sleeper.waitForSleepCount(tick + 1)
        }

        let taskIDs = await client.getTaskIDsSnapshot()
        let subscriptionCount = await client.watchSubscriptionCountSnapshot()
        let submittedTasks = await client.v2TaskRequestsSnapshot()
        let intervals = await sleeper.intervalsSnapshot()
        XCTAssertEqual(taskIDs, Array(repeating: taskID, count: 3))
        XCTAssertEqual(subscriptionCount, 1)
        XCTAssertEqual(submittedTasks.count, 1)
        XCTAssertEqual(intervals, Array(repeating: .seconds(30), count: 4))
        XCTAssertEqual(startedAt.duration(to: sleeper.clock.now()), .seconds(90))
        XCTAssertEqual(model.currentTask, initialTask)
        XCTAssertTrue(model.isWatching)
        XCTAssertFalse(model.isReconnecting)
        XCTAssertNil(model.watchErrorMessage)
        XCTAssertNil(model.errorMessage)

        model.clearTask()
    }

    func testImmediateInitialWatchSnapshotProbesAfterOneQuietInterval() async {
        let taskID = "bilibili-playback-1"
        let sleeper = TaskWatchReconciliationSleepGate()
        let startedAt = sleeper.clock.now()
        let initialTask = CacheTask.fixture(source: "BV1identical-activity", state: "TASK_STATE_PREPARING")
        let client = FakeBilibiliCacheControlClient(
            createResponses: [.success(initialTask)],
            getTaskResponses: [taskID: [.success(initialTask)]]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1identical-activity",
            watchReconciliationSleep: { delay in try await sleeper.sleep(delay) },
            watchReconciliationNow: { sleeper.clock.now() },
            clientFactory: { _ in client }
        )
        defer { model.clearTask() }

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await sleeper.waitForSleepCount(1)

        let snapshotApplied = expectation(description: "identical watch snapshot accepted")
        snapshotApplied.assertForOverFulfill = false
        let observation = model.objectWillChange.sink {
            snapshotApplied.fulfill()
        }
        await client.yield(initialTask)
        await fulfillment(of: [snapshotApplied], timeout: 1)
        observation.cancel()

        await sleeper.advanceNext()
        let probeArrived = await waitForTaskWatchCondition(timeout: .seconds(1)) {
            (await client.getTaskIDsSnapshot()).count >= 1
        }
        XCTAssertTrue(probeArrived, "Expected the first idle read after one quiet interval.")
        guard probeArrived else { return }
        await sleeper.waitForSleepCount(2)
        let taskIDs = await client.getTaskIDsSnapshot()
        let intervals = await sleeper.intervalsSnapshot()
        let subscriptionCount = await client.watchSubscriptionCountSnapshot()
        let createdTasks = await client.v2TaskRequestsSnapshot()
        let cancelledTaskIDs = await client.cancelledIDsSnapshot()
        XCTAssertEqual(taskIDs, [taskID])
        XCTAssertEqual(intervals, [.seconds(30), .seconds(30)])
        XCTAssertEqual(startedAt.duration(to: sleeper.clock.now()), .seconds(30))
        XCTAssertEqual(model.currentTask, initialTask)
        XCTAssertEqual(subscriptionCount, 1)
        XCTAssertEqual(createdTasks.count, 1)
        XCTAssertTrue(cancelledTaskIDs.isEmpty)
        XCTAssertTrue(model.isWatching)
        XCTAssertNil(model.watchErrorMessage)
    }

    func testIdenticalWatchActivityNearDeadlineSleepsOnlyRemainingQuietTime() async {
        let taskID = "bilibili-playback-1"
        let sleeper = TaskWatchReconciliationSleepGate()
        let startedAt = sleeper.clock.now()
        let initialTask = CacheTask.fixture(source: "BV1near-deadline", state: "TASK_STATE_PREPARING")
        let client = FakeBilibiliCacheControlClient(
            createResponses: [.success(initialTask)],
            getTaskResponses: [taskID: [.success(initialTask), .success(initialTask)]]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1near-deadline",
            watchReconciliationSleep: { delay in try await sleeper.sleep(delay) },
            watchReconciliationNow: { sleeper.clock.now() },
            clientFactory: { _ in client }
        )
        defer { model.clearTask() }

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await sleeper.waitForSleepCount(1)
        sleeper.clock.advance(by: .seconds(29))

        let firstHeartbeat = expectation(description: "identical heartbeat at 29 seconds accepted")
        firstHeartbeat.assertForOverFulfill = false
        let firstObservation = model.objectWillChange.sink { firstHeartbeat.fulfill() }
        await client.yield(initialTask)
        await fulfillment(of: [firstHeartbeat], timeout: 1)
        firstObservation.cancel()

        await sleeper.advanceNext()
        await sleeper.waitForSleepCount(2)
        var taskIDs = await client.getTaskIDsSnapshot()
        var intervals = await sleeper.intervalsSnapshot()
        XCTAssertTrue(taskIDs.isEmpty)
        XCTAssertEqual(intervals, [.seconds(30), .seconds(29)])
        XCTAssertEqual(startedAt.duration(to: sleeper.clock.now()), .seconds(30))

        await sleeper.advanceNext()
        let firstProbeArrived = await waitForTaskWatchCondition(timeout: .seconds(1)) {
            (await client.getTaskIDsSnapshot()).count >= 1
        }
        XCTAssertTrue(firstProbeArrived, "Expected the idle read at 59 virtual seconds.")
        guard firstProbeArrived else { return }
        await sleeper.waitForSleepCount(3)
        XCTAssertEqual(startedAt.duration(to: sleeper.clock.now()), .seconds(59))

        sleeper.clock.advance(by: .seconds(29))
        let secondHeartbeat = expectation(description: "repeated identical heartbeat at 88 seconds accepted")
        secondHeartbeat.assertForOverFulfill = false
        let secondObservation = model.objectWillChange.sink { secondHeartbeat.fulfill() }
        await client.yield(initialTask)
        await fulfillment(of: [secondHeartbeat], timeout: 1)
        secondObservation.cancel()

        await sleeper.advanceNext()
        await sleeper.waitForSleepCount(4)
        taskIDs = await client.getTaskIDsSnapshot()
        intervals = await sleeper.intervalsSnapshot()
        XCTAssertEqual(taskIDs, [taskID])
        XCTAssertEqual(intervals, [.seconds(30), .seconds(29), .seconds(30), .seconds(29)])

        await sleeper.advanceNext()
        let secondProbeArrived = await waitForTaskWatchCondition(timeout: .seconds(1)) {
            (await client.getTaskIDsSnapshot()).count >= 2
        }
        XCTAssertTrue(secondProbeArrived, "Expected the idle read after repeated identical activity.")
        guard secondProbeArrived else { return }
        await sleeper.waitForSleepCount(5)
        taskIDs = await client.getTaskIDsSnapshot()
        intervals = await sleeper.intervalsSnapshot()
        let subscriptionCount = await client.watchSubscriptionCountSnapshot()
        let createdTasks = await client.v2TaskRequestsSnapshot()
        let cancelledTaskIDs = await client.cancelledIDsSnapshot()
        XCTAssertEqual(startedAt.duration(to: sleeper.clock.now()), .seconds(118))
        XCTAssertEqual(taskIDs, [taskID, taskID])
        XCTAssertEqual(intervals, [.seconds(30), .seconds(29), .seconds(30), .seconds(29), .seconds(30)])
        XCTAssertEqual(model.currentTask, initialTask)
        XCTAssertEqual(subscriptionCount, 1)
        XCTAssertEqual(createdTasks.count, 1)
        XCTAssertTrue(cancelledTaskIDs.isEmpty)
        XCTAssertTrue(model.isWatching)
        XCTAssertNil(model.watchErrorMessage)
    }

    func testSuspendedIdleProbeDoesNotOverlapAnotherTick() async {
        let sleeper = TaskWatchReconciliationSleepGate()
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1singleflight", state: "TASK_STATE_PREPARING"))
            ],
            suspendsGetTaskResponses: true
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1singleflight",
            watchReconciliationSleep: { delay in try await sleeper.sleep(delay) },
            watchReconciliationNow: { sleeper.clock.now() },
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await sleeper.waitForSleepCount(1)
        await sleeper.advanceNext()
        await client.waitForGetTaskRequestCount(1)

        await sleeper.advanceNext()
        let taskIDsWhileSuspended = await client.getTaskIDsSnapshot()
        let sleepsWhileSuspended = await sleeper.sleepCountSnapshot()
        XCTAssertEqual(taskIDsWhileSuspended.count, 1)
        XCTAssertEqual(sleepsWhileSuspended, 1)

        await client.completeNextGetTask(
            with: .success(.fixture(source: "BV1singleflight", state: "TASK_STATE_RUNNING", progress: 0.5)))
        await client.waitForGetTaskResponseCompletionCount(1)
        await sleeper.waitForSleepCount(2)

        model.clearTask()
    }

    func testLateIdleProbeSuccessAndFailureAreFencedByAcceptedWatchActivity() async {
        let taskID = "bilibili-playback-1"
        let sleeper = TaskWatchReconciliationSleepGate()
        let initialTask = CacheTask.fixture(source: "BV1idle-race", state: "TASK_STATE_PREPARING")
        let client = FakeBilibiliCacheControlClient(
            createResponses: [.success(initialTask)],
            suspendsGetTaskResponses: true
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1idle-race",
            watchReconciliationSleep: { delay in try await sleeper.sleep(delay) },
            watchReconciliationNow: { sleeper.clock.now() },
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await sleeper.waitForSleepCount(1)
        await sleeper.advanceNext()
        await client.waitForGetTaskRequestCount(1)

        let heartbeatAccepted = expectation(description: "identical watch heartbeat accepted")
        heartbeatAccepted.assertForOverFulfill = false
        let observation = model.objectWillChange.sink {
            heartbeatAccepted.fulfill()
        }
        await client.yield(initialTask)
        await fulfillment(of: [heartbeatAccepted], timeout: 1)
        observation.cancel()
        await client.completeNextGetTask(
            with: .success(
                .fixture(source: "BV1idle-race", state: "TASK_STATE_RUNNING", progress: 0.2, message: "Late probe.")))
        await client.waitForGetTaskResponseCompletionCount(1)
        await sleeper.waitForSleepCount(2)
        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_PREPARING")
        XCTAssertNil(model.watchErrorMessage)

        await sleeper.advanceNext()
        await client.waitForGetTaskRequestCount(2)
        await client.yield(
            .fixture(source: "BV1idle-race", state: "TASK_STATE_RUNNING", progress: 0.8, message: "Newer watch."))
        await waitUntil(model.currentTask?.message == "Newer watch.")
        await client.completeNextGetTask(with: .failure(CacheTaskControlError.permissionDenied))
        await client.waitForGetTaskResponseCompletionCount(2)
        await sleeper.waitForSleepCount(3)

        let taskIDs = await client.getTaskIDsSnapshot()
        let subscriptionCount = await client.watchSubscriptionCountSnapshot()
        XCTAssertEqual(taskIDs, [taskID, taskID])
        XCTAssertEqual(subscriptionCount, 1)
        XCTAssertTrue(model.isWatching)
        XCTAssertNil(model.watchErrorMessage)
        XCTAssertEqual(model.currentTask?.message, "Newer watch.")

        model.clearTask()
    }

    func testLateIdleProbeCannotOverwriteCancellationAndObservationResumesAfterward() async {
        let sleeper = TaskWatchReconciliationSleepGate()
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1idle-cancel", state: "TASK_STATE_PREPARING"))
            ],
            suspendsGetTaskResponses: true
        )
        await client.setSuspendsCancelResponses(true)
        let model = BilibiliTaskViewModel(
            sourceText: "BV1idle-cancel",
            watchReconciliationSleep: { delay in try await sleeper.sleep(delay) },
            watchReconciliationNow: { sleeper.clock.now() },
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await sleeper.waitForSleepCount(1)
        await sleeper.advanceNext()
        await client.waitForGetTaskRequestCount(1)

        let cancelTask = Task {
            await model.cancel(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForCancelRequestCount(1)
        XCTAssertTrue(model.isCancelling)
        await client.completeNextGetTask(
            with: .success(.fixture(source: "BV1idle-cancel", state: "TASK_STATE_RUNNING", progress: 0.9)))
        await client.waitForGetTaskResponseCompletionCount(1)
        await sleeper.waitForSleepCount(2)
        await sleeper.advanceNext()
        await sleeper.waitForSleepCount(3)
        let taskIDsWhileCancelling = await client.getTaskIDsSnapshot()

        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_PREPARING")
        XCTAssertEqual(model.statusMessage, "Cancelling Ready video...")
        XCTAssertTrue(model.isCancelling)
        XCTAssertEqual(taskIDsWhileCancelling, ["bilibili-playback-1"])

        await client.completeNextCancel(
            with: .success(.fixture(source: "BV1idle-cancel", state: "TASK_STATE_CANCEL_REQUESTED")))
        await cancelTask.value
        XCTAssertFalse(model.isCancelling)

        await sleeper.advanceNext()
        await client.waitForGetTaskRequestCount(2)
        await client.completeNextGetTask(
            with: .success(.fixture(source: "BV1idle-cancel", state: "TASK_STATE_CANCEL_REQUESTED")))
        await client.waitForGetTaskResponseCompletionCount(2)
        await sleeper.waitForSleepCount(4)
        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_CANCEL_REQUESTED")
        XCTAssertTrue(model.isWatching)

        model.clearTask()
    }

    func testIdenticalWatchHeartbeatDuringCancelDoesNotInvalidateCancelResponse() async {
        let initialTask = CacheTask.fixture(source: "BV1cancel-heartbeat", state: "TASK_STATE_PREPARING")
        let client = FakeBilibiliCacheControlClient(createResponses: [.success(initialTask)])
        await client.setSuspendsCancelResponses(true)
        let model = BilibiliTaskViewModel(
            sourceText: "BV1cancel-heartbeat",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        let cancelTask = Task {
            await model.cancel(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForCancelRequestCount(1)

        let heartbeatAccepted = expectation(description: "identical heartbeat accepted while cancelling")
        heartbeatAccepted.assertForOverFulfill = false
        let observation = model.objectWillChange.sink {
            heartbeatAccepted.fulfill()
        }
        await client.yield(initialTask)
        await fulfillment(of: [heartbeatAccepted], timeout: 1)
        observation.cancel()

        XCTAssertTrue(model.isCancelling)
        XCTAssertNil(model.errorMessage)
        await client.completeNextCancel(with: .success(initialTask))
        await cancelTask.value

        XCTAssertFalse(model.isCancelling)
        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(model.currentTask, initialTask)
        XCTAssertTrue(model.isWatching)

        model.clearTask()
    }

    func testRetryableIdleProbeFailureHandsOffToBoundedRecovery() async {
        let taskID = "bilibili-playback-1"
        let sleeper = TaskWatchReconciliationSleepGate()
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1idle-retry", state: "TASK_STATE_PREPARING"))
            ],
            getTaskResponses: [
                taskID: [
                    .failure(CacheTaskControlError.unavailable),
                    .success(.fixture(source: "BV1idle-retry", state: "TASK_STATE_RUNNING", progress: 0.5)),
                ]
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1idle-retry",
            watchReconciliationSleep: { delay in try await sleeper.sleep(delay) },
            watchReconciliationNow: { sleeper.clock.now() },
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await sleeper.waitForSleepCount(1)
        await sleeper.advanceNext()
        await client.waitForGetTaskRequestCount(2)
        await client.waitForWatchSubscription(count: 2)

        let taskIDs = await client.getTaskIDsSnapshot()
        let subscriptionCount = await client.watchSubscriptionCountSnapshot()
        let submittedTasks = await client.v2TaskRequestsSnapshot()
        XCTAssertEqual(taskIDs, [taskID, taskID])
        XCTAssertEqual(subscriptionCount, 2)
        XCTAssertEqual(submittedTasks.count, 1)
        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_RUNNING")
        XCTAssertTrue(model.isWatching)
        XCTAssertNil(model.watchErrorMessage)

        model.clearTask()
    }

    func testRecoveredSilentStreamClearsReconnectPresentationAfterHealthyIdleRead() async {
        let taskID = "bilibili-playback-1"
        let recoverySleeper = TaskWatchRecoverySleepRecorder()
        let reconciliationSleeper = TaskWatchReconciliationSleepGate()
        let initialTask = CacheTask.fixture(source: "BV1recover-silent", state: "TASK_STATE_PREPARING")
        let client = FakeBilibiliCacheControlClient(
            createResponses: [.success(initialTask)],
            getTaskResponses: [taskID: [.success(initialTask), .success(initialTask)]]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1recover-silent",
            watchReconciliationSleep: { delay in try await reconciliationSleeper.sleep(delay) },
            watchRecoverySleep: { delay in await recoverySleeper.record(delay) },
            watchReconciliationNow: { reconciliationSleeper.clock.now() },
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.failWatching(with: CacheTaskControlError.unavailable)
        await recoverySleeper.waitForCount(1)
        await client.waitForGetTaskRequestCount(1)
        await client.waitForWatchSubscription(count: 2)
        await reconciliationSleeper.waitForSleepCount(1)

        XCTAssertTrue(model.isReconnecting)
        XCTAssertNotNil(model.watchErrorMessage)

        await reconciliationSleeper.waitForActiveSleeper()
        await reconciliationSleeper.advanceNext()
        await client.waitForGetTaskRequestCount(2)
        await reconciliationSleeper.waitForSleepCount(2)
        await waitUntil(!model.isReconnecting)

        let recoveryDelays = await recoverySleeper.delaysSnapshot()
        let subscriptionCount = await client.watchSubscriptionCountSnapshot()
        XCTAssertEqual(recoveryDelays, [.seconds(1)])
        XCTAssertEqual(subscriptionCount, 2)
        XCTAssertTrue(model.isWatching)
        XCTAssertFalse(model.isReconnecting)
        XCTAssertNil(model.watchErrorMessage)

        model.clearTask()
    }

    func testUnchangedIdleReadsAndFailuresDoNotResetRecoveryBudget() async {
        let taskID = "bilibili-playback-1"
        let recoverySleeper = TaskWatchRecoverySleepRecorder()
        let reconciliationSleeper = TaskWatchReconciliationSleepGate()
        let initialTask = CacheTask.fixture(source: "BV1idle-budget", state: "TASK_STATE_PREPARING")
        let client = FakeBilibiliCacheControlClient(
            createResponses: [.success(initialTask)],
            getTaskResponses: [
                taskID: [
                    .success(initialTask),
                    .success(initialTask),
                    .failure(CacheTaskControlError.unavailable),
                    .success(initialTask),
                    .success(initialTask),
                ]
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1idle-budget",
            watchReconciliationSleep: { delay in try await reconciliationSleeper.sleep(delay) },
            watchRecoverySleep: { delay in await recoverySleeper.record(delay) },
            watchReconciliationNow: { reconciliationSleeper.clock.now() },
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.failWatching(with: CacheTaskControlError.unavailable)
        await recoverySleeper.waitForCount(1)
        await client.waitForGetTaskRequestCount(1)
        await client.waitForWatchSubscription(count: 2)
        await reconciliationSleeper.waitForSleepCount(1)

        await reconciliationSleeper.waitForActiveSleeper()
        await reconciliationSleeper.advanceNext()
        await client.waitForGetTaskRequestCount(2)
        await reconciliationSleeper.waitForSleepCount(2)
        await reconciliationSleeper.waitForActiveSleeper()
        await reconciliationSleeper.advanceNext()
        await client.waitForGetTaskRequestCount(4)
        await client.waitForWatchSubscription(count: 3)
        await reconciliationSleeper.waitForSleepCount(3)

        await client.failWatching(with: CacheTaskControlError.unavailable)
        await recoverySleeper.waitForCount(2)
        await client.waitForGetTaskRequestCount(5)
        await client.waitForWatchSubscription(count: 4)

        let recoveryDelays = await recoverySleeper.delaysSnapshot()
        let taskIDs = await client.getTaskIDsSnapshot()
        let subscriptionCount = await client.watchSubscriptionCountSnapshot()
        let submittedTasks = await client.v2TaskRequestsSnapshot()
        XCTAssertEqual(recoveryDelays, [.seconds(1), .seconds(4)])
        XCTAssertEqual(taskIDs, Array(repeating: taskID, count: 5))
        XCTAssertEqual(subscriptionCount, 4)
        XCTAssertEqual(submittedTasks.count, 1)
        XCTAssertTrue(model.isWatching)

        model.clearTask()
    }

    func testRepeatedIdleFailuresExhaustBudgetDespiteUnchangedSuccesses() async {
        let taskID = "bilibili-playback-1"
        let recoverySleeper = TaskWatchRecoverySleepRecorder()
        let reconciliationSleeper = TaskWatchReconciliationSleepGate()
        let initialTask = CacheTask.fixture(source: "BV1idle-budget-exhaustion", state: "TASK_STATE_PREPARING")
        var responses: [Result<CacheTask, Error>] = [.success(initialTask)]
        for _ in 1...5 {
            responses.append(.success(initialTask))
            responses.append(.failure(CacheTaskControlError.unavailable))
            responses.append(.success(initialTask))
        }
        responses.append(.failure(CacheTaskControlError.unavailable))
        let client = FakeBilibiliCacheControlClient(
            createResponses: [.success(initialTask)],
            getTaskResponses: [taskID: responses]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1idle-budget-exhaustion",
            watchReconciliationSleep: { delay in try await reconciliationSleeper.sleep(delay) },
            watchRecoverySleep: { delay in await recoverySleeper.record(delay) },
            watchReconciliationNow: { reconciliationSleeper.clock.now() },
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await reconciliationSleeper.waitForSleepCount(1)
        await client.failWatching(with: CacheTaskControlError.unavailable)
        await recoverySleeper.waitForCount(1)
        await client.waitForGetTaskRequestCount(1)
        await client.waitForWatchSubscription(count: 2)
        await reconciliationSleeper.waitForSleepCount(2)

        var requestCount = 1
        var sleepCount = 2
        var subscriptionCount = 2
        for _ in 1...5 {
            await reconciliationSleeper.waitForActiveSleeper()
            await reconciliationSleeper.advanceNext()
            requestCount += 1
            await client.waitForGetTaskRequestCount(requestCount)
            sleepCount += 1
            await reconciliationSleeper.waitForSleepCount(sleepCount)
            await reconciliationSleeper.waitForActiveSleeper()

            await reconciliationSleeper.advanceNext()
            requestCount += 1
            await client.waitForGetTaskRequestCount(requestCount)
            requestCount += 1
            subscriptionCount += 1
            await client.waitForGetTaskRequestCount(requestCount)
            await client.waitForWatchSubscription(count: subscriptionCount)
            sleepCount += 1
            await reconciliationSleeper.waitForSleepCount(sleepCount)
            await reconciliationSleeper.waitForActiveSleeper()
        }
        await reconciliationSleeper.advanceNext()
        requestCount += 1
        await client.waitForGetTaskRequestCount(requestCount)
        await waitUntil(!model.isWatching)

        let recoveryDelays = await recoverySleeper.delaysSnapshot()
        let taskIDs = await client.getTaskIDsSnapshot()
        let finalSubscriptionCount = await client.watchSubscriptionCountSnapshot()
        XCTAssertEqual(recoveryDelays, [.seconds(1)])
        XCTAssertEqual(taskIDs.count, 17)
        XCTAssertEqual(finalSubscriptionCount, 7)
        XCTAssertTrue(model.canRetryTaskUpdates)
        XCTAssertEqual(
            model.watchErrorMessage,
            "Task updates could not be restored. Retry to resume observation."
        )

        model.clearTask()
    }

    func testAcceptedIdleProgressResetsRecoveryBudget() async {
        let taskID = "bilibili-playback-1"
        let recoverySleeper = TaskWatchRecoverySleepRecorder()
        let reconciliationSleeper = TaskWatchReconciliationSleepGate()
        let initialTask = CacheTask.fixture(source: "BV1idle-progress", state: "TASK_STATE_PREPARING")
        let progressedTask = CacheTask.fixture(
            source: "BV1idle-progress",
            state: "TASK_STATE_RUNNING",
            progress: 0.5
        )
        let client = FakeBilibiliCacheControlClient(
            createResponses: [.success(initialTask)],
            getTaskResponses: [taskID: [.success(initialTask), .success(progressedTask), .success(progressedTask)]]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1idle-progress",
            watchReconciliationSleep: { delay in try await reconciliationSleeper.sleep(delay) },
            watchRecoverySleep: { delay in await recoverySleeper.record(delay) },
            watchReconciliationNow: { reconciliationSleeper.clock.now() },
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.failWatching(with: CacheTaskControlError.unavailable)
        await recoverySleeper.waitForCount(1)
        await client.waitForGetTaskRequestCount(1)
        await client.waitForWatchSubscription(count: 2)
        await reconciliationSleeper.waitForSleepCount(1)

        await reconciliationSleeper.waitForActiveSleeper()
        await reconciliationSleeper.advanceNext()
        await client.waitForGetTaskRequestCount(2)
        await waitUntil(model.currentTask?.state == "TASK_STATE_RUNNING")
        await reconciliationSleeper.waitForSleepCount(2)
        await client.failWatching(with: CacheTaskControlError.unavailable)
        await recoverySleeper.waitForCount(2)
        await client.waitForGetTaskRequestCount(3)
        await client.waitForWatchSubscription(count: 3)

        let recoveryDelays = await recoverySleeper.delaysSnapshot()
        XCTAssertEqual(recoveryDelays, [.seconds(1), .seconds(1)])
        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_RUNNING")
        XCTAssertTrue(model.isWatching)

        model.clearTask()
    }

    func testFatalIdleProbeFailureStopsAndCleansUpObservation() async {
        let sleeper = TaskWatchReconciliationSleepGate()
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1idle-fatal", state: "TASK_STATE_PREPARING"))
            ],
            getTaskResponses: [
                "bilibili-playback-1": [.failure(CacheTaskControlError.permissionDenied)]
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1idle-fatal",
            watchReconciliationSleep: { delay in try await sleeper.sleep(delay) },
            watchReconciliationNow: { sleeper.clock.now() },
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await sleeper.waitForSleepCount(1)
        await sleeper.advanceNext()
        await waitUntil(!model.isWatching)
        await client.waitForWatchTermination()

        XCTAssertTrue(model.canRetryTaskUpdates)
        XCTAssertNotNil(model.watchErrorMessage)
        let subscriptionCount = await client.watchSubscriptionCountSnapshot()
        XCTAssertEqual(subscriptionCount, 1)

        model.clearTask()
    }

    func testClearingAndReplacingTaskIgnoresLateIdleProbeResponse() async {
        let sleeper = TaskWatchReconciliationSleepGate()
        let oldTaskID = "old-idle-task"
        let newTaskID = "new-idle-task"
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(id: oldTaskID, source: "BV1idle-old", state: "TASK_STATE_PREPARING")),
                .success(.fixture(id: newTaskID, source: "BV1idle-new", state: "TASK_STATE_PREPARING")),
            ],
            suspendsGetTaskResponses: true
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1idle-old",
            watchReconciliationSleep: { delay in try await sleeper.sleep(delay) },
            watchReconciliationNow: { sleeper.clock.now() },
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await sleeper.waitForSleepCount(1)
        await sleeper.advanceNext()
        await client.waitForGetTaskRequestCount(1)

        model.clearTask()
        model.sourceText = "BV1idle-new"
        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription(count: 2)
        await client.completeNextGetTask(
            with: .success(
                .fixture(id: oldTaskID, source: "BV1idle-old", state: "TASK_STATE_RUNNING", progress: 0.9)))
        await client.waitForGetTaskResponseCompletionCount(1)

        let subscriptionCount = await client.watchSubscriptionCountSnapshot()
        XCTAssertEqual(model.currentTask?.id, newTaskID)
        XCTAssertEqual(model.currentTask?.source, "BV1idle-new")
        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_PREPARING")
        XCTAssertTrue(model.isWatching)
        XCTAssertEqual(subscriptionCount, 2)

        model.clearTask()
    }

    func testModelDeinitCancelsIdleReconciliationSleeper() async {
        let sleeper = TaskWatchReconciliationSleepGate()
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1idle-deinit", state: "TASK_STATE_PREPARING"))
            ]
        )
        weak var weakModel: BilibiliTaskViewModel?

        do {
            let model = BilibiliTaskViewModel(
                sourceText: "BV1idle-deinit",
                watchReconciliationSleep: { delay in try await sleeper.sleep(delay) },
                watchReconciliationNow: { sleeper.clock.now() },
                clientFactory: { _ in client }
            )
            weakModel = model
            await model.submit(serverAddressText: "mac-mini.local:50051")
            await client.waitForWatchSubscription()
            await sleeper.waitForSleepCount(1)
        }

        await waitUntil(weakModel == nil)
        await sleeper.waitForCancellationCount(1)
        let taskIDs = await client.getTaskIDsSnapshot()
        XCTAssertNil(weakModel)
        XCTAssertTrue(taskIDs.isEmpty)
    }

    func testExplicitTaskWatchRetryReconcilesBeforeResubscribingWithoutCreatingTask() async {
        let taskID = "bilibili-playback-1"
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1manual-watch", state: "TASK_STATE_PREPARING"))
            ],
            suspendsGetTaskResponses: true
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1manual-watch",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.failWatching(with: CacheTaskControlError.notFound)
        await waitUntil(model.canRetryTaskUpdates)

        model.retryTaskUpdates()
        await client.waitForGetTaskRequestCount(1)
        let subscriptionCountBeforeReconcile = await client.watchSubscriptionCountSnapshot()
        XCTAssertEqual(subscriptionCountBeforeReconcile, 1)

        await client.completeNextGetTask(
            with: .success(
                .fixture(source: "BV1manual-watch", state: "TASK_STATE_RUNNING", progress: 0.5)))
        await client.waitForWatchSubscription(count: 2)

        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_RUNNING")
        XCTAssertTrue(model.isWatching)
        XCTAssertNil(model.watchErrorMessage)
        let reconciledTaskIDs = await client.getTaskIDsSnapshot()
        let submittedTasks = await client.v2TaskRequestsSnapshot()
        XCTAssertEqual(reconciledTaskIDs, [taskID])
        XCTAssertEqual(submittedTasks.count, 1)

        model.clearTask()
    }

    func testExplicitTaskWatchRetryHandlesImmediateReconcileResponse() async {
        let taskID = "bilibili-playback-1"
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1fast-watch", state: "TASK_STATE_PREPARING"))
            ],
            getTaskResponses: [
                taskID: [
                    .success(.fixture(source: "BV1fast-watch", state: "TASK_STATE_RUNNING", progress: 0.4))
                ]
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1fast-watch",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.failWatching(with: CacheTaskControlError.notFound)
        await waitUntil(model.canRetryTaskUpdates)

        model.retryTaskUpdates()
        await client.waitForGetTaskRequestCount(1)
        await client.waitForWatchSubscription(count: 2)

        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_RUNNING")
        XCTAssertTrue(model.isWatching)
        XCTAssertNil(model.watchErrorMessage)
        let reconciledTaskIDs = await client.getTaskIDsSnapshot()
        XCTAssertEqual(reconciledTaskIDs, [taskID])

        model.clearTask()
    }

    func testExplicitTaskWatchRetryStopsWhenReconcileFindsTerminalTask() async {
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1manual-terminal", state: "TASK_STATE_PREPARING"))
            ],
            getTaskResponses: [
                "bilibili-playback-1": [
                    .success(.fixture(source: "BV1manual-terminal", state: "TASK_STATE_COMPLETED"))
                ]
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1manual-terminal",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.failWatching(with: CacheTaskControlError.notFound)
        await waitUntil(model.canRetryTaskUpdates)

        model.retryTaskUpdates()
        await client.waitForGetTaskRequestCount(1)
        await waitUntil(!model.isWatching)

        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_COMPLETED")
        let subscriptionCount = await client.watchSubscriptionCountSnapshot()
        XCTAssertEqual(subscriptionCount, 1)
        XCTAssertFalse(model.canRetryTaskUpdates)

        model.clearTask()
    }

    func testModelDeinitDuringSuspendedTaskReconcileDoesNotResubscribe() async {
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1deinit-watch", state: "TASK_STATE_PREPARING"))
            ],
            suspendsGetTaskResponses: true
        )
        weak var weakModel: BilibiliTaskViewModel?

        do {
            let model = BilibiliTaskViewModel(
                sourceText: "BV1deinit-watch",
                clientFactory: { _ in client }
            )
            weakModel = model
            await model.submit(serverAddressText: "mac-mini.local:50051")
            await client.waitForWatchSubscription()
            await client.failWatching(with: CacheTaskControlError.notFound)
            await waitUntil(model.canRetryTaskUpdates)

            model.retryTaskUpdates()
            await client.waitForGetTaskRequestCount(1)
        }

        await waitUntil(weakModel == nil)
        await client.completeNextGetTask(
            with: .success(.fixture(source: "BV1deinit-watch", state: "TASK_STATE_RUNNING")))
        await client.waitForGetTaskResponseCompletionCount(1)
        for _ in 0..<5 {
            await Task.yield()
        }

        let subscriptionCount = await client.watchSubscriptionCountSnapshot()
        XCTAssertNil(weakModel)
        XCTAssertEqual(subscriptionCount, 1)
    }

    func testReplacedTaskIgnoresLateOldTaskReconcileResponse() async {
        let oldTaskID = "old-task"
        let newTaskID = "new-task"
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(
                    .fixture(id: oldTaskID, source: "BV1old-watch", state: "TASK_STATE_PREPARING")),
                .success(
                    .fixture(id: newTaskID, source: "BV1new-watch", state: "TASK_STATE_PREPARING")),
            ],
            suspendsGetTaskResponses: true
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1old-watch",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.failWatching(with: CacheTaskControlError.notFound)
        await waitUntil(model.canRetryTaskUpdates)

        model.retryTaskUpdates()
        await client.waitForGetTaskRequestCount(1)
        model.sourceText = "BV1new-watch"
        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription(count: 2)
        XCTAssertEqual(model.currentTask?.id, newTaskID)

        await client.completeNextGetTask(
            with: .success(
                .fixture(
                    id: oldTaskID,
                    source: "BV1old-watch",
                    state: "TASK_STATE_RUNNING",
                    progress: 0.9
                )))
        await client.waitForGetTaskResponseCompletionCount(1)
        for _ in 0..<5 {
            await Task.yield()
        }

        let subscriptionCount = await client.watchSubscriptionCountSnapshot()
        XCTAssertEqual(model.currentTask?.id, newTaskID)
        XCTAssertEqual(model.currentTask?.source, "BV1new-watch")
        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_PREPARING")
        XCTAssertEqual(subscriptionCount, 2)
        XCTAssertTrue(model.isWatching)

        model.clearTask()
    }

    func testClearPlaybackStatusRestoresTaskStatus() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.playableFixture(source: "BV1ready"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1ready",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        model.finishPreparedPlayback(didStartPlayback: true)
        XCTAssertEqual(model.statusMessage, "Playing Ready video.")

        model.clearPlaybackStatus()

        XCTAssertEqual(model.statusMessage, "Ready video is ready to play.")

        model.clearTask()
    }

    func testCancelClearsActivePlaybackStatus() async {
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.playableFixture(source: "BV1ready"))
            ],
            cancelResponsesByID: [
                "bilibili-playback-1": .fixture(source: "BV1ready", state: "TASK_STATE_CANCELLED")
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1ready",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        model.finishPreparedPlayback(didStartPlayback: true)
        XCTAssertEqual(model.statusMessage, "Playing Ready video.")

        await model.cancel(serverAddressText: "mac-mini.local:50051")

        let cancelledIDs = await client.cancelledIDsSnapshot()
        XCTAssertEqual(cancelledIDs, ["bilibili-playback-1"])
        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_CANCELLED")
        XCTAssertEqual(model.statusMessage, "Ready video was cancelled.")

        model.clearTask()
    }

    func testTerminalCancelResponseStopsWatchingAndIgnoresLostWatchError() async {
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(source: "BV1ready", state: "TASK_STATE_PREPARING"))
            ],
            cancelResponsesByID: [
                "bilibili-playback-1": .fixture(source: "BV1ready", state: "TASK_STATE_CANCELLED")
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1ready",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await model.cancel(serverAddressText: "mac-mini.local:50051")
        await waitUntil(!model.isWatching)
        await client.waitForWatchTermination()
        await client.failWatching()
        await Task.yield()

        XCTAssertFalse(model.isWatching)
        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_CANCELLED")
        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(model.statusMessage, "Ready video was cancelled.")

        model.clearTask()
    }

    func testLateCancelResponseDoesNotOverwriteTerminalWatchUpdate() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(source: "BV1race", state: "TASK_STATE_PREPARING"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1race",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.setSuspendsCancelResponses(true)

        let cancelTask = Task {
            await model.cancel(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForCancelRequestCount(1)
        XCTAssertTrue(model.isCancelling)

        await client.yield(.fixture(source: "BV1race", state: "TASK_STATE_CANCELLED"))
        await waitUntil(model.currentTask?.state == "TASK_STATE_CANCELLED")
        XCTAssertFalse(model.isCancelling)
        await client.completeNextCancel(
            with: .success(
                .fixture(source: "BV1race", state: "TASK_STATE_CANCEL_REQUESTED", message: "Cancelling task.")))
        await cancelTask.value

        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_CANCELLED")
        XCTAssertFalse(model.isCancelling)
        XCTAssertEqual(model.statusMessage, "Ready video was cancelled.")

        model.clearTask()
    }

    func testMatchingTerminalCancelResponseWinsOverCancelRequestedWatchSnapshot() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(source: "BV1cancel-terminal", state: "TASK_STATE_PREPARING"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1cancel-terminal",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.setSuspendsCancelResponses(true)
        let cancelTask = Task {
            await model.cancel(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForCancelRequestCount(1)
        await client.yield(.fixture(source: "BV1cancel-terminal", state: "TASK_STATE_CANCEL_REQUESTED"))
        await waitUntil(model.currentTask?.state == "TASK_STATE_CANCEL_REQUESTED")

        await client.completeNextCancel(
            with: .success(.fixture(source: "BV1cancel-terminal", state: "TASK_STATE_CANCELLED")))
        await cancelTask.value

        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_CANCELLED")
        XCTAssertNil(model.errorMessage)
        XCTAssertFalse(model.isCancelling)
        XCTAssertEqual(model.statusMessage, "Ready video was cancelled.")

        model.clearTask()
    }

    func testMismatchedCancelResponseCannotOverwriteTerminalWatchSnapshot() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(source: "BV1cancel-mismatch", state: "TASK_STATE_PREPARING"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1cancel-mismatch",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.setSuspendsCancelResponses(true)
        let cancelTask = Task {
            await model.cancel(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForCancelRequestCount(1)

        await client.yield(.fixture(source: "BV1cancel-mismatch", state: "TASK_STATE_COMPLETED"))
        await waitUntil(model.currentTask?.state == "TASK_STATE_COMPLETED")
        await client.completeNextCancel(
            with: .success(
                .fixture(
                    id: "some-other-task",
                    source: "BV1cancel-mismatch",
                    state: "TASK_STATE_CANCELLED"
                )))
        await cancelTask.value

        XCTAssertEqual(model.currentTask?.id, "bilibili-playback-1")
        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_COMPLETED")
        XCTAssertNil(model.errorMessage)
        XCTAssertFalse(model.isCancelling)
        XCTAssertEqual(model.statusMessage, "Ready video is cached for LAN playback.")

        model.clearTask()
    }

    func testWatchUpdateClearsTransientCancelErrorWhenTaskRecovers() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(source: "BV1recover", state: "TASK_STATE_PREPARING"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1recover",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.setSuspendsCancelResponses(true)

        let cancelTask = Task {
            await model.cancel(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForCancelRequestCount(1)
        await client.completeNextCancel(with: .failure(FakeBilibiliCacheControlClientError.cancelFailed))
        await cancelTask.value

        XCTAssertNotNil(model.errorMessage)

        await client.yield(.playableFixture(source: "BV1recover"))
        await waitUntil(model.currentTask?.state == "TASK_STATE_PLAYABLE")

        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(model.statusMessage, "Ready video is ready to play.")
        XCTAssertTrue(model.canPlay)

        model.clearTask()
    }

    func testCancelErrorDoesNotEnableRetryForActiveTask() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(source: "BV1active", state: "TASK_STATE_PREPARING"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1active",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.setSuspendsCancelResponses(true)
        let cancelTask = Task {
            await model.cancel(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForCancelRequestCount(1)

        await client.completeNextCancel(with: .failure(FakeBilibiliCacheControlClientError.cancelFailed))
        await cancelTask.value

        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_PREPARING")
        XCTAssertNotNil(model.errorMessage)
        XCTAssertFalse(model.canRetry)

        model.clearTask()
    }

    func testLateCancelErrorDoesNotOverwriteTerminalWatchUpdate() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(source: "BV1race", state: "TASK_STATE_PREPARING"))
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1race",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.setSuspendsCancelResponses(true)

        let cancelTask = Task {
            await model.cancel(serverAddressText: "mac-mini.local:50051")
        }
        await client.waitForCancelRequestCount(1)
        XCTAssertTrue(model.isCancelling)

        await client.yield(.fixture(source: "BV1race", state: "TASK_STATE_CANCELLED"))
        await waitUntil(model.currentTask?.state == "TASK_STATE_CANCELLED")
        await client.completeNextCancel(with: .failure(FakeBilibiliCacheControlClientError.cancelFailed))
        await cancelTask.value

        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_CANCELLED")
        XCTAssertNil(model.errorMessage)
        XCTAssertFalse(model.isCancelling)
        XCTAssertEqual(model.statusMessage, "Ready video was cancelled.")

        model.clearTask()
    }

    func testCancelUpdatesCurrentTask() async {
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(state: "TASK_STATE_PREPARING"))
            ],
            cancelResponsesByID: [
                "bilibili-playback-1": .fixture(state: "TASK_STATE_CANCEL_REQUESTED", message: "Cancelling task.")
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1cancel",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await model.cancel(serverAddressText: "mac-mini.local:50051")

        let cancelledIDs = await client.cancelledIDsSnapshot()
        XCTAssertEqual(cancelledIDs, ["bilibili-playback-1"])
        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_CANCEL_REQUESTED")
        XCTAssertEqual(model.statusMessage, "Cancelling task.")
        XCTAssertFalse(model.isCancelling)
        XCTAssertFalse(model.canCancel)

        await model.cancel(serverAddressText: "mac-mini.local:50051")
        let repeatedCancelledIDs = await client.cancelledIDsSnapshot()
        XCTAssertEqual(repeatedCancelledIDs, ["bilibili-playback-1"])

        model.clearTask()
    }

    func testCancelRequestedMultiResultTaskKeepsServerStatusMessage() async {
        let resultItems: [BilibiliTaskResultItem] = [
            .fixture(
                id: "bilibili-playback-1",
                selectionID: "page:1",
                title: "Part 1",
                state: "TASK_STATE_RUNNING",
                message: "Preparing."
            ),
            .fixture(
                id: "bilibili-playback-1-result-2",
                selectionID: "page:2",
                title: "Part 2",
                index: 2,
                state: "TASK_STATE_PREPARING",
                message: "Preparing."
            ),
        ]
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(.fixture(state: "TASK_STATE_PREPARING", resultItems: resultItems))
            ],
            cancelResponsesByID: [
                "bilibili-playback-1": .fixture(
                    state: "TASK_STATE_CANCEL_REQUESTED",
                    message: "Cancelling task.",
                    resultItems: resultItems
                )
            ]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1cancel-multi",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await model.cancel(serverAddressText: "mac-mini.local:50051")

        XCTAssertEqual(model.currentTask?.state, "TASK_STATE_CANCEL_REQUESTED")
        XCTAssertEqual(model.taskResultSummary?.pendingCount, 2)
        XCTAssertEqual(model.statusMessage, "Cancelling task.")
        XCTAssertFalse(model.canCancel)

        model.clearTask()
    }

    func testRetryUsesFailedTaskSource() async {
        let client = FakeBilibiliCacheControlClient(createResponses: [
            .success(.fixture(source: "BV1original", state: "TASK_STATE_PREPARING")),
            .success(.fixture(source: "BV1original", state: "TASK_STATE_PREPARING")),
        ])
        let model = BilibiliTaskViewModel(
            sourceText: "BV1original",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")
        await client.waitForWatchSubscription()
        await client.yield(.fixture(source: "BV1original", state: "TASK_STATE_FAILED", message: "Planning failed."))
        await waitUntil(model.canRetry)
        XCTAssertEqual(model.statusMessage, "Planning failed.")
        XCTAssertEqual(model.errorMessage, "Planning failed.")
        model.sourceText = "BV1different"

        await model.retry(serverAddressText: "mac-mini.local:50051")

        let requests = await client.createdRequestsSnapshot()
        XCTAssertEqual(requests.map(\.urlOrID), ["BV1original", "BV1original"])

        model.clearTask()
    }

    private func waitForTaskWatchCondition(
        timeout: Duration,
        _ condition: @MainActor () async -> Bool
    ) async -> Bool {
        let clock = ContinuousClock()
        let deadline = clock.now.advanced(by: timeout)
        while clock.now < deadline {
            if await condition() {
                return true
            }
            try? await Task.sleep(for: .milliseconds(10))
        }
        return await condition()
    }

    func testSavedTaskRestoreRejectsChangedLiveServerIDBeforeFetchingTask() async throws {
        let (defaults, store, suiteName) = try makeResumptionStore()
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let endpoint = try XCTUnwrap(CacheServerEndpoint.normalized(from: "mac-mini.local:50051"))
        let taskIdentity = ClientResumptionTaskIdentity(
            endpoint: endpoint,
            serverID: "original-server",
            taskID: "colliding-saved-task"
        )
        let playbackIdentity = ClientPlaybackIdentity(
            task: taskIdentity,
            libraryItemID: "saved-library",
            variantID: "h264"
        )
        store.rememberTask(taskIdentity)
        store.beginPlayback(playbackIdentity)
        let savedRecord = store.record
        let client = FakeBilibiliCacheControlClient(
            createResponses: [],
            getTaskResponses: [
                taskIdentity.taskID: [
                    .success(
                        .playableFixture(
                            id: taskIdentity.taskID,
                            state: "TASK_STATE_COMPLETED",
                            libraryItemID: "saved-library",
                            playbackSourceItemID: "saved-library"
                        ))
                ]
            ]
        )
        await client.setServerID("replacement-server")
        let model = BilibiliTaskViewModel(resumptionStore: store, clientFactory: { _ in client })
        defer { model.clearTask() }

        await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)

        XCTAssertNil(model.currentTask)
        XCTAssertFalse(model.canContinueSavedPlayback)
        XCTAssertFalse(model.canPlay)
        XCTAssertFalse(model.isWatching)
        XCTAssertFalse(model.isRestoringTask)
        XCTAssertTrue(model.hasSavedTask)
        XCTAssertEqual(store.record, savedRecord)
        XCTAssertTrue(model.resumptionMessage?.contains("server identity changed") == true)
        let serverInfoCount = await client.serverInfoRequestCountSnapshot()
        let taskIDs = await client.getTaskIDsSnapshot()
        let sources = await client.playbackSourceRequestsSnapshot()
        let results = await client.taskResultRequestsSnapshot()
        let resolved = await client.resolvedRequestsSnapshot()
        let created = await client.createdRequestsSnapshot()
        let cancelled = await client.cancelledIDsSnapshot()
        XCTAssertEqual(serverInfoCount, 1)
        XCTAssertTrue(taskIDs.isEmpty)
        XCTAssertTrue(sources.isEmpty)
        XCTAssertTrue(results.isEmpty)
        XCTAssertTrue(resolved.isEmpty)
        XCTAssertTrue(created.isEmpty)
        XCTAssertTrue(cancelled.isEmpty)
    }

    func testSavedPlaybackRevalidationRejectsChangedLiveServerIDBeforeFetchingTaskOrSource() async throws {
        let (defaults, store, suiteName) = try makeResumptionStore()
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let endpoint = try XCTUnwrap(CacheServerEndpoint.normalized(from: "mac-mini.local:50051"))
        let taskIdentity = ClientResumptionTaskIdentity(
            endpoint: endpoint,
            serverID: "fake-cache-server",
            taskID: "revalidation-server-change"
        )
        let playbackIdentity = ClientPlaybackIdentity(
            task: taskIdentity,
            libraryItemID: "saved-library",
            variantID: "h264"
        )
        store.rememberTask(taskIdentity)
        store.beginPlayback(playbackIdentity)
        let task = CacheTask.playableFixture(
            id: taskIdentity.taskID,
            state: "TASK_STATE_COMPLETED",
            libraryItemID: "saved-library",
            playbackSourceItemID: "saved-library"
        )
        let source = try XCTUnwrap(task.playbackSource)
        let client = FakeBilibiliCacheControlClient(
            createResponses: [],
            getTaskResponses: [taskIdentity.taskID: [.success(task), .success(task)]],
            playbackSourceResponses: ["saved-library\u{1F}h264": source]
        )
        let model = BilibiliTaskViewModel(resumptionStore: store, clientFactory: { _ in client })
        defer { model.clearTask() }
        await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)
        XCTAssertTrue(model.canContinueSavedPlayback)
        let savedRecord = store.record
        let initialTaskIDs = await client.getTaskIDsSnapshot()
        let initialSources = await client.playbackSourceRequestsSnapshot()
        XCTAssertEqual(initialTaskIDs, [taskIdentity.taskID])
        XCTAssertEqual(initialSources.count, 1)

        await client.setServerID("replacement-server")
        let prepared = await model.prepareSavedPlayback(serverAddressText: endpoint.displayAddress)

        XCTAssertNil(prepared)
        XCTAssertNil(model.currentTask)
        XCTAssertFalse(model.canContinueSavedPlayback)
        XCTAssertFalse(model.canPlay)
        XCTAssertFalse(model.isWatching)
        XCTAssertFalse(model.isRestoringTask)
        XCTAssertTrue(model.hasSavedTask)
        XCTAssertEqual(store.record, savedRecord)
        XCTAssertTrue(model.resumptionMessage?.contains("server identity changed") == true)
        let serverInfoCount = await client.serverInfoRequestCountSnapshot()
        let taskIDs = await client.getTaskIDsSnapshot()
        let sources = await client.playbackSourceRequestsSnapshot()
        let results = await client.taskResultRequestsSnapshot()
        let resolved = await client.resolvedRequestsSnapshot()
        let created = await client.createdRequestsSnapshot()
        let cancelled = await client.cancelledIDsSnapshot()
        XCTAssertEqual(serverInfoCount, 2)
        XCTAssertEqual(taskIDs, initialTaskIDs)
        XCTAssertEqual(sources.count, initialSources.count)
        XCTAssertTrue(results.isEmpty)
        XCTAssertTrue(resolved.isEmpty)
        XCTAssertTrue(created.isEmpty)
        XCTAssertTrue(cancelled.isEmpty)
    }

    func testSavedTaskRestoreAndPlaybackRevalidationAcceptTrimmedLiveServerID() async throws {
        let (defaults, store, suiteName) = try makeResumptionStore()
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let endpoint = try XCTUnwrap(CacheServerEndpoint.normalized(from: "mac-mini.local:50051"))
        let taskIdentity = ClientResumptionTaskIdentity(
            endpoint: endpoint,
            serverID: "fake-cache-server",
            taskID: "trimmed-server-identity"
        )
        let playbackIdentity = ClientPlaybackIdentity(
            task: taskIdentity,
            libraryItemID: "saved-library",
            variantID: "h264"
        )
        store.rememberTask(taskIdentity)
        store.beginPlayback(playbackIdentity)
        let task = CacheTask.playableFixture(
            id: taskIdentity.taskID,
            state: "TASK_STATE_COMPLETED",
            libraryItemID: "saved-library",
            playbackSourceItemID: "saved-library"
        )
        let source = try XCTUnwrap(task.playbackSource)
        let client = FakeBilibiliCacheControlClient(
            createResponses: [],
            getTaskResponses: [taskIdentity.taskID: [.success(task), .success(task)]],
            playbackSourceResponses: ["saved-library\u{1F}h264": source]
        )
        await client.setServerID(" \nfake-cache-server\t ")
        let model = BilibiliTaskViewModel(resumptionStore: store, clientFactory: { _ in client })
        defer { model.clearTask() }

        await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)
        XCTAssertTrue(model.canContinueSavedPlayback)
        let prepared = await model.prepareSavedPlayback(serverAddressText: endpoint.displayAddress)

        XCTAssertNotNil(prepared)
        XCTAssertEqual(prepared?.checkpoint.identity, playbackIdentity)
        XCTAssertEqual(prepared?.progressContext.resumptionIdentity, playbackIdentity)
        XCTAssertEqual(prepared?.url, source.explicitHTTPURL)
        XCTAssertEqual(store.record?.task, taskIdentity)
        XCTAssertTrue(model.canContinueSavedPlayback)
        XCTAssertFalse(model.isWatching)
        XCTAssertNil(model.resumptionMessage)
        let serverInfoCount = await client.serverInfoRequestCountSnapshot()
        let taskIDs = await client.getTaskIDsSnapshot()
        let sources = await client.playbackSourceRequestsSnapshot()
        let resolved = await client.resolvedRequestsSnapshot()
        let created = await client.createdRequestsSnapshot()
        let cancelled = await client.cancelledIDsSnapshot()
        XCTAssertEqual(serverInfoCount, 2)
        XCTAssertEqual(taskIDs, [taskIdentity.taskID, taskIdentity.taskID])
        XCTAssertEqual(sources.count, 2)
        XCTAssertTrue(sources.allSatisfy { $0.itemID == "saved-library" && $0.variantID == "h264" })
        XCTAssertTrue(resolved.isEmpty)
        XCTAssertTrue(created.isEmpty)
        XCTAssertTrue(cancelled.isEmpty)
    }

    func testConcurrentRestoreForSameIdentityMakesOneRequest() async throws {
        let (defaults, store, suiteName) = try makeResumptionStore()
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let endpoint = try XCTUnwrap(CacheServerEndpoint.normalized(from: "mac-mini.local:50051"))
        let taskIdentity = ClientResumptionTaskIdentity(
            endpoint: endpoint,
            serverID: "fake-cache-server",
            taskID: "restore-single-flight"
        )
        store.rememberTask(taskIdentity)
        // A terminal snapshot excludes the normal running-task watch reconciliation RPC.
        let client = FakeBilibiliCacheControlClient(
            createResponses: [],
            getTaskResponses: [
                taskIdentity.taskID: [.success(.fixture(id: taskIdentity.taskID, state: "TASK_STATE_COMPLETED"))]
            ],
            suspendsServerInfoResponses: true
        )
        let model = BilibiliTaskViewModel(
            resumptionStore: store,
            clientFactory: { _ in client }
        )

        let firstRestore = ResumptionTestOperation {
            await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)
            return nil
        }
        guard
            await waitForResumptionPhase(
                "initial server info held", model: model, client: client, operations: [firstRestore],
                until: {
                    let pending = await client.pendingResumptionResponseCountsSnapshot()
                    return await client.serverInfoRequestCountSnapshot() == 1 && pending.serverInfo == 1
                })
        else { return }
        let secondRestore = ResumptionTestOperation {
            await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)
            return nil
        }
        guard
            await waitForResumptionPhase(
                "duplicate restore coalesced", model: model, client: client, operations: [firstRestore, secondRestore],
                until: {
                    secondRestore.isFinished
                })
        else { return }

        let serverInfoRequests = await client.serverInfoRequestCountSnapshot()
        XCTAssertEqual(serverInfoRequests, 1)
        await client.completeNextServerInfo(with: .success(.fixture(mediaBaseURIs: ["http://mac-mini.local:8080"])))
        guard
            await waitForResumptionPhase(
                "single restore completed", model: model, client: client, operations: [firstRestore],
                until: {
                    firstRestore.isFinished
                })
        else { return }

        let taskIDs = await client.getTaskIDsSnapshot()
        XCTAssertEqual(taskIDs, [taskIdentity.taskID])
        XCTAssertEqual(model.currentTask?.id, taskIdentity.taskID)
        XCTAssertFalse(model.isWatching)
        XCTAssertNil(store.record?.playback)
        let resolved = await client.resolvedRequestsSnapshot()
        let created = await client.createdRequestsSnapshot()
        let cancelled = await client.cancelledIDsSnapshot()
        XCTAssertTrue(resolved.isEmpty)
        XCTAssertTrue(created.isEmpty)
        XCTAssertTrue(cancelled.isEmpty)
        model.clearTask()
    }

    func testAcceptedReplacementTaskFencesBlockedSavedRestore() async throws {
        let (defaults, store, suiteName) = try makeResumptionStore()
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let endpoint = try XCTUnwrap(CacheServerEndpoint.normalized(from: "mac-mini.local:50051"))
        let savedTask = ClientResumptionTaskIdentity(
            endpoint: endpoint,
            serverID: "fake-cache-server",
            taskID: "restore-replaced"
        )
        let savedPlayback = ClientPlaybackIdentity(task: savedTask, variantID: "h264")
        store.rememberTask(savedTask)
        store.beginPlayback(
            savedPlayback,
            checkpoint: ClientPlaybackCheckpoint(
                identity: savedPlayback,
                positionSeconds: 19,
                durationSeconds: 100
            ))
        let client = FakeBilibiliCacheControlClient(
            createResponses: [
                .success(
                    .fixture(
                        id: "replacement-task",
                        source: "BV1replacement",
                        state: "TASK_STATE_PREPARING"
                    ))
            ],
            getTaskResponses: [savedTask.taskID: [.success(.playableFixture(id: savedTask.taskID))]],
            suspendsGetTaskResponses: true
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1replacement",
            resumptionStore: store,
            clientFactory: { _ in client }
        )
        let restore = ResumptionTestOperation {
            await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)
            return nil
        }
        guard
            await waitForResumptionPhase(
                "original GetTask held", model: model, client: client, operations: [restore],
                until: {
                    let pending = await client.pendingResumptionResponseCountsSnapshot()
                    return await client.getTaskIDsSnapshot().count == 1 && pending.getTask == 1
                })
        else { return }

        await model.submit(serverAddressText: endpoint.displayAddress)
        XCTAssertEqual(model.currentTask?.id, "replacement-task")
        XCTAssertEqual(store.record?.task.taskID, "replacement-task")
        XCTAssertNil(store.record?.playback)
        let acceptedStatus = model.statusMessage

        await client.completeNextGetTask(with: .success(.playableFixture(id: savedTask.taskID)))
        guard
            await waitForResumptionPhase(
                "replaced restore completed", model: model, client: client, operations: [restore],
                until: {
                    restore.isFinished
                })
        else { return }

        XCTAssertEqual(model.currentTask?.id, "replacement-task")
        XCTAssertEqual(model.statusMessage, acceptedStatus)
        XCTAssertEqual(store.record?.task.taskID, "replacement-task")
        XCTAssertNil(store.record?.playback)
        model.clearTask()
    }

    func testSucceededRootRestoresTerminalPlaybackAndPreparesSavedSource() async throws {
        let (defaults, store, suiteName) = try makeResumptionStore()
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let endpoint = try XCTUnwrap(CacheServerEndpoint.normalized(from: "mac-mini.local:50051"))
        let taskIdentity = ClientResumptionTaskIdentity(
            endpoint: endpoint, serverID: "fake-cache-server", taskID: "succeeded-root"
        )
        let playbackIdentity = ClientPlaybackIdentity(
            task: taskIdentity, libraryItemID: "succeeded-root-library", variantID: "h264"
        )
        store.rememberTask(taskIdentity)
        store.beginPlayback(playbackIdentity)
        let task = CacheTask.playableFixture(
            id: taskIdentity.taskID,
            state: "succeeded",
            libraryItemID: playbackIdentity.libraryItemID,
            playbackSourceItemID: playbackIdentity.libraryItemID
        )
        let source = try XCTUnwrap(task.playbackSource)
        let client = FakeBilibiliCacheControlClient(
            createResponses: [],
            getTaskResponses: [taskIdentity.taskID: [.success(task), .success(task)]],
            playbackSourceResponses: ["succeeded-root-library\u{1F}h264": source]
        )
        let model = BilibiliTaskViewModel(resumptionStore: store, clientFactory: { _ in client })
        defer { model.clearTask() }

        await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)
        XCTAssertTrue(model.canPlay)
        XCTAssertTrue(model.canContinueSavedPlayback)
        XCTAssertFalse(model.isWatching)
        XCTAssertEqual(model.playableURL, source.explicitHTTPURL)
        let manualContext = model.playbackProgressContext(serverAddressText: endpoint.displayAddress)
        XCTAssertEqual(manualContext?.libraryItemID, playbackIdentity.libraryItemID)
        let prepared = await model.prepareSavedPlayback(serverAddressText: endpoint.displayAddress)
        let unwrappedPrepared = try XCTUnwrap(prepared)
        XCTAssertEqual(unwrappedPrepared.url, source.explicitHTTPURL)
        XCTAssertEqual(unwrappedPrepared.checkpoint.identity, playbackIdentity)
        XCTAssertEqual(unwrappedPrepared.progressContext.libraryItemID, playbackIdentity.libraryItemID)
        model.finishPreparedResumption(unwrappedPrepared, didStartPlayback: true)
        XCTAssertTrue(model.isActivePlaybackLibraryItem(id: playbackIdentity.libraryItemID))
        let taskIDs = await client.getTaskIDsSnapshot()
        let watchCount = await client.watchSubscriptionCountSnapshot()
        let sources = await client.playbackSourceRequestsSnapshot()
        let resolved = await client.resolvedRequestsSnapshot()
        let created = await client.createdRequestsSnapshot()
        let cancelled = await client.cancelledIDsSnapshot()
        XCTAssertEqual(taskIDs, [taskIdentity.taskID, taskIdentity.taskID])
        XCTAssertEqual(watchCount, 0)
        XCTAssertEqual(sources.count, 2)
        XCTAssertTrue(sources.allSatisfy { $0.itemID == playbackIdentity.libraryItemID && $0.variantID == "h264" })
        XCTAssertTrue(resolved.isEmpty)
        XCTAssertTrue(created.isEmpty)
        XCTAssertTrue(cancelled.isEmpty)
    }

    func testSucceededEmbeddedExactChildRestoresAndPreparesSavedSource() async throws {
        try await assertTerminalExactChildState(usesV2Output: false, state: "succeeded", shouldRestore: true)
    }

    func testSucceededV2ExactChildRestoresAndPreparesSavedSource() async throws {
        try await assertTerminalExactChildState(usesV2Output: true, state: "succeeded", shouldRestore: true)
    }

    func testCompletedSuccessAliasesKeepWatchingWhileEmbeddedChildIsPlayable() async throws {
        for rootState in ["completed", "succeeded"] {
            let (defaults, store, suiteName) = try makeResumptionStore()
            defer { defaults.removePersistentDomain(forName: suiteName) }
            let endpoint = try XCTUnwrap(CacheServerEndpoint.normalized(from: "mac-mini.local:50051"))
            let taskIdentity = ClientResumptionTaskIdentity(
                endpoint: endpoint, serverID: "fake-cache-server", taskID: "terminal-root-\(rootState)"
            )
            store.rememberTask(taskIdentity)
            let child = BilibiliTaskResultItem.fixture(id: "still-playable-child", state: "playable")
            let task = CacheTask.fixture(id: taskIdentity.taskID, state: rootState, resultItems: [child])
            let client = FakeBilibiliCacheControlClient(
                createResponses: [],
                getTaskResponses: [taskIdentity.taskID: [.success(task), .success(task)]]
            )
            let model = BilibiliTaskViewModel(resumptionStore: store, clientFactory: { _ in client })
            defer { model.clearTask() }
            await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)
            guard
                await waitForResumptionPhase(
                    "terminal root observing playable child", model: model, client: client,
                    until: {
                        await client.watchSubscriptionCountSnapshot() == 1
                    })
            else { return }
            XCTAssertTrue(model.isWatching, rootState)
            XCTAssertTrue(model.canPlay, rootState)

            let finishedChild = BilibiliTaskResultItem.fixture(
                id: child.id,
                state: "succeeded",
                libraryItemID: "finished-child-library",
                playbackSourceItemID: "finished-child-library"
            )
            await client.yield(.fixture(id: taskIdentity.taskID, state: rootState, resultItems: [finishedChild]))
            guard
                await waitForResumptionPhase(
                    "last child succeeded", model: model, client: client,
                    until: {
                        model.currentTask?.resultItems.first?.state == "succeeded" && !model.isWatching
                    })
            else { return }
            XCTAssertTrue(model.canPlay, rootState)
            XCTAssertEqual(model.taskResults.first?.isCached, true, rootState)
            let context = model.playbackProgressContext(serverAddressText: endpoint.displayAddress)
            XCTAssertEqual(context?.libraryItemID, "finished-child-library", rootState)
            XCTAssertEqual(context?.resumptionIdentity?.resultID, child.id, rootState)
            let taskIDs = await client.getTaskIDsSnapshot()
            let resolved = await client.resolvedRequestsSnapshot()
            let created = await client.createdRequestsSnapshot()
            let cancelled = await client.cancelledIDsSnapshot()
            XCTAssertEqual(taskIDs, [taskIdentity.taskID, taskIdentity.taskID], rootState)
            XCTAssertTrue(resolved.isEmpty, rootState)
            XCTAssertTrue(created.isEmpty, rootState)
            XCTAssertTrue(cancelled.isEmpty, rootState)
        }
    }

    func testUnknownFailedAndCancelledRootStatesRemainUnavailable() async throws {
        for state in ["unrecognized(99)", "failed", "cancelled"] {
            let (defaults, store, suiteName) = try makeResumptionStore()
            defer { defaults.removePersistentDomain(forName: suiteName) }
            let endpoint = try XCTUnwrap(CacheServerEndpoint.normalized(from: "mac-mini.local:50051"))
            let taskIdentity = ClientResumptionTaskIdentity(
                endpoint: endpoint, serverID: "fake-cache-server", taskID: "non-success-root"
            )
            let playbackIdentity = ClientPlaybackIdentity(task: taskIdentity, variantID: "h264")
            store.rememberTask(taskIdentity)
            store.beginPlayback(playbackIdentity)
            let task = CacheTask.playableFixture(id: taskIdentity.taskID, state: state)
            let shouldWatch = state == "unrecognized(99)"
            let expectedTaskCount = shouldWatch ? 3 : 2
            let client = FakeBilibiliCacheControlClient(
                createResponses: [],
                getTaskResponses: [taskIdentity.taskID: Array(repeating: .success(task), count: expectedTaskCount)]
            )
            let model = BilibiliTaskViewModel(resumptionStore: store, clientFactory: { _ in client })
            defer { model.clearTask() }
            await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)
            if shouldWatch {
                guard
                    await waitForResumptionPhase(
                        "unknown root observation", model: model, client: client,
                        until: {
                            await client.watchSubscriptionCountSnapshot() == 1
                        })
                else { return }
            }
            XCTAssertFalse(model.canPlay, state)
            XCTAssertFalse(model.canContinueSavedPlayback, state)
            XCTAssertNil(model.playableURL, state)
            let prepared = await model.prepareSavedPlayback(serverAddressText: endpoint.displayAddress)
            XCTAssertNil(prepared, state)
            XCTAssertTrue(model.hasSavedTask, state)
            XCTAssertEqual(store.record?.playback?.identity, playbackIdentity, state)
            let taskIDs = await client.getTaskIDsSnapshot()
            let sources = await client.playbackSourceRequestsSnapshot()
            let resolved = await client.resolvedRequestsSnapshot()
            let created = await client.createdRequestsSnapshot()
            let cancelled = await client.cancelledIDsSnapshot()
            XCTAssertEqual(taskIDs, Array(repeating: taskIdentity.taskID, count: expectedTaskCount), state)
            XCTAssertTrue(sources.isEmpty, state)
            XCTAssertTrue(resolved.isEmpty, state)
            XCTAssertTrue(created.isEmpty, state)
            XCTAssertTrue(cancelled.isEmpty, state)
        }
    }

    func testUnknownFailedAndCancelledExactChildStatesRemainUnavailable() async throws {
        for usesV2Output in [false, true] {
            for state in ["unrecognized(99)", "failed", "cancelled"] {
                try await assertTerminalExactChildState(usesV2Output: usesV2Output, state: state, shouldRestore: false)
            }
        }
    }

    func testChangedSelectedWatchSourceInvalidatesSavedContinueAndRevalidates() async throws {
        try await assertSavedWatchSourceChangeRevalidation(becomesUnplayable: false)
    }

    func testUnplayableSelectedWatchSourceInvalidatesSavedContinueUntilRecovery() async throws {
        try await assertSavedWatchSourceChangeRevalidation(becomesUnplayable: true)
    }

    func testEmbeddedFailedSavedChildWithOfflineFillMarkerRestoresExactSource() async throws {
        try await assertEmbeddedFailedSavedChildRestoration(
            message: "Playable online; offline cache fill failed: upstream failed.",
            shouldRestore: true
        )
    }

    func testEmbeddedFailedSavedChildWithoutOfflineFillMarkerRemainsUnavailable() async throws {
        try await assertEmbeddedFailedSavedChildRestoration(
            message: "Transcoding failed before a playable source was available.",
            shouldRestore: false
        )
    }

    func testRestoredContinueSurvivesOrdinaryWatchProgressWithoutPolling() async throws {
        let (defaults, store, suiteName) = try makeResumptionStore()
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let endpoint = try XCTUnwrap(CacheServerEndpoint.normalized(from: "mac-mini.local:50051"))
        let taskIdentity = ClientResumptionTaskIdentity(
            endpoint: endpoint,
            serverID: "fake-cache-server",
            taskID: "restore-progress"
        )
        let playbackIdentity = ClientPlaybackIdentity(task: taskIdentity, variantID: "h264")
        store.rememberTask(taskIdentity)
        store.beginPlayback(
            playbackIdentity,
            checkpoint: ClientPlaybackCheckpoint(
                identity: playbackIdentity,
                positionSeconds: 12,
                durationSeconds: 120
            ))
        let initialTask = CacheTask.playableFixture(
            id: taskIdentity.taskID,
            progress: 0.4,
            downloadedBytes: 40,
            totalBytes: 100
        )
        let client = FakeBilibiliCacheControlClient(
            createResponses: [],
            getTaskResponses: [taskIdentity.taskID: [.success(initialTask), .success(initialTask)]]
        )
        let model = BilibiliTaskViewModel(resumptionStore: store, clientFactory: { _ in client })

        await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)
        guard
            await waitForResumptionPhase(
                "restored watch subscribed", model: model, client: client,
                until: {
                    await client.watchSubscriptionCountSnapshot() == 1
                })
        else { return }
        XCTAssertTrue(model.canContinueSavedPlayback)

        let serverInfoCount = await client.serverInfoRequestCountSnapshot()
        let getTaskIDs = await client.getTaskIDsSnapshot()
        let resultRequests = await client.taskResultRequestsSnapshot()
        await client.yield(
            .playableFixture(
                id: taskIdentity.taskID,
                progress: 0.65,
                downloadedBytes: 65,
                totalBytes: 100
            ))
        guard
            await waitForResumptionPhase(
                "watch progress accepted", model: model, client: client,
                until: {
                    model.currentTask?.downloadedBytes == 65
                })
        else { return }
        try? await Task.sleep(for: .milliseconds(100))

        XCTAssertTrue(model.canContinueSavedPlayback)
        let finalServerInfoCount = await client.serverInfoRequestCountSnapshot()
        let finalGetTaskIDs = await client.getTaskIDsSnapshot()
        let finalResultRequests = await client.taskResultRequestsSnapshot()
        XCTAssertEqual(finalServerInfoCount, serverInfoCount)
        XCTAssertEqual(finalGetTaskIDs, getTaskIDs)
        XCTAssertEqual(finalResultRequests.count, resultRequests.count)
        model.clearTask()
    }

    func testSameIdentityCheckpointUpdateKeepsContinueWithoutPollingAndPreparationUsesLatestPosition() async throws {
        let (defaults, store, suiteName) = try makeResumptionStore()
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let endpoint = try XCTUnwrap(CacheServerEndpoint.normalized(from: "mac-mini.local:50051"))
        let taskIdentity = ClientResumptionTaskIdentity(
            endpoint: endpoint,
            serverID: "fake-cache-server",
            taskID: "saved-checkpoint-advance"
        )
        let playbackIdentity = ClientPlaybackIdentity(task: taskIdentity, variantID: "h264")
        store.rememberTask(taskIdentity)
        store.beginPlayback(
            playbackIdentity,
            checkpoint: ClientPlaybackCheckpoint(
                identity: playbackIdentity,
                positionSeconds: 12,
                durationSeconds: 120
            ))
        let initialTask = CacheTask.playableFixture(
            id: taskIdentity.taskID,
            progress: 0.4,
            downloadedBytes: 40,
            totalBytes: 100
        )
        let watchedTask = CacheTask.playableFixture(
            id: taskIdentity.taskID,
            progress: 0.65,
            downloadedBytes: 65,
            totalBytes: 100
        )
        let client = FakeBilibiliCacheControlClient(
            createResponses: [],
            getTaskResponses: [
                taskIdentity.taskID: [.success(initialTask), .success(initialTask), .success(watchedTask)]
            ]
        )
        let model = BilibiliTaskViewModel(resumptionStore: store, clientFactory: { _ in client })
        defer { model.clearTask() }
        await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)
        guard
            await waitForResumptionPhase(
                "checkpoint test bootstrap", model: model, client: client,
                until: {
                    await client.watchSubscriptionCountSnapshot() == 1
                })
        else { return }
        XCTAssertTrue(model.canContinueSavedPlayback)
        let infoBaseline = await client.serverInfoRequestCountSnapshot()
        let taskIDsBaseline = await client.getTaskIDsSnapshot()
        let resultsBaseline = await client.taskResultRequestsSnapshot().count
        let sourcesBaseline = await client.playbackSourceRequestsSnapshot().count

        store.updatePlayback(
            identity: playbackIdentity,
            positionSeconds: 150,
            durationSeconds: 150,
            completed: true
        )
        let latestCheckpoint = try XCTUnwrap(store.record?.playback)
        XCTAssertEqual(latestCheckpoint.positionSeconds, 150)
        XCTAssertEqual(latestCheckpoint.durationSeconds, 150)
        XCTAssertTrue(latestCheckpoint.completed)
        XCTAssertTrue(model.canContinueSavedPlayback)
        await client.yield(watchedTask)
        guard
            await waitForResumptionPhase(
                "same-source watch after checkpoint update", model: model, client: client,
                until: {
                    model.currentTask?.downloadedBytes == 65
                })
        else { return }
        try? await Task.sleep(for: .milliseconds(100))
        XCTAssertTrue(model.canContinueSavedPlayback)
        let infoAfterWatch = await client.serverInfoRequestCountSnapshot()
        let taskIDsAfterWatch = await client.getTaskIDsSnapshot()
        let resultsAfterWatch = await client.taskResultRequestsSnapshot().count
        let sourcesAfterWatch = await client.playbackSourceRequestsSnapshot().count
        XCTAssertEqual(infoAfterWatch, infoBaseline)
        XCTAssertEqual(taskIDsAfterWatch, taskIDsBaseline)
        XCTAssertEqual(resultsAfterWatch, resultsBaseline)
        XCTAssertEqual(sourcesAfterWatch, sourcesBaseline)

        let prepared = await model.prepareSavedPlayback(serverAddressText: endpoint.displayAddress)
        XCTAssertEqual(prepared?.checkpoint, latestCheckpoint)
        XCTAssertEqual(prepared?.checkpoint.positionSeconds, 150)
        XCTAssertEqual(prepared?.progressContext.resumptionIdentity, playbackIdentity)
        XCTAssertTrue(model.canContinueSavedPlayback)
        let finalInfoCount = await client.serverInfoRequestCountSnapshot()
        let finalTaskIDs = await client.getTaskIDsSnapshot()
        let resolved = await client.resolvedRequestsSnapshot()
        let created = await client.createdRequestsSnapshot()
        let cancelled = await client.cancelledIDsSnapshot()
        XCTAssertEqual(finalInfoCount, infoBaseline + 1)
        XCTAssertEqual(finalTaskIDs, taskIDsBaseline + [taskIdentity.taskID])
        XCTAssertTrue(resolved.isEmpty)
        XCTAssertTrue(created.isEmpty)
        XCTAssertTrue(cancelled.isEmpty)
    }

    func testSavedPlaybackPreparationToleratesWatchProgressDuringBlockedRPCs() async throws {
        let (defaults, store, suiteName) = try makeResumptionStore()
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let endpoint = try XCTUnwrap(CacheServerEndpoint.normalized(from: "mac-mini.local:50051"))
        let taskIdentity = ClientResumptionTaskIdentity(
            endpoint: endpoint,
            serverID: "fake-cache-server",
            taskID: "restore-blocked-progress"
        )
        let playbackIdentity = ClientPlaybackIdentity(task: taskIdentity, variantID: "h264")
        store.rememberTask(taskIdentity)
        store.beginPlayback(playbackIdentity)
        let initialTask = CacheTask.playableFixture(id: taskIdentity.taskID)
        let client = FakeBilibiliCacheControlClient(
            createResponses: [],
            getTaskResponses: [taskIdentity.taskID: [.success(initialTask), .success(initialTask)]]
        )
        let model = BilibiliTaskViewModel(resumptionStore: store, clientFactory: { _ in client })
        await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)
        guard
            await waitForResumptionPhase(
                "bootstrap watch subscribed", model: model, client: client,
                until: {
                    await client.watchSubscriptionCountSnapshot() == 1
                })
        else { return }

        let serverInfoBaseline = await client.serverInfoRequestCountSnapshot()
        let getTaskBaseline = await client.getTaskIDsSnapshot().count
        await client.setSuspendsServerInfoResponses(true)
        await client.setSuspendsGetTaskResponses(true)
        let preparation = ResumptionTestOperation {
            await model.prepareSavedPlayback(serverAddressText: endpoint.displayAddress)
        }
        guard
            await waitForResumptionPhase(
                "prepare server info held", model: model, client: client, operations: [preparation],
                until: {
                    let pending = await client.pendingResumptionResponseCountsSnapshot()
                    return await client.serverInfoRequestCountSnapshot() == serverInfoBaseline + 1
                        && pending.serverInfo == 1
                })
        else { return }
        store.updatePlayback(
            identity: playbackIdentity,
            positionSeconds: 46,
            durationSeconds: 120,
            completed: false
        )
        await client.yield(
            .playableFixture(
                id: taskIdentity.taskID,
                progress: 0.5,
                downloadedBytes: 50,
                totalBytes: 100
            ))
        guard
            await waitForResumptionPhase(
                "progress during server info", model: model, client: client, operations: [preparation],
                until: {
                    model.currentTask?.downloadedBytes == 50
                })
        else { return }
        await client.completeNextServerInfo(with: .success(.fixture(mediaBaseURIs: ["http://mac-mini.local:8080"])))
        guard
            await waitForResumptionPhase(
                "prepare GetTask held", model: model, client: client, operations: [preparation],
                until: {
                    let pending = await client.pendingResumptionResponseCountsSnapshot()
                    return await client.getTaskIDsSnapshot().count == getTaskBaseline + 1 && pending.getTask == 1
                })
        else { return }
        store.updatePlayback(
            identity: playbackIdentity,
            positionSeconds: 83,
            durationSeconds: 180,
            completed: true
        )
        let latestCheckpoint = try XCTUnwrap(store.record?.playback)
        await client.yield(
            .playableFixture(
                id: taskIdentity.taskID,
                progress: 0.7,
                downloadedBytes: 70,
                totalBytes: 100
            ))
        guard
            await waitForResumptionPhase(
                "progress during GetTask", model: model, client: client, operations: [preparation],
                until: {
                    model.currentTask?.downloadedBytes == 70
                })
        else { return }
        await client.completeNextGetTask(
            with: .success(
                .playableFixture(
                    id: taskIdentity.taskID,
                    progress: 0.7,
                    downloadedBytes: 70,
                    totalBytes: 100
                )))

        guard
            await waitForResumptionPhase(
                "prepare completed", model: model, client: client, operations: [preparation],
                until: {
                    preparation.isFinished
                })
        else { return }
        let prepared = preparation.prepared
        XCTAssertNotNil(prepared)
        XCTAssertEqual(prepared?.checkpoint.identity, playbackIdentity)
        XCTAssertEqual(prepared?.checkpoint, latestCheckpoint)
        XCTAssertEqual(prepared?.checkpoint.positionSeconds, 83)
        XCTAssertEqual(prepared?.checkpoint.durationSeconds, 180)
        XCTAssertTrue(prepared?.checkpoint.completed == true)
        let finalServerInfoCount = await client.serverInfoRequestCountSnapshot()
        let finalTaskIDs = await client.getTaskIDsSnapshot()
        XCTAssertEqual(finalServerInfoCount, serverInfoBaseline + 1)
        XCTAssertEqual(finalTaskIDs.count, getTaskBaseline + 1)
        XCTAssertTrue(model.canContinueSavedPlayback)
        model.clearTask()
    }

    func testSavedPlaybackPreparationRejectsChangedPlaybackIdentityDuringRPC() async throws {
        let (defaults, store, suiteName) = try makeResumptionStore()
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let endpoint = try XCTUnwrap(CacheServerEndpoint.normalized(from: "mac-mini.local:50051"))
        let taskIdentity = ClientResumptionTaskIdentity(
            endpoint: endpoint,
            serverID: "fake-cache-server",
            taskID: "restore-identity-change"
        )
        let playbackIdentity = ClientPlaybackIdentity(task: taskIdentity, variantID: "h264")
        let changedIdentity = ClientPlaybackIdentity(task: taskIdentity, variantID: "vp9")
        store.rememberTask(taskIdentity)
        store.beginPlayback(
            playbackIdentity,
            checkpoint: ClientPlaybackCheckpoint(
                identity: playbackIdentity,
                positionSeconds: 12,
                durationSeconds: 120
            ))
        let initialTask = CacheTask.playableFixture(id: taskIdentity.taskID)
        let client = FakeBilibiliCacheControlClient(
            createResponses: [],
            getTaskResponses: [taskIdentity.taskID: [.success(initialTask), .success(initialTask)]]
        )
        let model = BilibiliTaskViewModel(resumptionStore: store, clientFactory: { _ in client })
        defer { model.clearTask() }
        await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)
        guard
            await waitForResumptionPhase(
                "identity-change bootstrap", model: model, client: client,
                until: {
                    await client.watchSubscriptionCountSnapshot() == 1
                })
        else { return }

        let serverInfoBaseline = await client.serverInfoRequestCountSnapshot()
        let taskIDsBaseline = await client.getTaskIDsSnapshot()
        await client.setSuspendsServerInfoResponses(true)
        let preparation = ResumptionTestOperation {
            await model.prepareSavedPlayback(serverAddressText: endpoint.displayAddress)
        }
        guard
            await waitForResumptionPhase(
                "identity-change preparation held", model: model, client: client, operations: [preparation],
                until: {
                    let pending = await client.pendingResumptionResponseCountsSnapshot()
                    return await client.serverInfoRequestCountSnapshot() == serverInfoBaseline + 1
                        && pending.serverInfo == 1
                })
        else { return }

        store.beginPlayback(
            changedIdentity,
            checkpoint: ClientPlaybackCheckpoint(
                identity: changedIdentity,
                positionSeconds: 12,
                durationSeconds: 120
            ))
        await client.completeNextServerInfo(with: .success(.fixture(mediaBaseURIs: ["http://mac-mini.local:8080"])))
        guard
            await waitForResumptionPhase(
                "identity-change preparation rejected", model: model, client: client, operations: [preparation],
                until: {
                    preparation.isFinished
                })
        else { return }

        XCTAssertNil(preparation.prepared)
        XCTAssertFalse(model.canContinueSavedPlayback)
        XCTAssertFalse(model.isRestoringTask)
        let finalTaskIDs = await client.getTaskIDsSnapshot()
        XCTAssertEqual(finalTaskIDs, taskIDsBaseline)
    }

    func testSubmissionDuringBlockedAutomaticValidationIgnoresLateSuccess() async throws {
        try await assertSubmissionDuringBlockedAutomaticValidationIgnoresLateResponse(
            .success(.fixture(mediaBaseURIs: ["http://mac-mini.local:8080"]))
        )
    }

    func testSubmissionDuringBlockedAutomaticValidationIgnoresLateFailure() async throws {
        try await assertSubmissionDuringBlockedAutomaticValidationIgnoresLateResponse(
            .failure(FakeLocalizedError(message: "Late saved-source validation failure."))
        )
    }

    func testStalePreparationDeferDoesNotClearNewRestoreOwner() async throws {
        let (defaults, store, suiteName) = try makeResumptionStore()
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let endpoint = try XCTUnwrap(CacheServerEndpoint.normalized(from: "mac-mini.local:50051"))
        let taskIdentity = ClientResumptionTaskIdentity(
            endpoint: endpoint,
            serverID: "fake-cache-server",
            taskID: "restore-owner-replaced"
        )
        let playbackIdentity = ClientPlaybackIdentity(task: taskIdentity, variantID: "h264")
        store.rememberTask(taskIdentity)
        store.beginPlayback(
            playbackIdentity,
            checkpoint: ClientPlaybackCheckpoint(
                identity: playbackIdentity,
                positionSeconds: 12,
                durationSeconds: 120
            ))
        let initialTask = CacheTask.playableFixture(id: taskIdentity.taskID)
        let client = FakeBilibiliCacheControlClient(
            createResponses: [],
            getTaskResponses: [taskIdentity.taskID: Array(repeating: .success(initialTask), count: 8)]
        )
        let model = BilibiliTaskViewModel(resumptionStore: store, clientFactory: { _ in client })
        defer { model.clearTask() }
        await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)
        guard
            await waitForResumptionPhase(
                "restore-owner bootstrap", model: model, client: client,
                until: {
                    await client.watchSubscriptionCountSnapshot() == 1
                })
        else { return }

        await client.setSuspendsServerInfoResponses(true)
        let preparation = ResumptionTestOperation {
            await model.prepareSavedPlayback(serverAddressText: endpoint.displayAddress)
        }
        guard
            await waitForResumptionPhase(
                "older preparation held", model: model, client: client, operations: [preparation],
                until: {
                    let pending = await client.pendingResumptionResponseCountsSnapshot()
                    return pending.serverInfo == 1 && model.isRestoringTask
                })
        else { return }

        let restore = ResumptionTestOperation {
            await model.retrySavedTask(serverAddressText: endpoint.displayAddress)
            return nil
        }
        guard
            await waitForResumptionPhase(
                "new restore owns restoring state", model: model, client: client, operations: [restore],
                until: {
                    let pending = await client.pendingResumptionResponseCountsSnapshot()
                    return pending.serverInfo == 2 && model.isRestoringTask
                })
        else { return }
        let newRestoreMessage = model.resumptionMessage

        await client.completeNextServerInfo(with: .success(.fixture(mediaBaseURIs: ["http://mac-mini.local:8080"])))
        guard
            await waitForResumptionPhase(
                "older preparation returns without releasing new owner", model: model, client: client,
                operations: [preparation, restore],
                until: {
                    let pending = await client.pendingResumptionResponseCountsSnapshot()
                    return preparation.isFinished && pending.serverInfo == 1
                })
        else { return }
        XCTAssertNil(preparation.prepared)
        XCTAssertTrue(model.isRestoringTask)
        XCTAssertEqual(model.resumptionMessage, newRestoreMessage)
        XCTAssertFalse(restore.isFinished)

        await client.setSuspendsServerInfoResponses(false)
        await client.completeNextServerInfo(with: .success(.fixture(mediaBaseURIs: ["http://mac-mini.local:8080"])))
        guard
            await waitForResumptionPhase(
                "new restore completes", model: model, client: client, operations: [restore],
                until: {
                    restore.isFinished && !model.isRestoringTask
                })
        else { return }

        XCTAssertEqual(model.currentTask?.id, taskIdentity.taskID)
        XCTAssertTrue(model.canContinueSavedPlayback)
        XCTAssertNil(model.resumptionMessage)
    }

    private func assertSubmissionDuringBlockedAutomaticValidationIgnoresLateResponse(
        _ lateResponse: Result<CacheServerSummary, Error>
    ) async throws {
        let (defaults, store, suiteName) = try makeResumptionStore()
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let endpoint = try XCTUnwrap(CacheServerEndpoint.normalized(from: "mac-mini.local:50051"))
        let taskIdentity = ClientResumptionTaskIdentity(
            endpoint: endpoint,
            serverID: "fake-cache-server",
            taskID: "restore-submit-replacement"
        )
        let playbackIdentity = ClientPlaybackIdentity(task: taskIdentity, variantID: "h264")
        store.rememberTask(taskIdentity)
        store.beginPlayback(playbackIdentity)
        let initialTask = CacheTask.playableFixture(id: taskIdentity.taskID)
        let changedSource = CachePlaybackSource(
            itemID: taskIdentity.taskID,
            variantID: "h264",
            playbackProtocol: "PLAYBACK_PROTOCOL_HLS",
            uri: "http://mac-mini.local:8080/hls/\(taskIdentity.taskID)/changed.m3u8"
        )
        let changedTask = CacheTask.fixture(
            id: taskIdentity.taskID,
            state: "TASK_STATE_PLAYABLE",
            downloadedBytes: 50,
            totalBytes: 100,
            playbackSource: changedSource
        )
        let replacementTask = CacheTask.fixture(
            id: "replacement-task",
            source: "BV1replacement",
            state: "TASK_STATE_PREPARING"
        )
        let client = FakeBilibiliCacheControlClient(
            createResponses: [.success(replacementTask)],
            getTaskResponses: [taskIdentity.taskID: [.success(initialTask), .success(initialTask)]]
        )
        let model = BilibiliTaskViewModel(
            sourceText: "BV1original",
            resumptionStore: store,
            clientFactory: { _ in client }
        )
        defer { model.clearTask() }
        await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)
        guard
            await waitForResumptionPhase(
                "submission-cancellation bootstrap", model: model, client: client,
                until: {
                    await client.watchSubscriptionCountSnapshot() == 1
                })
        else { return }

        await client.setSuspendsServerInfoResponses(true)
        await client.yield(changedTask)
        guard
            await waitForResumptionPhase(
                "automatic validation held", model: model, client: client,
                until: {
                    let pending = await client.pendingResumptionResponseCountsSnapshot()
                    return pending.serverInfo == 1 && model.isRestoringTask
                        && model.currentTask?.playbackSource == changedSource
                })
        else { return }

        model.sourceText = "BV1replacement"
        await client.setSuspendsServerInfoResponses(false)
        await model.submit(serverAddressText: endpoint.displayAddress)
        XCTAssertEqual(model.currentTask?.id, replacementTask.id)
        XCTAssertEqual(store.record?.task.taskID, replacementTask.id)
        XCTAssertFalse(model.isSubmitting)
        XCTAssertFalse(model.isResolving)
        XCTAssertFalse(model.isRestoringTask)
        XCTAssertNil(model.errorMessage)
        XCTAssertNil(model.resumptionMessage)
        let acceptedStatus = model.statusMessage
        let serverInfoCompletionBaseline = await client.serverInfoResponseCompletionCountSnapshot()

        await client.completeNextServerInfo(with: lateResponse)
        guard
            await waitForResumptionPhase(
                "late validation response resumed", model: model, client: client,
                until: {
                    await client.serverInfoResponseCompletionCountSnapshot() >= serverInfoCompletionBaseline + 1
                })
        else { return }
        await Task.yield()

        XCTAssertEqual(model.currentTask?.id, replacementTask.id)
        XCTAssertEqual(store.record?.task.taskID, replacementTask.id)
        XCTAssertEqual(model.statusMessage, acceptedStatus)
        XCTAssertNil(model.resumptionMessage)
        XCTAssertNil(model.errorMessage)
        XCTAssertFalse(model.isSubmitting)
        XCTAssertFalse(model.isResolving)
        XCTAssertFalse(model.isRestoringTask)
    }

    func testStalePreparationAfterClearDoesNotOverwriteClearState() async throws {
        let (defaults, store, suiteName) = try makeResumptionStore()
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let endpoint = try XCTUnwrap(CacheServerEndpoint.normalized(from: "mac-mini.local:50051"))
        let taskIdentity = ClientResumptionTaskIdentity(
            endpoint: endpoint,
            serverID: "fake-cache-server",
            taskID: "restore-clear-fence"
        )
        let playbackIdentity = ClientPlaybackIdentity(task: taskIdentity, variantID: "h264")
        store.rememberTask(taskIdentity)
        store.beginPlayback(playbackIdentity)
        let initialTask = CacheTask.playableFixture(id: taskIdentity.taskID)
        let client = FakeBilibiliCacheControlClient(
            createResponses: [],
            getTaskResponses: [taskIdentity.taskID: [.success(initialTask), .success(initialTask)]]
        )
        let model = BilibiliTaskViewModel(resumptionStore: store, clientFactory: { _ in client })
        await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)
        guard
            await waitForResumptionPhase(
                "clear test bootstrap subscribed", model: model, client: client,
                until: {
                    await client.watchSubscriptionCountSnapshot() == 1
                })
        else { return }

        let serverInfoBaseline = await client.serverInfoRequestCountSnapshot()
        let bootstrapTaskIDs = await client.getTaskIDsSnapshot()
        XCTAssertEqual(bootstrapTaskIDs, [taskIdentity.taskID, taskIdentity.taskID])
        await client.setSuspendsServerInfoResponses(true)
        let preparation = ResumptionTestOperation {
            await model.prepareSavedPlayback(serverAddressText: endpoint.displayAddress)
        }
        guard
            await waitForResumptionPhase(
                "stale prepare server info held", model: model, client: client, operations: [preparation],
                until: {
                    let pending = await client.pendingResumptionResponseCountsSnapshot()
                    return await client.serverInfoRequestCountSnapshot() == serverInfoBaseline + 1
                        && pending.serverInfo == 1
                })
        else { return }
        model.clearTask()
        let clearedStatus = model.statusMessage
        await client.completeNextServerInfo(with: .success(.fixture(mediaBaseURIs: ["http://mac-mini.local:8080"])))

        guard
            await waitForResumptionPhase(
                "cleared prepare completed", model: model, client: client, operations: [preparation],
                until: {
                    preparation.isFinished
                })
        else { return }
        let stalePrepared = preparation.prepared
        XCTAssertNil(stalePrepared)
        XCTAssertNil(model.currentTask)
        XCTAssertFalse(model.hasSavedTask)
        XCTAssertEqual(model.statusMessage, clearedStatus)
        XCTAssertNil(model.resumptionMessage)
        let getTaskIDs = await client.getTaskIDsSnapshot()
        XCTAssertEqual(getTaskIDs, bootstrapTaskIDs)
    }

    func testAcceptedPlayableOutputCanEnableSavedContinueFromWatch() async throws {
        let (defaults, store, suiteName) = try makeResumptionStore()
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let endpoint = try XCTUnwrap(CacheServerEndpoint.normalized(from: "mac-mini.local:50051"))
        let taskIdentity = ClientResumptionTaskIdentity(
            endpoint: endpoint,
            serverID: "fake-cache-server",
            taskID: "restore-later-playable"
        )
        let playbackIdentity = ClientPlaybackIdentity(task: taskIdentity, variantID: "h264")
        store.rememberTask(taskIdentity)
        store.beginPlayback(playbackIdentity)
        let preparingTask = CacheTask.fixture(
            id: taskIdentity.taskID,
            state: "TASK_STATE_PREPARING",
            message: "Preparing."
        )
        let playableTask = CacheTask.playableFixture(id: taskIdentity.taskID, progress: 0.7)
        let client = FakeBilibiliCacheControlClient(
            createResponses: [],
            getTaskResponses: [
                taskIdentity.taskID: [
                    .success(preparingTask),
                    .success(preparingTask),
                    .success(playableTask),
                ]
            ]
        )
        let model = BilibiliTaskViewModel(resumptionStore: store, clientFactory: { _ in client })

        await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)
        guard
            await waitForResumptionPhase(
                "preparing watch subscribed", model: model, client: client,
                until: {
                    await client.watchSubscriptionCountSnapshot() == 1
                })
        else { return }
        XCTAssertFalse(model.canContinueSavedPlayback)
        XCTAssertTrue(model.hasSavedTask)
        XCTAssertEqual(model.currentTask, preparingTask)

        await client.yield(playableTask)
        guard
            await waitForResumptionPhase(
                "watched source validated", model: model, client: client,
                until: {
                    model.canContinueSavedPlayback
                })
        else { return }
        let validatedTaskIDs = await client.getTaskIDsSnapshot()
        XCTAssertEqual(validatedTaskIDs, [taskIdentity.taskID, taskIdentity.taskID, taskIdentity.taskID])
        let serverInfoCount = await client.serverInfoRequestCountSnapshot()
        let taskCount = await client.getTaskIDsSnapshot().count
        await client.yield(
            .playableFixture(
                id: taskIdentity.taskID,
                progress: 0.85,
                downloadedBytes: 85,
                totalBytes: 100
            ))
        guard
            await waitForResumptionPhase(
                "validated source progress accepted", model: model, client: client,
                until: {
                    model.currentTask?.downloadedBytes == 85
                })
        else { return }
        try? await Task.sleep(for: .milliseconds(100))

        XCTAssertTrue(model.canContinueSavedPlayback)
        let finalServerInfoCount = await client.serverInfoRequestCountSnapshot()
        let finalTaskCount = await client.getTaskIDsSnapshot().count
        XCTAssertEqual(finalServerInfoCount, serverInfoCount)
        XCTAssertEqual(finalTaskCount, taskCount)
        let resolved = await client.resolvedRequestsSnapshot()
        let created = await client.createdRequestsSnapshot()
        let cancelled = await client.cancelledIDsSnapshot()
        XCTAssertTrue(resolved.isEmpty)
        XCTAssertTrue(created.isEmpty)
        XCTAssertTrue(cancelled.isEmpty)
        model.clearTask()
    }

    func testRetryRevalidatesSavedSourceThatBecamePlayable() async throws {
        let (defaults, store, suiteName) = try makeResumptionStore()
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let endpoint = try XCTUnwrap(CacheServerEndpoint.normalized(from: "mac-mini.local:50051"))
        let taskIdentity = ClientResumptionTaskIdentity(
            endpoint: endpoint,
            serverID: "fake-cache-server",
            taskID: "restore-retry-playable"
        )
        let playbackIdentity = ClientPlaybackIdentity(task: taskIdentity, variantID: "h264")
        store.rememberTask(taskIdentity)
        store.beginPlayback(
            playbackIdentity,
            checkpoint: ClientPlaybackCheckpoint(
                identity: playbackIdentity,
                positionSeconds: 22,
                durationSeconds: 100
            ))
        let preparingTask = CacheTask.fixture(
            id: taskIdentity.taskID,
            state: "TASK_STATE_PREPARING",
            message: "Preparing."
        )
        let playableTask = CacheTask.playableFixture(id: taskIdentity.taskID)
        let client = FakeBilibiliCacheControlClient(
            createResponses: [],
            getTaskResponses: [
                taskIdentity.taskID: [
                    // Each restore or retry is followed by one running-task reconciliation.
                    .success(preparingTask),
                    .success(preparingTask),
                    .success(preparingTask),
                    .success(preparingTask),
                    .success(playableTask),
                    .success(playableTask),
                ]
            ]
        )
        let model = BilibiliTaskViewModel(resumptionStore: store, clientFactory: { _ in client })

        await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)
        guard
            await waitForResumptionPhase(
                "initial retry watch subscribed", model: model, client: client,
                until: {
                    await client.watchSubscriptionCountSnapshot() == 1
                })
        else { return }
        XCTAssertFalse(model.canContinueSavedPlayback)
        XCTAssertTrue(model.hasSavedTask)

        await model.retrySavedTask(serverAddressText: endpoint.displayAddress)
        guard
            await waitForResumptionPhase(
                "unavailable retry watch subscribed", model: model, client: client,
                until: {
                    await client.watchSubscriptionCountSnapshot() == 2
                })
        else { return }
        XCTAssertFalse(model.canContinueSavedPlayback)
        XCTAssertTrue(model.hasSavedTask)

        await model.retrySavedTask(serverAddressText: endpoint.displayAddress)
        guard
            await waitForResumptionPhase(
                "playable retry watch subscribed", model: model, client: client,
                until: {
                    await client.watchSubscriptionCountSnapshot() == 3
                })
        else { return }
        XCTAssertTrue(model.canContinueSavedPlayback)
        let taskIDs = await client.getTaskIDsSnapshot()
        XCTAssertEqual(taskIDs, Array(repeating: taskIdentity.taskID, count: 6))
        let resolved = await client.resolvedRequestsSnapshot()
        let created = await client.createdRequestsSnapshot()
        let cancelled = await client.cancelledIDsSnapshot()
        XCTAssertTrue(resolved.isEmpty)
        XCTAssertTrue(created.isEmpty)
        XCTAssertTrue(cancelled.isEmpty)
        model.clearTask()
    }

    func testSavedChildOnSecondPagePromotesLibraryIdentityWithoutUIPageLoad() async throws {
        let (defaults, store, suiteName) = try makeResumptionStore()
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let endpoint = try XCTUnwrap(CacheServerEndpoint.normalized(from: "mac-mini.local:50051"))
        let taskIdentity = ClientResumptionTaskIdentity(
            endpoint: endpoint,
            serverID: "fake-cache-server",
            taskID: "restore-child-pages"
        )
        let playbackIdentity = ClientPlaybackIdentity(
            task: taskIdentity,
            resultID: "child-2",
            variantID: "h264"
        )
        store.rememberTask(taskIdentity)
        store.beginPlayback(
            playbackIdentity,
            checkpoint: ClientPlaybackCheckpoint(
                identity: playbackIdentity,
                positionSeconds: 34,
                durationSeconds: 120
            ))
        let summary = CacheTaskOutputSummary(
            revision: 7,
            resultCount: 2,
            terminalResultCount: 2,
            successfulResultCount: 2,
            failedResultCount: 0,
            cancelledResultCount: 0,
            availableArtifactCount: 0,
            primaryResultID: "child-1"
        )
        let task = CacheTask.fixture(
            id: taskIdentity.taskID,
            state: "TASK_STATE_PLAYABLE",
            outputSummary: summary
        )
        let firstResult = CacheTaskResult.fixture(
            id: "child-1",
            title: "First result",
            state: "TASK_RESULT_STATE_COMPLETED",
            libraryItemID: "library-child-1"
        )
        let secondResult = CacheTaskResult.fixture(
            id: "child-2",
            title: "Second result",
            state: "TASK_RESULT_STATE_COMPLETED",
            libraryItemID: "library-child-2"
        )
        let firstPage = CacheTaskResultsPage(
            results: [firstResult],
            pageInfo: CachePageInfo(totalSize: 2, nextPageToken: "second", snapshotID: "stable-snapshot"),
            outputRevision: 7
        )
        let secondPage = CacheTaskResultsPage(
            results: [secondResult],
            pageInfo: CachePageInfo(totalSize: 2, nextPageToken: "", snapshotID: "stable-snapshot"),
            outputRevision: 7
        )
        let freshSource = CachePlaybackSource(
            itemID: "library-child-2",
            variantID: "h264",
            playbackProtocol: "PLAYBACK_PROTOCOL_HLS",
            uri: "http://mac-mini.local:8080/hls/library-child-2/master.m3u8"
        )
        let client = FakeBilibiliCacheControlClient(
            createResponses: [],
            getTaskResponses: [taskIdentity.taskID: [.success(task), .success(task), .success(task)]],
            playbackSourceResponses: ["library-child-2\u{1F}h264": freshSource]
        )
        for _ in 0..<3 {
            await client.enqueueTaskResultPage(taskID: taskIdentity.taskID, pageToken: "", result: .success(firstPage))
        }
        for _ in 0..<2 {
            await client.enqueueTaskResultPage(
                taskID: taskIdentity.taskID,
                pageToken: "second",
                result: .success(secondPage)
            )
        }
        let model = BilibiliTaskViewModel(resumptionStore: store, clientFactory: { _ in client })
        defer { model.clearTask() }

        await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)
        guard
            await waitForResumptionPhase(
                "child restore watch subscribed", model: model, client: client,
                until: {
                    await client.watchSubscriptionCountSnapshot() == 1
                })
        else { return }
        guard
            await waitForResumptionPhase(
                "first UI result page loaded", model: model, client: client,
                until: {
                    model.taskResults.first?.id == "child-1"
                })
        else { return }
        XCTAssertFalse(model.taskResults.contains(where: { $0.id == "child-2" }))
        XCTAssertTrue(model.canContinueSavedPlayback)

        let prepared = await model.prepareSavedPlayback(serverAddressText: endpoint.displayAddress)
        let unwrappedPrepared = try XCTUnwrap(prepared)
        XCTAssertEqual(unwrappedPrepared.checkpoint.identity, playbackIdentity)
        XCTAssertEqual(unwrappedPrepared.progressContext.resumptionIdentity, playbackIdentity)
        XCTAssertEqual(unwrappedPrepared.progressContext.libraryItemID, "library-child-2")
        XCTAssertEqual(unwrappedPrepared.url, freshSource.explicitHTTPURL)

        model.finishPreparedResumption(unwrappedPrepared, didStartPlayback: true)
        XCTAssertTrue(model.isActivePlaybackLibraryItem(id: "library-child-2"))
        await client.yield(
            .fixture(
                id: taskIdentity.taskID,
                state: "TASK_STATE_PLAYABLE",
                progress: 0.8,
                downloadedBytes: 80,
                totalBytes: 100,
                outputSummary: summary
            ))
        guard
            await waitForResumptionPhase(
                "child watch progress accepted", model: model, client: client,
                until: {
                    model.currentTask?.downloadedBytes == 80
                })
        else { return }
        XCTAssertTrue(model.isActivePlaybackLibraryItem(id: "library-child-2"))
        let sourceRequests = await client.playbackSourceRequestsSnapshot()
        XCTAssertTrue(sourceRequests.allSatisfy { $0.itemID == "library-child-2" && $0.variantID == "h264" })
        XCTAssertTrue(model.clearTaskIfCachedLibraryItemDeleted(id: "library-child-2"))
        XCTAssertFalse(model.canContinueSavedPlayback)
        XCTAssertTrue(model.hasSavedTask)
        XCTAssertEqual(store.record?.playback?.identity, playbackIdentity)
        XCTAssertTrue(model.resumptionMessage?.contains("cache was deleted") == true)
        model.clearTask()
    }

    func testSavedMediaGuardRejectsWrongPortAndDifferentOrigin() async throws {
        for uri in [
            "http://mac-mini.local:8081/hls/restore-media-guard/master.m3u8",
            "http://other-origin.local:8080/hls/restore-media-guard/master.m3u8",
        ] {
            let (defaults, store, suiteName) = try makeResumptionStore()
            defer { defaults.removePersistentDomain(forName: suiteName) }
            let endpoint = try XCTUnwrap(CacheServerEndpoint.normalized(from: "mac-mini.local:50051"))
            let taskIdentity = ClientResumptionTaskIdentity(
                endpoint: endpoint,
                serverID: "fake-cache-server",
                taskID: "restore-media-guard"
            )
            let playbackIdentity = ClientPlaybackIdentity(task: taskIdentity, variantID: "h264")
            store.rememberTask(taskIdentity)
            store.beginPlayback(playbackIdentity)
            let unsafeTask = CacheTask.fixture(
                id: taskIdentity.taskID,
                state: "TASK_STATE_PLAYABLE",
                playbackSource: CachePlaybackSource(
                    itemID: taskIdentity.taskID,
                    variantID: "h264",
                    playbackProtocol: "PLAYBACK_PROTOCOL_HLS",
                    uri: uri
                )
            )
            let client = FakeBilibiliCacheControlClient(
                createResponses: [],
                getTaskResponses: [taskIdentity.taskID: [.success(unsafeTask), .success(unsafeTask)]]
            )
            let model = BilibiliTaskViewModel(resumptionStore: store, clientFactory: { _ in client })

            await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)
            guard
                await waitForResumptionPhase(
                    "rejected media watch subscribed", model: model, client: client,
                    until: {
                        await client.watchSubscriptionCountSnapshot() == 1
                    })
            else { return }

            XCTAssertFalse(model.canContinueSavedPlayback)
            XCTAssertTrue(model.resumptionMessage?.contains("not an allowed LAN media URL") == true)
            XCTAssertTrue(model.hasSavedTask)
            model.clearTask()
        }
    }

    func testDeletingSavedCacheSourceKeepsRecordButDisablesContinue() async throws {
        let (defaults, store, suiteName) = try makeResumptionStore()
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let endpoint = try XCTUnwrap(CacheServerEndpoint.normalized(from: "mac-mini.local:50051"))
        let taskIdentity = ClientResumptionTaskIdentity(
            endpoint: endpoint,
            serverID: "fake-cache-server",
            taskID: "restore-deleted-library"
        )
        let playbackIdentity = ClientPlaybackIdentity(
            task: taskIdentity,
            libraryItemID: "library-root",
            variantID: "h264"
        )
        store.rememberTask(taskIdentity)
        store.beginPlayback(playbackIdentity)
        let cachedSource = CachePlaybackSource(
            itemID: "library-root",
            variantID: "h264",
            playbackProtocol: "PLAYBACK_PROTOCOL_HLS",
            uri: "http://mac-mini.local:8080/hls/library-root/master.m3u8"
        )
        let task = CacheTask.playableFixture(
            id: taskIdentity.taskID,
            state: "TASK_STATE_COMPLETED",
            libraryItemID: "library-root",
            playbackSourceItemID: "library-root"
        )
        let client = FakeBilibiliCacheControlClient(
            createResponses: [],
            getTaskResponses: [taskIdentity.taskID: [.success(task)]],
            playbackSourceResponses: ["library-root\u{1F}h264": cachedSource]
        )
        let model = BilibiliTaskViewModel(resumptionStore: store, clientFactory: { _ in client })

        await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)
        XCTAssertTrue(model.canContinueSavedPlayback)

        XCTAssertTrue(model.clearTaskIfCachedLibraryItemDeleted(id: "library-root"))

        XCTAssertFalse(model.canContinueSavedPlayback)
        XCTAssertTrue(model.hasSavedTask)
        XCTAssertEqual(store.record?.playback?.identity, playbackIdentity)
        XCTAssertTrue(model.resumptionMessage?.contains("cache was deleted") == true)
        model.forgetSavedTask()
        XCTAssertFalse(model.hasSavedTask)
    }

    private func assertTerminalExactChildState(
        usesV2Output: Bool,
        state: String,
        shouldRestore: Bool,
        file: StaticString = #filePath,
        line: UInt = #line
    ) async throws {
        let (defaults, store, suiteName) = try makeResumptionStore()
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let endpoint = try XCTUnwrap(CacheServerEndpoint.normalized(from: "mac-mini.local:50051"))
        let taskIdentity = ClientResumptionTaskIdentity(
            endpoint: endpoint, serverID: "fake-cache-server", taskID: "terminal-exact-child"
        )
        let libraryItemID = shouldRestore ? "exact-child-library" : ""
        let sourceItemID = shouldRestore ? libraryItemID : "exact-child"
        let child = BilibiliTaskResultItem.fixture(
            id: "exact-child", state: state, libraryItemID: libraryItemID, playbackSourceItemID: sourceItemID
        )
        let sibling = BilibiliTaskResultItem.fixture(
            id: "other-child", state: "succeeded", libraryItemID: "other-library", playbackSourceItemID: "other-library"
        )
        let source = try XCTUnwrap(child.playbackSource)
        let playbackIdentity = ClientPlaybackIdentity(
            task: taskIdentity, resultID: child.id, libraryItemID: libraryItemID, variantID: "h264"
        )
        store.rememberTask(taskIdentity)
        store.beginPlayback(playbackIdentity)
        let summary = CacheTaskOutputSummary(
            revision: 7,
            resultCount: 2,
            terminalResultCount: state == "unrecognized(99)" ? 1 : 2,
            successfulResultCount: shouldRestore ? 2 : 1,
            failedResultCount: state == "failed" ? 1 : 0,
            cancelledResultCount: state == "cancelled" ? 1 : 0,
            availableArtifactCount: 0,
            primaryResultID: sibling.id
        )
        let task = CacheTask.fixture(
            id: taskIdentity.taskID,
            state: "succeeded",
            resultItems: usesV2Output ? [] : [sibling, child],
            outputSummary: usesV2Output ? summary : nil
        )
        let client = FakeBilibiliCacheControlClient(
            createResponses: [],
            getTaskResponses: [taskIdentity.taskID: [.success(task), .success(task)]],
            playbackSourceResponses: ["exact-child-library\u{1F}h264": source]
        )
        if usesV2Output {
            let page = CacheTaskResultsPage(
                results: [
                    .fixture(
                        id: sibling.id, title: sibling.title, state: "succeeded", libraryItemID: sibling.libraryItemID,
                        playbackSource: sibling.playbackSource),
                    .fixture(
                        id: child.id, title: child.title, state: state, libraryItemID: libraryItemID,
                        playbackSource: source),
                ],
                pageInfo: CachePageInfo(totalSize: 2, nextPageToken: "", snapshotID: "terminal-child-snapshot"),
                outputRevision: 7
            )
            for _ in 0..<3 {
                await client.enqueueTaskResultPage(taskID: taskIdentity.taskID, pageToken: "", result: .success(page))
            }
        }
        let model = BilibiliTaskViewModel(resumptionStore: store, clientFactory: { _ in client })
        defer { model.clearTask() }
        await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)
        guard
            await waitForResumptionPhase(
                "terminal child presentation", model: model, client: client, file: file, line: line,
                until: {
                    model.taskResults.contains { $0.id == child.id }
                })
        else { return }
        let presentation = try XCTUnwrap(model.taskResults.first { $0.id == child.id })
        XCTAssertEqual(presentation.isReady, shouldRestore, file: file, line: line)
        XCTAssertEqual(presentation.isCached, shouldRestore, file: file, line: line)
        XCTAssertEqual(model.canPlay(result: presentation), shouldRestore, file: file, line: line)
        XCTAssertEqual(model.canContinueSavedPlayback, shouldRestore, file: file, line: line)
        XCTAssertFalse(model.isWatching, file: file, line: line)
        if shouldRestore {
            XCTAssertEqual(model.playableURL(for: presentation), source.explicitHTTPURL, file: file, line: line)
            let context = model.playbackProgressContext(for: presentation, serverAddressText: endpoint.displayAddress)
            XCTAssertEqual(context?.libraryItemID, libraryItemID, file: file, line: line)
            model.finishPreparedPlayback(result: presentation, didStartPlayback: true)
            XCTAssertTrue(model.isActivePlaybackLibraryItem(id: libraryItemID), file: file, line: line)
        }
        let prepared = await model.prepareSavedPlayback(serverAddressText: endpoint.displayAddress)
        if shouldRestore {
            XCTAssertNotNil(prepared, file: file, line: line)
            XCTAssertEqual(prepared?.url, source.explicitHTTPURL, file: file, line: line)
            XCTAssertNotEqual(prepared?.url, sibling.playbackSource?.explicitHTTPURL, file: file, line: line)
            XCTAssertEqual(prepared?.checkpoint.identity, playbackIdentity, file: file, line: line)
            XCTAssertEqual(prepared?.progressContext.libraryItemID, libraryItemID, file: file, line: line)
        } else {
            XCTAssertNil(prepared, file: file, line: line)
            XCTAssertFalse(model.canContinueSavedPlayback, file: file, line: line)
        }
        XCTAssertEqual(store.record?.playback?.identity, playbackIdentity, file: file, line: line)
        let taskIDs = await client.getTaskIDsSnapshot()
        let results = await client.taskResultRequestsSnapshot()
        let sources = await client.playbackSourceRequestsSnapshot()
        let resolved = await client.resolvedRequestsSnapshot()
        let created = await client.createdRequestsSnapshot()
        let cancelled = await client.cancelledIDsSnapshot()
        XCTAssertEqual(taskIDs, [taskIdentity.taskID, taskIdentity.taskID], file: file, line: line)
        XCTAssertEqual(results.count, usesV2Output ? 3 : 0, file: file, line: line)
        XCTAssertEqual(sources.count, shouldRestore ? 2 : 0, file: file, line: line)
        XCTAssertTrue(
            sources.allSatisfy { $0.itemID == libraryItemID && $0.variantID == "h264" }, file: file, line: line)
        XCTAssertTrue(resolved.isEmpty, file: file, line: line)
        XCTAssertTrue(created.isEmpty, file: file, line: line)
        XCTAssertTrue(cancelled.isEmpty, file: file, line: line)
    }

    private func assertSavedWatchSourceChangeRevalidation(
        becomesUnplayable: Bool,
        file: StaticString = #filePath,
        line: UInt = #line
    ) async throws {
        let (defaults, store, suiteName) = try makeResumptionStore()
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let endpoint = try XCTUnwrap(CacheServerEndpoint.normalized(from: "mac-mini.local:50051"))
        let taskIdentity = ClientResumptionTaskIdentity(
            endpoint: endpoint,
            serverID: "fake-cache-server",
            taskID: becomesUnplayable ? "saved-watch-unplayable" : "saved-watch-source-change"
        )
        let playbackIdentity = ClientPlaybackIdentity(task: taskIdentity, variantID: "h264")
        store.rememberTask(taskIdentity)
        store.beginPlayback(playbackIdentity)
        let initialTask = CacheTask.playableFixture(id: taskIdentity.taskID, progress: 0.4)
        let freshSource = CachePlaybackSource(
            itemID: taskIdentity.taskID,
            variantID: "h264",
            playbackProtocol: "PLAYBACK_PROTOCOL_HLS",
            uri: "http://mac-mini.local:8080/hls/\(taskIdentity.taskID)/revalidated.m3u8"
        )
        let changedTask = CacheTask.fixture(
            id: taskIdentity.taskID,
            state: "TASK_STATE_PLAYABLE",
            progress: 0.5,
            downloadedBytes: 50,
            totalBytes: 100,
            playbackSource: becomesUnplayable ? nil : freshSource
        )
        let recoveredTask = CacheTask.fixture(
            id: taskIdentity.taskID,
            state: "TASK_STATE_PLAYABLE",
            progress: 0.9,
            downloadedBytes: 90,
            totalBytes: 100,
            playbackSource: freshSource
        )
        var responses: [Result<CacheTask, Error>] = [
            .success(initialTask), .success(initialTask), .success(changedTask),
        ]
        if becomesUnplayable {
            responses.append(.success(recoveredTask))
        }
        let client = FakeBilibiliCacheControlClient(
            createResponses: [],
            getTaskResponses: [taskIdentity.taskID: responses]
        )
        let model = BilibiliTaskViewModel(resumptionStore: store, clientFactory: { _ in client })
        defer { model.clearTask() }
        await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)
        guard
            await waitForResumptionPhase(
                "source-change bootstrap", model: model, client: client,
                until: {
                    await client.watchSubscriptionCountSnapshot() == 1
                })
        else { return }
        XCTAssertTrue(model.canContinueSavedPlayback, file: file, line: line)
        let infoBaseline = await client.serverInfoRequestCountSnapshot()
        let taskBaseline = await client.getTaskIDsSnapshot().count

        await client.setSuspendsServerInfoResponses(true)
        await client.yield(changedTask)
        guard
            await waitForResumptionPhase(
                "changed source awaiting validation", model: model, client: client,
                until: {
                    let pending = await client.pendingResumptionResponseCountsSnapshot()
                    return await client.serverInfoRequestCountSnapshot() == infoBaseline + 1
                        && pending.serverInfo == 1 && model.currentTask?.downloadedBytes == 50
                })
        else { return }
        XCTAssertFalse(model.canContinueSavedPlayback, file: file, line: line)
        XCTAssertTrue(model.isRestoringTask, file: file, line: line)
        XCTAssertEqual(store.record?.playback?.identity, playbackIdentity, file: file, line: line)
        store.updatePlayback(
            identity: playbackIdentity,
            positionSeconds: 81,
            durationSeconds: 240,
            completed: true
        )
        let latestCheckpoint = try XCTUnwrap(store.record?.playback)
        XCTAssertTrue(latestCheckpoint.completed, file: file, line: line)

        await client.yield(
            .fixture(
                id: taskIdentity.taskID,
                state: "TASK_STATE_PLAYABLE",
                progress: 0.6,
                downloadedBytes: 60,
                totalBytes: 100,
                playbackSource: changedTask.playbackSource
            ))
        guard
            await waitForResumptionPhase(
                "progress during source validation", model: model, client: client,
                until: {
                    model.currentTask?.downloadedBytes == 60
                })
        else { return }
        let heldInfoCount = await client.serverInfoRequestCountSnapshot()
        let heldTaskCount = await client.getTaskIDsSnapshot().count
        XCTAssertEqual(heldInfoCount, infoBaseline + 1, file: file, line: line)
        XCTAssertEqual(heldTaskCount, taskBaseline, file: file, line: line)
        let heldPending = await client.pendingResumptionResponseCountsSnapshot()
        XCTAssertEqual(heldPending.serverInfo, 1, file: file, line: line)
        XCTAssertFalse(model.canContinueSavedPlayback, file: file, line: line)
        await client.setSuspendsServerInfoResponses(false)
        await client.completeNextServerInfo(with: .success(.fixture(mediaBaseURIs: ["http://mac-mini.local:8080"])))
        guard
            await waitForResumptionPhase(
                "changed source validation completed", model: model, client: client,
                until: {
                    let taskCount = await client.getTaskIDsSnapshot().count
                    return !model.isRestoringTask && taskCount == taskBaseline + 1
                })
        else { return }
        XCTAssertEqual(model.canContinueSavedPlayback, !becomesUnplayable, file: file, line: line)
        XCTAssertEqual(store.record?.playback, latestCheckpoint, file: file, line: line)

        if !becomesUnplayable {
            let url = try XCTUnwrap(freshSource.explicitHTTPURL)
            let progressContext = PlayerPlaybackProgressContext(
                endpoint: endpoint,
                playbackURI: url.absoluteString,
                libraryItemID: "",
                variantID: "h264",
                resumptionIdentity: playbackIdentity
            )
            model.finishPreparedResumption(
                ClientPreparedResumption(
                    url: url,
                    progressContext: progressContext,
                    checkpoint: latestCheckpoint
                ),
                didStartPlayback: true
            )
            XCTAssertEqual(model.statusMessage, "Playing Ready video.", file: file, line: line)
            XCTAssertNil(model.resumptionMessage, file: file, line: line)
        }

        if becomesUnplayable {
            XCTAssertTrue(model.resumptionMessage?.contains("not playable yet") == true, file: file, line: line)
            await client.yield(recoveredTask)
            guard
                await waitForResumptionPhase(
                    "selected source playable again", model: model, client: client,
                    until: {
                        model.canContinueSavedPlayback && model.currentTask?.downloadedBytes == 90
                    })
            else { return }
        }
        let validatedInfoCount = await client.serverInfoRequestCountSnapshot()
        let validatedTaskIDs = await client.getTaskIDsSnapshot()
        let validationCount = becomesUnplayable ? 2 : 1
        XCTAssertEqual(validatedInfoCount, infoBaseline + validationCount, file: file, line: line)
        XCTAssertEqual(
            validatedTaskIDs, Array(repeating: taskIdentity.taskID, count: taskBaseline + validationCount), file: file,
            line: line)
        await client.yield(
            .fixture(
                id: taskIdentity.taskID,
                state: "TASK_STATE_PLAYABLE",
                progress: 0.95,
                downloadedBytes: 95,
                totalBytes: 100,
                playbackSource: freshSource
            ))
        guard
            await waitForResumptionPhase(
                "progress after source revalidation", model: model, client: client,
                until: {
                    model.currentTask?.downloadedBytes == 95 && model.canContinueSavedPlayback
                })
        else { return }
        try? await Task.sleep(for: .milliseconds(100))
        let finalInfoCount = await client.serverInfoRequestCountSnapshot()
        let finalTaskIDs = await client.getTaskIDsSnapshot()
        let sourceRequests = await client.playbackSourceRequestsSnapshot()
        let resolved = await client.resolvedRequestsSnapshot()
        let created = await client.createdRequestsSnapshot()
        let cancelled = await client.cancelledIDsSnapshot()
        XCTAssertEqual(finalInfoCount, validatedInfoCount, file: file, line: line)
        XCTAssertEqual(finalTaskIDs, validatedTaskIDs, file: file, line: line)
        XCTAssertTrue(sourceRequests.isEmpty, file: file, line: line)
        XCTAssertTrue(resolved.isEmpty, file: file, line: line)
        XCTAssertTrue(created.isEmpty, file: file, line: line)
        XCTAssertTrue(cancelled.isEmpty, file: file, line: line)
    }

    private func assertEmbeddedFailedSavedChildRestoration(
        message: String,
        shouldRestore: Bool,
        file: StaticString = #filePath,
        line: UInt = #line
    ) async throws {
        let (defaults, store, suiteName) = try makeResumptionStore()
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let endpoint = try XCTUnwrap(CacheServerEndpoint.normalized(from: "mac-mini.local:50051"))
        let taskIdentity = ClientResumptionTaskIdentity(
            endpoint: endpoint,
            serverID: "fake-cache-server",
            taskID: "embedded-failed-child"
        )
        let child = BilibiliTaskResultItem.fixture(
            id: "exact-saved-child",
            state: "TASK_STATE_FAILED",
            message: message
        )
        let sibling = BilibiliTaskResultItem.fixture(id: "other-playable-child")
        let playbackIdentity = ClientPlaybackIdentity(task: taskIdentity, resultID: child.id, variantID: "h264")
        store.rememberTask(taskIdentity)
        store.beginPlayback(playbackIdentity)
        let task = CacheTask.fixture(
            id: taskIdentity.taskID,
            state: "TASK_STATE_PLAYABLE",
            resultItems: [sibling, child]
        )
        XCTAssertNil(task.outputSummary, file: file, line: line)
        let client = FakeBilibiliCacheControlClient(
            createResponses: [],
            getTaskResponses: [taskIdentity.taskID: [.success(task), .success(task), .success(task)]]
        )
        let model = BilibiliTaskViewModel(resumptionStore: store, clientFactory: { _ in client })
        defer { model.clearTask() }
        await model.restoreSavedTask(serverAddressText: endpoint.displayAddress)
        guard
            await waitForResumptionPhase(
                "embedded child watch subscribed", model: model, client: client,
                until: {
                    await client.watchSubscriptionCountSnapshot() == 1
                })
        else { return }
        XCTAssertEqual(model.canContinueSavedPlayback, shouldRestore, file: file, line: line)
        let prepared = await model.prepareSavedPlayback(serverAddressText: endpoint.displayAddress)
        if shouldRestore {
            XCTAssertNotNil(prepared, file: file, line: line)
            XCTAssertEqual(prepared?.url, child.playbackSource?.explicitHTTPURL, file: file, line: line)
            XCTAssertNotEqual(prepared?.url, sibling.playbackSource?.explicitHTTPURL, file: file, line: line)
            XCTAssertEqual(prepared?.checkpoint.identity, playbackIdentity, file: file, line: line)
            XCTAssertEqual(prepared?.progressContext.resumptionIdentity, playbackIdentity, file: file, line: line)
            XCTAssertEqual(prepared?.progressContext.variantID, "h264", file: file, line: line)
        } else {
            XCTAssertNil(prepared, file: file, line: line)
            XCTAssertFalse(model.canContinueSavedPlayback, file: file, line: line)
            XCTAssertTrue(model.resumptionMessage?.contains("not playable yet") == true, file: file, line: line)
        }
        XCTAssertTrue(model.hasSavedTask, file: file, line: line)
        XCTAssertEqual(store.record?.playback?.identity, playbackIdentity, file: file, line: line)
        let taskIDs = await client.getTaskIDsSnapshot()
        let results = await client.taskResultRequestsSnapshot()
        let sources = await client.playbackSourceRequestsSnapshot()
        let resolved = await client.resolvedRequestsSnapshot()
        let created = await client.createdRequestsSnapshot()
        let cancelled = await client.cancelledIDsSnapshot()
        XCTAssertEqual(taskIDs, Array(repeating: taskIdentity.taskID, count: 3), file: file, line: line)
        XCTAssertTrue(results.isEmpty, file: file, line: line)
        XCTAssertTrue(sources.isEmpty, file: file, line: line)
        XCTAssertTrue(resolved.isEmpty, file: file, line: line)
        XCTAssertTrue(created.isEmpty, file: file, line: line)
        XCTAssertTrue(cancelled.isEmpty, file: file, line: line)
    }

    private func waitForResumptionPhase(
        _ phase: String,
        model: BilibiliTaskViewModel,
        client: FakeBilibiliCacheControlClient,
        operations: [ResumptionTestOperation] = [],
        file: StaticString = #filePath,
        line: UInt = #line,
        until condition: @MainActor () async -> Bool
    ) async -> Bool {
        if await waitForTaskWatchCondition(timeout: .seconds(2), condition) {
            return true
        }

        let serverInfoCount = await client.serverInfoRequestCountSnapshot()
        let taskIDs = await client.getTaskIDsSnapshot()
        let watchCount = await client.watchSubscriptionCountSnapshot()
        let pending = await client.pendingResumptionResponseCountsSnapshot()
        XCTFail(
            "\(phase): task=\(model.currentTask?.id ?? "nil") restoring=\(model.isRestoringTask) "
                + "watching=\(model.isWatching) info=\(serverInfoCount) getTask=\(taskIDs) watches=\(watchCount) "
                + "held=\(pending) message=\(model.resumptionMessage ?? model.watchErrorMessage ?? model.statusMessage)",
            file: file,
            line: line
        )
        operations.forEach { $0.cancel() }
        model.clearTask()
        await client.cancelPendingResumptionResponses()
        let operationsFinished = await waitForTaskWatchCondition(timeout: .seconds(2)) {
            operations.allSatisfy { $0.isFinished }
        }
        XCTAssertTrue(operationsFinished, "\(phase): cancelled operation did not finish", file: file, line: line)
        return false
    }

    private func makeResumptionStore() throws -> (
        defaults: UserDefaults, store: ClientResumptionStore, suiteName: String
    ) {
        let suiteName = "BilibiliTaskViewModelResumption-\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defaults.removePersistentDomain(forName: suiteName)
        return (defaults, ClientResumptionStore(defaults: defaults), suiteName)
    }

    private func waitUntil(
        _ condition: @autoclosure @escaping @MainActor () -> Bool,
        file: StaticString = #filePath,
        line: UInt = #line
    ) async {
        for _ in 0..<100 {
            if condition() {
                return
            }
            try? await Task.sleep(nanoseconds: 10_000_000)
        }

        XCTAssertTrue(condition(), file: file, line: line)
    }

    private func makeCandidates(count: Int, prefix: String) -> [BilibiliResolvedCandidate] {
        (1...count).map { index in
            .fixture(
                selectionID: "\(prefix)-token-\(index)",
                title: "Item \(index)",
                index: index
            )
        }
    }
}

@MainActor
private final class ResumptionTestOperation {
    private var task: Task<Void, Never>?
    private(set) var isFinished = false
    private(set) var prepared: ClientPreparedResumption?

    init(_ operation: @escaping @MainActor @Sendable () async -> ClientPreparedResumption?) {
        task = Task { @MainActor [weak self] in
            let prepared = await operation()
            self?.prepared = prepared
            self?.isFinished = true
        }
    }

    func cancel() {
        task?.cancel()
    }
}

private struct FakeBilibiliTaskV2Request: Sendable {
    let sessionID: String
    let selection: BilibiliResolutionSelection
    let execution: BilibiliTaskExecution
}

private struct FakeCandidatePageCursor: Sendable {
    let sessionID: String
    let offset: Int
}

private actor TaskWatchRecoverySleepRecorder {
    private var delays: [Duration] = []
    private var waiters: [(count: Int, continuation: CheckedContinuation<Void, Never>)] = []

    func record(_ delay: Duration) {
        delays.append(delay)
        let ready = waiters.filter { $0.count <= delays.count }
        waiters.removeAll { $0.count <= delays.count }
        ready.forEach { $0.continuation.resume() }
    }

    func waitForCount(_ count: Int) async {
        guard delays.count < count else {
            return
        }

        await withCheckedContinuation { continuation in
            waiters.append((count, continuation))
        }
    }

    func delaysSnapshot() -> [Duration] {
        delays
    }
}

private final class TaskWatchReconciliationClock: @unchecked Sendable {
    private let lock = NSLock()
    private var instant = ContinuousClock().now

    func now() -> ContinuousClock.Instant {
        lock.withLock { instant }
    }

    func advance(by duration: Duration) {
        lock.withLock { instant = instant.advanced(by: duration) }
    }

    func advance(to deadline: ContinuousClock.Instant) {
        lock.withLock { instant = max(instant, deadline) }
    }
}

private actor TaskWatchReconciliationSleepGate {
    nonisolated let clock = TaskWatchReconciliationClock()
    private var intervals: [Duration] = []
    private var sleepers:
        [(id: UUID, deadline: ContinuousClock.Instant, continuation: CheckedContinuation<Void, Never>)] = []
    private var activeSleeperWaiters: [CheckedContinuation<Void, Never>] = []
    private var sleepCountWaiters: [(count: Int, continuation: CheckedContinuation<Void, Never>)] = []
    private var cancellationCountWaiters: [(count: Int, continuation: CheckedContinuation<Void, Never>)] = []
    private var cancellationCount = 0

    func sleep(_ interval: Duration) async throws {
        let id = UUID()
        await withTaskCancellationHandler {
            await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
                intervals.append(interval)
                sleepers.append((id, clock.now().advanced(by: interval), continuation))
                let activeWaiters = activeSleeperWaiters
                activeSleeperWaiters = []
                activeWaiters.forEach { $0.resume() }
                let ready = sleepCountWaiters.filter { $0.count <= intervals.count }
                sleepCountWaiters.removeAll { $0.count <= intervals.count }
                ready.forEach { $0.continuation.resume() }
            }
        } onCancel: {
            Task {
                await self.cancelSleep(id: id)
            }
        }
        try Task.checkCancellation()
    }

    func waitForSleepCount(_ count: Int) async {
        guard intervals.count < count else {
            return
        }
        await withCheckedContinuation { continuation in
            sleepCountWaiters.append((count, continuation))
        }
    }

    func waitForActiveSleeper() async {
        guard sleepers.isEmpty else {
            return
        }
        await withCheckedContinuation { continuation in
            activeSleeperWaiters.append(continuation)
        }
    }

    func advanceNext() {
        guard !sleepers.isEmpty else {
            return
        }
        let sleeper = sleepers.removeFirst()
        clock.advance(to: sleeper.deadline)
        sleeper.continuation.resume()
    }

    func sleepCountSnapshot() -> Int {
        intervals.count
    }

    func intervalsSnapshot() -> [Duration] {
        intervals
    }

    func waitForCancellationCount(_ count: Int) async {
        guard cancellationCount < count else {
            return
        }
        await withCheckedContinuation { continuation in
            cancellationCountWaiters.append((count, continuation))
        }
    }

    private func cancelSleep(id: UUID) {
        guard let index = sleepers.firstIndex(where: { $0.id == id }) else {
            return
        }
        let sleeper = sleepers.remove(at: index)
        cancellationCount += 1
        sleeper.continuation.resume()
        let ready = cancellationCountWaiters.filter { $0.count <= cancellationCount }
        cancellationCountWaiters.removeAll { $0.count <= cancellationCount }
        ready.forEach { $0.continuation.resume() }
    }
}

private actor FakeBilibiliCacheControlClient: CacheControlClient {
    private var resolveResponses: [Result<BilibiliResolveResult, Error>]
    private var resolutionStartResponses: [Result<BilibiliResolutionPage, Error>]
    private var createResponses: [Result<CacheTask, Error>]
    private let cancelResponsesByID: [String: CacheTask]
    private let supportsTaskSelection: Bool
    private let supportsBilibiliResolutionV2: Bool
    private let supportsBilibiliExecutionV2: Bool
    private let supportsTaskOutputV2: Bool
    private let mediaBaseURIs: [String]
    private var serverID = "fake-cache-server"
    private var suspendsResolveResponses: Bool
    private var suspendsCreateResponses: Bool
    private var suspendsTaskResultResponses = false
    private var suspendsCandidatePageResponses = false
    private var suspendsCancelResponses = false
    private var suspendsGetTaskResponses = false
    private var suspendsServerInfoResponses = false
    private var serverInfoRequestCount = 0
    private var serverInfoRequestWaiters: [(count: Int, continuation: CheckedContinuation<Void, Never>)] = []
    private var serverInfoResponseCompletionCount = 0
    private var pendingServerInfoContinuations: [CheckedContinuation<CacheServerSummary, Error>] = []
    private let playbackSourceResponses: [String: CachePlaybackSource]
    private var playbackSourceRequests: [(itemID: String, variantID: String)] = []
    private var resolvedRequests: [(urlOrID: String, options: BilibiliPlaybackTaskOptions)] = []
    private var downloadRequests: [(urlOrID: String, options: BilibiliDownloadTaskOptions)] = []
    private var resolutionPageRequests: [(sessionID: String, pageToken: String, pageSize: Int)] = []
    private var v2TaskRequests: [FakeBilibiliTaskV2Request] = []
    private var legacyResolveCallCount = 0
    private var legacyCreateCallCount = 0
    private var resolutionSessions: [String: BilibiliResolutionSession] = [:]
    private var resolutionCandidates: [String: [BilibiliResolutionCandidate]] = [:]
    private var resolutionSnapshots: [String: String] = [:]
    private var resolutionOptionsBySession: [String: BilibiliPlaybackTaskOptions] = [:]
    private var resolutionSourcesBySession: [String: String] = [:]
    private var candidateIndexesBySession: [String: [String: Int]] = [:]
    private var candidatePageCursors: [String: FakeCandidatePageCursor] = [:]
    private var candidatePageFailureNext: String?
    private var candidatePageCancellationNext = false
    private var candidatePageSnapshotMismatchNext = false
    private var candidatePageSessionTruncationMismatchNext = false
    private var nextResolutionSessionID = 1
    private var createdTasksByID: [String: CacheTask] = [:]
    private var taskResultPagesByRequest: [String: [Result<CacheTaskResultsPage, Error>]] = [:]
    private var taskResultRequests: [(taskID: String, pageToken: String, pageSize: Int)] = []
    private var pendingTaskResultContinuations: [CheckedContinuation<CacheTaskResultsPage, Error>] = []
    private var pendingCandidatePageContinuations:
        [(page: BilibiliResolutionPage, continuation: CheckedContinuation<BilibiliResolutionPage, Error>)] = []
    private var taskResultRequestWaiters: [(count: Int, continuation: CheckedContinuation<Void, Never>)] = []
    private var candidatePageRequestWaiters: [(count: Int, continuation: CheckedContinuation<Void, Never>)] = []
    private var createdRequests:
        [(
            urlOrID: String,
            selectionID: String?,
            selection: BilibiliTaskSelection?,
            options: BilibiliPlaybackTaskOptions
        )] = []
    private var cancelledIDs: [String] = []
    private var pendingResolveContinuations: [CheckedContinuation<BilibiliResolveResult, Error>] = []
    private var pendingCreateContinuations: [CheckedContinuation<CacheTask, Error>] = []
    private var pendingCancelContinuations: [CheckedContinuation<CacheTask, Error>] = []
    private var pendingGetTaskContinuations: [CheckedContinuation<CacheTask, Error>] = []
    private var resolveRequestWaiters: [(count: Int, continuation: CheckedContinuation<Void, Never>)] = []
    private var createRequestWaiters: [(count: Int, continuation: CheckedContinuation<Void, Never>)] = []
    private var cancelRequestWaiters: [(count: Int, continuation: CheckedContinuation<Void, Never>)] = []
    private var watchContinuations: [AsyncThrowingStream<CacheTask, Error>.Continuation] = []
    private var getTaskResponses: [String: [Result<CacheTask, Error>]] = [:]
    private var getTaskIDs: [String] = []
    private var getTaskRequestWaiters: [(count: Int, continuation: CheckedContinuation<Void, Never>)] = []
    private var getTaskResponseCompletionCount = 0
    private var getTaskResponseCompletionWaiters: [(count: Int, continuation: CheckedContinuation<Void, Never>)] = []
    private var watchSubscriptionCount = 0
    private var watchWaiters: [(count: Int, continuation: CheckedContinuation<Void, Never>)] = []
    private var watchTerminationCount = 0
    private var watchTerminationWaiters: [CheckedContinuation<Void, Never>] = []

    init(
        resolveResponses: [Result<BilibiliResolveResult, Error>] = [],
        resolutionStartResponses: [Result<BilibiliResolutionPage, Error>] = [],
        createResponses: [Result<CacheTask, Error>],
        getTaskResponses: [String: [Result<CacheTask, Error>]] = [:],
        suspendsGetTaskResponses: Bool = false,
        cancelResponsesByID: [String: CacheTask] = [:],
        supportsTaskSelection: Bool = true,
        supportsBilibiliResolutionV2: Bool = true,
        supportsBilibiliExecutionV2: Bool = true,
        supportsTaskOutputV2: Bool = true,
        taskResultPagesByTaskID: [String: [CacheTaskResultsPage]] = [:],
        playbackSourceResponses: [String: CachePlaybackSource] = [:],
        mediaBaseURIs: [String] = ["http://mac-mini.local:8080"],
        suspendsResolveResponses: Bool = false,
        suspendsCreateResponses: Bool = false,
        suspendsTaskResultResponses: Bool = false,
        suspendsServerInfoResponses: Bool = false
    ) {
        self.resolveResponses = resolveResponses
        self.resolutionStartResponses = resolutionStartResponses
        self.createResponses = createResponses
        self.getTaskResponses = getTaskResponses
        self.suspendsGetTaskResponses = suspendsGetTaskResponses
        self.cancelResponsesByID = cancelResponsesByID
        self.supportsTaskSelection = supportsTaskSelection
        self.supportsBilibiliResolutionV2 = supportsBilibiliResolutionV2
        self.supportsBilibiliExecutionV2 = supportsBilibiliExecutionV2
        self.supportsTaskOutputV2 = supportsTaskOutputV2
        self.playbackSourceResponses = playbackSourceResponses
        self.mediaBaseURIs = mediaBaseURIs
        self.suspendsServerInfoResponses = suspendsServerInfoResponses
        self.suspendsTaskResultResponses = suspendsTaskResultResponses
        self.suspendsResolveResponses = suspendsResolveResponses
        self.suspendsCreateResponses = suspendsCreateResponses
        for (taskID, pages) in taskResultPagesByTaskID {
            var pageToken = ""
            for page in pages {
                taskResultPagesByRequest[Self.taskResultRequestKey(taskID: taskID, pageToken: pageToken), default: []]
                    .append(.success(page))
                pageToken = page.nextPageToken
            }
        }
    }

    func setServerID(_ serverID: String) {
        self.serverID = serverID
    }

    func getServerInfo() async throws -> CacheServerSummary {
        serverInfoRequestCount += 1
        let ready = serverInfoRequestWaiters.filter { $0.count <= serverInfoRequestCount }
        serverInfoRequestWaiters.removeAll { $0.count <= serverInfoRequestCount }
        ready.forEach { $0.continuation.resume() }
        do {
            let response: CacheServerSummary
            if suspendsServerInfoResponses {
                response = try await withCheckedThrowingContinuation { continuation in
                    pendingServerInfoContinuations.append(continuation)
                }
            } else {
                response = makeServerInfo()
            }
            recordServerInfoResponseCompletion()
            return response
        } catch {
            recordServerInfoResponseCompletion()
            throw error
        }
    }

    private func recordServerInfoResponseCompletion() {
        serverInfoResponseCompletionCount += 1
    }

    private func makeServerInfo() -> CacheServerSummary {
        var capabilities: [String] = []
        if supportsBilibiliResolutionV2 {
            capabilities.append(CacheServerCapability.bilibiliResolutionV2)
        }
        if supportsBilibiliExecutionV2 {
            capabilities.append(CacheServerCapability.bilibiliExecutionV2)
        }
        if supportsTaskOutputV2 {
            capabilities.append(CacheServerCapability.taskOutputV2)
        }
        return CacheServerSummary(
            id: serverID,
            name: "Fake cache server",
            version: "test",
            mediaBaseURIs: mediaBaseURIs,
            capabilities: capabilities
        )
    }

    func listCacheRoots() async throws -> [CacheRoot] {
        throw FakeBilibiliCacheControlClientError.notImplemented
    }

    func listLibraryItemsPage(
        pageToken: String,
        pageSize: Int,
        searchText: String?
    ) async throws -> CacheLibraryItemsPage {
        throw FakeBilibiliCacheControlClientError.notImplemented
    }

    func getPlaybackSource(itemID: String, variantID: String) async throws -> CachePlaybackSource {
        playbackSourceRequests.append((itemID, variantID))
        guard let source = playbackSourceResponses[Self.playbackSourceRequestKey(itemID: itemID, variantID: variantID)]
        else {
            throw FakeBilibiliCacheControlClientError.notImplemented
        }
        return source
    }

    func deleteLibraryItem(id: String) async throws -> Bool {
        throw FakeBilibiliCacheControlClientError.notImplemented
    }

    func getTask(id: String) async throws -> CacheTask {
        getTaskIDs.append(id)
        resumeGetTaskRequestWaiters()
        if suspendsGetTaskResponses {
            do {
                let task = try await withCheckedThrowingContinuation { continuation in
                    pendingGetTaskContinuations.append(continuation)
                }
                recordGetTaskResponseCompletion()
                return task
            } catch {
                recordGetTaskResponseCompletion()
                throw error
            }
        }
        guard var responses = getTaskResponses[id], !responses.isEmpty else {
            throw FakeBilibiliCacheControlClientError.notImplemented
        }
        let response = responses.removeFirst()
        getTaskResponses[id] = responses
        return try response.get()
    }

    func watchTasks(ids: [String]) async -> AsyncThrowingStream<CacheTask, Error> {
        AsyncThrowingStream { continuation in
            continuation.onTermination = { _ in
                Task {
                    await self.recordWatchTermination()
                }
            }
            Task {
                self.storeWatchContinuation(continuation)
            }
        }
    }

    func cancelTask(id: String) async throws -> CacheTask {
        cancelledIDs.append(id)
        resumeCancelRequestWaiters()
        if suspendsCancelResponses {
            return try await withCheckedThrowingContinuation { continuation in
                pendingCancelContinuations.append(continuation)
            }
        }

        return cancelResponsesByID[id] ?? .fixture(state: "TASK_STATE_CANCEL_REQUESTED")
    }

    func resolveBilibiliInput(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> BilibiliResolveResult {
        legacyResolveCallCount += 1
        resolvedRequests.append((urlOrID, options))
        resumeResolveRequestWaiters()
        return try await nextResolveResult(source: urlOrID)
    }

    func startBilibiliResolution(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions,
        pageSize: Int
    ) async throws -> BilibiliResolutionPage {
        try await startBilibiliResolution(
            urlOrID: urlOrID,
            options: options,
            context: .default,
            pageSize: pageSize
        )
    }

    func startBilibiliResolution(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions,
        context: BilibiliRequestContext,
        pageSize: Int
    ) async throws -> BilibiliResolutionPage {
        resolvedRequests.append((urlOrID, options))
        resumeResolveRequestWaiters()
        if !resolutionStartResponses.isEmpty {
            switch resolutionStartResponses.removeFirst() {
            case let .success(page):
                resolutionSessions[page.session.id] = page.session
                resolutionCandidates[page.session.id] = page.candidates
                resolutionSnapshots[page.session.id] = page.snapshotID
                resolutionOptionsBySession[page.session.id] = options
                resolutionSourcesBySession[page.session.id] = urlOrID
                candidateIndexesBySession[page.session.id] = Dictionary(
                    uniqueKeysWithValues: page.candidates.map { ($0.candidateToken, $0.index) }
                )
                return page
            case let .failure(error):
                throw error
            }
        }

        let result = try await nextResolveResult(source: urlOrID)
        return makeResolutionPage(
            result: result,
            source: urlOrID,
            options: options,
            context: context,
            pageSize: pageSize
        )
    }

    func listBilibiliResolutionCandidates(
        sessionID: String,
        pageToken: String,
        pageSize: Int
    ) async throws -> BilibiliResolutionPage {
        resolutionPageRequests.append((sessionID, pageToken, pageSize))
        resumeCandidatePageRequestWaiters()
        guard let cursor = candidatePageCursors[pageToken],
            cursor.sessionID == sessionID,
            let session = resolutionSessions[sessionID],
            resolutionSnapshots[sessionID] != nil
        else {
            throw FakeLocalizedError(message: "Bilibili resolution page token is invalid or expired.")
        }
        if let message = candidatePageFailureNext {
            candidatePageFailureNext = nil
            throw FakeLocalizedError(message: message)
        }
        if candidatePageCancellationNext {
            candidatePageCancellationNext = false
            throw CancellationError()
        }
        if !suspendsCandidatePageResponses {
            candidatePageCursors.removeValue(forKey: pageToken)
        }
        let page = makeResolutionPage(
            session: session,
            candidates: resolutionCandidates[sessionID] ?? [],
            offset: cursor.offset,
            pageSize: pageSize
        )
        if suspendsCandidatePageResponses {
            return try await withCheckedThrowingContinuation { continuation in
                pendingCandidatePageContinuations.append((page, continuation))
            }
        }
        guard candidatePageSnapshotMismatchNext else {
            guard candidatePageSessionTruncationMismatchNext else {
                return page
            }
            candidatePageSessionTruncationMismatchNext = false
            return BilibiliResolutionPage(
                session: BilibiliResolutionSession(
                    id: page.session.id,
                    source: page.session.source,
                    title: page.session.title,
                    sourceKind: page.session.sourceKind,
                    createdAt: page.session.createdAt,
                    expiresAt: page.session.expiresAt,
                    defaultCandidateToken: page.session.defaultCandidateToken,
                    context: page.session.context,
                    candidatesTruncated: !page.session.candidatesTruncated
                ),
                candidates: page.candidates,
                pageInfo: CachePageInfo(
                    totalSize: page.totalSize,
                    nextPageToken: page.nextPageToken,
                    snapshotID: page.snapshotID
                )
            )
        }
        candidatePageSnapshotMismatchNext = false
        return BilibiliResolutionPage(
            session: page.session,
            candidates: page.candidates,
            pageInfo: CachePageInfo(
                totalSize: page.totalSize,
                nextPageToken: page.nextPageToken,
                snapshotID: "changed-candidate-snapshot"
            )
        )
    }

    private func nextResolveResult(source: String) async throws -> BilibiliResolveResult {
        if suspendsResolveResponses {
            return try await withCheckedThrowingContinuation { continuation in
                pendingResolveContinuations.append(continuation)
            }
        }

        if resolveResponses.isEmpty {
            return .fixture(source: source)
        }

        switch resolveResponses.removeFirst() {
        case let .success(result):
            return result
        case let .failure(error):
            throw error
        }
    }

    private func makeResolutionPage(
        result: BilibiliResolveResult,
        source: String,
        options: BilibiliPlaybackTaskOptions,
        context: BilibiliRequestContext,
        pageSize: Int
    ) -> BilibiliResolutionPage {
        let sessionID = "resolution-session-\(nextResolutionSessionID)"
        nextResolutionSessionID += 1
        let candidates = result.candidates.map { candidate in
            let numericID = UInt64(candidate.contentID.filter(\.isNumber)) ?? 0
            let identity = BilibiliContentIdentity(
                kind: candidate.sourceKind.localizedCaseInsensitiveContains("episode") ? .seasonEpisode : .videoPage,
                aid: numericID,
                bvid: candidate.contentID,
                cid: numericID,
                epid: candidate.sourceKind.localizedCaseInsensitiveContains("episode") ? numericID : 0
            )
            return BilibiliResolutionCandidate(
                candidateToken: candidate.selectionID,
                title: candidate.title,
                subtitle: candidate.subtitle,
                sourceKind: candidate.sourceKind,
                identity: identity,
                index: candidate.index,
                durationSeconds: candidate.durationSeconds
            )
        }
        let session = BilibiliResolutionSession(
            id: sessionID,
            source: result.source,
            title: result.title,
            sourceKind: result.sourceKind,
            createdAt: nil,
            expiresAt: nil,
            defaultCandidateToken: result.defaultSelectionID,
            context: context,
            candidatesTruncated: result.candidatesTruncated
        )
        resolutionSessions[sessionID] = session
        resolutionCandidates[sessionID] = candidates
        resolutionSnapshots[sessionID] = "candidate-snapshot-\(sessionID)"
        resolutionOptionsBySession[sessionID] = options
        resolutionSourcesBySession[sessionID] = source
        candidateIndexesBySession[sessionID] = Dictionary(
            uniqueKeysWithValues: candidates.map { ($0.candidateToken, $0.index) }
        )
        return makeResolutionPage(session: session, candidates: candidates, offset: 0, pageSize: pageSize)
    }

    private func makeResolutionPage(
        session: BilibiliResolutionSession,
        candidates: [BilibiliResolutionCandidate],
        offset: Int,
        pageSize: Int
    ) -> BilibiliResolutionPage {
        let start = min(max(offset, 0), candidates.count)
        let end = min(start + max(pageSize, 1), candidates.count)
        let nextToken = end < candidates.count ? "opaque-candidate-page-\(session.id)-\(end)" : ""
        if !nextToken.isEmpty {
            candidatePageCursors[nextToken] = FakeCandidatePageCursor(sessionID: session.id, offset: end)
        }
        return BilibiliResolutionPage(
            session: session,
            candidates: Array(candidates[start..<end]),
            pageInfo: CachePageInfo(
                totalSize: UInt64(candidates.count),
                nextPageToken: nextToken,
                snapshotID: resolutionSnapshots[session.id] ?? "candidate-snapshot-\(session.id)"
            )
        )
    }

    func createBilibiliTask(
        urlOrID: String,
        options: BilibiliDownloadTaskOptions
    ) async throws -> CacheTask {
        legacyCreateCallCount += 1
        downloadRequests.append((urlOrID, options))
        return try await nextCreateResult()
    }

    func createBilibiliPlaybackTask(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask {
        try await createBilibiliPlaybackTask(urlOrID: urlOrID, selectionID: nil, options: options)
    }

    func createBilibiliPlaybackTask(
        urlOrID: String,
        selectionID: String?,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask {
        legacyCreateCallCount += 1
        return try await recordCreateRequest(
            urlOrID: urlOrID,
            selectionID: selectionID,
            selection: nil,
            options: options
        )
    }

    func createBilibiliPlaybackTask(
        urlOrID: String,
        selection: BilibiliTaskSelection?,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask {
        legacyCreateCallCount += 1
        guard supportsTaskSelection else {
            throw CacheControlClientUnsupportedFeature.bilibiliTaskSelection
        }

        return try await recordCreateRequest(
            urlOrID: urlOrID,
            selectionID: selection?.legacySingleSelectionID,
            selection: selection,
            options: options
        )
    }

    func createBilibiliPlaybackTaskV2(
        sessionID: String,
        selection: BilibiliResolutionSelection
    ) async throws -> CacheTask {
        try await createBilibiliTaskV2(
            sessionID: sessionID,
            selection: selection,
            execution: .playback(BilibiliPlaybackSpec())
        )
    }

    func createBilibiliTaskV2(
        sessionID: String,
        selection: BilibiliResolutionSelection,
        execution: BilibiliTaskExecution
    ) async throws -> CacheTask {
        v2TaskRequests.append(
            FakeBilibiliTaskV2Request(
                sessionID: sessionID,
                selection: selection,
                execution: execution
            ))
        if let options = resolutionOptionsBySession[sessionID],
            let source = resolutionSourcesBySession[sessionID]
        {
            switch execution {
            case let .playback(spec):
                let oldSelection = legacySelection(for: selection, sessionID: sessionID)
                createdRequests.append((source, oldSelection.legacySingleSelectionID, oldSelection, options))
                _ = spec
            case let .download(spec):
                downloadRequests.append(
                    (
                        source,
                        BilibiliDownloadTaskOptions(
                            qualityPreference: options.qualityPreference,
                            encodingPreference: "",
                            audioLanguagePreference: spec.audioLanguage,
                            preferTVAPI: options.preferTVAPI,
                            downloadSubtitles: spec.downloadSubtitles,
                            downloadDanmaku: spec.downloadDanmaku,
                            downloadCover: spec.downloadCover,
                            subtitleAIPolicy: spec.subtitleAIPolicy,
                            danmakuFormats: spec.danmakuFormats,
                            downloadMode: spec.mode
                        )
                    ))
            }
        }
        resumeCreateRequestWaiters()
        let task = try await nextCreateResult()
        createdTasksByID[task.id] = task
        return task
    }

    private func legacySelection(
        for selection: BilibiliResolutionSelection,
        sessionID: String
    ) -> BilibiliTaskSelection {
        switch selection {
        case let .single(candidateToken):
            return BilibiliTaskSelection(mode: "single", selectionIDs: [candidateToken])
        case let .multiple(candidateTokens):
            return BilibiliTaskSelection(mode: "multiple", selectionIDs: candidateTokens)
        case let .range(startCandidateToken, endCandidateToken):
            let indexes = candidateIndexesBySession[sessionID] ?? [:]
            return BilibiliTaskSelection(
                mode: "range",
                rangeStartIndex: indexes[startCandidateToken] ?? 0,
                rangeEndIndex: indexes[endCandidateToken] ?? 0
            )
        case .all:
            return BilibiliTaskSelection(mode: "all")
        }
    }

    private func nextCreateResult() async throws -> CacheTask {
        if suspendsCreateResponses {
            return try await withCheckedThrowingContinuation { continuation in
                pendingCreateContinuations.append(continuation)
            }
        }
        guard !createResponses.isEmpty else {
            throw FakeBilibiliCacheControlClientError.noCreateResponse
        }
        switch createResponses.removeFirst() {
        case let .success(task):
            return task
        case let .failure(error):
            throw error
        }
    }

    func listTaskResults(
        taskID: String,
        pageToken: String,
        pageSize: Int
    ) async throws -> CacheTaskResultsPage {
        taskResultRequests.append((taskID, pageToken, pageSize))
        resumeTaskResultRequestWaiters()
        if suspendsTaskResultResponses {
            return try await withCheckedThrowingContinuation { continuation in
                pendingTaskResultContinuations.append(continuation)
            }
        }
        let key = Self.taskResultRequestKey(taskID: taskID, pageToken: pageToken)
        if var responses = taskResultPagesByRequest[key], !responses.isEmpty {
            let response = responses.removeFirst()
            taskResultPagesByRequest[key] = responses
            return try response.get()
        }
        return CacheTaskResultsPage(
            results: [],
            pageInfo: CachePageInfo(totalSize: 0, nextPageToken: "", snapshotID: "empty-\(taskID)"),
            outputRevision: createdTasksByID[taskID]?.outputSummary?.revision ?? 0
        )
    }

    private static func taskResultRequestKey(taskID: String, pageToken: String) -> String {
        "\(taskID)\u{1F}\(pageToken)"
    }

    private static func playbackSourceRequestKey(itemID: String, variantID: String) -> String {
        "\(itemID)\u{1F}\(variantID)"
    }

    func enqueueTaskResultPage(
        taskID: String,
        pageToken: String,
        result: Result<CacheTaskResultsPage, Error>
    ) {
        taskResultPagesByRequest[Self.taskResultRequestKey(taskID: taskID, pageToken: pageToken), default: []]
            .append(result)
    }

    func setSuspendsTaskResultResponses(_ suspends: Bool) {
        suspendsTaskResultResponses = suspends
    }

    func completeNextTaskResult(with result: Result<CacheTaskResultsPage, Error>) {
        guard !pendingTaskResultContinuations.isEmpty else {
            return
        }
        pendingTaskResultContinuations.removeFirst().resume(with: result)
    }

    func waitForTaskResultRequestCount(_ count: Int) async {
        guard taskResultRequests.count < count else {
            return
        }
        await withCheckedContinuation { continuation in
            taskResultRequestWaiters.append((count, continuation))
        }
    }

    func taskResultRequestsSnapshot() -> [(taskID: String, pageToken: String, pageSize: Int)] {
        taskResultRequests
    }

    func serverInfoRequestCountSnapshot() -> Int {
        serverInfoRequestCount
    }

    func serverInfoResponseCompletionCountSnapshot() -> Int {
        serverInfoResponseCompletionCount
    }

    func pendingResumptionResponseCountsSnapshot() -> (serverInfo: Int, getTask: Int) {
        (pendingServerInfoContinuations.count, pendingGetTaskContinuations.count)
    }

    func cancelPendingResumptionResponses() {
        suspendsServerInfoResponses = false
        suspendsGetTaskResponses = false
        let serverInfoContinuations = pendingServerInfoContinuations
        let getTaskContinuations = pendingGetTaskContinuations
        pendingServerInfoContinuations.removeAll()
        pendingGetTaskContinuations.removeAll()
        serverInfoContinuations.forEach { $0.resume(throwing: CancellationError()) }
        getTaskContinuations.forEach { $0.resume(throwing: CancellationError()) }
    }

    func waitForServerInfoRequestCount(_ count: Int) async {
        guard serverInfoRequestCount < count else {
            return
        }
        await withCheckedContinuation { continuation in
            serverInfoRequestWaiters.append((count, continuation))
        }
    }

    func setSuspendsServerInfoResponses(_ suspends: Bool) {
        suspendsServerInfoResponses = suspends
    }

    func completeNextServerInfo(with result: Result<CacheServerSummary, Error>) {
        guard !pendingServerInfoContinuations.isEmpty else {
            return
        }
        pendingServerInfoContinuations.removeFirst().resume(with: result)
    }

    func playbackSourceRequestsSnapshot() -> [(itemID: String, variantID: String)] {
        playbackSourceRequests
    }

    func setCandidatePageFailureNext(_ message: String) {
        candidatePageFailureNext = message
    }

    func setCandidatePageCancellationNext() {
        candidatePageCancellationNext = true
    }

    func setCandidatePageSnapshotMismatchNext() {
        candidatePageSnapshotMismatchNext = true
    }

    func setCandidatePageSessionTruncationMismatchNext() {
        candidatePageSessionTruncationMismatchNext = true
    }

    func setSuspendsCandidatePageResponses(_ suspends: Bool) {
        suspendsCandidatePageResponses = suspends
    }

    func waitForCandidatePageRequestCount(_ count: Int) async {
        guard resolutionPageRequests.count < count else {
            return
        }
        await withCheckedContinuation { continuation in
            candidatePageRequestWaiters.append((count, continuation))
        }
    }

    func completeNextCandidatePage() {
        guard !pendingCandidatePageContinuations.isEmpty else {
            return
        }
        let pending = pendingCandidatePageContinuations.removeFirst()
        pending.continuation.resume(returning: pending.page)
    }

    func resolutionPageRequestsSnapshot() -> [(sessionID: String, pageToken: String, pageSize: Int)] {
        resolutionPageRequests
    }

    func v2TaskRequestsSnapshot() -> [FakeBilibiliTaskV2Request] {
        v2TaskRequests
    }

    func legacyRPCCallCountsSnapshot() -> (resolve: Int, create: Int) {
        (legacyResolveCallCount, legacyCreateCallCount)
    }

    private func recordCreateRequest(
        urlOrID: String,
        selectionID: String?,
        selection: BilibiliTaskSelection?,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask {
        createdRequests.append((urlOrID, selectionID, selection, options))
        resumeCreateRequestWaiters()
        if suspendsCreateResponses {
            return try await withCheckedThrowingContinuation { continuation in
                pendingCreateContinuations.append(continuation)
            }
        }

        guard !createResponses.isEmpty else {
            throw FakeBilibiliCacheControlClientError.noCreateResponse
        }

        switch createResponses.removeFirst() {
        case let .success(task):
            return task
        case let .failure(error):
            throw error
        }
    }

    func setSuspendsCreateResponses(_ suspendsCreateResponses: Bool) {
        self.suspendsCreateResponses = suspendsCreateResponses
    }

    func setSuspendsResolveResponses(_ suspendsResolveResponses: Bool) {
        self.suspendsResolveResponses = suspendsResolveResponses
    }

    func setSuspendsCancelResponses(_ suspendsCancelResponses: Bool) {
        self.suspendsCancelResponses = suspendsCancelResponses
    }

    func completeNextResolve(with result: Result<BilibiliResolveResult, Error>) {
        guard !pendingResolveContinuations.isEmpty else {
            return
        }

        pendingResolveContinuations.removeFirst().resume(with: result)
    }

    func completeNextCreate(with result: Result<CacheTask, Error>) {
        guard !pendingCreateContinuations.isEmpty else {
            return
        }

        pendingCreateContinuations.removeFirst().resume(with: result)
    }

    func completeNextCancel(with result: Result<CacheTask, Error>) {
        guard !pendingCancelContinuations.isEmpty else {
            return
        }

        pendingCancelContinuations.removeFirst().resume(with: result)
    }

    func completeNextGetTask(with result: Result<CacheTask, Error>) {
        guard !pendingGetTaskContinuations.isEmpty else {
            return
        }

        pendingGetTaskContinuations.removeFirst().resume(with: result)
    }

    func setSuspendsGetTaskResponses(_ suspends: Bool) {
        suspendsGetTaskResponses = suspends
    }

    func resolvedRequestsSnapshot() -> [(urlOrID: String, options: BilibiliPlaybackTaskOptions)] {
        resolvedRequests
    }

    func downloadRequestsSnapshot() -> [(urlOrID: String, options: BilibiliDownloadTaskOptions)] {
        downloadRequests
    }

    func createdRequestsSnapshot() -> [(
        urlOrID: String,
        selectionID: String?,
        selection: BilibiliTaskSelection?,
        options: BilibiliPlaybackTaskOptions
    )] {
        createdRequests
    }

    func getTaskIDsSnapshot() -> [String] {
        getTaskIDs
    }

    func watchSubscriptionCountSnapshot() -> Int {
        watchSubscriptionCount
    }

    func cancelledIDsSnapshot() -> [String] {
        cancelledIDs
    }

    func waitForResolveRequestCount(_ count: Int) async {
        guard resolvedRequests.count < count else {
            return
        }

        await withCheckedContinuation { continuation in
            resolveRequestWaiters.append((count, continuation))
        }
    }

    func waitForCreateRequestCount(_ count: Int) async {
        guard v2TaskRequests.count < count else {
            return
        }

        await withCheckedContinuation { continuation in
            createRequestWaiters.append((count, continuation))
        }
    }

    func waitForCancelRequestCount(_ count: Int) async {
        guard cancelledIDs.count < count else {
            return
        }

        await withCheckedContinuation { continuation in
            cancelRequestWaiters.append((count, continuation))
        }
    }

    func waitForGetTaskRequestCount(_ count: Int) async {
        guard getTaskIDs.count < count else {
            return
        }

        await withCheckedContinuation { continuation in
            getTaskRequestWaiters.append((count, continuation))
        }
    }

    func waitForGetTaskResponseCompletionCount(_ count: Int) async {
        guard getTaskResponseCompletionCount < count else {
            return
        }

        await withCheckedContinuation { continuation in
            getTaskResponseCompletionWaiters.append((count, continuation))
        }
    }

    func waitForWatchSubscription(count: Int = 1) async {
        guard watchSubscriptionCount < count else {
            return
        }

        await withCheckedContinuation { continuation in
            watchWaiters.append((count, continuation))
        }
    }

    func waitForWatchTermination() async {
        guard watchTerminationCount == 0 else {
            return
        }

        await withCheckedContinuation { continuation in
            watchTerminationWaiters.append(continuation)
        }
    }

    func yield(_ task: CacheTask) {
        watchContinuations.forEach { $0.yield(task) }
    }

    func finishWatching() {
        let continuations = watchContinuations
        watchContinuations = []
        continuations.forEach { $0.finish() }
    }

    func failWatching(with error: Error = FakeBilibiliCacheControlClientError.watchFailed) {
        let continuations = watchContinuations
        watchContinuations = []
        continuations.forEach { $0.finish(throwing: error) }
    }

    private func resumeResolveRequestWaiters() {
        let ready = resolveRequestWaiters.filter { $0.count <= resolvedRequests.count }
        resolveRequestWaiters.removeAll { $0.count <= resolvedRequests.count }
        ready.forEach { $0.continuation.resume() }
    }

    private func resumeCreateRequestWaiters() {
        let ready = createRequestWaiters.filter { $0.count <= v2TaskRequests.count }
        createRequestWaiters.removeAll { $0.count <= v2TaskRequests.count }
        ready.forEach { $0.continuation.resume() }
    }

    private func resumeTaskResultRequestWaiters() {
        let ready = taskResultRequestWaiters.filter { $0.count <= taskResultRequests.count }
        taskResultRequestWaiters.removeAll { $0.count <= taskResultRequests.count }
        ready.forEach { $0.continuation.resume() }
    }

    private func resumeCandidatePageRequestWaiters() {
        let ready = candidatePageRequestWaiters.filter { $0.count <= resolutionPageRequests.count }
        candidatePageRequestWaiters.removeAll { $0.count <= resolutionPageRequests.count }
        ready.forEach { $0.continuation.resume() }
    }

    private func resumeCancelRequestWaiters() {
        let ready = cancelRequestWaiters.filter { $0.count <= cancelledIDs.count }
        cancelRequestWaiters.removeAll { $0.count <= cancelledIDs.count }
        ready.forEach { $0.continuation.resume() }
    }

    private func storeWatchContinuation(
        _ continuation: AsyncThrowingStream<CacheTask, Error>.Continuation
    ) {
        watchContinuations.append(continuation)
        watchSubscriptionCount += 1
        let ready = watchWaiters.filter { $0.count <= watchSubscriptionCount }
        watchWaiters.removeAll { $0.count <= watchSubscriptionCount }
        ready.forEach { $0.continuation.resume() }
    }

    private func resumeGetTaskRequestWaiters() {
        let ready = getTaskRequestWaiters.filter { $0.count <= getTaskIDs.count }
        getTaskRequestWaiters.removeAll { $0.count <= getTaskIDs.count }
        ready.forEach { $0.continuation.resume() }
    }

    private func resumeGetTaskResponseCompletionWaiters() {
        let ready = getTaskResponseCompletionWaiters.filter { $0.count <= getTaskResponseCompletionCount }
        getTaskResponseCompletionWaiters.removeAll { $0.count <= getTaskResponseCompletionCount }
        ready.forEach { $0.continuation.resume() }
    }

    private func recordGetTaskResponseCompletion() {
        getTaskResponseCompletionCount += 1
        resumeGetTaskResponseCompletionWaiters()
    }

    private func recordWatchTermination() {
        watchTerminationCount += 1
        let waiters = watchTerminationWaiters
        watchTerminationWaiters = []
        waiters.forEach { $0.resume() }
    }
}

private enum FakeBilibiliCacheControlClientError: Error {
    case notImplemented
    case noCreateResponse
    case cancelFailed
    case watchFailed
}

private struct FakeLocalizedError: LocalizedError {
    let message: String

    var errorDescription: String? {
        message
    }
}

private extension BilibiliTaskSelection {
    var legacySingleSelectionID: String? {
        guard mode.lowercased() == "single", selectionIDs.count == 1 else {
            return nil
        }

        return selectionIDs[0]
    }
}

private extension BilibiliResolveResult {
    static func fixture(
        source: String = "BV1test",
        title: String = "Ready video",
        sourceKind: String = "video",
        candidates: [BilibiliResolvedCandidate] = [
            .fixture()
        ],
        defaultSelectionID: String = "page:1",
        candidatesTruncated: Bool = false
    ) -> Self {
        Self(
            source: source.trimmingCharacters(in: .whitespacesAndNewlines),
            title: title,
            sourceKind: sourceKind,
            candidates: candidates,
            defaultSelectionID: defaultSelectionID,
            candidatesTruncated: candidatesTruncated
        )
    }
}

private extension BilibiliResolvedCandidate {
    static func fixture(
        selectionID: String = "page:1",
        title: String = "Ready video",
        subtitle: String = "Page 1",
        index: Int = 1
    ) -> Self {
        Self(
            selectionID: selectionID,
            title: title,
            subtitle: subtitle,
            sourceKind: "video_page",
            contentID: "100\(index)",
            index: index,
            durationSeconds: 60,
            coverURI: "https://example.test/cover.jpg"
        )
    }
}

private extension BilibiliTaskResultItem {
    static func fixture(
        id: String = "bilibili-playback-1",
        selectionID: String = "page:1",
        title: String = "Ready video",
        subtitle: String = "Page 1",
        index: Int = 1,
        state: String = "TASK_STATE_PLAYABLE",
        message: String = "Playable.",
        libraryItemID: String = "",
        playbackSourceItemID: String? = nil,
        hlsCacheFillStatus: HlsCacheFillStatus? = nil
    ) -> Self {
        let itemID = playbackSourceItemID ?? (state == "TASK_STATE_COMPLETED" ? libraryItemID : id)
        return Self(
            id: id,
            selectionID: selectionID,
            title: title,
            subtitle: subtitle,
            sourceKind: "video_page",
            contentID: "100\(index)",
            index: index,
            state: state,
            message: message,
            libraryItemID: libraryItemID,
            playbackSource: itemID.isEmpty
                ? nil
                : CachePlaybackSource(
                    itemID: itemID,
                    variantID: "h264",
                    playbackProtocol: "PLAYBACK_PROTOCOL_HLS",
                    uri: "http://mac-mini.local:8080/hls/\(itemID)/master.m3u8"
                ),
            playbackSession: itemID.isEmpty
                ? nil
                : CacheBilibiliPlaybackSession(
                    id: id,
                    title: title,
                    contentID: "cid-\(index)",
                    selectedVariantID: "h264",
                    selectedVariant: nil,
                    variants: []
                ),
            hlsCacheFillStatus: hlsCacheFillStatus
        )
    }
}

private extension CacheTaskArtifact {
    static func fixture(
        id: String,
        state: String = "TASK_ARTIFACT_STATE_AVAILABLE",
        uri: String,
        libraryItemID: String = ""
    ) -> Self {
        Self(
            id: id,
            kind: "subtitle",
            state: state,
            title: "English subtitles",
            format: "srt",
            languageTag: "en",
            isAIGenerated: false,
            resource: CacheResourceReference(
                id: "resource-\(id)",
                uri: uri,
                contentType: "text/plain",
                sizeBytes: 256,
                sizeKnown: true,
                supportsByteRanges: false,
                etag: "fixture-etag",
                expiresAt: nil
            ),
            problem: nil,
            libraryItemID: libraryItemID
        )
    }
}

private extension CacheTaskResult {
    static func fixture(
        id: String,
        title: String,
        state: String = "TASK_RESULT_STATE_COMPLETED",
        libraryItemID: String = "",
        playbackSource: CachePlaybackSource? = nil,
        artifacts: [CacheTaskArtifact] = [],
        hlsCacheFillStatus: HlsCacheFillStatus? = nil
    ) -> Self {
        Self(
            id: id,
            state: state,
            title: title,
            subtitle: "Bilibili result",
            progress: nil,
            problem: nil,
            libraryItemID: libraryItemID,
            playbackSource: playbackSource,
            artifacts: artifacts,
            createdAt: nil,
            updatedAt: nil,
            providerDetails: hlsCacheFillStatus.map {
                .bilibili(
                    BilibiliTaskResultDetails(
                        identity: nil,
                        playbackSession: nil,
                        hlsCacheFillStatus: $0
                    ))
            }
        )
    }
}

private extension CacheServerSummary {
    static func fixture(mediaBaseURIs: [String]) -> Self {
        Self(
            id: "fake-cache-server",
            name: "Fake cache server",
            version: "test",
            mediaBaseURIs: mediaBaseURIs,
            capabilities: []
        )
    }
}

private extension CacheTask {
    static func fixture(
        id: String = "bilibili-playback-1",
        source: String = "BV1test",
        title: String = "Ready video",
        state: String,
        progress: Double = 0.25,
        downloadedBytes: Int64 = 0,
        totalBytes: Int64 = 0,
        message: String = "Preparing playback.",
        libraryItemID: String = "",
        playbackSource: CachePlaybackSource? = nil,
        playbackSession: CacheBilibiliPlaybackSession? = nil,
        bilibiliSelection: BilibiliTaskSelection? = nil,
        resultItems: [BilibiliTaskResultItem] = [],
        outputSummary: CacheTaskOutputSummary? = nil,
        hlsCacheFillStatus: HlsCacheFillStatus? = nil
    ) -> Self {
        Self(
            id: id,
            kind: "TASK_KIND_BILIBILI_PROGRESSIVE_PLAYBACK",
            state: state,
            source: source,
            title: title,
            progress: progress,
            downloadedBytes: downloadedBytes,
            totalBytes: totalBytes,
            message: message,
            libraryItemID: libraryItemID,
            playbackSource: playbackSource,
            playbackSession: playbackSession,
            bilibiliSelection: bilibiliSelection,
            resultItems: resultItems,
            outputSummary: outputSummary,
            hlsCacheFillStatus: hlsCacheFillStatus
        )
    }

    static func playableFixture(
        id: String = "bilibili-playback-1",
        source: String = "BV1test",
        state: String = "TASK_STATE_PLAYABLE",
        libraryItemID: String = "",
        playbackSourceItemID: String? = nil,
        message: String = "Bilibili playback session is playable.",
        progress: Double? = nil,
        downloadedBytes: Int64 = 0,
        totalBytes: Int64 = 0,
        resultItems: [BilibiliTaskResultItem] = [],
        hlsCacheFillStatus: HlsCacheFillStatus? = nil
    ) -> Self {
        let resolvedPlaybackSourceItemID = playbackSourceItemID ?? id
        return .fixture(
            id: id,
            source: source,
            state: state,
            progress: progress ?? (totalBytes > 0 ? Double(downloadedBytes) / Double(totalBytes) : 1),
            downloadedBytes: downloadedBytes,
            totalBytes: totalBytes,
            message: message,
            libraryItemID: libraryItemID,
            playbackSource: CachePlaybackSource(
                itemID: resolvedPlaybackSourceItemID,
                variantID: "h264",
                playbackProtocol: "PLAYBACK_PROTOCOL_HLS",
                uri: "http://mac-mini.local:8080/hls/\(resolvedPlaybackSourceItemID)/master.m3u8"
            ),
            playbackSession: CacheBilibiliPlaybackSession(
                id: id,
                title: "Ready video",
                contentID: "BV1ready-cid1",
                selectedVariantID: "h264",
                selectedVariant: nil,
                variants: []
            ),
            resultItems: resultItems,
            hlsCacheFillStatus: hlsCacheFillStatus
        )
    }
}
