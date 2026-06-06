import XCTest
import TVOSNetPlayerCacheClient
@testable import TVOSNetPlayer

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
        let requestedPlayback = await client.requestedPlayback
        XCTAssertEqual(requestedPlayback?.itemID, "item-1")
        XCTAssertEqual(requestedPlayback?.variantID, "original")
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
            getPlaybackSourceDelayNanosecondsByItemID: [
                "item-a": 100_000_000
            ]
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
        try? await Task.sleep(nanoseconds: 10_000_000)

        let latestURL = await model.playbackURL(for: secondItem)
        let staleURL = await stalePlayback.value

        XCTAssertNil(staleURL)
        XCTAssertEqual(latestURL?.absoluteString, "http://mac-mini.local:8080/media/item-b/original")
        XCTAssertFalse(model.isLoading)
        XCTAssertEqual(model.statusMessage, "Playing Second cached video from Server A.")
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
            getServerInfoDelayNanoseconds: 100_000_000
        )
        let model = CacheLibraryViewModel(
            defaultServerAddressText: "server-a.local:50051",
            defaults: defaults,
            clientFactory: { _ in client }
        )

        let initialRefresh = Task {
            await model.refresh()
        }
        try? await Task.sleep(nanoseconds: 10_000_000)
        XCTAssertTrue(model.isLoading)

        model.serverAddressText = "https://server-a.local:50051"

        XCTAssertFalse(model.isLoading)
        XCTAssertTrue(model.items.isEmpty)
        XCTAssertEqual(model.serverName, "LAN cache")
        XCTAssertEqual(model.statusMessage, "Cache server address is invalid.")
        XCTAssertNotNil(model.errorMessage)

        await initialRefresh.value

        XCTAssertFalse(model.isLoading)
        XCTAssertTrue(model.items.isEmpty)
        XCTAssertEqual(model.serverName, "LAN cache")
        XCTAssertEqual(model.statusMessage, "Cache server address is invalid.")
        XCTAssertNotNil(model.errorMessage)
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
            getServerInfoDelayNanoseconds: 100_000_000
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
        try? await Task.sleep(nanoseconds: 10_000_000)

        model.serverAddressText = "server-b.local:50051"
        await model.refresh()
        await staleRefresh.value

        XCTAssertEqual(model.serverAddressText, "server-b.local:50051")
        XCTAssertEqual(model.serverName, "Server B")
        XCTAssertEqual(model.items.map(\.id), ["item-b"])
        XCTAssertEqual(model.statusMessage, "Loaded 1 cached item(s) from Server B.")
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

    private(set) var getServerInfoCallCount = 0
    private(set) var requestedPlayback: (itemID: String, variantID: String)?

    init(
        serverInfo: CacheServerSummary,
        items: [CacheLibraryItem],
        playbackSource: CachePlaybackSource,
        getServerInfoDelayNanoseconds: UInt64 = 0,
        playbackSourcesByItemID: [String: CachePlaybackSource] = [:],
        getPlaybackSourceDelayNanosecondsByItemID: [String: UInt64] = [:],
        getServerInfoError: FakeCacheError? = nil
    ) {
        self.serverInfo = serverInfo
        self.items = items
        self.playbackSource = playbackSource
        self.getServerInfoDelayNanoseconds = getServerInfoDelayNanoseconds
        self.playbackSourcesByItemID = playbackSourcesByItemID
        self.getPlaybackSourceDelayNanosecondsByItemID = getPlaybackSourceDelayNanosecondsByItemID
        self.getServerInfoError = getServerInfoError
    }

    func getServerInfo() async throws -> CacheServerSummary {
        getServerInfoCallCount += 1
        if getServerInfoDelayNanoseconds > 0 {
            try await Task.sleep(nanoseconds: getServerInfoDelayNanoseconds)
        }
        if let getServerInfoError {
            throw getServerInfoError
        }
        return serverInfo
    }

    func listLibraryItems(pageSize: Int) async throws -> [CacheLibraryItem] {
        items
    }

    func getPlaybackSource(itemID: String, variantID: String) async throws -> CachePlaybackSource {
        requestedPlayback = (itemID, variantID)
        if let delay = getPlaybackSourceDelayNanosecondsByItemID[itemID], delay > 0 {
            try await Task.sleep(nanoseconds: delay)
        }

        return playbackSourcesByItemID[itemID] ?? playbackSource
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
        uri: String = "http://mac-mini.local:8080/media/item-1/original"
    ) -> Self {
        Self(itemID: itemID, variantID: variantID, playbackProtocol: "httpFile", uri: uri)
    }
}
