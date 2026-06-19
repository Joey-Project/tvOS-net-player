import XCTest
@testable import TVOSNetPlayerCacheClient

final class CacheLibraryPaginationTests: XCTestCase {
    func testGeneratedBilibiliResolveCapabilityMatchesPublicConstant() {
        XCTAssertEqual(
            String(describing: TvosNetPlayer_V1_ServerCapability.bilibiliResolve),
            CacheServerCapability.bilibiliResolve
        )
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

        XCTAssertEqual(task.bilibiliSelection?.mode, expectedSelectionMode)
        XCTAssertEqual(task.bilibiliSelection?.selectionIDs, ["page:1", "page:2"])
        XCTAssertEqual(task.bilibiliSelection?.rangeStartIndex, 1)
        XCTAssertEqual(task.bilibiliSelection?.rangeEndIndex, 2)
        XCTAssertEqual(task.resultItems.map(\.selectionID), ["page:1"])
        XCTAssertEqual(task.resultItems.first?.state, expectedResultState)
        XCTAssertEqual(task.resultItems.first?.playbackSource?.playbackProtocol, expectedPlaybackProtocol)
    }

    func testCacheServerSummaryExposesBilibiliResolveSupport() {
        let supported = CacheServerSummary(
            id: "server-1",
            name: "Test cache",
            version: "0.1.0",
            mediaBaseURIs: [],
            capabilities: [CacheServerCapability.bilibiliResolve]
        )
        let unsupported = CacheServerSummary(
            id: "server-2",
            name: "Old cache",
            version: "0.1.0",
            mediaBaseURIs: [],
            capabilities: []
        )

        XCTAssertTrue(supported.supportsBilibiliResolve)
        XCTAssertFalse(unsupported.supportsBilibiliResolve)
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
