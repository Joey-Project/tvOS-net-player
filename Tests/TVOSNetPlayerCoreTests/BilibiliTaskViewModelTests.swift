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
        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(requests.first?.urlOrID, "BV1test")
        XCTAssertEqual(requests.first?.selectionID, "page:1")
        XCTAssertEqual(requests.first?.selection?.mode, "single")
        XCTAssertEqual(requests.first?.selection?.selectionIDs, ["page:1"])
        XCTAssertEqual(requests.first?.options.qualityPreference, "1080p")
        XCTAssertEqual(requests.first?.options.encodingPreference, "h264")
        XCTAssertEqual(requests.first?.options.audioLanguagePreference, "ja-jp")
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
        XCTAssertTrue(resolvedRequests.isEmpty)
        XCTAssertTrue(playbackRequests.isEmpty)
        XCTAssertEqual(model.currentTask?.id, "bilibili-download-1")

        model.clearTask()
    }

    func testSubmitFallsBackToLegacySelectionWhenStructuredSelectionIsUnsupported() async {
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
        XCTAssertNil(requests.first?.selection)

        model.clearTask()
    }

    func testSubmitFallsBackToCreateWhenResolveIsUnsupported() async {
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
        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(requests.first?.urlOrID, "BV1legacy")
        XCTAssertNil(requests.first?.selectionID)
        XCTAssertEqual(requests.first?.options.qualityPreference, "720p")
        XCTAssertEqual(requests.first?.options.encodingPreference, "h265")
        XCTAssertEqual(requests.first?.options.audioLanguagePreference, "en-US")
        XCTAssertEqual(model.currentTask?.source, "BV1legacy")
        XCTAssertFalse(model.isResolving)

        model.clearTask()
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

    func testBatchSelectionDoesNotFallbackToLegacySelectionWhenUnsupported() async {
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
            createResponses: [],
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
        XCTAssertTrue(requests.isEmpty)
        XCTAssertNil(model.currentTask)
        XCTAssertEqual(model.statusMessage, "Could not submit Bilibili playback task.")
    }

    func testAllSelectionIsUnavailableWhenResolvedCandidatesAreTruncated() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "fav123",
                        candidates: [
                            .fixture(selectionID: "fav:1", title: "Item 1", index: 1),
                            .fixture(selectionID: "fav:2", title: "Item 2", index: 2),
                        ],
                        defaultSelectionID: "",
                        candidatesTruncated: true
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
        XCTAssertFalse(model.availableCandidateSelectionModes.contains(.all))
        model.candidateSelectionMode = .all

        XCTAssertFalse(model.canSubmit)
        XCTAssertEqual(model.selectedCandidateCount, 0)
        XCTAssertEqual(
            model.candidateSelectionSummary,
            "All selection is unavailable because the resolved item list is truncated."
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        let requests = await client.createdRequestsSnapshot()
        XCTAssertTrue(requests.isEmpty)
    }

    func testTruncatedResolveShowsBoundedWindowNotice() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "fav123",
                        sourceKind: "favorite",
                        candidates: [
                            .fixture(selectionID: "item:1", title: "Item 1", index: 1),
                            .fixture(selectionID: "item:2", title: "Item 2", index: 2),
                        ],
                        defaultSelectionID: "",
                        candidatesTruncated: true
                    ))
            ],
            createResponses: []
        )
        let model = BilibiliTaskViewModel(
            sourceText: "fav123",
            clientFactory: { _ in client }
        )

        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertEqual(model.fetchNotice?.title, "Showing a bounded window")
        XCTAssertEqual(model.fetchNotice?.tone, .warning)
        XCTAssertEqual(model.fetchNotice?.actionTitle, "Re-resolve")
        XCTAssertFalse(model.availableCandidateSelectionModes.contains(.all))
        XCTAssertTrue(model.canReResolve)
    }

    func testResolvedFetchNoticeClearsAfterPlaybackTaskSubmission() async {
        let client = FakeBilibiliCacheControlClient(
            resolveResponses: [
                .success(
                    .fixture(
                        source: "fav123",
                        sourceKind: "favorite",
                        candidates: [
                            .fixture(selectionID: "item:1", title: "Item 1", index: 1),
                            .fixture(selectionID: "item:2", title: "Item 2", index: 2),
                        ],
                        defaultSelectionID: "",
                        candidatesTruncated: true
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
        XCTAssertEqual(model.fetchNotice?.title, "Showing a bounded window")

        model.selectedCandidateID = "item:1"
        await model.submit(serverAddressText: "mac-mini.local:50051")

        XCTAssertEqual(model.currentTask?.source, "fav123")
        XCTAssertNil(model.fetchNotice)
        XCTAssertFalse(model.canReResolve)
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
                "This Bilibili list or feed can reorder between refreshes. Single and multiple selections submit stable item IDs; Range and All follow the refreshed list order.",
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
        let requests = await client.createdRequestsSnapshot()
        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(requests.first?.selectionID, "page:2")
        XCTAssertEqual(
            requests.first?.options.playbackPolicy,
            BilibiliPlaybackPolicy(
                transcodingPreference: .force,
                compatibleVariantPreference: .preferRequested,
                weakNetworkPreference: .holdDowngrade
            )
        )

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
                    resultItems: resultItems
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
}

private actor FakeBilibiliCacheControlClient: CacheControlClient {
    private var resolveResponses: [Result<BilibiliResolveResult, Error>]
    private var createResponses: [Result<CacheTask, Error>]
    private let cancelResponsesByID: [String: CacheTask]
    private let supportsTaskSelection: Bool
    private var suspendsResolveResponses: Bool
    private var suspendsCreateResponses: Bool
    private var suspendsCancelResponses = false
    private var resolvedRequests: [(urlOrID: String, options: BilibiliPlaybackTaskOptions)] = []
    private var downloadRequests: [(urlOrID: String, options: BilibiliDownloadTaskOptions)] = []
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
    private var resolveRequestWaiters: [(count: Int, continuation: CheckedContinuation<Void, Never>)] = []
    private var createRequestWaiters: [(count: Int, continuation: CheckedContinuation<Void, Never>)] = []
    private var cancelRequestWaiters: [(count: Int, continuation: CheckedContinuation<Void, Never>)] = []
    private var watchContinuations: [AsyncThrowingStream<CacheTask, Error>.Continuation] = []
    private var watchWaiters: [CheckedContinuation<Void, Never>] = []
    private var watchTerminationCount = 0
    private var watchTerminationWaiters: [CheckedContinuation<Void, Never>] = []

    init(
        resolveResponses: [Result<BilibiliResolveResult, Error>] = [],
        createResponses: [Result<CacheTask, Error>],
        cancelResponsesByID: [String: CacheTask] = [:],
        supportsTaskSelection: Bool = true,
        suspendsResolveResponses: Bool = false,
        suspendsCreateResponses: Bool = false
    ) {
        self.resolveResponses = resolveResponses
        self.createResponses = createResponses
        self.cancelResponsesByID = cancelResponsesByID
        self.supportsTaskSelection = supportsTaskSelection
        self.suspendsResolveResponses = suspendsResolveResponses
        self.suspendsCreateResponses = suspendsCreateResponses
    }

    func getServerInfo() async throws -> CacheServerSummary {
        throw FakeBilibiliCacheControlClientError.notImplemented
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
        throw FakeBilibiliCacheControlClientError.notImplemented
    }

    func deleteLibraryItem(id: String) async throws -> Bool {
        throw FakeBilibiliCacheControlClientError.notImplemented
    }

    func getTask(id: String) async throws -> CacheTask {
        throw FakeBilibiliCacheControlClientError.notImplemented
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
        resolvedRequests.append((urlOrID, options))
        resumeResolveRequestWaiters()
        if suspendsResolveResponses {
            return try await withCheckedThrowingContinuation { continuation in
                pendingResolveContinuations.append(continuation)
            }
        }

        if resolveResponses.isEmpty {
            return .fixture(source: urlOrID)
        }

        switch resolveResponses.removeFirst() {
        case let .success(result):
            return result
        case let .failure(error):
            throw error
        }
    }

    func createBilibiliTask(
        urlOrID: String,
        options: BilibiliDownloadTaskOptions
    ) async throws -> CacheTask {
        downloadRequests.append((urlOrID, options))
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
        try await recordCreateRequest(
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
        guard createdRequests.count < count else {
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

    func waitForWatchSubscription() async {
        guard watchContinuations.isEmpty else {
            return
        }

        await withCheckedContinuation { continuation in
            watchWaiters.append(continuation)
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

    func failWatching() {
        let continuations = watchContinuations
        watchContinuations = []
        continuations.forEach { $0.finish(throwing: FakeBilibiliCacheControlClientError.watchFailed) }
    }

    private func resumeResolveRequestWaiters() {
        let ready = resolveRequestWaiters.filter { $0.count <= resolvedRequests.count }
        resolveRequestWaiters.removeAll { $0.count <= resolvedRequests.count }
        ready.forEach { $0.continuation.resume() }
    }

    private func resumeCreateRequestWaiters() {
        let ready = createRequestWaiters.filter { $0.count <= createdRequests.count }
        createRequestWaiters.removeAll { $0.count <= createdRequests.count }
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
        let waiters = watchWaiters
        watchWaiters = []
        waiters.forEach { $0.resume() }
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
        playbackSourceItemID: String? = nil
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
                )
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
        resultItems: [BilibiliTaskResultItem] = []
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
            resultItems: resultItems
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
        resultItems: [BilibiliTaskResultItem] = []
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
            resultItems: resultItems
        )
    }
}
