import TVOSNetPlayerCacheClient
import XCTest
@testable import TVOSNetPlayerCore

final class CacheServerDiscoveryViewModelTests: XCTestCase {
    private var defaultsSuiteName: String!
    private var defaults: UserDefaults!

    override func setUpWithError() throws {
        try super.setUpWithError()
        defaultsSuiteName = "CacheServerDiscoveryViewModelTests-\(UUID().uuidString)"
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
    func testDiscoverySnapshotUpdatesServersAndSelection() async {
        let client = FakeDiscoveryClient()
        let model = CacheServerDiscoveryViewModel(discoveryClient: client)
        let livingRoom = DiscoveredCacheServer(
            id: "living-room",
            name: "Living Room Cache",
            endpoint: CacheServerEndpoint(host: "living-room.local", port: 50_051),
            serverID: "server-living-room",
            version: "0.1.0"
        )
        let office = DiscoveredCacheServer(
            id: "office",
            name: "Office Cache",
            endpoint: CacheServerEndpoint(host: "office.local", port: 50_051)
        )

        model.start()
        client.yield(
            CacheServerDiscoverySnapshot(
                servers: [office, livingRoom],
                isSearching: true
            )
        )
        await waitUntil { model.discoveredServers.count == 2 }

        XCTAssertEqual(model.discoveredServers.map(\.id), ["living-room", "office"])
        XCTAssertTrue(model.isSearching)
        XCTAssertEqual(model.statusMessage, "Found 2 LAN cache servers.")

        model.select(office)
        XCTAssertEqual(model.preferredServer?.id, "office")

        client.yield(
            CacheServerDiscoverySnapshot(
                servers: [livingRoom],
                isSearching: true
            )
        )
        await waitUntil { model.discoveredServers.count == 1 }

        XCTAssertNil(model.selectedServerID)
        XCTAssertEqual(model.preferredServer?.id, "living-room")
    }

    @MainActor
    func testCacheLibraryStagesDiscoveredServerAddressWithoutPersistingBeforeRefresh() {
        let server = DiscoveredCacheServer(
            id: "living-room",
            name: "Living Room Cache",
            endpoint: CacheServerEndpoint(host: "living-room.local", port: 50_051)
        )
        let model = CacheLibraryViewModel(defaults: defaults)

        model.useDiscoveredServer(server)

        XCTAssertEqual(model.serverAddressText, "living-room.local:50051")
        XCTAssertNil(defaults.string(forKey: CacheLibraryViewModel.serverAddressDefaultsKey))
        XCTAssertEqual(model.statusMessage, "Discovered Living Room Cache. Refresh cache server to load videos.")
        XCTAssertNil(model.errorMessage)
    }

    @MainActor
    func testCacheLibraryCanClearFailedDiscoveredServerAddress() {
        let server = DiscoveredCacheServer(
            id: "living-room",
            name: "Living Room Cache",
            endpoint: CacheServerEndpoint(host: "living-room.local", port: 50_051)
        )
        let model = CacheLibraryViewModel(defaults: defaults)

        model.useDiscoveredServer(server)
        model.clearFailedDiscoveredServer(server)

        XCTAssertEqual(model.serverAddressText, "")
        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(model.statusMessage, "Cache server not connected.")
    }

    @MainActor
    private func waitUntil(predicate: @escaping @MainActor () -> Bool) async {
        for _ in 0..<100 where !predicate() {
            await Task.yield()
        }
    }
}

private final class FakeDiscoveryClient: CacheServerDiscoveryClient, @unchecked Sendable {
    private var continuation: AsyncStream<CacheServerDiscoverySnapshot>.Continuation?

    func snapshots() -> AsyncStream<CacheServerDiscoverySnapshot> {
        AsyncStream { continuation in
            self.continuation = continuation
        }
    }

    func yield(_ snapshot: CacheServerDiscoverySnapshot) {
        continuation?.yield(snapshot)
    }
}
