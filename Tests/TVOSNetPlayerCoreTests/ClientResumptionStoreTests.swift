import Foundation
import XCTest
import TVOSNetPlayerCacheClient
@testable import TVOSNetPlayerCore

final class ClientResumptionStoreTests: XCTestCase {
    private var defaultsSuiteName: String!
    private var defaults: UserDefaults!

    override func setUpWithError() throws {
        try super.setUpWithError()
        defaultsSuiteName = "ClientResumptionStoreTests-\(UUID().uuidString)"
        defaults = try XCTUnwrap(UserDefaults(suiteName: defaultsSuiteName))
        defaults.removePersistentDomain(forName: defaultsSuiteName)
    }

    override func tearDown() {
        defaults.removePersistentDomain(forName: defaultsSuiteName)
        defaults = nil
        defaultsSuiteName = nil
        super.tearDown()
    }

    func testTaskIdentityMatchesCanonicalDNSCaseAndDefaultPort() throws {
        let identity = ClientResumptionTaskIdentity(
            endpoint: try XCTUnwrap(CacheServerEndpoint.normalized(from: "https://cache.example")),
            serverID: "server-1",
            taskID: "task-7"
        )
        let uppercaseDNS = try XCTUnwrap(
            CacheServerEndpoint(host: "CACHE.Example", port: CacheServerEndpoint.defaultTLSPort, scheme: .https)
        )
        let explicitDefaultPort = try XCTUnwrap(
            CacheServerEndpoint.normalized(from: "https://cache.example:443")
        )

        XCTAssertTrue(identity.matches(endpoint: uppercaseDNS))
        XCTAssertTrue(identity.matches(endpoint: explicitDefaultPort))
    }

    func testTaskIdentityDoesNotMatchDifferentSchemeOrPort() throws {
        let identity = ClientResumptionTaskIdentity(
            endpoint: try XCTUnwrap(CacheServerEndpoint.normalized(from: "https://cache.example")),
            serverID: "server-1",
            taskID: "task-7"
        )
        let differentScheme = try XCTUnwrap(
            CacheServerEndpoint(host: "cache.example", port: CacheServerEndpoint.defaultTLSPort, scheme: .http)
        )
        let differentPort = try XCTUnwrap(
            CacheServerEndpoint(host: "cache.example", port: 444, scheme: .https)
        )

        XCTAssertFalse(identity.matches(endpoint: differentScheme))
        XCTAssertFalse(identity.matches(endpoint: differentPort))
    }

    @MainActor
    func testCanonicalIdentityAndCheckpointSurviveStoreRecreation() throws {
        let task = ClientResumptionTaskIdentity(
            endpoint: CacheServerEndpoint(host: "CACHE.Example", port: 50_051),
            serverID: "server-1",
            taskID: "task-7"
        )
        let identity = ClientPlaybackIdentity(
            task: task,
            resultID: "result-2",
            libraryItemID: "library-item-4",
            variantID: "h264"
        )
        let store = ClientResumptionStore(defaults: defaults)

        store.rememberTask(task)
        store.beginPlayback(identity)
        store.updatePlayback(
            identity: identity,
            positionSeconds: 48.25,
            durationSeconds: 120,
            completed: false
        )

        let restoredStore = ClientResumptionStore(defaults: defaults)
        XCTAssertEqual(task.endpoint.host, "cache.example")
        XCTAssertEqual(restoredStore.record?.task, task)
        XCTAssertEqual(
            restoredStore.record?.playback,
            ClientPlaybackCheckpoint(
                identity: identity,
                positionSeconds: 48.25,
                durationSeconds: 120,
                completed: false
            )
        )

        let payload = try XCTUnwrap(defaults.data(forKey: ClientResumptionStore.defaultsKey))
        XCTAssertLessThanOrEqual(payload.count, ClientResumptionStore.maximumPayloadBytes)
        let payloadText = try XCTUnwrap(String(data: payload, encoding: .utf8))
        XCTAssertFalse(payloadText.contains("playbackURI"))
        XCTAssertFalse(payloadText.contains("playbackURL"))
    }

    @MainActor
    func testRememberTaskPreservesOnlyMatchingTaskCheckpoint() {
        let endpoint = CacheServerEndpoint(host: "cache.example")
        let task = ClientResumptionTaskIdentity(endpoint: endpoint, serverID: "server-1", taskID: "task-7")
        let identity = ClientPlaybackIdentity(task: task, libraryItemID: "item-3", variantID: "h264")
        let store = ClientResumptionStore(defaults: defaults)
        store.rememberTask(task)
        store.beginPlayback(identity)
        store.updatePlayback(identity: identity, positionSeconds: 32, durationSeconds: 90, completed: false)

        store.rememberTask(task)
        XCTAssertEqual(store.record?.playback?.positionSeconds, 32)

        store.rememberTask(
            ClientResumptionTaskIdentity(endpoint: endpoint, serverID: "server-1", taskID: "task-8")
        )
        XCTAssertNil(store.record?.playback)
    }

    @MainActor
    func testMismatchedPlaybackAndInvalidUpdatesCannotReplaceCheckpoint() {
        let task = ClientResumptionTaskIdentity(
            endpoint: CacheServerEndpoint(host: "cache.example"),
            serverID: "server-1",
            taskID: "task-7"
        )
        let identity = ClientPlaybackIdentity(task: task, libraryItemID: "item-3", variantID: "h264")
        let differentIdentity = ClientPlaybackIdentity(task: task, libraryItemID: "item-4", variantID: "h264")
        let store = ClientResumptionStore(defaults: defaults)
        store.rememberTask(task)
        store.beginPlayback(identity)
        store.updatePlayback(identity: identity, positionSeconds: 32, durationSeconds: 90, completed: false)
        let original = store.record

        store.beginPlayback(
            differentIdentity,
            checkpoint: ClientPlaybackCheckpoint(
                identity: identity,
                positionSeconds: 80,
                durationSeconds: 90
            ))
        store.updatePlayback(identity: differentIdentity, positionSeconds: 80, durationSeconds: 90, completed: false)
        store.updatePlayback(identity: identity, positionSeconds: .infinity, durationSeconds: 90, completed: false)
        store.updatePlayback(identity: identity, positionSeconds: 80, durationSeconds: .nan, completed: false)
        store.updatePlayback(identity: identity, positionSeconds: -1, durationSeconds: 90, completed: false)

        XCTAssertEqual(store.record, original)
    }

    @MainActor
    func testInvalidTaskIdentityAndOversizedIdentifiersAreRejected() {
        let store = ClientResumptionStore(defaults: defaults)
        store.rememberTask(
            ClientResumptionTaskIdentity(
                endpoint: CacheServerEndpoint(host: "", port: 50_051),
                serverID: "server-1",
                taskID: "task-1"
            )
        )
        XCTAssertNil(store.record)

        store.rememberTask(
            ClientResumptionTaskIdentity(
                endpoint: CacheServerEndpoint(host: "cache.example"),
                serverID: " \n",
                taskID: "task-1"
            )
        )
        XCTAssertNil(store.record)

        store.rememberTask(
            ClientResumptionTaskIdentity(
                endpoint: CacheServerEndpoint(host: "cache.example"),
                serverID: "server-1",
                taskID: String(repeating: "x", count: ClientResumptionStore.maximumIdentifierBytes + 1)
            )
        )
        XCTAssertNil(store.record)
    }

    @MainActor
    func testUnsupportedVersionOversizedAndNonWhitelistedPayloadsAreDiscarded() {
        let unsupportedVersion = Data(
            #"{"version":2,"task":{"endpoint":{"scheme":"http","host":"cache.example","port":50051},"serverID":"server-1","taskID":"task-1"}}"#
                .utf8
        )
        defaults.set(unsupportedVersion, forKey: ClientResumptionStore.defaultsKey)
        XCTAssertNil(ClientResumptionStore(defaults: defaults).record)
        XCTAssertNil(defaults.object(forKey: ClientResumptionStore.defaultsKey))

        defaults.set(
            Data(repeating: 0x20, count: ClientResumptionStore.maximumPayloadBytes + 1),
            forKey: ClientResumptionStore.defaultsKey
        )
        XCTAssertNil(ClientResumptionStore(defaults: defaults).record)
        XCTAssertNil(defaults.object(forKey: ClientResumptionStore.defaultsKey))

        let extraField = Data(
            #"{"version":1,"task":{"endpoint":{"scheme":"http","host":"cache.example","port":50051},"serverID":"server-1","taskID":"task-1"},"unexpectedField":"extra"}"#
                .utf8
        )
        defaults.set(extraField, forKey: ClientResumptionStore.defaultsKey)
        XCTAssertNil(ClientResumptionStore(defaults: defaults).record)
        XCTAssertNil(defaults.object(forKey: ClientResumptionStore.defaultsKey))
    }

    @MainActor
    func testForgetRemovesPersistedRecord() {
        let store = ClientResumptionStore(defaults: defaults)
        store.rememberTask(
            ClientResumptionTaskIdentity(
                endpoint: CacheServerEndpoint(host: "cache.example"),
                serverID: "server-1",
                taskID: "task-1"
            )
        )

        store.forget()

        XCTAssertNil(store.record)
        XCTAssertNil(defaults.object(forKey: ClientResumptionStore.defaultsKey))
    }
}
