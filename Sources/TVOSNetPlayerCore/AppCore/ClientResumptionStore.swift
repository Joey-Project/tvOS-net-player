import Combine
import Foundation
import TVOSNetPlayerCacheClient

public struct ClientResumptionTaskIdentity: Equatable, Sendable {
    public let endpoint: CacheServerEndpoint
    public let serverID: String
    public let taskID: String

    public init(endpoint: CacheServerEndpoint, serverID: String, taskID: String) {
        self.endpoint = Self.canonicalEndpoint(endpoint) ?? endpoint
        self.serverID = serverID
        self.taskID = taskID
    }

    public func matches(endpoint: CacheServerEndpoint) -> Bool {
        Self.canonicalEndpoint(endpoint) == self.endpoint
    }

    var isValidForPersistence: Bool {
        Self.canonicalEndpoint(endpoint) == endpoint
            && Self.isValidIdentifier(serverID)
            && Self.isValidIdentifier(taskID)
    }

    static func canonicalEndpoint(_ endpoint: CacheServerEndpoint) -> CacheServerEndpoint? {
        guard let parsed = CacheServerEndpoint.normalized(from: endpoint.displayAddress) else {
            return nil
        }

        return CacheServerEndpoint(host: parsed.host.lowercased(), port: parsed.port, scheme: parsed.scheme)
    }

    static func isValidIdentifier(_ value: String) -> Bool {
        !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && value.utf8.count <= ClientResumptionStore.maximumIdentifierBytes
            && !value.unicodeScalars.contains { CharacterSet.controlCharacters.contains($0) }
    }
}

public struct ClientPlaybackIdentity: Equatable, Sendable {
    public let task: ClientResumptionTaskIdentity
    public let resultID: String?
    public let libraryItemID: String
    public let variantID: String

    public init(
        task: ClientResumptionTaskIdentity,
        resultID: String? = nil,
        libraryItemID: String = "",
        variantID: String = ""
    ) {
        self.task = task
        self.resultID = resultID
        self.libraryItemID = libraryItemID
        self.variantID = variantID
    }

    var isValidForPersistence: Bool {
        task.isValidForPersistence
            && (resultID.map(ClientResumptionTaskIdentity.isValidIdentifier) ?? true)
            && Self.isValidOptionalIdentifier(libraryItemID)
            && Self.isValidOptionalIdentifier(variantID)
    }

    private static func isValidOptionalIdentifier(_ value: String) -> Bool {
        value.isEmpty || ClientResumptionTaskIdentity.isValidIdentifier(value)
    }
}

public struct ClientPlaybackCheckpoint: Equatable, Sendable {
    public let identity: ClientPlaybackIdentity
    public let positionSeconds: Double
    public let durationSeconds: Double?
    public let completed: Bool

    public init(
        identity: ClientPlaybackIdentity,
        positionSeconds: Double,
        durationSeconds: Double? = nil,
        completed: Bool = false
    ) {
        self.identity = identity
        self.positionSeconds = positionSeconds
        self.durationSeconds = durationSeconds
        self.completed = completed
    }

    var isValidForPersistence: Bool {
        identity.isValidForPersistence
            && positionSeconds.isFinite
            && positionSeconds >= 0
            && positionSeconds <= ClientResumptionStore.maximumPlaybackSeconds
            && (durationSeconds.map {
                $0.isFinite && $0 >= 0 && $0 <= ClientResumptionStore.maximumPlaybackSeconds
            } ?? true)
    }
}

public struct ClientResumptionRecord: Equatable, Sendable {
    public let task: ClientResumptionTaskIdentity
    public let playback: ClientPlaybackCheckpoint?

    public init(task: ClientResumptionTaskIdentity, playback: ClientPlaybackCheckpoint? = nil) {
        self.task = task
        self.playback = playback
    }

    var isValidForPersistence: Bool {
        task.isValidForPersistence
            && (playback.map { $0.isValidForPersistence && $0.identity.task == task } ?? true)
    }
}

@MainActor
public final class ClientResumptionStore: ObservableObject {
    public static let defaultsKey = "ClientResumptionRecord"
    static let maximumPayloadBytes = 16 * 1024
    nonisolated static let maximumIdentifierBytes = 1_024
    nonisolated static let maximumPlaybackSeconds = 100 * 365 * 24 * 60 * 60.0

    @Published public private(set) var record: ClientResumptionRecord?

    private let defaults: UserDefaults

    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        record = Self.readRecord(from: defaults)
        if record == nil, defaults.object(forKey: Self.defaultsKey) != nil {
            defaults.removeObject(forKey: Self.defaultsKey)
        }
    }

    public func rememberTask(_ identity: ClientResumptionTaskIdentity) {
        guard identity.isValidForPersistence else { return }

        let playback = record?.task == identity ? record?.playback : nil
        replaceRecord(ClientResumptionRecord(task: identity, playback: playback))
    }

    public func beginPlayback(
        _ identity: ClientPlaybackIdentity,
        checkpoint: ClientPlaybackCheckpoint? = nil
    ) {
        guard identity.isValidForPersistence, record?.task == identity.task else { return }

        let initialCheckpoint: ClientPlaybackCheckpoint
        if let checkpoint {
            guard checkpoint.isValidForPersistence, checkpoint.identity == identity else { return }
            initialCheckpoint = checkpoint
        } else {
            initialCheckpoint = ClientPlaybackCheckpoint(identity: identity, positionSeconds: 0)
        }

        replaceRecord(ClientResumptionRecord(task: identity.task, playback: initialCheckpoint))
    }

    public func updatePlayback(
        identity: ClientPlaybackIdentity,
        positionSeconds: Double,
        durationSeconds: Double?,
        completed: Bool
    ) {
        guard identity.isValidForPersistence,
            let record,
            record.task == identity.task,
            record.playback?.identity == identity
        else {
            return
        }

        let checkpoint = ClientPlaybackCheckpoint(
            identity: identity,
            positionSeconds: positionSeconds,
            durationSeconds: durationSeconds,
            completed: completed
        )
        guard checkpoint.isValidForPersistence else { return }
        replaceRecord(ClientResumptionRecord(task: identity.task, playback: checkpoint))
    }

    public func forget() {
        defaults.removeObject(forKey: Self.defaultsKey)
        record = nil
    }

    private func replaceRecord(_ nextRecord: ClientResumptionRecord) {
        guard nextRecord.isValidForPersistence,
            let data = Self.encodedData(for: nextRecord), data.count <= Self.maximumPayloadBytes
        else {
            return
        }

        defaults.set(data, forKey: Self.defaultsKey)
        record = nextRecord
    }

    private static func readRecord(from defaults: UserDefaults) -> ClientResumptionRecord? {
        guard let data = defaults.data(forKey: defaultsKey), data.count <= maximumPayloadBytes,
            hasOnlyWhitelistedKeys(in: data),
            let payload = try? JSONDecoder().decode(PersistedPayload.self, from: data),
            payload.version == 1,
            let record = payload.clientRecord,
            record.isValidForPersistence
        else {
            return nil
        }

        return record
    }

    private static func encodedData(for record: ClientResumptionRecord) -> Data? {
        let task = PersistedTask(
            endpoint: PersistedEndpoint(
                scheme: record.task.endpoint.scheme.rawValue,
                host: record.task.endpoint.host,
                port: record.task.endpoint.port
            ),
            serverID: record.task.serverID,
            taskID: record.task.taskID
        )
        let playback = record.playback.map {
            PersistedPlayback(
                resultID: $0.identity.resultID,
                libraryItemID: $0.identity.libraryItemID,
                variantID: $0.identity.variantID,
                positionSeconds: $0.positionSeconds,
                durationSeconds: $0.durationSeconds,
                completed: $0.completed
            )
        }
        return try? JSONEncoder().encode(PersistedPayload(version: 1, task: task, playback: playback))
    }

    private static func hasOnlyWhitelistedKeys(in data: Data) -> Bool {
        guard let root = try? JSONSerialization.jsonObject(with: data),
            let payload = dictionary(root, allows: ["version", "task", "playback"], requires: ["version", "task"]),
            let taskValue = payload["task"],
            let task = dictionary(
                taskValue,
                allows: ["endpoint", "serverID", "taskID"],
                requires: ["endpoint", "serverID", "taskID"]
            ),
            let endpointValue = task["endpoint"],
            dictionary(
                endpointValue,
                allows: ["scheme", "host", "port"],
                requires: ["scheme", "host", "port"]
            ) != nil
        else {
            return false
        }

        guard let playbackValue = payload["playback"] else { return true }
        return dictionary(
            playbackValue,
            allows: ["resultID", "libraryItemID", "variantID", "positionSeconds", "durationSeconds", "completed"],
            requires: ["libraryItemID", "variantID", "positionSeconds", "completed"]
        ) != nil
    }

    private static func dictionary(
        _ value: Any,
        allows allowedKeys: Set<String>,
        requires requiredKeys: Set<String>
    ) -> [String: Any]? {
        guard let dictionary = value as? [String: Any],
            requiredKeys.isSubset(of: Set(dictionary.keys)),
            Set(dictionary.keys).isSubset(of: allowedKeys)
        else {
            return nil
        }
        return dictionary
    }

    private struct PersistedPayload: Codable {
        let version: Int
        let task: PersistedTask
        let playback: PersistedPlayback?

        var clientRecord: ClientResumptionRecord? {
            guard let scheme = CacheServerEndpointScheme(rawValue: task.endpoint.scheme),
                let endpoint = CacheServerEndpoint(
                    host: task.endpoint.host,
                    port: task.endpoint.port,
                    scheme: scheme
                ),
                ClientResumptionTaskIdentity.canonicalEndpoint(endpoint) == endpoint
            else {
                return nil
            }

            let identity = ClientResumptionTaskIdentity(
                endpoint: endpoint,
                serverID: task.serverID,
                taskID: task.taskID
            )
            let playbackCheckpoint = playback.map {
                ClientPlaybackCheckpoint(
                    identity: ClientPlaybackIdentity(
                        task: identity,
                        resultID: $0.resultID,
                        libraryItemID: $0.libraryItemID,
                        variantID: $0.variantID
                    ),
                    positionSeconds: $0.positionSeconds,
                    durationSeconds: $0.durationSeconds,
                    completed: $0.completed
                )
            }
            return ClientResumptionRecord(task: identity, playback: playbackCheckpoint)
        }
    }

    private struct PersistedTask: Codable {
        let endpoint: PersistedEndpoint
        let serverID: String
        let taskID: String
    }

    private struct PersistedEndpoint: Codable {
        let scheme: String
        let host: String
        let port: Int
    }

    private struct PersistedPlayback: Codable {
        let resultID: String?
        let libraryItemID: String
        let variantID: String
        let positionSeconds: Double
        let durationSeconds: Double?
        let completed: Bool
    }
}
