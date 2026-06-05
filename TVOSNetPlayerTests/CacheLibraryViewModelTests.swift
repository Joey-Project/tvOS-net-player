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

        let url = await model.playbackURL(for: item)

        XCTAssertEqual(url?.absoluteString, "http://mac-mini.local:8080/media/item-1/original")
        let requestedPlayback = await client.requestedPlayback
        XCTAssertEqual(requestedPlayback?.itemID, "item-1")
        XCTAssertEqual(requestedPlayback?.variantID, "original")
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
}

private actor FakeCacheControlClient: CacheControlClient {
    let serverInfo: CacheServerSummary
    let items: [CacheLibraryItem]
    let playbackSource: CachePlaybackSource

    private(set) var getServerInfoCallCount = 0
    private(set) var requestedPlayback: (itemID: String, variantID: String?)?

    init(serverInfo: CacheServerSummary, items: [CacheLibraryItem], playbackSource: CachePlaybackSource) {
        self.serverInfo = serverInfo
        self.items = items
        self.playbackSource = playbackSource
    }

    func getServerInfo() async throws -> CacheServerSummary {
        getServerInfoCallCount += 1
        return serverInfo
    }

    func listLibraryItems(pageSize: Int) async throws -> [CacheLibraryItem] {
        items
    }

    func getPlaybackSource(itemID: String, variantID: String?) async throws -> CachePlaybackSource {
        requestedPlayback = (itemID, variantID)
        return playbackSource
    }
}

extension CacheServerSummary {
    fileprivate static func fixture() -> Self {
        Self(id: "server-1", name: "Mac mini cache", version: "0.1.0", mediaBaseURIs: [], capabilities: [])
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
    fileprivate static func fixture(id: String = "original") -> Self {
        Self(
            id: id,
            label: "Original",
            playbackProtocol: "httpFile",
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
    fileprivate static func fixture(uri: String = "http://mac-mini.local:8080/media/item-1/original") -> Self {
        Self(itemID: "item-1", variantID: "original", playbackProtocol: "httpFile", uri: uri)
    }
}
