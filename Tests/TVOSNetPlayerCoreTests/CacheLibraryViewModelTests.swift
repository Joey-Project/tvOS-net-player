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
    func testRefreshRequestsPreviewPageSize() async {
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
        XCTAssertEqual(requestedPageSizes, [200])
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

private actor FakeCacheControlClient: CacheControlClient {
    let serverInfo: CacheServerSummary
    let items: [CacheLibraryItem]
    let playbackSource: CachePlaybackSource
    let getServerInfoDelayNanoseconds: UInt64
    let getPlaybackSourceDelayNanosecondsByItemID: [String: UInt64]
    let playbackSourcesByItemID: [String: CachePlaybackSource]
    let getServerInfoError: FakeCacheError?
    let getServerInfoIgnoresCancellation: Bool
    let suspendServerInfoUntilReleased: Bool
    let suspendedPlaybackItemIDs: Set<String>

    private(set) var getServerInfoCallCount = 0
    private(set) var requestedLibraryPageSizes: [Int] = []
    private(set) var requestedPlayback: (itemID: String, variantID: String)?
    private var getServerInfoWaiters: [(minimumCallCount: Int, continuation: CheckedContinuation<Void, Never>)] = []
    private var serverInfoReleaseContinuations: [CheckedContinuation<Void, Never>] = []
    private var serverInfoRequestsReleased = false
    private var playbackStartedItemIDs: [String] = []
    private var playbackWaiters: [(itemID: String, continuation: CheckedContinuation<Void, Never>)] = []
    private var playbackReleaseContinuations: [String: [CheckedContinuation<Void, Never>]] = [:]
    private var releasedPlaybackItemIDs: Set<String> = []

    init(
        serverInfo: CacheServerSummary,
        items: [CacheLibraryItem],
        playbackSource: CachePlaybackSource,
        getServerInfoDelayNanoseconds: UInt64 = 0,
        playbackSourcesByItemID: [String: CachePlaybackSource] = [:],
        getPlaybackSourceDelayNanosecondsByItemID: [String: UInt64] = [:],
        getServerInfoError: FakeCacheError? = nil,
        getServerInfoIgnoresCancellation: Bool = false,
        suspendServerInfoUntilReleased: Bool = false,
        suspendedPlaybackItemIDs: Set<String> = []
    ) {
        self.serverInfo = serverInfo
        self.items = items
        self.playbackSource = playbackSource
        self.getServerInfoDelayNanoseconds = getServerInfoDelayNanoseconds
        self.playbackSourcesByItemID = playbackSourcesByItemID
        self.getPlaybackSourceDelayNanosecondsByItemID = getPlaybackSourceDelayNanosecondsByItemID
        self.getServerInfoError = getServerInfoError
        self.getServerInfoIgnoresCancellation = getServerInfoIgnoresCancellation
        self.suspendServerInfoUntilReleased = suspendServerInfoUntilReleased
        self.suspendedPlaybackItemIDs = suspendedPlaybackItemIDs
    }

    func getServerInfo() async throws -> CacheServerSummary {
        getServerInfoCallCount += 1
        notifyServerInfoWaiters()
        if suspendServerInfoUntilReleased {
            await waitForServerInfoRelease()
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

    func listLibraryItemsPage(
        pageToken: String,
        pageSize: Int,
        searchText: String?
    ) async throws -> CacheLibraryItemsPage {
        requestedLibraryPageSizes.append(pageSize)
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

    func releaseServerInfoRequests() {
        serverInfoRequestsReleased = true
        let continuations = serverInfoReleaseContinuations
        serverInfoReleaseContinuations = []
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

    private func waitForServerInfoRelease() async {
        guard !serverInfoRequestsReleased else {
            return
        }

        await withCheckedContinuation { continuation in
            serverInfoReleaseContinuations.append(continuation)
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
    fileprivate static func fixture(name: String = "Mac mini cache") -> Self {
        Self(id: "server-1", name: name, version: "0.1.0", mediaBaseURIs: [], capabilities: [])
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
