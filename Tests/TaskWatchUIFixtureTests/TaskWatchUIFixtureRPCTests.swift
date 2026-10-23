import Foundation
import GRPCCore
import GRPCNIOTransportHTTP2Posix
import TaskWatchUIFixture
import TVOSNetPlayerCacheClient
import TVOSNetPlayerCore
import XCTest

@MainActor
final class TaskWatchUIFixtureRPCTests: XCTestCase {
    private let mediaAddress = "http://127.0.0.1:8080/media/local.default.Rml4dHVyZSAwNTUubXA0/original"

    func testAutomaticRecoveryUsesProductionViewModelAndV2Mappers() async throws {
        let (directory, metricsURL) = try makeMetricsFile()
        defer { try? FileManager.default.removeItem(at: directory) }
        let (defaults, suiteName) = try makeDefaults()
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let model = makeModel(source: FixtureScenario.automatic.source, defaults: defaults)
        let mediaURL = try XCTUnwrap(URL(string: mediaAddress))

        try await withFixtureServer(metricsURL: metricsURL) { endpoint in
            defer { model.clearTask() }
            await model.submit(serverAddressText: endpoint.displayAddress)

            let completed = await waitUntil {
                model.currentTask?.message == "Automatic recovery complete"
                    && model.currentTask?.state.lowercased().contains("succeeded") == true
                    && !model.isWatching
            }
            XCTAssertTrue(completed, "The bounded automatic recovery sequence did not finish.")
            guard completed else { return }

            XCTAssertEqual(model.currentTask?.id, FixtureScenario.automatic.taskID)
            XCTAssertEqual(model.currentTask?.progress, 1)
            XCTAssertEqual(model.currentTask?.outputSummary?.revision, 2)
            XCTAssertEqual(model.resolutionSession?.defaultCandidateToken, FixtureScenario.automatic.candidateToken)
            XCTAssertEqual(model.selectedCandidateID, FixtureScenario.automatic.candidateToken)
            XCTAssertEqual(model.currentTask?.playbackSource?.uri, mediaAddress)

            let outputReady = await waitUntil {
                model.taskResults.first?.id == FixtureScenario.automatic.resultID
                    && model.taskResults.first?.playbackURL == mediaURL
                    && model.taskResults.first?.isCached == true
            }
            XCTAssertTrue(outputReady, "The production result mapper did not expose the playable MP4 URL.")
            guard outputReady, let result = model.taskResults.first else { return }
            assertPlayableResult(result, in: model, expectedID: FixtureScenario.automatic.resultID, url: mediaURL)

            let metrics = try readScenario("automatic", from: metricsURL)
            XCTAssertEqual(metrics.taskID, FixtureScenario.automatic.taskID)
            XCTAssertEqual(metrics.createCount, 1)
            XCTAssertEqual(metrics.watchCount, 4)
            XCTAssertEqual(metrics.getTaskCount, 3)
            XCTAssertGreaterThanOrEqual(metrics.outputCount, 1)
            XCTAssertEqual(metrics.cancelCount, 0)
        }
    }

    func testManualRetryReconcilesWithGetTaskBeforeWatchAndKeepsPlaybackAvailable() async throws {
        let (directory, metricsURL) = try makeMetricsFile()
        defer { try? FileManager.default.removeItem(at: directory) }
        let (defaults, suiteName) = try makeDefaults()
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let model = makeModel(source: FixtureScenario.manual.source, defaults: defaults)
        let mediaURL = try XCTUnwrap(URL(string: mediaAddress))

        try await withFixtureServer(metricsURL: metricsURL) { endpoint in
            defer { model.clearTask() }
            await model.submit(serverAddressText: endpoint.displayAddress)

            let stopped = await waitUntil {
                model.watchErrorMessage != nil
                    && model.canRetryTaskUpdates
                    && !model.isReconnecting
                    && !model.isWatching
            }
            XCTAssertTrue(stopped, "Automatic recovery did not stop with Retry available.")
            guard stopped else { return }

            let outputReady = await waitUntil {
                model.currentTask?.id == FixtureScenario.manual.taskID
                    && model.taskResults.first?.id == FixtureScenario.manual.resultID
                    && model.taskResults.first?.playbackURL == mediaURL
            }
            XCTAssertTrue(outputReady, "The stalled task lost its available playback output.")
            guard outputReady, let initialResult = model.taskResults.first else { return }
            assertPlayableResult(initialResult, in: model, expectedID: FixtureScenario.manual.resultID, url: mediaURL)

            let stableTaskID = try XCTUnwrap(model.currentTask?.id)
            let stableResultID = initialResult.id
            XCTAssertEqual(stableTaskID, FixtureScenario.manual.taskID)
            XCTAssertEqual(model.currentTask?.progress, 0.42)
            XCTAssertEqual(model.currentTask?.outputSummary?.revision, 1)

            let stoppedMetrics = try readScenario("manual", from: metricsURL)
            XCTAssertEqual(stoppedMetrics.createCount, 1)
            XCTAssertEqual(stoppedMetrics.watchCount, 7)
            XCTAssertEqual(stoppedMetrics.getTaskCount, 6)
            XCTAssertGreaterThanOrEqual(stoppedMetrics.outputCount, 1)
            XCTAssertEqual(stoppedMetrics.cancelCount, 0)
            XCTAssertFalse(stoppedMetrics.manualResumeReadBeforeWatch)

            model.retryTaskUpdates()
            let completed = await waitUntil {
                model.currentTask?.message == "Manual recovery complete"
                    && model.currentTask?.state.lowercased().contains("succeeded") == true
                    && !model.isWatching
            }
            XCTAssertTrue(completed, "Explicit ViewModel retry did not complete task observation.")
            guard completed else { return }

            XCTAssertEqual(model.currentTask?.id, stableTaskID)
            XCTAssertEqual(model.currentTask?.progress, 1)
            XCTAssertEqual(model.currentTask?.outputSummary?.revision, 2)
            let finalOutputReady = await waitUntil {
                model.taskResults.first?.id == stableResultID
                    && model.taskResults.first?.playbackURL == mediaURL
                    && model.taskResults.first?.isCached == true
            }
            XCTAssertTrue(finalOutputReady, "The completed result changed identity or lost its playback URL.")
            guard finalOutputReady, let finalResult = model.taskResults.first else { return }
            assertPlayableResult(finalResult, in: model, expectedID: stableResultID, url: mediaURL)

            let resumedMetrics = try readScenario("manual", from: metricsURL)
            XCTAssertEqual(resumedMetrics.taskID, stableTaskID)
            XCTAssertEqual(resumedMetrics.createCount, 1)
            XCTAssertEqual(resumedMetrics.watchCount, 8)
            XCTAssertEqual(resumedMetrics.getTaskCount, 7)
            XCTAssertGreaterThanOrEqual(resumedMetrics.outputCount, 2)
            XCTAssertEqual(resumedMetrics.cancelCount, 0)
            XCTAssertTrue(resumedMetrics.manualResumeReadBeforeWatch)
            let finalWatchIndex = try XCTUnwrap(resumedMetrics.rpcTrace.lastIndex(of: "watch"))
            XCTAssertGreaterThan(finalWatchIndex, 0)
            if finalWatchIndex > 0 {
                XCTAssertEqual(resumedMetrics.rpcTrace[finalWatchIndex - 1], "get_task")
            }
        }
    }

    private func makeModel(source: String, defaults: UserDefaults) -> BilibiliTaskViewModel {
        BilibiliTaskViewModel(
            sourceText: source,
            defaults: defaults,
            operationTimeout: .seconds(3),
            resolutionTimeout: .seconds(3),
            watchRecoverySleep: { _ in try await Task.sleep(for: .milliseconds(1)) },
            clientFactory: { endpoint in
                GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(3))
            }
        )
    }

    private func withFixtureServer(
        metricsURL: URL,
        operation: (CacheServerEndpoint) async throws -> Void
    ) async throws {
        let mediaURL = try FixtureConfiguration.validatedMediaURL(mediaAddress)
        let metrics = FixtureMetricsStore(fileURL: metricsURL)
        try await metrics.initialize()
        let service = TaskWatchUIFixtureService(metrics: metrics, mediaURL: mediaURL)

        try await withGRPCServer(
            transport: .http2NIOPosix(
                address: .ipv4(host: "127.0.0.1", port: 0),
                transportSecurity: .plaintext
            ),
            services: [service]
        ) { server in
            let address = try await server.transport.listeningAddress
            guard let port = address.ipv4?.port else {
                throw FixtureTestError.invalidListeningAddress
            }
            try await operation(CacheServerEndpoint(host: "127.0.0.1", port: port))
        }
    }

    private func waitUntil(
        timeout: Duration = .seconds(10),
        condition: @MainActor () -> Bool
    ) async -> Bool {
        let clock = ContinuousClock()
        let deadline = clock.now.advanced(by: timeout)
        while clock.now < deadline {
            if condition() { return true }
            try? await Task.sleep(for: .milliseconds(10))
        }
        return condition()
    }

    private func assertPlayableResult(
        _ result: BilibiliTaskResultPresentation,
        in model: BilibiliTaskViewModel,
        expectedID: String,
        url: URL,
        file: StaticString = #filePath,
        line: UInt = #line
    ) {
        XCTAssertEqual(result.id, expectedID, file: file, line: line)
        XCTAssertTrue(result.isReady, file: file, line: line)
        XCTAssertEqual(result.playbackURL, url, file: file, line: line)
        XCTAssertTrue(model.canPlay(result: result), file: file, line: line)
        XCTAssertEqual(model.playableURL(for: result), url, file: file, line: line)
    }

    private func makeMetricsFile() throws -> (directory: URL, file: URL) {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("task-watch-ui-e2e-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        return (directory, directory.appendingPathComponent("metrics.json"))
    }

    private func makeDefaults() throws -> (defaults: UserDefaults, suiteName: String) {
        let suiteName = "TaskWatchUIFixtureTests-\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defaults.removePersistentDomain(forName: suiteName)
        return (defaults, suiteName)
    }

    private func readScenario(_ name: String, from url: URL) throws -> MetricsScenario {
        let document = try JSONDecoder().decode(MetricsDocument.self, from: Data(contentsOf: url))
        XCTAssertEqual(document.schemaVersion, 1)
        return try XCTUnwrap(document.scenarios[name])
    }
}

private struct MetricsDocument: Decodable {
    let schemaVersion: Int
    let scenarios: [String: MetricsScenario]

    private enum CodingKeys: String, CodingKey {
        case schemaVersion = "schema_version"
        case scenarios
    }
}

private struct MetricsScenario: Decodable {
    let taskID: String
    let createCount: UInt64
    let watchCount: UInt64
    let getTaskCount: UInt64
    let outputCount: UInt64
    let cancelCount: UInt64
    let manualResumeReadBeforeWatch: Bool
    let rpcTrace: [String]

    private enum CodingKeys: String, CodingKey {
        case taskID = "task_id"
        case createCount = "create_count"
        case watchCount = "watch_count"
        case getTaskCount = "get_task_count"
        case outputCount = "output_count"
        case cancelCount = "cancel_count"
        case manualResumeReadBeforeWatch = "manual_resume_read_before_watch"
        case rpcTrace = "rpc_trace"
    }
}

private enum FixtureTestError: Error {
    case invalidListeningAddress
}
