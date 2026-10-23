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

    func testSilentWatchUsesUnchangedGetTaskBeforeReturningCompletedOutput() async throws {
        let (directory, metricsURL) = try makeMetricsFile()
        defer { try? FileManager.default.removeItem(at: directory) }
        let (defaults, suiteName) = try makeDefaults()
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let model = makeModel(
            source: FixtureScenario.silent.source,
            defaults: defaults,
            acceleratesWatchRecovery: false,
            rpcTimeout: .seconds(90)
        )
        let mediaURL = try XCTUnwrap(URL(string: mediaAddress))

        try await withFixtureServer(metricsURL: metricsURL) { endpoint in
            defer { model.clearTask() }
            await model.submit(serverAddressText: endpoint.displayAddress)

            let playable = await waitUntil {
                model.isWatching
                    && model.currentTask?.id == FixtureScenario.silent.taskID
                    && model.currentTask?.progress == 0.42
                    && model.taskResults.first?.id == FixtureScenario.silent.resultID
                    && model.taskResults.first?.playbackURL == mediaURL
            }
            XCTAssertTrue(playable, "The silent task did not expose its initial playable output and open watch.")
            guard playable else { return }
            XCTAssertNil(model.watchErrorMessage)
            XCTAssertFalse(model.isReconnecting)
            XCTAssertFalse(model.canRetryTaskUpdates)

            var observedFalseWatchState = false
            let unchangedReadObserved = await waitUntil(
                timeout: .seconds(45),
                pollInterval: .milliseconds(100)
            ) {
                if model.watchErrorMessage != nil || model.isReconnecting || model.canRetryTaskUpdates {
                    observedFalseWatchState = true
                    return true
                }
                return ((try? readScenario("silent", from: metricsURL).unchangedGetTaskCount) ?? 0) >= 1
            }
            XCTAssertTrue(
                unchangedReadObserved && !observedFalseWatchState,
                "The first healthy silent GetTask read did not arrive within 45 seconds."
            )
            guard unchangedReadObserved, !observedFalseWatchState else { return }
            XCTAssertEqual(model.currentTask?.message, "Playable fixture output is available.")
            XCTAssertEqual(model.currentTask?.progress, 0.42)
            XCTAssertEqual(model.currentTask?.outputSummary?.revision, 1)
            XCTAssertTrue(model.isWatching)

            let completed = await waitUntil(
                timeout: .seconds(45),
                pollInterval: .milliseconds(100)
            ) {
                if model.watchErrorMessage != nil || model.isReconnecting || model.canRetryTaskUpdates {
                    observedFalseWatchState = true
                    return true
                }
                return model.currentTask?.message == "Silent task reconciliation complete"
                    && model.currentTask?.state.lowercased().contains("succeeded") == true
                    && !model.isWatching
            }
            XCTAssertFalse(observedFalseWatchState, "A healthy silent watch exposed retry or reconnect UI state.")
            XCTAssertTrue(
                completed && !observedFalseWatchState, "The silent task did not reconcile within 45 more seconds.")
            guard completed, !observedFalseWatchState else { return }

            XCTAssertEqual(model.currentTask?.id, FixtureScenario.silent.taskID)
            XCTAssertEqual(model.currentTask?.progress, 1)
            XCTAssertEqual(model.currentTask?.outputSummary?.revision, 2)
            XCTAssertNil(model.watchErrorMessage)
            XCTAssertFalse(model.isReconnecting)
            XCTAssertFalse(model.canRetryTaskUpdates)

            let outputReady = await waitUntil {
                model.taskResults.first?.id == FixtureScenario.silent.resultID
                    && model.taskResults.first?.playbackURL == mediaURL
                    && model.taskResults.first?.isCached == true
            }
            XCTAssertTrue(outputReady, "GetTask completion did not expose the updated playable output.")
            guard outputReady, let result = model.taskResults.first else { return }
            assertPlayableResult(result, in: model, expectedID: FixtureScenario.silent.resultID, url: mediaURL)

            let metrics = try readScenario("silent", from: metricsURL)
            XCTAssertEqual(metrics.taskID, FixtureScenario.silent.taskID)
            XCTAssertEqual(metrics.createCount, 1)
            XCTAssertEqual(metrics.watchCount, 1)
            XCTAssertEqual(metrics.watchStreamOpenCount, 1)
            XCTAssertGreaterThanOrEqual(metrics.getTaskCount, 2)
            XCTAssertGreaterThanOrEqual(metrics.unchangedGetTaskCount, 1)
            XCTAssertGreaterThanOrEqual(metrics.completedGetTaskCount, 1)
            XCTAssertGreaterThanOrEqual(metrics.outputCount, 2)
            XCTAssertEqual(metrics.cancelCount, 0)
            let watchIndex = try XCTUnwrap(metrics.rpcTrace.firstIndex(of: "watch"))
            let postWatchGetTaskIndex = metrics.rpcTrace.indices.first {
                $0 > watchIndex && metrics.rpcTrace[$0] == "get_task"
            }
            XCTAssertNotNil(postWatchGetTaskIndex, "The original watch must open before unary reconciliation.")
        }
    }

    func testSilentWatchRejectsOverlappingRPCsAndReleasesCancelledSubscriptions() async throws {
        let (directory, metricsURL) = try makeMetricsFile()
        defer { try? FileManager.default.removeItem(at: directory) }

        try await withFixtureServer(metricsURL: metricsURL) { endpoint in
            let setupClient = GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(3))
            _ = try await setupClient.startBilibiliResolution(urlOrID: FixtureScenario.silent.source)
            let task = try await setupClient.createBilibiliPlaybackTaskV2(
                sessionID: FixtureScenario.silent.resolutionSessionID,
                selection: .single(candidateToken: FixtureScenario.silent.candidateToken)
            )
            XCTAssertEqual(task.id, FixtureScenario.silent.taskID)

            try await withGRPCClient(
                transport: .http2NIOPosix(
                    target: .ipv4(address: endpoint.host, port: endpoint.port),
                    transportSecurity: .plaintext
                )
            ) { client in
                let service = TvosNetPlayer_V1_TaskService.Client(wrapping: client)
                var request = TvosNetPlayer_V1_WatchTasksRequest()
                request.ids = [FixtureScenario.silent.taskID]
                let watchRequest = request
                var options = CallOptions.defaults
                options.timeout = .seconds(15)
                let watchOptions = options
                options.timeout = .seconds(3)
                let duplicateOptions = options

                for phase in 0..<2 {
                    let initialSnapshot = self.expectation(
                        description: "silent watch \(phase) sends its initial snapshot"
                    )
                    let finished = self.expectation(description: "silent watch \(phase) is cancelled")
                    var watchDidFinish = false
                    let watch = Task { @MainActor in
                        defer {
                            watchDidFinish = true
                            finished.fulfill()
                        }
                        let clock = ContinuousClock()
                        let reopenDeadline = clock.now.advanced(by: .seconds(3))
                        while !Task.isCancelled {
                            do {
                                try await service.watchTasks(watchRequest, options: watchOptions) { response in
                                    var eventCount = 0
                                    for try await event in response.messages {
                                        eventCount += 1
                                        XCTAssertEqual(eventCount, 1, "The silent watch emitted a subsequent event.")
                                        XCTAssertTrue(event.hasTask)
                                        XCTAssertEqual(event.task.id, FixtureScenario.silent.taskID)
                                        XCTAssertEqual(event.task.state, .playable)
                                        XCTAssertEqual(event.task.progress, 0.42)
                                        XCTAssertTrue(event.task.hasOutputSummary)
                                        XCTAssertEqual(event.task.outputSummary.revision, 1)
                                        XCTAssertEqual(
                                            event.task.outputSummary.primaryResultID,
                                            FixtureScenario.silent.resultID
                                        )
                                        if eventCount == 1 {
                                            initialSnapshot.fulfill()
                                        }
                                    }
                                }
                                if !Task.isCancelled {
                                    XCTFail("The silent watch ended before client cancellation.")
                                }
                                return
                            } catch let error as RPCError
                                where error.code == .alreadyExists
                                && phase > 0 && clock.now < reopenDeadline
                            {
                                // Wait for the preceding RPC's server-side cancellation to arrive.
                                do {
                                    try await Task.sleep(for: .milliseconds(10))
                                } catch {
                                    return
                                }
                            } catch {
                                if !Task.isCancelled {
                                    XCTFail("The silent watch failed unexpectedly: \(error)")
                                }
                                return
                            }
                        }
                    }
                    defer { watch.cancel() }

                    let opened = await self.waitUntil(timeout: .seconds(3)) {
                        ((try? self.readScenario("silent", from: metricsURL).watchStreamOpenCount) ?? 0)
                            == UInt64(phase + 1)
                    }
                    XCTAssertTrue(opened, "Cancellation did not release the previous silent watch's ownership.")
                    guard opened else { return }
                    let snapshotReceived = await XCTWaiter.fulfillment(of: [initialSnapshot], timeout: 3)
                    XCTAssertEqual(
                        snapshotReceived,
                        .completed,
                        "The accepted silent watch did not send its initial snapshot."
                    )
                    guard snapshotReceived == .completed else { return }
                    XCTAssertFalse(watchDidFinish, "The initial snapshot ended the accepted silent watch.")

                    do {
                        try await service.watchTasks(watchRequest, options: duplicateOptions) { response in
                            for try await _ in response.messages {
                                XCTFail("An overlapping watch unexpectedly emitted an event.")
                            }
                        }
                        XCTFail("An overlapping silent watch was accepted.")
                    } catch let error as RPCError {
                        XCTAssertEqual(error.code, .alreadyExists)
                    }
                    XCTAssertFalse(watchDidFinish, "Rejecting a duplicate ended the accepted silent watch.")
                    let overlappingMetrics = try self.readScenario("silent", from: metricsURL)
                    XCTAssertEqual(overlappingMetrics.watchStreamOpenCount, UInt64(phase + 1))

                    watch.cancel()
                    let cancellation = await XCTWaiter.fulfillment(of: [finished], timeout: 3)
                    XCTAssertEqual(cancellation, .completed, "The cancelled watch did not release its client task.")
                    guard cancellation == .completed else { return }
                    await watch.value
                }
            }

            let metrics = try self.readScenario("silent", from: metricsURL)
            XCTAssertEqual(metrics.createCount, 1)
            XCTAssertEqual(metrics.watchStreamOpenCount, 2)
            XCTAssertGreaterThanOrEqual(metrics.watchCount, 4)
            XCTAssertEqual(metrics.getTaskCount, 0)
            XCTAssertEqual(metrics.cancelCount, 0)
        }
    }

    private func makeModel(
        source: String,
        defaults: UserDefaults,
        acceleratesWatchRecovery: Bool = true,
        rpcTimeout: Duration = .seconds(3)
    ) -> BilibiliTaskViewModel {
        if acceleratesWatchRecovery {
            return BilibiliTaskViewModel(
                sourceText: source,
                defaults: defaults,
                operationTimeout: .seconds(3),
                resolutionTimeout: .seconds(3),
                watchRecoverySleep: { _ in try await Task.sleep(for: .milliseconds(1)) },
                clientFactory: { endpoint in
                    GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: rpcTimeout)
                }
            )
        }
        return BilibiliTaskViewModel(
            sourceText: source,
            defaults: defaults,
            operationTimeout: .seconds(3),
            resolutionTimeout: .seconds(3),
            clientFactory: { endpoint in
                GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: rpcTimeout)
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
        pollInterval: Duration = .milliseconds(10),
        condition: @MainActor () -> Bool
    ) async -> Bool {
        let clock = ContinuousClock()
        let deadline = clock.now.advanced(by: timeout)
        while clock.now < deadline {
            if condition() { return true }
            try? await Task.sleep(for: pollInterval)
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
    let watchStreamOpenCount: UInt64
    let getTaskCount: UInt64
    let unchangedGetTaskCount: UInt64
    let completedGetTaskCount: UInt64
    let outputCount: UInt64
    let cancelCount: UInt64
    let manualResumeReadBeforeWatch: Bool
    let rpcTrace: [String]

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

private enum FixtureTestError: Error {
    case invalidListeningAddress
}
