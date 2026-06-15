import XCTest
import TVOSNetPlayerCacheClient
@testable import TVOSNetPlayerCore

final class CacheLibraryViewModelTests: XCTestCase {
    private var defaultsSuiteName: String!
    private var defaults: UserDefaults!

    override func setUpWithError() throws {
        try super.setUpWithError()
        defaultsSuiteName = "CacheLibraryViewModelTests-\(UUID().uuidString)"
        defaults = try XCTUnwrap(UserDefaults(suiteName: defaultsSuiteName))
        defaults.removePersistentDomain(forName: defaultsSuiteName)
    }

    override func tearDown() {
        defaults.removePersistentDomain(forName: defaultsSuiteName)
        defaults = nil
        defaultsSuiteName = nil
        super.tearDown()
    }

    @MainActor
    func testRefreshLoadsServerInfoAndLibraryItems() async {
        let client = FakeCacheControlClient(
            serverInfo: CacheServerSummary(
                id: "server-1",
                name: "Mac mini cache",
                version: "0.1.0",
                mediaBaseURIs: ["http://mac-mini.local:8080"],
                capabilities: ["httpRange"]
            ),
            items: [
                CacheLibraryItem.fixture(id: "item-1", title: "Cached video")
            ],
            playbackSource: .fixture()
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "mac-mini.local",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()

        XCTAssertEqual(model.serverAddressText, "mac-mini.local:50051")
        XCTAssertEqual(model.serverName, "Mac mini cache")
        XCTAssertEqual(model.items.map(\.id), ["item-1"])
        XCTAssertEqual(defaults.string(forKey: CacheLibraryViewModel.serverAddressDefaultsKey), "mac-mini.local:50051")
        XCTAssertNil(model.errorMessage)
    }

    @MainActor
    func testRefreshLoadsCacheRoots() async {
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [],
            playbackSource: .fixture(),
            cacheRoots: [
                .fixture(id: "default", label: "Local Cache", freeBytes: 64_000_000, totalBytes: 128_000_000)
            ]
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()

        XCTAssertEqual(model.cacheRoots.map(\.id), ["default"])
        XCTAssertEqual(model.cacheRoots.first?.displayLabel, "Local Cache")
        XCTAssertTrue(model.cacheRootSummary.contains("Local Cache"))
    }

    @MainActor
    func testDeleteItemRefreshesCacheRoots() async {
        let item = CacheLibraryItem.fixture(id: "item-a", title: "Server A item")
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [item],
            playbackSource: .fixture(),
            cacheRootResponses: [
                [.fixture(id: "default", label: "Local Cache", freeBytes: 64_000_000, totalBytes: 128_000_000)],
                [.fixture(id: "default", label: "Local Cache", freeBytes: 96_000_000, totalBytes: 128_000_000)],
            ]
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        XCTAssertEqual(model.cacheRoots.first?.freeBytes, 64_000_000)

        let deleted = await model.deleteItem(item)

        XCTAssertTrue(deleted)
        XCTAssertEqual(model.cacheRoots.first?.freeBytes, 96_000_000)
    }

    @MainActor
    func testDeleteItemConfirmsBeforeAdvisoryRefreshCompletes() async throws {
        let item = CacheLibraryItem.fixture(
            id: "item-a",
            title: "Server A item",
            variants: [.fixture(id: "original")]
        )
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [item],
            playbackSource: .fixture(
                itemID: "item-a",
                uri: "http://mac-mini.local:8080/media/item-a/original"
            ),
            suspendedCacheRootCallCounts: [2],
            suspendedDeleteItemIDs: ["item-a"]
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        let preparedURL = await model.playbackURL(for: item)
        XCTAssertNotNil(preparedURL)
        model.finishPreparedPlayback(for: item, didStartPlayback: true)
        XCTAssertTrue(model.isActivePlaybackItem(item))

        var didConfirmDelete = false
        let deleteTask = Task {
            await model.deleteItem(item) {
                didConfirmDelete = true
            }
        }
        await client.waitForDeleteRequest(itemID: "item-a")
        await client.releaseDeleteRequest(itemID: "item-a")
        await client.waitForCacheRootRequest(callCount: 2)

        XCTAssertTrue(didConfirmDelete)
        XCTAssertFalse(model.isActivePlaybackItem(item))
        XCTAssertTrue(model.deletingItemIDs.contains("item-a"))

        await client.releaseCacheRootRequest(callCount: 2)
        let deleted = await deleteTask.value

        XCTAssertTrue(deleted)
        XCTAssertTrue(model.deletingItemIDs.isEmpty)
    }

    @MainActor
    func testDeleteItemDoesNotMutateNewEndpointAfterAdvisoryRefresh() async {
        let originalItem = CacheLibraryItem.fixture(id: "item-a", title: "Server A item")
        let replacementItem = CacheLibraryItem.fixture(id: "item-a", title: "Server B item")
        let originalClient = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [originalItem],
            playbackSource: .fixture(),
            suspendedCacheRootCallCounts: [2]
        )
        let replacementClient = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server B"),
            items: [replacementItem],
            playbackSource: .fixture()
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { endpoint in
                endpoint.host == "server-a.local" ? originalClient : replacementClient
            }
        )

        await model.refresh()
        let deleteTask = Task {
            await model.deleteItem(originalItem)
        }
        await originalClient.waitForCacheRootRequest(callCount: 2)

        model.serverAddressText = "server-b.local:50051"
        await model.refresh()
        await originalClient.releaseCacheRootRequest(callCount: 2)
        let deleted = await deleteTask.value

        XCTAssertTrue(deleted)
        XCTAssertEqual(model.serverName, "Server B")
        XCTAssertEqual(model.items.map(\.displayTitle), ["Server B item"])
        XCTAssertEqual(model.statusMessage, "Loaded 1 cached item(s) from Server B.")
    }

    @MainActor
    func testStaleDeleteConfirmationDoesNotMutateNewEndpointPlaybackOrDeleteState() async {
        let originalItem = CacheLibraryItem.fixture(id: "item-a", title: "Server A item")
        let replacementItem = CacheLibraryItem.fixture(
            id: "item-a",
            title: "Server B item",
            variants: [.fixture(id: "original")]
        )
        let originalClient = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [originalItem],
            playbackSource: .fixture(),
            suspendedDeleteItemIDs: ["item-a"]
        )
        let replacementClient = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server B"),
            items: [replacementItem],
            playbackSource: .fixture(
                itemID: "item-a",
                uri: "http://server-b.local:8080/media/item-a/original"
            ),
            suspendedDeleteItemIDs: ["item-a"]
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { endpoint in
                endpoint.host == "server-a.local" ? originalClient : replacementClient
            }
        )

        await model.refresh()
        var originalDeleteConfirmed = false
        let originalDeleteTask = Task {
            await model.deleteItem(originalItem) {
                originalDeleteConfirmed = true
            }
        }
        await originalClient.waitForDeleteRequest(itemID: "item-a")

        model.serverAddressText = "server-b.local:50051"
        await model.refresh()
        let preparedURL = await model.playbackURL(for: replacementItem)
        XCTAssertNotNil(preparedURL)
        model.finishPreparedPlayback(for: replacementItem, didStartPlayback: true)
        XCTAssertTrue(model.isActivePlaybackItem(replacementItem))

        var replacementDeleteConfirmed = false
        let replacementDeleteTask = Task {
            await model.deleteItem(replacementItem) {
                replacementDeleteConfirmed = true
            }
        }
        await replacementClient.waitForDeleteRequest(itemID: "item-a")

        await originalClient.releaseDeleteRequest(itemID: "item-a")
        let originalDeleted = await originalDeleteTask.value

        XCTAssertTrue(originalDeleted)
        XCTAssertFalse(originalDeleteConfirmed)
        XCTAssertTrue(model.isActivePlaybackItem(replacementItem))
        XCTAssertTrue(model.deletingItemIDs.contains("item-a"))

        await replacementClient.releaseDeleteRequest(itemID: "item-a")
        let replacementDeleted = await replacementDeleteTask.value

        XCTAssertTrue(replacementDeleted)
        XCTAssertTrue(replacementDeleteConfirmed)
        XCTAssertFalse(model.isActivePlaybackItem(replacementItem))
        XCTAssertTrue(model.deletingItemIDs.isEmpty)
    }

    @MainActor
    func testDeleteItemErrorDoesNotMutateNewEndpointAfterAddressChange() async {
        let originalItem = CacheLibraryItem.fixture(id: "item-a", title: "Server A item")
        let replacementItem = CacheLibraryItem.fixture(id: "item-a", title: "Server B item")
        let originalClient = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [originalItem],
            playbackSource: .fixture(),
            deleteError: .serverUnavailable,
            suspendedDeleteItemIDs: ["item-a"]
        )
        let replacementClient = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server B"),
            items: [replacementItem],
            playbackSource: .fixture()
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { endpoint in
                endpoint.host == "server-a.local" ? originalClient : replacementClient
            }
        )

        await model.refresh()
        let deleteTask = Task {
            await model.deleteItem(originalItem)
        }
        await originalClient.waitForDeleteRequest(itemID: "item-a")

        model.serverAddressText = "server-b.local:50051"
        await model.refresh()
        await originalClient.releaseDeleteRequest(itemID: "item-a")
        let deleted = await deleteTask.value

        XCTAssertFalse(deleted)
        XCTAssertTrue(model.deletingItemIDs.isEmpty)
        XCTAssertEqual(model.serverName, "Server B")
        XCTAssertEqual(model.items.map(\.displayTitle), ["Server B item"])
        XCTAssertEqual(model.statusMessage, "Loaded 1 cached item(s) from Server B.")
        XCTAssertNil(model.errorMessage)
    }

    @MainActor
    func testDeleteItemRequiresServerCapability() async {
        let item = CacheLibraryItem.fixture(id: "item-a", title: "Server A item")
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A", capabilities: []),
            items: [item],
            playbackSource: .fixture()
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        let deleted = await model.deleteItem(item)

        XCTAssertFalse(deleted)
        XCTAssertFalse(model.canDelete(item))
        XCTAssertEqual(model.items.map(\.id), ["item-a"])
        let requestedDeleteItemIDs = await client.requestedDeleteItemIDs
        XCTAssertTrue(requestedDeleteItemIDs.isEmpty)
    }

    @MainActor
    func testDeleteItemRemovesLoadedItemAndCallsClient() async {
        let firstItem = CacheLibraryItem.fixture(id: "item-a", title: "Server A item")
        let secondItem = CacheLibraryItem.fixture(id: "item-b", title: "Server B item")
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [firstItem, secondItem],
            playbackSource: .fixture()
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        let deleted = await model.deleteItem(firstItem)

        XCTAssertTrue(deleted)
        XCTAssertEqual(model.items.map(\.id), ["item-b"])
        XCTAssertTrue(model.deletingItemIDs.isEmpty)
        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(
            model.statusMessage, "Deleted Server A item from Server A. Loaded 1 cached item(s) from Server A.")
        let requestedDeleteItemIDs = await client.requestedDeleteItemIDs
        XCTAssertEqual(requestedDeleteItemIDs, ["item-a"])
    }

    @MainActor
    func testDeleteItemReloadsFirstPageWhenMoreItemsAreAvailable() async {
        let firstItem = CacheLibraryItem.fixture(id: "item-a", title: "Server A item")
        let secondItem = CacheLibraryItem.fixture(id: "item-b", title: "Server B item")
        let thirdItem = CacheLibraryItem.fixture(id: "item-c", title: "Server C item")
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [],
            playbackSource: .fixture(),
            libraryPageResponsesByToken: [
                "": [
                    CacheLibraryItemsPage(items: [firstItem, secondItem], nextPageToken: "2"),
                    CacheLibraryItemsPage(items: [secondItem, thirdItem], nextPageToken: ""),
                ],
                "2": [
                    CacheLibraryItemsPage(items: [thirdItem], nextPageToken: "")
                ],
            ]
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        XCTAssertEqual(model.items.map(\.id), ["item-a", "item-b"])
        XCTAssertTrue(model.hasMoreItems)

        let deleted = await model.deleteItem(firstItem)

        XCTAssertTrue(deleted)
        XCTAssertEqual(model.items.map(\.id), ["item-b", "item-c"])
        XCTAssertFalse(model.hasMoreItems)
        let requestedPageTokens = await client.requestedLibraryPageTokens
        XCTAssertEqual(requestedPageTokens, ["", ""])

        await model.loadMore()

        let requestedPageTokensAfterLoadMore = await client.requestedLibraryPageTokens
        XCTAssertEqual(requestedPageTokensAfterLoadMore, ["", ""])
    }

    @MainActor
    func testDeleteItemRemovesStaleRowWhenServerReportsAlreadyDeleted() async {
        let item = CacheLibraryItem.fixture(id: "item-a", title: "Server A item")
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [item],
            playbackSource: .fixture(),
            deleteResponsesByItemID: ["item-a": false]
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        let deleted = await model.deleteItem(item)

        XCTAssertTrue(deleted)
        XCTAssertTrue(model.items.isEmpty)
        XCTAssertTrue(model.deletingItemIDs.isEmpty)
        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(
            model.statusMessage,
            "Removed stale Server A item from Server A. Loaded 0 cached item(s) from Server A."
        )
    }

    @MainActor
    func testLoadMoreDisablesDeleteUntilPageCompletes() async {
        let item = CacheLibraryItem.fixture(id: "item-a", title: "Server A item")
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [],
            playbackSource: .fixture(),
            libraryPagesByToken: [
                "": CacheLibraryItemsPage(items: [item], nextPageToken: "1"),
                "1": CacheLibraryItemsPage(
                    items: [.fixture(id: "item-b", title: "Server B item")],
                    nextPageToken: ""
                ),
            ],
            suspendedLibraryPageTokens: ["1"]
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        let loadMoreTask = Task {
            await model.loadMore()
        }
        await client.waitForLibraryPageRequest(pageToken: "1")

        XCTAssertTrue(model.isLoadingMore)
        XCTAssertFalse(model.canDelete(item))
        let deleted = await model.deleteItem(item)

        XCTAssertFalse(deleted)
        XCTAssertEqual(model.items.map(\.id), ["item-a"])
        let requestedDeleteItemIDs = await client.requestedDeleteItemIDs
        XCTAssertTrue(requestedDeleteItemIDs.isEmpty)

        await client.releaseLibraryPageRequest(pageToken: "1")
        await loadMoreTask.value

        XCTAssertFalse(model.isLoadingMore)
        XCTAssertTrue(model.canDelete(item))
        XCTAssertEqual(model.items.map(\.id), ["item-a", "item-b"])
    }

    @MainActor
    func testDeleteDisablesLoadMoreUntilDeleteCompletes() async {
        let firstItem = CacheLibraryItem.fixture(id: "item-a", title: "Server A item")
        let secondItem = CacheLibraryItem.fixture(id: "item-b", title: "Server B item")
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [],
            playbackSource: .fixture(),
            libraryPageResponsesByToken: [
                "": [
                    CacheLibraryItemsPage(items: [firstItem], nextPageToken: "1"),
                    CacheLibraryItemsPage(items: [secondItem], nextPageToken: ""),
                ],
                "1": [
                    CacheLibraryItemsPage(items: [secondItem], nextPageToken: "")
                ],
            ],
            suspendedDeleteItemIDs: ["item-a"]
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        let deleteTask = Task {
            await model.deleteItem(firstItem)
        }
        await client.waitForDeleteRequest(itemID: "item-a")

        XCTAssertTrue(model.deletingItemIDs.contains("item-a"))
        XCTAssertFalse(model.canLoadMore)
        await model.loadMore()

        let requestedPageTokensBeforeDeleteCompletes = await client.requestedLibraryPageTokens
        XCTAssertEqual(requestedPageTokensBeforeDeleteCompletes, [""])

        await client.releaseDeleteRequest(itemID: "item-a")
        let deleted = await deleteTask.value

        XCTAssertTrue(deleted)
        XCTAssertFalse(model.deletingItemIDs.contains("item-a"))
        XCTAssertEqual(model.items.map(\.id), ["item-b"])
        let requestedPageTokensAfterDeleteCompletes = await client.requestedLibraryPageTokens
        XCTAssertEqual(requestedPageTokensAfterDeleteCompletes, ["", ""])
    }

    @MainActor
    func testDeleteDisablesRefreshUntilDeleteCompletes() async {
        let item = CacheLibraryItem.fixture(id: "item-a", title: "Server A item")
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [item],
            playbackSource: .fixture(),
            suspendedDeleteItemIDs: ["item-a"]
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        let deleteTask = Task {
            await model.deleteItem(item)
        }
        await client.waitForDeleteRequest(itemID: "item-a")

        XCTAssertFalse(model.canRefresh)
        await model.refresh()
        XCTAssertTrue(model.deletingItemIDs.contains("item-a"))

        await client.releaseDeleteRequest(itemID: "item-a")
        let deleted = await deleteTask.value

        XCTAssertTrue(deleted)
        XCTAssertTrue(model.canRefresh)
        XCTAssertTrue(model.items.isEmpty)
        let getServerInfoCallCount = await client.getServerInfoCallCount
        XCTAssertEqual(getServerInfoCallCount, 1)
    }

    @MainActor
    func testDeleteDisablesPlaybackForDeletingItem() async {
        let item = CacheLibraryItem.fixture(id: "item-a", title: "Server A item")
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [item],
            playbackSource: .fixture(),
            suspendedDeleteItemIDs: ["item-a"]
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        let deleteTask = Task {
            await model.deleteItem(item)
        }
        await client.waitForDeleteRequest(itemID: "item-a")

        let playbackURL = await model.playbackURL(for: item)

        XCTAssertNil(playbackURL)
        let requestedPlayback = await client.requestedPlayback
        XCTAssertNil(requestedPlayback)

        await client.releaseDeleteRequest(itemID: "item-a")
        _ = await deleteTask.value
    }

    @MainActor
    func testDeleteItemClearsCachedPlaybackStatusForDeletedItem() async throws {
        let (cacheModel, _) = try await makeConfirmedCachedPlayback()
        let item = try XCTUnwrap(cacheModel.items.first)

        XCTAssertEqual(cacheModel.statusMessage, "Playing Server A item from Server A.")
        XCTAssertTrue(cacheModel.isActivePlaybackItem(item))

        let deleted = await cacheModel.deleteItem(item)

        XCTAssertTrue(deleted)
        XCTAssertTrue(cacheModel.items.isEmpty)
        XCTAssertFalse(cacheModel.isActivePlaybackItem(item))
        XCTAssertEqual(
            cacheModel.statusMessage, "Deleted Server A item from Server A. Loaded 0 cached item(s) from Server A.")
    }

    @MainActor
    func testPlaybackURLRequestsPrimaryVariant() async {
        let client = FakeCacheControlClient(
            serverInfo: .fixture(),
            items: [],
            playbackSource: .fixture(uri: "http://mac-mini.local:8080/media/item-1/original")
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "mac-mini.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )
        let item = CacheLibraryItem.fixture(
            id: "item-1",
            title: "Cached video",
            variants: [.fixture(id: "original")]
        )

        await model.refresh()
        let url = await model.playbackURL(for: item)

        XCTAssertEqual(url?.absoluteString, "http://mac-mini.local:8080/media/item-1/original")
        XCTAssertEqual(model.statusMessage, "Prepared Cached video from Mac mini cache.")
        let requestedPlayback = await client.requestedPlayback
        XCTAssertEqual(requestedPlayback?.itemID, "item-1")
        XCTAssertEqual(requestedPlayback?.variantID, "original")
    }

    @MainActor
    func testPlaybackURLRejectsSchemelessPlaybackSourceURI() async {
        let item = CacheLibraryItem.fixture(
            id: "item-a",
            title: "Server A item",
            variants: [.fixture(id: "original")]
        )
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [item],
            playbackSource: .fixture(
                itemID: "item-a",
                uri: "mac-mini.local:8080/media/item-a/original"
            )
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        let url = await model.playbackURL(for: item)

        XCTAssertNil(url)
        XCTAssertFalse(model.isLoading)
        XCTAssertEqual(model.statusMessage, "Cannot play Server A item.")
        XCTAssertEqual(model.errorMessage, "Cache server returned a non-HTTP playback URL.")
    }

    @MainActor
    func testConfirmedPreparedPlaybackUpdatesPlayingStatus() async throws {
        let item = CacheLibraryItem.fixture(
            id: "item-a",
            title: "Server A item",
            variants: [.fixture(id: "original")]
        )
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [item],
            playbackSource: .fixture(
                itemID: "item-a",
                uri: "http://mac-mini.local:8080/media/item-a/original"
            )
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        let url = await model.playbackURL(for: item)
        let playerModel = PlayerViewModel(defaults: defaults, autoplay: false)
        let sequence = playerModel.manualInteractionSequence
        let didStartPlayback = playerModel.loadTransient(
            streamURLText: try XCTUnwrap(url).absoluteString,
            ifManualInteractionSequenceMatches: sequence
        )
        model.finishPreparedPlayback(for: item, didStartPlayback: didStartPlayback)

        XCTAssertTrue(didStartPlayback)
        XCTAssertEqual(model.statusMessage, "Playing Server A item from Server A.")
    }

    @MainActor
    func testPreparedPlaybackRejectedAfterManualInteractionDoesNotShowPlayingStatus() async throws {
        let item = CacheLibraryItem.fixture(
            id: "item-a",
            title: "Server A item",
            variants: [.fixture(id: "original")]
        )
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [item],
            playbackSource: .fixture(
                itemID: "item-a",
                uri: "http://mac-mini.local:8080/media/item-a/original"
            )
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        let url = await model.playbackURL(for: item)
        let playerModel = PlayerViewModel(defaults: defaults, autoplay: false)
        let staleSequence = playerModel.manualInteractionSequence
        playerModel.load(streamURLText: "example.com/manual.m3u8")
        let didStartPlayback = playerModel.loadTransient(
            streamURLText: try XCTUnwrap(url).absoluteString,
            ifManualInteractionSequenceMatches: staleSequence
        )
        model.finishPreparedPlayback(for: item, didStartPlayback: didStartPlayback)

        XCTAssertFalse(didStartPlayback)
        XCTAssertEqual(playerModel.loadedURL?.absoluteString, "http://example.com/manual.m3u8")
        XCTAssertEqual(model.statusMessage, "Loaded 1 cached item(s) from Server A.")
    }

    @MainActor
    func testClearPlaybackStatusRestoresLoadedLibraryAfterCachedPlayback() async throws {
        let (cacheModel, _) = try await makeConfirmedCachedPlayback()

        cacheModel.clearPlaybackStatus()

        XCTAssertFalse(cacheModel.isLoading)
        XCTAssertNil(cacheModel.errorMessage)
        XCTAssertEqual(cacheModel.statusMessage, "Loaded 1 cached item(s) from Server A.")
    }

    @MainActor
    func testManualStopClearsCachedPlaybackStatus() async throws {
        let (cacheModel, playerModel) = try await makeConfirmedCachedPlayback()

        cacheModel.clearPlaybackStatus()
        playerModel.stop()

        XCTAssertEqual(cacheModel.statusMessage, "Loaded 1 cached item(s) from Server A.")
        XCTAssertEqual(playerModel.statusMessage, "Stopped.")
    }

    @MainActor
    func testManualClearClearsCachedPlaybackStatus() async throws {
        let (cacheModel, playerModel) = try await makeConfirmedCachedPlayback()

        cacheModel.clearPlaybackStatus()
        playerModel.clear()

        XCTAssertEqual(cacheModel.statusMessage, "Loaded 1 cached item(s) from Server A.")
        XCTAssertEqual(playerModel.statusMessage, "Ready for an HTTP or HTTPS stream on your network.")
    }

    @MainActor
    func testManualPlayClearsCachedPlaybackStatus() async throws {
        let (cacheModel, playerModel) = try await makeConfirmedCachedPlayback()

        cacheModel.clearPlaybackStatus()
        playerModel.load(streamURLText: "example.com/manual.m3u8")

        XCTAssertEqual(cacheModel.statusMessage, "Loaded 1 cached item(s) from Server A.")
        XCTAssertEqual(playerModel.loadedURL?.absoluteString, "http://example.com/manual.m3u8")
        XCTAssertEqual(playerModel.statusMessage, "Playing http://example.com/manual.m3u8")
    }

    @MainActor
    func testClearPlaybackStatusCancelsPendingCachedPlayback() async {
        let item = CacheLibraryItem.fixture(
            id: "item-a",
            title: "Server A item",
            variants: [.fixture(id: "original")]
        )
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [item],
            playbackSource: .fixture(
                itemID: "item-a",
                uri: "http://mac-mini.local:8080/media/item-a/original"
            ),
            suspendedPlaybackItemIDs: ["item-a"]
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        let pendingPlayback = Task {
            await model.playbackURL(for: item)
        }
        await client.waitForPlaybackSourceRequest(itemID: "item-a")

        XCTAssertTrue(model.isLoading)
        XCTAssertEqual(model.statusMessage, "Preparing Server A item...")

        model.clearPlaybackStatus()
        await client.releasePlaybackSourceRequest(itemID: "item-a")
        let url = await pendingPlayback.value

        XCTAssertNil(url)
        XCTAssertFalse(model.isLoading)
        XCTAssertEqual(model.statusMessage, "Loaded 1 cached item(s) from Server A.")
    }

    @MainActor
    func testStalePlaybackResultDoesNotReturnURLForEarlierSelection() async {
        let firstItem = CacheLibraryItem.fixture(
            id: "item-a",
            title: "First cached video",
            variants: [.fixture(id: "original")]
        )
        let secondItem = CacheLibraryItem.fixture(
            id: "item-b",
            title: "Second cached video",
            variants: [.fixture(id: "original")]
        )
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [firstItem, secondItem],
            playbackSource: .fixture(),
            playbackSourcesByItemID: [
                "item-a": .fixture(
                    itemID: "item-a",
                    uri: "http://mac-mini.local:8080/media/item-a/original"
                ),
                "item-b": .fixture(
                    itemID: "item-b",
                    uri: "http://mac-mini.local:8080/media/item-b/original"
                ),
            ],
            suspendedPlaybackItemIDs: ["item-a"]
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()

        let stalePlayback = Task {
            await model.playbackURL(for: firstItem)
        }
        await client.waitForPlaybackSourceRequest(itemID: "item-a")

        let latestURL = await model.playbackURL(for: secondItem)
        await client.releasePlaybackSourceRequest(itemID: "item-a")
        let staleURL = await stalePlayback.value

        XCTAssertNil(staleURL)
        XCTAssertEqual(latestURL?.absoluteString, "http://mac-mini.local:8080/media/item-b/original")
        XCTAssertFalse(model.isLoading)
        XCTAssertEqual(model.statusMessage, "Prepared Second cached video from Server A.")
    }

    @MainActor
    func testPlaybackURLRejectsItemsWithoutPlayableVariants() async {
        let client = FakeCacheControlClient(
            serverInfo: .fixture(),
            items: [.fixture(id: "item-a", title: "Server A item")],
            playbackSource: .fixture()
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        let url = await model.playbackURL(
            for: .fixture(id: "item-without-variants", title: "No variant item", variants: [])
        )

        XCTAssertNil(url)
        let requestedPlayback = await client.requestedPlayback
        XCTAssertNil(requestedPlayback)
        XCTAssertFalse(model.isLoading)
        XCTAssertEqual(model.statusMessage, "Cannot play No variant item.")
        XCTAssertEqual(model.errorMessage, "Cached item has no playable media variants.")
    }

    @MainActor
    func testPlaybackURLRejectsUnsupportedVariantProtocol() async {
        let client = FakeCacheControlClient(
            serverInfo: .fixture(),
            items: [.fixture(id: "item-a", title: "Server A item")],
            playbackSource: .fixture()
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        let url = await model.playbackURL(
            for: .fixture(
                id: "item-with-unsupported-variant",
                title: "Unsupported variant item",
                variants: [.fixture(id: "dash", playbackProtocol: "dash")]
            )
        )

        XCTAssertNil(url)
        let requestedPlayback = await client.requestedPlayback
        XCTAssertNil(requestedPlayback)
        XCTAssertFalse(model.isLoading)
        XCTAssertEqual(model.statusMessage, "Cannot play Unsupported variant item.")
        XCTAssertEqual(model.errorMessage, "Cached item has no playable media variants.")
    }

    @MainActor
    func testPlaybackURLRejectsUnsupportedPlaybackSourceProtocol() async {
        let client = FakeCacheControlClient(
            serverInfo: .fixture(),
            items: [.fixture(id: "item-a", title: "Server A item")],
            playbackSource: .fixture(
                itemID: "item-with-unsupported-source",
                playbackProtocol: "dash"
            )
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        let url = await model.playbackURL(
            for: .fixture(
                id: "item-with-unsupported-source",
                title: "Unsupported source item",
                variants: [.fixture(id: "original", playbackProtocol: "httpFile")]
            )
        )

        XCTAssertNil(url)
        let requestedPlayback = await client.requestedPlayback
        XCTAssertEqual(requestedPlayback?.itemID, "item-with-unsupported-source")
        XCTAssertEqual(requestedPlayback?.variantID, "original")
        XCTAssertFalse(model.isLoading)
        XCTAssertEqual(model.statusMessage, "Cannot play Unsupported source item.")
        XCTAssertEqual(model.errorMessage, "Cache server returned an unsupported playback protocol.")
    }

    @MainActor
    func testPlaybackURLRejectsMismatchedPlaybackSourceIdentity() async {
        let client = FakeCacheControlClient(
            serverInfo: .fixture(),
            items: [.fixture(id: "item-a", title: "Server A item")],
            playbackSource: .fixture(
                itemID: "different-item",
                variantID: "different-variant",
                uri: "http://mac-mini.local:8080/media/different-item/different-variant"
            )
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        let url = await model.playbackURL(
            for: .fixture(
                id: "item-a",
                title: "Server A item",
                variants: [.fixture(id: "original", playbackProtocol: "httpFile")]
            )
        )

        XCTAssertNil(url)
        let requestedPlayback = await client.requestedPlayback
        XCTAssertEqual(requestedPlayback?.itemID, "item-a")
        XCTAssertEqual(requestedPlayback?.variantID, "original")
        XCTAssertFalse(model.isLoading)
        XCTAssertEqual(model.statusMessage, "Cannot play Server A item.")
        XCTAssertEqual(model.errorMessage, "Cache server returned a mismatched playback source.")
    }

    @MainActor
    func testInvalidServerAddressDoesNotCallClient() async {
        let client = FakeCacheControlClient(serverInfo: .fixture(), items: [], playbackSource: .fixture())
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "https://mac-mini.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()

        XCTAssertEqual(model.statusMessage, "Cache server address is invalid.")
        XCTAssertNotNil(model.errorMessage)
        let getServerInfoCallCount = await client.getServerInfoCallCount
        XCTAssertEqual(getServerInfoCallCount, 0)
    }

    @MainActor
    func testServerAddressChangeClearsLoadedLibraryItems() async {
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [.fixture(id: "item-a", title: "Server A item")],
            playbackSource: .fixture()
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        XCTAssertEqual(model.items.map(\.id), ["item-a"])

        model.serverAddressText = "server-b.local:50051"

        XCTAssertTrue(model.items.isEmpty)
        XCTAssertEqual(model.serverName, "LAN cache")
        XCTAssertEqual(model.statusMessage, "Refresh cache server to load videos.")
        XCTAssertNil(model.errorMessage)
    }

    @MainActor
    func testClearingServerAddressRemovesSavedAddress() async {
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [.fixture(id: "item-a", title: "Server A item")],
            playbackSource: .fixture()
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        XCTAssertEqual(defaults.string(forKey: CacheLibraryViewModel.serverAddressDefaultsKey), "server-a.local:50051")

        model.serverAddressText = " "

        XCTAssertNil(defaults.string(forKey: CacheLibraryViewModel.serverAddressDefaultsKey))
        XCTAssertTrue(model.items.isEmpty)
        XCTAssertEqual(model.serverName, "LAN cache")
        XCTAssertEqual(model.statusMessage, "Cache server not connected.")
        XCTAssertNil(model.errorMessage)
    }

    @MainActor
    func testInvalidAddressDuringInitialRefreshClearsLoadingState() async {
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [.fixture(id: "item-a", title: "Server A item")],
            playbackSource: .fixture(),
            suspendServerInfoUntilReleased: true
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        let initialRefresh = Task {
            await model.refresh()
        }
        await client.waitForServerInfoRequest()
        XCTAssertTrue(model.isLoading)

        model.serverAddressText = "https://server-a.local:50051"

        XCTAssertFalse(model.isLoading)
        XCTAssertTrue(model.items.isEmpty)
        XCTAssertEqual(model.serverName, "LAN cache")
        XCTAssertEqual(model.statusMessage, "Cache server address is invalid.")
        XCTAssertNotNil(model.errorMessage)

        await client.releaseServerInfoRequests()
        await initialRefresh.value

        XCTAssertFalse(model.isLoading)
        XCTAssertTrue(model.items.isEmpty)
        XCTAssertEqual(model.serverName, "LAN cache")
        XCTAssertEqual(model.statusMessage, "Cache server address is invalid.")
        XCTAssertNotNil(model.errorMessage)
    }

    @MainActor
    func testRefreshRequestsFirstLibraryPage() async {
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [.fixture(id: "item-a", title: "Server A item")],
            playbackSource: .fixture()
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()

        let requestedPageSizes = await client.requestedLibraryPageSizes
        let requestedPageTokens = await client.requestedLibraryPageTokens
        let requestedSearchTexts = await client.requestedLibrarySearchTexts
        XCTAssertEqual(requestedPageSizes, [50])
        XCTAssertEqual(requestedPageTokens, [""])
        XCTAssertEqual(requestedSearchTexts, [nil])
    }

    @MainActor
    func testRefreshAppliesSearchTextToFirstLibraryPage() async {
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [.fixture(id: "item-a", title: "Server A item")],
            playbackSource: .fixture()
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        model.searchText = "  ocean clip  "
        await model.refresh()

        let requestedSearchTexts = await client.requestedLibrarySearchTexts
        XCTAssertEqual(requestedSearchTexts, ["ocean clip"])
        XCTAssertEqual(model.activeSearchText, "ocean clip")
        XCTAssertEqual(model.statusMessage, "Loaded 1 cached item(s) matching \"ocean clip\" from Server A.")
    }

    @MainActor
    func testLoadMoreAppendsNextLibraryPage() async {
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [],
            playbackSource: .fixture(),
            libraryPagesByToken: [
                "": CacheLibraryItemsPage(
                    items: [.fixture(id: "item-a", title: "Server A item")],
                    nextPageToken: "1"
                ),
                "1": CacheLibraryItemsPage(
                    items: [.fixture(id: "item-b", title: "Server B item")],
                    nextPageToken: ""
                ),
            ]
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        model.searchText = "clip"
        await model.refresh()

        XCTAssertEqual(model.items.map(\.id), ["item-a"])
        XCTAssertTrue(model.hasMoreItems)
        XCTAssertTrue(model.canLoadMore)
        XCTAssertEqual(
            model.statusMessage, "Loaded 1 cached item(s) matching \"clip\" from Server A. More items available.")

        await model.loadMore()

        let requestedPageTokens = await client.requestedLibraryPageTokens
        let requestedSearchTexts = await client.requestedLibrarySearchTexts
        XCTAssertEqual(requestedPageTokens, ["", "1"])
        XCTAssertEqual(requestedSearchTexts, ["clip", "clip"])
        XCTAssertEqual(model.items.map(\.id), ["item-a", "item-b"])
        XCTAssertFalse(model.hasMoreItems)
        XCTAssertFalse(model.canLoadMore)
        XCTAssertEqual(model.statusMessage, "Loaded 2 cached item(s) matching \"clip\" from Server A.")
    }

    @MainActor
    func testLoadMoreAppendsAfterEmptyPageWithNextPageToken() async {
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [],
            playbackSource: .fixture(),
            libraryPagesByToken: [
                "": CacheLibraryItemsPage(
                    items: [],
                    nextPageToken: "1"
                ),
                "1": CacheLibraryItemsPage(
                    items: [.fixture(id: "item-b", title: "Server B item")],
                    nextPageToken: ""
                ),
            ]
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()

        XCTAssertTrue(model.items.isEmpty)
        XCTAssertTrue(model.hasMoreItems)
        XCTAssertTrue(model.canLoadMore)
        XCTAssertEqual(model.statusMessage, "Loaded 0 cached item(s) from Server A. More items available.")

        await model.loadMore()

        let requestedPageTokens = await client.requestedLibraryPageTokens
        XCTAssertEqual(requestedPageTokens, ["", "1"])
        XCTAssertEqual(model.items.map(\.id), ["item-b"])
        XCTAssertFalse(model.hasMoreItems)
        XCTAssertFalse(model.canLoadMore)
        XCTAssertEqual(model.statusMessage, "Loaded 1 cached item(s) from Server A.")
    }

    @MainActor
    func testLoadMoreRejectsRepeatedNextPageToken() async {
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [],
            playbackSource: .fixture(),
            libraryPagesByToken: [
                "": CacheLibraryItemsPage(
                    items: [.fixture(id: "item-a", title: "Server A item")],
                    nextPageToken: "1"
                ),
                "1": CacheLibraryItemsPage(
                    items: [.fixture(id: "item-b", title: "Server B item")],
                    nextPageToken: "1"
                ),
            ]
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        await model.loadMore()

        XCTAssertEqual(model.items.map(\.id), ["item-a"])
        XCTAssertFalse(model.hasMoreItems)
        XCTAssertFalse(model.canLoadMore)
        XCTAssertEqual(model.errorMessage, "Cache server returned a repeated library page token.")
        XCTAssertEqual(model.statusMessage, "Could not load more cached videos.")
    }

    @MainActor
    func testRefreshSupersedesLoadMoreAndClearsLoadingMoreState() async {
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [],
            playbackSource: .fixture(),
            libraryPagesByToken: [
                "": CacheLibraryItemsPage(
                    items: [.fixture(id: "item-a", title: "Server A item")],
                    nextPageToken: "1"
                ),
                "1": CacheLibraryItemsPage(
                    items: [.fixture(id: "item-b", title: "Server B item")],
                    nextPageToken: ""
                ),
            ],
            suspendedLibraryPageTokens: ["1"]
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        XCTAssertEqual(model.items.map(\.id), ["item-a"])
        XCTAssertTrue(model.canLoadMore)

        let loadMoreTask = Task {
            await model.loadMore()
        }
        await client.waitForLibraryPageRequest(pageToken: "1")
        XCTAssertTrue(model.isLoading)
        XCTAssertTrue(model.isLoadingMore)

        await model.refresh()

        XCTAssertFalse(model.isLoading)
        XCTAssertFalse(model.isLoadingMore)
        XCTAssertEqual(model.items.map(\.id), ["item-a"])

        await client.releaseLibraryPageRequest(pageToken: "1")
        await loadMoreTask.value

        let requestedPageTokens = await client.requestedLibraryPageTokens
        XCTAssertEqual(requestedPageTokens, ["", "1", ""])
        XCTAssertFalse(model.isLoading)
        XCTAssertFalse(model.isLoadingMore)
        XCTAssertEqual(model.items.map(\.id), ["item-a"])
    }

    @MainActor
    func testStaleLoadMoreDoesNotClearNewLoadMoreState() async {
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [],
            playbackSource: .fixture(),
            libraryPagesByRequest: [
                LibraryPageRequestKey(pageToken: "", searchText: nil): CacheLibraryItemsPage(
                    items: [.fixture(id: "item-a", title: "Server A item")],
                    nextPageToken: "1"
                ),
                LibraryPageRequestKey(pageToken: "1", searchText: nil): CacheLibraryItemsPage(
                    items: [.fixture(id: "item-b", title: "Server B item")],
                    nextPageToken: ""
                ),
                LibraryPageRequestKey(pageToken: "", searchText: "new query"): CacheLibraryItemsPage(
                    items: [.fixture(id: "item-c", title: "Server C item")],
                    nextPageToken: "2"
                ),
                LibraryPageRequestKey(pageToken: "2", searchText: "new query"): CacheLibraryItemsPage(
                    items: [.fixture(id: "item-d", title: "Server D item")],
                    nextPageToken: ""
                ),
            ],
            suspendedLibraryPageTokens: ["1", "2"]
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        let staleLoadMore = Task {
            await model.loadMore()
        }
        await client.waitForLibraryPageRequest(pageToken: "1")
        XCTAssertTrue(model.isLoadingMore)

        model.searchText = "new query"
        await model.refresh()
        XCTAssertEqual(model.items.map(\.id), ["item-c"])
        XCTAssertTrue(model.canLoadMore)
        XCTAssertFalse(model.isLoadingMore)

        let currentLoadMore = Task {
            await model.loadMore()
        }
        await client.waitForLibraryPageRequest(pageToken: "2")
        XCTAssertTrue(model.isLoadingMore)

        await client.releaseLibraryPageRequest(pageToken: "1")
        await staleLoadMore.value

        XCTAssertTrue(model.isLoading)
        XCTAssertTrue(model.isLoadingMore)
        XCTAssertEqual(model.items.map(\.id), ["item-c"])

        await client.releaseLibraryPageRequest(pageToken: "2")
        await currentLoadMore.value

        XCTAssertFalse(model.isLoading)
        XCTAssertFalse(model.isLoadingMore)
        XCTAssertEqual(model.items.map(\.id), ["item-c", "item-d"])
    }

    @MainActor
    func testClearPlaybackStatusDoesNotEnableConcurrentLoadMore() async {
        let item = CacheLibraryItem.fixture(
            id: "item-a",
            title: "Server A item",
            variants: [.fixture(id: "original")]
        )
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [],
            playbackSource: .fixture(
                itemID: "item-a",
                uri: "http://mac-mini.local:8080/media/item-a/original"
            ),
            libraryPagesByToken: [
                "": CacheLibraryItemsPage(
                    items: [item],
                    nextPageToken: "1"
                ),
                "1": CacheLibraryItemsPage(
                    items: [.fixture(id: "item-b", title: "Server B item")],
                    nextPageToken: ""
                ),
            ],
            suspendedLibraryPageTokens: ["1"]
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        let playbackURL = await model.playbackURL(for: item)
        XCTAssertNotNil(playbackURL)
        model.finishPreparedPlayback(for: item, didStartPlayback: true)

        let loadMore = Task {
            await model.loadMore()
        }
        await client.waitForLibraryPageRequest(pageToken: "1")
        XCTAssertTrue(model.isLoadingMore)
        XCTAssertFalse(model.canLoadMore)

        model.clearPlaybackStatus()

        XCTAssertTrue(model.isLoading)
        XCTAssertTrue(model.isLoadingMore)
        XCTAssertFalse(model.canLoadMore)
        XCTAssertEqual(model.statusMessage, "Loading more cached videos...")
        await model.loadMore()

        let requestedPageTokensBeforeRelease = await client.requestedLibraryPageTokens
        XCTAssertEqual(requestedPageTokensBeforeRelease, ["", "1"])

        await client.releaseLibraryPageRequest(pageToken: "1")
        await loadMore.value

        let requestedPageTokensAfterRelease = await client.requestedLibraryPageTokens
        XCTAssertEqual(requestedPageTokensAfterRelease, ["", "1"])
        XCTAssertFalse(model.isLoading)
        XCTAssertFalse(model.isLoadingMore)
        XCTAssertEqual(model.items.map(\.id), ["item-a", "item-b"])
    }

    @MainActor
    func testChangingSearchTextMarksLoadedResultsPendingAndDisablesLoadMore() async {
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [],
            playbackSource: .fixture(),
            libraryPagesByToken: [
                "": CacheLibraryItemsPage(
                    items: [.fixture(id: "item-a", title: "Server A item")],
                    nextPageToken: "1"
                )
            ]
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await model.refresh()
        XCTAssertTrue(model.hasMoreItems)

        model.searchText = "new query"

        XCTAssertTrue(model.hasPendingSearch)
        XCTAssertFalse(model.hasMoreItems)
        XCTAssertFalse(model.canLoadMore)
        XCTAssertFalse(model.isLoading)
        XCTAssertEqual(model.statusMessage, "Search cache library to update results.")
    }

    @MainActor
    func testSearchChangeDuringInitialRefreshClearsLoadingState() async {
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [.fixture(id: "item-a", title: "Server A item")],
            playbackSource: .fixture(),
            suspendServerInfoUntilReleased: true
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        let initialRefresh = Task {
            await model.refresh()
        }
        await client.waitForServerInfoRequest()
        XCTAssertTrue(model.isLoading)

        model.searchText = "later query"

        XCTAssertFalse(model.isLoading)
        XCTAssertTrue(model.items.isEmpty)
        XCTAssertEqual(model.statusMessage, "Refresh cache server to load videos.")

        await client.releaseServerInfoRequests()
        await initialRefresh.value

        XCTAssertFalse(model.isLoading)
        XCTAssertTrue(model.items.isEmpty)
        XCTAssertEqual(model.statusMessage, "Refresh cache server to load videos.")
    }

    @MainActor
    func testWhitespaceOnlySearchEditDoesNotCancelInFlightRefresh() async {
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [.fixture(id: "item-a", title: "Server A item")],
            playbackSource: .fixture(),
            suspendedServerInfoCallCounts: [2]
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        model.searchText = "clip"
        await model.refresh()
        XCTAssertFalse(model.hasPendingSearch)
        XCTAssertEqual(model.activeSearchText, "clip")

        let reload = Task {
            await model.refresh()
        }
        await client.waitForServerInfoRequest(minimumCallCount: 2)
        XCTAssertTrue(model.isLoading)

        model.searchText = "  clip  "

        XCTAssertTrue(model.isLoading)
        XCTAssertFalse(model.hasPendingSearch)

        await client.releaseServerInfoRequest(callCount: 2)
        await reload.value

        let requestedSearchTexts = await client.requestedLibrarySearchTexts
        XCTAssertEqual(requestedSearchTexts, ["clip", "clip"])
        XCTAssertFalse(model.isLoading)
        XCTAssertEqual(model.activeSearchText, "clip")
        XCTAssertEqual(model.items.map(\.id), ["item-a"])
    }

    @MainActor
    func testRefreshTimeoutClearsLoadingState() async {
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [.fixture(id: "item-a", title: "Server A item")],
            playbackSource: .fixture(),
            getServerInfoDelayNanoseconds: 1_000_000_000
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            operationTimeout: .milliseconds(10),
            clientFactory: { _ in client }
        )

        await model.refresh()

        XCTAssertFalse(model.isLoading)
        XCTAssertTrue(model.items.isEmpty)
        XCTAssertEqual(model.serverName, "LAN cache")
        XCTAssertEqual(model.statusMessage, "Could not load cache library.")
        XCTAssertEqual(model.errorMessage, "Cache server request timed out.")
    }

    @MainActor
    func testRefreshTimeoutDoesNotWaitForNonCooperativeClient() async {
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [.fixture(id: "item-a", title: "Server A item")],
            playbackSource: .fixture(),
            getServerInfoDelayNanoseconds: 750_000_000,
            getServerInfoIgnoresCancellation: true
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            operationTimeout: .milliseconds(10),
            clientFactory: { _ in client }
        )

        let start = Date()
        await model.refresh()
        let elapsed = Date().timeIntervalSince(start)

        XCTAssertLessThan(elapsed, 0.5)
        XCTAssertFalse(model.isLoading)
        XCTAssertEqual(model.statusMessage, "Could not load cache library.")
        XCTAssertEqual(model.errorMessage, "Cache server request timed out.")
    }

    @MainActor
    func testPlaybackTimeoutClearsLoadingState() async {
        let item = CacheLibraryItem.fixture(
            id: "item-a",
            title: "Server A item",
            variants: [.fixture(id: "original")]
        )
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [item],
            playbackSource: .fixture(itemID: "item-a"),
            getPlaybackSourceDelayNanosecondsByItemID: [
                "item-a": 1_000_000_000
            ]
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            operationTimeout: .milliseconds(10),
            clientFactory: { _ in client }
        )

        await model.refresh()
        let url = await model.playbackURL(for: item)

        XCTAssertNil(url)
        XCTAssertFalse(model.isLoading)
        XCTAssertEqual(model.statusMessage, "Could not prepare Server A item.")
        XCTAssertEqual(model.errorMessage, "Cache server request timed out.")
    }

    @MainActor
    func testRefreshFailureClearsLoadedLibraryItems() async {
        let loadingClient = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [.fixture(id: "item-a", title: "Server A item")],
            playbackSource: .fixture()
        )
        let failingClient = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server B"),
            items: [.fixture(id: "item-b", title: "Server B item")],
            playbackSource: .fixture(),
            getServerInfoError: FakeCacheError.serverUnavailable
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { endpoint in
                endpoint.host == "server-a.local" ? loadingClient : failingClient
            }
        )

        await model.refresh()
        XCTAssertEqual(model.items.map(\.id), ["item-a"])

        model.serverAddressText = "server-b.local:50051"
        await model.refresh()

        XCTAssertTrue(model.items.isEmpty)
        XCTAssertEqual(model.serverName, "LAN cache")
        XCTAssertEqual(model.statusMessage, "Could not load cache library.")
        XCTAssertNotNil(model.errorMessage)
    }

    @MainActor
    func testStaleRefreshResultDoesNotOverwriteNewEndpoint() async {
        let slowClient = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [.fixture(id: "item-a", title: "Server A item")],
            playbackSource: .fixture(),
            suspendServerInfoUntilReleased: true
        )
        let fastClient = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server B"),
            items: [.fixture(id: "item-b", title: "Server B item")],
            playbackSource: .fixture()
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { endpoint in
                endpoint.host == "server-a.local" ? slowClient : fastClient
            }
        )

        let staleRefresh = Task {
            await model.refresh()
        }
        await slowClient.waitForServerInfoRequest()

        model.serverAddressText = "server-b.local:50051"
        await model.refresh()
        await slowClient.releaseServerInfoRequests()
        await staleRefresh.value

        XCTAssertEqual(model.serverAddressText, "server-b.local:50051")
        XCTAssertEqual(model.serverName, "Server B")
        XCTAssertEqual(model.items.map(\.id), ["item-b"])
        XCTAssertEqual(model.statusMessage, "Loaded 1 cached item(s) from Server B.")
    }

    @MainActor
    private func makeConfirmedCachedPlayback() async throws -> (CacheLibraryViewModel, PlayerViewModel) {
        let item = CacheLibraryItem.fixture(
            id: "item-a",
            title: "Server A item",
            variants: [.fixture(id: "original")]
        )
        let client = FakeCacheControlClient(
            serverInfo: .fixture(name: "Server A"),
            items: [item],
            playbackSource: .fixture(
                itemID: "item-a",
                uri: "http://mac-mini.local:8080/media/item-a/original"
            )
        )
        let cacheModel = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        await cacheModel.refresh()
        let preparedURL = await cacheModel.playbackURL(for: item)
        let url = try XCTUnwrap(preparedURL)
        let playerModel = PlayerViewModel(defaults: defaults, autoplay: false)
        let didStartPlayback = playerModel.loadTransient(streamURLText: url.absoluteString)
        cacheModel.finishPreparedPlayback(for: item, didStartPlayback: didStartPlayback)

        XCTAssertTrue(didStartPlayback)
        XCTAssertEqual(cacheModel.statusMessage, "Playing Server A item from Server A.")
        return (cacheModel, playerModel)
    }
}

private struct LibraryPageRequestKey: Hashable {
    let pageToken: String
    let searchText: String?
}

private actor FakeCacheControlClient: CacheControlClient {
    let serverInfo: CacheServerSummary
    let items: [CacheLibraryItem]
    let playbackSource: CachePlaybackSource
    let cacheRoots: [CacheRoot]
    let cacheRootResponses: [[CacheRoot]]
    let deleteResponsesByItemID: [String: Bool]
    let deleteError: FakeCacheError?
    let libraryPagesByRequest: [LibraryPageRequestKey: CacheLibraryItemsPage]
    let libraryPagesByToken: [String: CacheLibraryItemsPage]
    let libraryPageResponsesByToken: [String: [CacheLibraryItemsPage]]
    let getServerInfoDelayNanoseconds: UInt64
    let getPlaybackSourceDelayNanosecondsByItemID: [String: UInt64]
    let playbackSourcesByItemID: [String: CachePlaybackSource]
    let getServerInfoError: FakeCacheError?
    let getServerInfoIgnoresCancellation: Bool
    let suspendServerInfoUntilReleased: Bool
    let suspendedServerInfoCallCounts: Set<Int>
    let suspendedCacheRootCallCounts: Set<Int>
    let suspendedLibraryPageTokens: Set<String>
    let suspendedPlaybackItemIDs: Set<String>
    let suspendedDeleteItemIDs: Set<String>

    private(set) var getServerInfoCallCount = 0
    private(set) var cacheRootCallCount = 0
    private(set) var requestedLibraryPageSizes: [Int] = []
    private(set) var requestedLibraryPageTokens: [String] = []
    private(set) var requestedLibrarySearchTexts: [String?] = []
    private(set) var requestedDeleteItemIDs: [String] = []
    private(set) var requestedPlayback: (itemID: String, variantID: String)?
    private var cacheRootResponseIndex = 0
    private var getServerInfoWaiters: [(minimumCallCount: Int, continuation: CheckedContinuation<Void, Never>)] = []
    private var serverInfoReleaseContinuations: [CheckedContinuation<Void, Never>] = []
    private var serverInfoCallReleaseContinuations: [Int: [CheckedContinuation<Void, Never>]] = [:]
    private var serverInfoRequestsReleased = false
    private var releasedServerInfoCallCounts: Set<Int> = []
    private var cacheRootWaiters: [(minimumCallCount: Int, continuation: CheckedContinuation<Void, Never>)] = []
    private var cacheRootReleaseContinuations: [Int: [CheckedContinuation<Void, Never>]] = [:]
    private var releasedCacheRootCallCounts: Set<Int> = []
    private var libraryPageWaiters: [(pageToken: String, continuation: CheckedContinuation<Void, Never>)] = []
    private var libraryPageReleaseContinuations: [String: [CheckedContinuation<Void, Never>]] = [:]
    private var requestedLibraryPageTokenSet: Set<String> = []
    private var releasedLibraryPageTokens: Set<String> = []
    private var libraryPageResponseIndexesByToken: [String: Int] = [:]
    private var playbackStartedItemIDs: [String] = []
    private var playbackWaiters: [(itemID: String, continuation: CheckedContinuation<Void, Never>)] = []
    private var playbackReleaseContinuations: [String: [CheckedContinuation<Void, Never>]] = [:]
    private var releasedPlaybackItemIDs: Set<String> = []
    private var deleteStartedItemIDs: [String] = []
    private var deleteWaiters: [(itemID: String, continuation: CheckedContinuation<Void, Never>)] = []
    private var deleteReleaseContinuations: [String: [CheckedContinuation<Void, Never>]] = [:]
    private var releasedDeleteItemIDs: Set<String> = []

    init(
        serverInfo: CacheServerSummary,
        items: [CacheLibraryItem],
        playbackSource: CachePlaybackSource,
        cacheRoots: [CacheRoot] = [.fixture()],
        cacheRootResponses: [[CacheRoot]] = [],
        deleteResponsesByItemID: [String: Bool] = [:],
        deleteError: FakeCacheError? = nil,
        libraryPagesByRequest: [LibraryPageRequestKey: CacheLibraryItemsPage] = [:],
        libraryPagesByToken: [String: CacheLibraryItemsPage] = [:],
        libraryPageResponsesByToken: [String: [CacheLibraryItemsPage]] = [:],
        getServerInfoDelayNanoseconds: UInt64 = 0,
        playbackSourcesByItemID: [String: CachePlaybackSource] = [:],
        getPlaybackSourceDelayNanosecondsByItemID: [String: UInt64] = [:],
        getServerInfoError: FakeCacheError? = nil,
        getServerInfoIgnoresCancellation: Bool = false,
        suspendServerInfoUntilReleased: Bool = false,
        suspendedServerInfoCallCounts: Set<Int> = [],
        suspendedCacheRootCallCounts: Set<Int> = [],
        suspendedLibraryPageTokens: Set<String> = [],
        suspendedPlaybackItemIDs: Set<String> = [],
        suspendedDeleteItemIDs: Set<String> = []
    ) {
        self.serverInfo = serverInfo
        self.items = items
        self.playbackSource = playbackSource
        self.cacheRoots = cacheRoots
        self.cacheRootResponses = cacheRootResponses
        self.deleteResponsesByItemID = deleteResponsesByItemID
        self.deleteError = deleteError
        self.libraryPagesByRequest = libraryPagesByRequest
        self.libraryPagesByToken = libraryPagesByToken
        self.libraryPageResponsesByToken = libraryPageResponsesByToken
        self.getServerInfoDelayNanoseconds = getServerInfoDelayNanoseconds
        self.playbackSourcesByItemID = playbackSourcesByItemID
        self.getPlaybackSourceDelayNanosecondsByItemID = getPlaybackSourceDelayNanosecondsByItemID
        self.getServerInfoError = getServerInfoError
        self.getServerInfoIgnoresCancellation = getServerInfoIgnoresCancellation
        self.suspendServerInfoUntilReleased = suspendServerInfoUntilReleased
        self.suspendedServerInfoCallCounts = suspendedServerInfoCallCounts
        self.suspendedCacheRootCallCounts = suspendedCacheRootCallCounts
        self.suspendedLibraryPageTokens = suspendedLibraryPageTokens
        self.suspendedPlaybackItemIDs = suspendedPlaybackItemIDs
        self.suspendedDeleteItemIDs = suspendedDeleteItemIDs
    }

    func getServerInfo() async throws -> CacheServerSummary {
        getServerInfoCallCount += 1
        let callCount = getServerInfoCallCount
        notifyServerInfoWaiters()
        if suspendServerInfoUntilReleased {
            await waitForServerInfoRelease()
        }
        if suspendedServerInfoCallCounts.contains(callCount) {
            await waitForServerInfoRelease(callCount: callCount)
        }
        if getServerInfoDelayNanoseconds > 0 {
            if getServerInfoIgnoresCancellation {
                await sleepIgnoringCancellation(nanoseconds: getServerInfoDelayNanoseconds)
            } else {
                try await Task.sleep(nanoseconds: getServerInfoDelayNanoseconds)
            }
        }
        if let getServerInfoError {
            throw getServerInfoError
        }
        return serverInfo
    }

    func listCacheRoots() async throws -> [CacheRoot] {
        cacheRootCallCount += 1
        let callCount = cacheRootCallCount
        notifyCacheRootWaiters()
        if suspendedCacheRootCallCounts.contains(callCount) {
            await waitForCacheRootRelease(callCount: callCount)
        }

        if cacheRootResponseIndex < cacheRootResponses.count {
            let response = cacheRootResponses[cacheRootResponseIndex]
            cacheRootResponseIndex += 1
            return response
        }

        return cacheRoots
    }

    func listLibraryItemsPage(
        pageToken: String,
        pageSize: Int,
        searchText: String?
    ) async throws -> CacheLibraryItemsPage {
        requestedLibraryPageTokens.append(pageToken)
        requestedLibraryPageTokenSet.insert(pageToken)
        requestedLibraryPageSizes.append(pageSize)
        requestedLibrarySearchTexts.append(searchText)
        notifyLibraryPageWaiters(for: pageToken)
        if suspendedLibraryPageTokens.contains(pageToken) {
            await waitForLibraryPageRelease(pageToken: pageToken)
        }

        let requestKey = LibraryPageRequestKey(pageToken: pageToken, searchText: searchText)
        if let page = libraryPagesByRequest[requestKey] {
            return page
        }

        if let pages = libraryPageResponsesByToken[pageToken], !pages.isEmpty {
            let index = libraryPageResponseIndexesByToken[pageToken, default: 0]
            libraryPageResponseIndexesByToken[pageToken] = index + 1
            return pages[min(index, pages.count - 1)]
        }

        if let page = libraryPagesByToken[pageToken] {
            return page
        }

        return CacheLibraryItemsPage(items: items, nextPageToken: "")
    }

    func getPlaybackSource(itemID: String, variantID: String) async throws -> CachePlaybackSource {
        requestedPlayback = (itemID, variantID)
        playbackStartedItemIDs.append(itemID)
        notifyPlaybackWaiters(for: itemID)
        if suspendedPlaybackItemIDs.contains(itemID) {
            await waitForPlaybackRelease(itemID: itemID)
        }
        if let delay = getPlaybackSourceDelayNanosecondsByItemID[itemID], delay > 0 {
            try await Task.sleep(nanoseconds: delay)
        }

        return playbackSourcesByItemID[itemID] ?? playbackSource
    }

    func deleteLibraryItem(id: String) async throws -> Bool {
        requestedDeleteItemIDs.append(id)
        deleteStartedItemIDs.append(id)
        notifyDeleteWaiters(for: id)
        if suspendedDeleteItemIDs.contains(id) {
            await waitForDeleteRelease(itemID: id)
        }
        if let deleteError {
            throw deleteError
        }

        return deleteResponsesByItemID[id] ?? true
    }

    func getTask(id: String) async throws -> CacheTask {
        throw FakeCacheError.serverUnavailable
    }

    func watchTasks(ids: [String]) async -> AsyncThrowingStream<CacheTask, Error> {
        AsyncThrowingStream { continuation in
            continuation.finish(throwing: FakeCacheError.serverUnavailable)
        }
    }

    func cancelTask(id: String) async throws -> CacheTask {
        throw FakeCacheError.serverUnavailable
    }

    func createBilibiliPlaybackTask(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask {
        throw FakeCacheError.serverUnavailable
    }

    func waitForServerInfoRequest(minimumCallCount: Int = 1) async {
        guard getServerInfoCallCount < minimumCallCount else {
            return
        }

        await withCheckedContinuation { continuation in
            getServerInfoWaiters.append((minimumCallCount, continuation))
        }
    }

    func waitForCacheRootRequest(callCount: Int) async {
        guard cacheRootCallCount < callCount else {
            return
        }

        await withCheckedContinuation { continuation in
            cacheRootWaiters.append((callCount, continuation))
        }
    }

    func releaseCacheRootRequest(callCount: Int) {
        releasedCacheRootCallCounts.insert(callCount)
        let continuations = cacheRootReleaseContinuations.removeValue(forKey: callCount) ?? []
        continuations.forEach { $0.resume() }
    }

    func releaseServerInfoRequests() {
        serverInfoRequestsReleased = true
        let continuations = serverInfoReleaseContinuations
        serverInfoReleaseContinuations = []
        continuations.forEach { $0.resume() }
    }

    func releaseServerInfoRequest(callCount: Int) {
        releasedServerInfoCallCounts.insert(callCount)
        let continuations = serverInfoCallReleaseContinuations.removeValue(forKey: callCount) ?? []
        continuations.forEach { $0.resume() }
    }

    func waitForLibraryPageRequest(pageToken: String) async {
        guard !requestedLibraryPageTokenSet.contains(pageToken) else {
            return
        }

        await withCheckedContinuation { continuation in
            libraryPageWaiters.append((pageToken, continuation))
        }
    }

    func releaseLibraryPageRequest(pageToken: String) {
        releasedLibraryPageTokens.insert(pageToken)
        let continuations = libraryPageReleaseContinuations.removeValue(forKey: pageToken) ?? []
        continuations.forEach { $0.resume() }
    }

    func waitForPlaybackSourceRequest(itemID: String) async {
        guard !playbackStartedItemIDs.contains(itemID) else {
            return
        }

        await withCheckedContinuation { continuation in
            playbackWaiters.append((itemID, continuation))
        }
    }

    func releasePlaybackSourceRequest(itemID: String) {
        releasedPlaybackItemIDs.insert(itemID)
        let continuations = playbackReleaseContinuations.removeValue(forKey: itemID) ?? []
        continuations.forEach { $0.resume() }
    }

    func waitForDeleteRequest(itemID: String) async {
        guard !deleteStartedItemIDs.contains(itemID) else {
            return
        }

        await withCheckedContinuation { continuation in
            deleteWaiters.append((itemID, continuation))
        }
    }

    func releaseDeleteRequest(itemID: String) {
        releasedDeleteItemIDs.insert(itemID)
        let continuations = deleteReleaseContinuations.removeValue(forKey: itemID) ?? []
        continuations.forEach { $0.resume() }
    }

    private func notifyServerInfoWaiters() {
        var readyContinuations: [CheckedContinuation<Void, Never>] = []
        getServerInfoWaiters.removeAll { waiter in
            guard getServerInfoCallCount >= waiter.minimumCallCount else {
                return false
            }

            readyContinuations.append(waiter.continuation)
            return true
        }
        readyContinuations.forEach { $0.resume() }
    }

    private func notifyCacheRootWaiters() {
        var readyContinuations: [CheckedContinuation<Void, Never>] = []
        cacheRootWaiters.removeAll { waiter in
            guard cacheRootCallCount >= waiter.minimumCallCount else {
                return false
            }

            readyContinuations.append(waiter.continuation)
            return true
        }
        readyContinuations.forEach { $0.resume() }
    }

    private func waitForCacheRootRelease(callCount: Int) async {
        guard !releasedCacheRootCallCounts.contains(callCount) else {
            return
        }

        await withCheckedContinuation { continuation in
            cacheRootReleaseContinuations[callCount, default: []].append(continuation)
        }
    }

    private func waitForServerInfoRelease() async {
        guard !serverInfoRequestsReleased else {
            return
        }

        await withCheckedContinuation { continuation in
            serverInfoReleaseContinuations.append(continuation)
        }
    }

    private func waitForServerInfoRelease(callCount: Int) async {
        guard !releasedServerInfoCallCounts.contains(callCount) else {
            return
        }

        await withCheckedContinuation { continuation in
            serverInfoCallReleaseContinuations[callCount, default: []].append(continuation)
        }
    }

    private func notifyLibraryPageWaiters(for pageToken: String) {
        var readyContinuations: [CheckedContinuation<Void, Never>] = []
        libraryPageWaiters.removeAll { waiter in
            guard waiter.pageToken == pageToken else {
                return false
            }

            readyContinuations.append(waiter.continuation)
            return true
        }
        readyContinuations.forEach { $0.resume() }
    }

    private func waitForLibraryPageRelease(pageToken: String) async {
        guard !releasedLibraryPageTokens.contains(pageToken) else {
            return
        }

        await withCheckedContinuation { continuation in
            libraryPageReleaseContinuations[pageToken, default: []].append(continuation)
        }
    }

    private func notifyPlaybackWaiters(for itemID: String) {
        var readyContinuations: [CheckedContinuation<Void, Never>] = []
        playbackWaiters.removeAll { waiter in
            guard waiter.itemID == itemID else {
                return false
            }

            readyContinuations.append(waiter.continuation)
            return true
        }
        readyContinuations.forEach { $0.resume() }
    }

    private func waitForPlaybackRelease(itemID: String) async {
        guard !releasedPlaybackItemIDs.contains(itemID) else {
            return
        }

        await withCheckedContinuation { continuation in
            playbackReleaseContinuations[itemID, default: []].append(continuation)
        }
    }

    private func notifyDeleteWaiters(for itemID: String) {
        var readyContinuations: [CheckedContinuation<Void, Never>] = []
        deleteWaiters.removeAll { waiter in
            guard waiter.itemID == itemID else {
                return false
            }

            readyContinuations.append(waiter.continuation)
            return true
        }
        readyContinuations.forEach { $0.resume() }
    }

    private func waitForDeleteRelease(itemID: String) async {
        guard !releasedDeleteItemIDs.contains(itemID) else {
            return
        }

        await withCheckedContinuation { continuation in
            deleteReleaseContinuations[itemID, default: []].append(continuation)
        }
    }

    private func sleepIgnoringCancellation(nanoseconds: UInt64) async {
        let deadline = Date().addingTimeInterval(Double(nanoseconds) / 1_000_000_000)
        while Date() < deadline {
            let remainingNanoseconds = UInt64(max(0, deadline.timeIntervalSinceNow) * 1_000_000_000)
            let interval = min(remainingNanoseconds, 10_000_000)
            guard interval > 0 else {
                return
            }

            do {
                try await Task.sleep(nanoseconds: interval)
            } catch {
                continue
            }
        }
    }
}

private enum FakeCacheError: Error {
    case serverUnavailable
}

extension CacheServerSummary {
    fileprivate static func fixture(
        name: String = "Mac mini cache",
        capabilities: [String] = [CacheServerCapability.libraryItemDelete]
    ) -> Self {
        Self(id: "server-1", name: name, version: "0.1.0", mediaBaseURIs: [], capabilities: capabilities)
    }
}

extension CacheRoot {
    fileprivate static func fixture(
        id: String = "default",
        label: String = "Local Cache",
        writable: Bool = true,
        freeBytes: Int64 = 128_000_000,
        totalBytes: Int64 = 256_000_000
    ) -> Self {
        Self(
            id: id,
            label: label,
            kind: "CACHE_ROOT_KIND_LOCAL_DIRECTORY",
            path: "/tmp/cache",
            writable: writable,
            freeBytes: freeBytes,
            totalBytes: totalBytes
        )
    }
}

extension CacheLibraryItem {
    fileprivate static func fixture(
        id: String,
        title: String,
        variants: [CacheMediaVariant] = [.fixture()]
    ) -> Self {
        Self(
            id: id,
            title: title,
            subtitle: "",
            source: "localCache",
            sourceID: id,
            posterURI: "",
            variants: variants
        )
    }
}

extension CacheMediaVariant {
    fileprivate static func fixture(id: String = "original", playbackProtocol: String = "httpFile") -> Self {
        Self(
            id: id,
            label: "Original",
            playbackProtocol: playbackProtocol,
            container: "mp4",
            videoCodec: "",
            audioCodec: "",
            width: 1920,
            height: 1080,
            bitrate: 0,
            sizeBytes: 0
        )
    }
}

extension CachePlaybackSource {
    fileprivate static func fixture(
        itemID: String = "item-1",
        variantID: String = "original",
        playbackProtocol: String = "httpFile",
        uri: String = "http://mac-mini.local:8080/media/item-1/original"
    ) -> Self {
        Self(itemID: itemID, variantID: variantID, playbackProtocol: playbackProtocol, uri: uri)
    }
}
