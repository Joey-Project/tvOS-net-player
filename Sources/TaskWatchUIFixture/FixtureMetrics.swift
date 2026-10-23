import Foundation

package struct FixtureMetricsDocument: Codable, Sendable {
    package var schemaVersion = 1
    package var scenarios = Dictionary(
        uniqueKeysWithValues: FixtureScenario.allCases.map { ($0.rawValue, FixtureScenarioMetrics(taskID: $0.taskID)) }
    )
    package var serverInfoCount: UInt64 = 0
    package var libraryListCount: UInt64 = 0
    package var rejectedRequestCount: UInt64 = 0

    private enum CodingKeys: String, CodingKey {
        case schemaVersion = "schema_version"
        case scenarios
        case serverInfoCount = "server_info_count"
        case libraryListCount = "library_list_count"
        case rejectedRequestCount = "rejected_request_count"
    }
}

package struct FixtureScenarioMetrics: Codable, Sendable {
    package var taskID: String
    package var createCount: UInt64 = 0
    package var watchCount: UInt64 = 0
    package var watchStreamOpenCount: UInt64 = 0
    package var getTaskCount: UInt64 = 0
    package var unchangedGetTaskCount: UInt64 = 0
    package var completedGetTaskCount: UInt64 = 0
    package var outputCount: UInt64 = 0
    package var cancelCount: UInt64 = 0
    package var manualResumeReadBeforeWatch = false
    package var rpcTrace: [String] = []

    private enum CodingKeys: String, CodingKey {
        case taskID = "task_id"
        case createCount = "create_count"
        case watchCount = "watch_count"
        case watchStreamOpenCount = "watch_stream_open_count"
        case getTaskCount = "get_task_count"
        case unchangedGetTaskCount = "unchanged_get_task_count"
        case completedGetTaskCount = "completed_get_task_count"
        case outputCount = "output_count"
        case cancelCount = "cancel_count"
        case manualResumeReadBeforeWatch = "manual_resume_read_before_watch"
        case rpcTrace = "rpc_trace"
    }
}

package enum FixtureRPC: String, Sendable {
    case startResolution = "start_resolution"
    case listCandidates = "list_candidates"
    case create
    case watch
    case getTask = "get_task"
    case output
    case cancel
}

package actor FixtureMetricsStore {
    private static let maxFileBytes = 16 * 1_024
    private static let maxTraceEntries = 64
    private static let maxCount: UInt64 = 1_000_000

    private let fileURL: URL
    private var document = FixtureMetricsDocument()
    private var manualResumeReadPending = false

    package init(fileURL: URL) {
        self.fileURL = fileURL
    }

    package func initialize() throws {
        try persist()
    }

    package func record(_ rpc: FixtureRPC, for scenario: FixtureScenario) throws -> UInt64 {
        guard var values = document.scenarios[scenario.rawValue] else {
            throw FixtureMetricsError.invalidScenario
        }

        if rpc == .watch, scenario == .manual,
            manualResumeReadPending,
            values.watchCount == 7,
            values.rpcTrace.last == FixtureRPC.getTask.rawValue
        {
            values.manualResumeReadBeforeWatch = true
            manualResumeReadPending = false
        } else if rpc != .getTask {
            manualResumeReadPending = false
        }

        switch rpc {
        case .create:
            values.createCount = increment(values.createCount)
        case .watch:
            values.watchCount = increment(values.watchCount)
        case .getTask:
            values.getTaskCount = increment(values.getTaskCount)
            if scenario == .manual, values.watchCount == 7 {
                manualResumeReadPending = true
            }
        case .output:
            values.outputCount = increment(values.outputCount)
        case .cancel:
            values.cancelCount = increment(values.cancelCount)
        case .startResolution, .listCandidates:
            break
        }

        if values.rpcTrace.count < Self.maxTraceEntries {
            values.rpcTrace.append(rpc.rawValue)
        }
        document.scenarios[scenario.rawValue] = values
        try persist()
        return count(for: rpc, in: values)
    }

    package func recordWatchStreamOpen(for scenario: FixtureScenario) throws {
        guard var values = document.scenarios[scenario.rawValue] else {
            throw FixtureMetricsError.invalidScenario
        }
        values.watchStreamOpenCount = increment(values.watchStreamOpenCount)
        document.scenarios[scenario.rawValue] = values
        try persist()
    }

    package func recordGetTaskObservation(for scenario: FixtureScenario, completed: Bool) throws {
        guard var values = document.scenarios[scenario.rawValue] else {
            throw FixtureMetricsError.invalidScenario
        }
        if completed {
            values.completedGetTaskCount = increment(values.completedGetTaskCount)
        } else {
            values.unchangedGetTaskCount = increment(values.unchangedGetTaskCount)
        }
        document.scenarios[scenario.rawValue] = values
        try persist()
    }

    package func recordServerInfo() throws {
        document.serverInfoCount = increment(document.serverInfoCount)
        try persist()
    }

    package func recordLibraryList() throws {
        document.libraryListCount = increment(document.libraryListCount)
        try persist()
    }

    package func recordRejectedRequest() throws {
        document.rejectedRequestCount = increment(document.rejectedRequestCount)
        try persist()
    }

    package func snapshotData() throws -> Data {
        try encodedDocument()
    }

    private func count(for rpc: FixtureRPC, in values: FixtureScenarioMetrics) -> UInt64 {
        switch rpc {
        case .create: values.createCount
        case .watch: values.watchCount
        case .getTask: values.getTaskCount
        case .output: values.outputCount
        case .cancel: values.cancelCount
        case .startResolution, .listCandidates: 0
        }
    }

    private func persist() throws {
        try encodedDocument().write(to: fileURL, options: .atomic)
    }

    private func encodedDocument() throws -> Data {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        let data = try encoder.encode(document)
        guard data.count <= Self.maxFileBytes else {
            throw FixtureMetricsError.fileTooLarge
        }
        return data
    }

    private func increment(_ value: UInt64) -> UInt64 {
        min(value &+ 1, Self.maxCount)
    }
}

package enum FixtureMetricsError: Error {
    case invalidScenario
    case fileTooLarge
}
