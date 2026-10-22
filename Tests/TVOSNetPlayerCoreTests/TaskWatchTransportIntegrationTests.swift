import Combine
import GRPCCore
import GRPCNIOTransportHTTP2Posix
import GRPCProtobuf
import SwiftProtobuf
import XCTest
@testable import TVOSNetPlayerCacheClient
@testable import TVOSNetPlayerCore

/// Exercises AppCore recovery through production client code and real loopback HTTP/2 dispatch.
/// This is protocol integration coverage, not native UI, media, or live-service validation.
@MainActor
final class TaskWatchTransportIntegrationTests: XCTestCase {
    private let taskID = "loopback-task-1"

    func testInterruptedWatchReconcilesThenResumesAndPublishesConsistentResults() async throws {
        let probe = TaskWatchRPCProbe(scenario: .recoverAfterUnavailable)
        let service = TaskWatchTestService(probe: probe)

        try await withLoopbackServer(service: service) { endpoint in
            let model = self.makeModel(endpoint: endpoint, source: "BV1loopback")
            let reconnecting = self.expectation(description: "watch enters recovery")
            let completed = self.expectation(description: "task and result snapshot complete")
            let history = TaskUpdateHistory()
            let historyToken = model.$currentTask.sink { task in
                if let task {
                    history.append(task)
                }
            }
            var observedRecovery: (watching: Bool, reconnecting: Bool, hasError: Bool, canRetry: Bool)?
            let reconnectToken = model.$watchErrorMessage.sink { message in
                guard message != nil, model.isWatching, model.isReconnecting,
                    observedRecovery == nil
                else {
                    return
                }
                observedRecovery = (model.isWatching, model.isReconnecting, message != nil, model.canRetryTaskUpdates)
                reconnecting.fulfill()
            }
            let completionToken = self.observeTerminalResult(
                in: model,
                resultID: "loopback-result-1",
                expectation: completed
            )

            await model.submit(serverAddressText: endpoint.displayAddress)

            guard
                await self.assertTaskSubmitted(
                    model: model,
                    probe: probe,
                    expectations: [reconnecting, completed]
                )
            else {
                model.clearTask()
                return
            }

            let reconnectWait = await XCTWaiter.fulfillment(of: [reconnecting], timeout: 20)
            XCTAssertEqual(reconnectWait, .completed)
            XCTAssertEqual(observedRecovery?.watching, true)
            XCTAssertEqual(observedRecovery?.reconnecting, true)
            XCTAssertEqual(observedRecovery?.hasError, true)
            XCTAssertEqual(observedRecovery?.canRetry, false)
            let completionWait = await XCTWaiter.fulfillment(of: [completed], timeout: 20)
            XCTAssertEqual(completionWait, .completed)

            XCTAssertEqual(model.currentTask?.id, self.taskID)
            XCTAssertTrue(model.currentTask?.state.localizedCaseInsensitiveContains("succeeded") == true)
            XCTAssertEqual(model.currentTask?.outputSummary?.revision, 42)
            XCTAssertEqual(model.taskResults.map(\.id), ["loopback-result-1"])
            XCTAssertEqual(model.taskResults.first?.title, "Loopback result")
            XCTAssertNil(model.watchErrorMessage)
            XCTAssertFalse(model.isWatching)

            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.resolutionSources, ["BV1loopback"])
            XCTAssertEqual(snapshot.createdTaskIDs, [self.taskID])
            XCTAssertEqual(snapshot.watchRequestIDs, [[self.taskID], [self.taskID]])
            XCTAssertEqual(snapshot.getTaskIDs, [self.taskID])
            XCTAssertEqual(snapshot.taskResultQueries, [self.taskID])
            XCTAssertEqual(snapshot.resultOutputRevisions, [42])

            let updates = history.snapshot()
            XCTAssertGreaterThanOrEqual(updates.count, 4)
            XCTAssertTrue(updates.allSatisfy { $0.id == self.taskID })
            XCTAssertTrue(zip(updates, updates.dropFirst()).allSatisfy { $1.progress >= $0.progress })
            XCTAssertTrue(updates.last?.state.localizedCaseInsensitiveContains("succeeded") == true)

            model.clearTask()
            _ = [historyToken, reconnectToken, completionToken]
        }
    }

    func testPermissionDeniedStopsAutomaticRecovery() async throws {
        let probe = TaskWatchRPCProbe(scenario: .permissionDenied)
        let service = TaskWatchTestService(probe: probe)

        try await withLoopbackServer(service: service) { endpoint in
            let model = self.makeModel(endpoint: endpoint, source: "BV1permission")
            let denied = self.expectation(description: "permission denial is surfaced")
            let errorToken = model.$watchErrorMessage.sink { message in
                if message != nil {
                    denied.fulfill()
                }
            }

            await model.submit(serverAddressText: endpoint.displayAddress)

            guard await self.assertTaskSubmitted(model: model, probe: probe, expectations: [denied]) else {
                model.clearTask()
                return
            }

            let deniedWait = await XCTWaiter.fulfillment(of: [denied], timeout: 20)
            XCTAssertEqual(deniedWait, .completed)
            XCTAssertFalse(model.isWatching)
            XCTAssertFalse(model.isReconnecting)
            XCTAssertNotNil(model.watchErrorMessage)
            XCTAssertTrue(model.canRetryTaskUpdates)

            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.resolutionSources, ["BV1permission"])
            XCTAssertEqual(snapshot.createdTaskIDs, [self.taskID])
            XCTAssertEqual(snapshot.watchRequestIDs, [[self.taskID]])
            XCTAssertTrue(snapshot.getTaskIDs.isEmpty)

            model.clearTask()
            _ = errorToken
        }
    }

    func testExplicitRetryReconcilesNotFoundWithoutResubscribingOrRecreating() async throws {
        let probe = TaskWatchRPCProbe(scenario: .notFoundAfterUnavailable)
        let service = TaskWatchTestService(probe: probe)

        try await withLoopbackServer(service: service) { endpoint in
            let model = self.makeModel(endpoint: endpoint, source: "BV1not-found")
            let firstFailure = self.expectation(description: "automatic reconciliation reports not found")
            let settledRetry = self.expectation(description: "explicit retry reconciliation reports not found")
            var manualRetryStarted = false
            let firstFailureOnce = TaskWatchExpectationOnce(firstFailure)
            let settledRetryOnce = TaskWatchExpectationOnce(settledRetry)
            let errorToken = model.$watchErrorMessage.sink { message in
                if message != nil, !model.isWatching {
                    if manualRetryStarted {
                        settledRetryOnce.fulfill()
                    } else {
                        firstFailureOnce.fulfill()
                    }
                }
            }

            await model.submit(serverAddressText: endpoint.displayAddress)

            guard
                await self.assertTaskSubmitted(
                    model: model,
                    probe: probe,
                    expectations: [firstFailure, settledRetry]
                )
            else {
                model.clearTask()
                return
            }

            let firstFailureWait = await XCTWaiter.fulfillment(of: [firstFailure], timeout: 20)
            XCTAssertEqual(firstFailureWait, .completed)
            XCTAssertFalse(model.isWatching)
            XCTAssertNotNil(model.watchErrorMessage)
            XCTAssertTrue(model.canRetryTaskUpdates)

            let beforeRetry = await probe.snapshot()
            XCTAssertEqual(beforeRetry.watchRequestIDs, [[self.taskID]])
            XCTAssertEqual(beforeRetry.getTaskIDs, [self.taskID])

            manualRetryStarted = true
            model.retryTaskUpdates()

            let retryWait = await XCTWaiter.fulfillment(of: [settledRetry], timeout: 20)
            XCTAssertEqual(retryWait, .completed)
            XCTAssertFalse(model.isWatching)
            XCTAssertFalse(model.isReconnecting)
            XCTAssertNotNil(model.watchErrorMessage)
            XCTAssertEqual(model.currentTask?.id, self.taskID)
            XCTAssertTrue(model.currentTask?.state.localizedCaseInsensitiveContains("succeeded") != true)

            let afterRetry = await probe.snapshot()
            XCTAssertEqual(afterRetry.resolutionSources, ["BV1not-found"])
            XCTAssertEqual(afterRetry.createdTaskIDs, [self.taskID])
            XCTAssertEqual(afterRetry.getTaskIDs, [self.taskID, self.taskID])
            XCTAssertEqual(afterRetry.watchRequestIDs, [[self.taskID]])

            model.clearTask()
            _ = errorToken
        }
    }

    func testClearingTaskCancelsRecoveryBeforeGetTask() async throws {
        let probe = TaskWatchRPCProbe(scenario: .interruptThenComplete)
        let sleepProbe = RecoverySleepProbe()
        let service = TaskWatchTestService(probe: probe)

        try await withLoopbackServer(service: service) { endpoint in
            let model = BilibiliTaskViewModel(
                sourceText: "BV1clear",
                operationTimeout: .seconds(10),
                resolutionTimeout: .seconds(10),
                watchRecoverySleep: { duration in
                    try await sleepProbe.pause(duration)
                },
                clientFactory: { endpoint in
                    GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(10))
                }
            )
            let reconnecting = self.expectation(description: "watch enters recovery")
            let sleepStarted = self.expectation(description: "recovery delay starts")
            let sleepCancelled = self.expectation(description: "clearing cancels recovery delay")
            let reconnectToken = model.$isReconnecting.sink { value in
                if value {
                    reconnecting.fulfill()
                }
            }
            await model.submit(serverAddressText: endpoint.displayAddress)

            guard
                await self.assertTaskSubmitted(
                    model: model,
                    probe: probe,
                    expectations: [reconnecting, sleepStarted, sleepCancelled]
                )
            else {
                model.clearTask()
                return
            }

            let startedWaiter = Task {
                await sleepProbe.waitUntilStarted()
                sleepStarted.fulfill()
            }
            let cancelledWaiter = Task {
                await sleepProbe.waitUntilCancelled()
                sleepCancelled.fulfill()
            }

            let recoveryWait = await XCTWaiter.fulfillment(of: [reconnecting, sleepStarted], timeout: 20)
            XCTAssertEqual(recoveryWait, .completed)
            XCTAssertTrue(model.isReconnecting)
            model.clearTask()
            XCTAssertNil(model.currentTask)
            XCTAssertFalse(model.isWatching)
            let cancellationWait = await XCTWaiter.fulfillment(of: [sleepCancelled], timeout: 20)
            XCTAssertEqual(cancellationWait, .completed)

            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.resolutionSources, ["BV1clear"])
            XCTAssertEqual(snapshot.createdTaskIDs, [self.taskID])
            XCTAssertEqual(snapshot.watchRequestIDs, [[self.taskID]])
            XCTAssertTrue(snapshot.getTaskIDs.isEmpty)

            _ = reconnectToken
            _ = startedWaiter
            _ = cancelledWaiter
        }
    }

    private func makeModel(endpoint: CacheServerEndpoint, source: String) -> BilibiliTaskViewModel {
        BilibiliTaskViewModel(
            sourceText: source,
            operationTimeout: .seconds(10),
            resolutionTimeout: .seconds(10),
            clientFactory: { endpoint in
                GRPCCacheControlClient(endpoint: endpoint, rpcTimeout: .seconds(10))
            }
        )
    }

    private func assertTaskSubmitted(
        model: BilibiliTaskViewModel,
        probe: TaskWatchRPCProbe,
        expectations: [XCTestExpectation]
    ) async -> Bool {
        let snapshot = await probe.snapshot()
        guard snapshot.serverInfoRequests > 0,
            snapshot.createdTaskIDs == [taskID],
            model.currentTask?.id == taskID,
            model.errorMessage == nil
        else {
            XCTFail(
                "Task submit did not reach its expected state; "
                    + "serverInfo=\(snapshot.serverInfoRequests), created=\(snapshot.createdTaskIDs), "
                    + "task=\(model.currentTask?.id ?? "none"), "
                    + "error=\(model.errorMessage ?? "none"), status=\(model.statusMessage)"
            )
            expectations.forEach { $0.fulfill() }
            return false
        }
        return true
    }

    private func observeTerminalResult(
        in model: BilibiliTaskViewModel,
        resultID: String,
        expectation: XCTestExpectation
    ) -> AnyCancellable {
        let once = TaskWatchExpectationOnce(expectation)
        return model.objectWillChange.sink {
            Task { @MainActor in
                guard model.currentTask?.state.localizedCaseInsensitiveContains("succeeded") == true,
                    model.currentTask?.outputSummary?.revision == 42,
                    !model.isLoadingMoreTaskResults,
                    model.taskResults.contains(where: { $0.id == resultID && $0.title == "Loopback result" })
                else {
                    return
                }
                once.fulfill()
            }
        }
    }

    private func withLoopbackServer(
        service: TaskWatchTestService,
        operation: (CacheServerEndpoint) async throws -> Void
    ) async throws {
        try await withGRPCServer(
            transport: .http2NIOPosix(
                address: .ipv4(host: "127.0.0.1", port: 0),
                transportSecurity: .plaintext
            ),
            services: [service]
        ) { server in
            let address = try await server.transport.listeningAddress
            guard let port = address.ipv4?.port else {
                throw LoopbackFixtureError.invalidListeningAddress
            }
            try await operation(CacheServerEndpoint(host: "127.0.0.1", port: port))
        }
    }
}

@MainActor
private final class TaskWatchExpectationOnce {
    private let expectation: XCTestExpectation
    private var fulfilled = false

    init(_ expectation: XCTestExpectation) {
        self.expectation = expectation
    }

    func fulfill() {
        guard !fulfilled else {
            return
        }
        fulfilled = true
        expectation.fulfill()
    }
}

private enum LoopbackFixtureError: Error {
    case invalidListeningAddress
}

private enum TaskWatchFixtureScenario: Equatable, Sendable {
    case recoverAfterUnavailable
    case permissionDenied
    case notFoundAfterUnavailable
    case interruptThenComplete
}

private enum TaskWatchFixturePlan: Sendable {
    case unavailableAfterProgress
    case permissionDenied
    case complete
}

private actor TaskWatchRPCProbe {
    struct Snapshot: Sendable {
        let serverInfoRequests: Int
        let resolutionSources: [String]
        let createdTaskIDs: [String]
        let watchRequestIDs: [[String]]
        let getTaskIDs: [String]
        let taskResultQueries: [String]
        let resultOutputRevisions: [UInt64]
    }

    private let scenario: TaskWatchFixtureScenario
    private var serverInfoRequests = 0
    private var resolutionSources: [String] = []
    private var createdTaskIDs: [String] = []
    private var watchRequestIDs: [[String]] = []
    private var getTaskIDs: [String] = []
    private var taskResultQueries: [String] = []
    private var resultOutputRevisions: [UInt64] = []

    init(scenario: TaskWatchFixtureScenario) {
        self.scenario = scenario
    }

    func recordServerInfo() {
        serverInfoRequests += 1
    }

    func recordResolution(source: String) {
        resolutionSources.append(source)
    }

    func recordCreation(taskID: String) {
        createdTaskIDs.append(taskID)
    }

    func beginWatch(ids: [String]) -> Int {
        watchRequestIDs.append(ids)
        return watchRequestIDs.count
    }

    func watchPlan(attempt: Int) -> TaskWatchFixturePlan {
        switch scenario {
        case .recoverAfterUnavailable, .notFoundAfterUnavailable, .interruptThenComplete:
            return attempt == 1 ? .unavailableAfterProgress : .complete
        case .permissionDenied:
            return attempt == 1 ? .permissionDenied : .complete
        }
    }

    func recordGetTask(id: String) {
        getTaskIDs.append(id)
    }

    func getTaskShouldBeNotFound() -> Bool {
        scenario == .notFoundAfterUnavailable
    }

    func recordTaskResults(id: String, outputRevision: UInt64) {
        taskResultQueries.append(id)
        resultOutputRevisions.append(outputRevision)
    }

    func snapshot() -> Snapshot {
        Snapshot(
            serverInfoRequests: serverInfoRequests,
            resolutionSources: resolutionSources,
            createdTaskIDs: createdTaskIDs,
            watchRequestIDs: watchRequestIDs,
            getTaskIDs: getTaskIDs,
            taskResultQueries: taskResultQueries,
            resultOutputRevisions: resultOutputRevisions
        )
    }

}

private actor RecoverySleepProbe {
    private var started = false
    private var cancelled = false
    private var startedWaiters: [CheckedContinuation<Void, Never>] = []
    private var cancelledWaiters: [CheckedContinuation<Void, Never>] = []

    func pause(_ duration: Duration) async throws {
        _ = duration
        started = true
        startedWaiters.forEach { $0.resume() }
        startedWaiters.removeAll()
        do {
            try await Task.sleep(for: .seconds(60))
        } catch {
            cancelled = true
            cancelledWaiters.forEach { $0.resume() }
            cancelledWaiters.removeAll()
            throw error
        }
    }

    func waitUntilStarted() async {
        guard !started else {
            return
        }
        await withCheckedContinuation { startedWaiters.append($0) }
    }

    func waitUntilCancelled() async {
        guard !cancelled else {
            return
        }
        await withCheckedContinuation { cancelledWaiters.append($0) }
    }
}

private final class TaskUpdateHistory: @unchecked Sendable {
    private let lock = NSLock()
    private var updates: [CacheTask] = []

    func append(_ task: CacheTask) {
        lock.lock()
        updates.append(task)
        lock.unlock()
    }

    func snapshot() -> [CacheTask] {
        lock.lock()
        defer { lock.unlock() }
        return updates
    }
}

private struct TaskWatchTestService: RegistrableRPCService {
    let probe: TaskWatchRPCProbe

    init(probe: TaskWatchRPCProbe) {
        self.probe = probe
    }

    func registerMethods<Transport: ServerTransport>(with router: inout RPCRouter<Transport>) {
        let probe = self.probe
        Self.registerUnary(
            TvosNetPlayer_V1_ServerService.Method.GetServerInfo.descriptor,
            with: &router
        ) { (_: TvosNetPlayer_V1_GetServerInfoRequest) in
            await probe.recordServerInfo()
            var info = TvosNetPlayer_V1_ServerInfo()
            info.id = "loopback-test-server"
            info.name = "Loopback test server"
            info.version = "test"
            info.capabilities = [
                .bilibiliResolutionV2,
                .bilibiliExecutionV2,
                .bilibiliPlaybackPolicy,
                .taskOutputV2,
            ]
            return info
        }
        Self.registerUnary(
            TvosNetPlayer_V1_TaskService.Method.StartBilibiliResolution.descriptor,
            with: &router
        ) { (request: TvosNetPlayer_V1_StartBilibiliResolutionRequest) in
            await probe.recordResolution(source: request.urlOrID)
            return Self.resolutionPage(source: request.urlOrID)
        }
        Self.registerUnary(
            TvosNetPlayer_V1_TaskService.Method.CreateBilibiliTaskV2.descriptor,
            with: &router
        ) { (_: TvosNetPlayer_V1_CreateBilibiliTaskV2Request) in
            await probe.recordCreation(taskID: "loopback-task-1")
            return Self.task(state: .preparing, progress: 0.1)
        }
        Self.registerUnary(
            TvosNetPlayer_V1_TaskService.Method.GetTask.descriptor,
            with: &router
        ) { (request: TvosNetPlayer_V1_GetTaskRequest) in
            await probe.recordGetTask(id: request.id)
            if await probe.getTaskShouldBeNotFound() {
                throw RPCError(code: .notFound, message: "Synthetic task not found.")
            }
            return Self.task(state: .running, progress: 0.65)
        }
        Self.registerUnary(
            TvosNetPlayer_V1_TaskService.Method.ListTaskResults.descriptor,
            with: &router
        ) { (request: TvosNetPlayer_V1_ListTaskResultsRequest) in
            await probe.recordTaskResults(id: request.taskID, outputRevision: 42)
            var response = TvosNetPlayer_V1_ListTaskResultsResponse()
            response.results = [Self.result()]
            response.outputRevision = 42
            response.pageInfo.totalSize = 1
            response.pageInfo.snapshotID = "loopback-results-revision-42"
            return response
        }

        router.registerHandler(
            forMethod: TvosNetPlayer_V1_TaskService.Method.WatchTasks.descriptor,
            deserializer: ProtobufDeserializer<TvosNetPlayer_V1_WatchTasksRequest>(),
            serializer: ProtobufSerializer<TvosNetPlayer_V1_TaskEvent>()
        ) { request, _ in
            let request = try await ServerRequest(stream: request)
            let attempt = await probe.beginWatch(ids: request.message.ids)
            return StreamingServerResponse(of: TvosNetPlayer_V1_TaskEvent.self) { writer in
                switch await probe.watchPlan(attempt: attempt) {
                case .unavailableAfterProgress:
                    try await writer.write(Self.event(task: Self.task(state: .running, progress: 0.3)))
                    throw RPCError(code: .unavailable, message: "Synthetic watch interruption.")
                case .permissionDenied:
                    throw RPCError(code: .permissionDenied, message: "Synthetic permission denial.")
                case .complete:
                    try await writer.write(
                        Self.event(
                            task: Self.task(
                                state: .succeeded,
                                progress: 1,
                                outputRevision: 42
                            )))
                    return [:]
                }
            }
        }
    }

    private static func registerUnary<Transport, Input, Output>(
        _ descriptor: MethodDescriptor,
        with router: inout RPCRouter<Transport>,
        handler: @escaping @Sendable (Input) async throws -> Output
    )
    where
        Transport: ServerTransport, Input: SwiftProtobuf.Message & Sendable,
        Output: SwiftProtobuf.Message & Sendable
    {
        router.registerHandler(
            forMethod: descriptor,
            deserializer: ProtobufDeserializer<Input>(),
            serializer: ProtobufSerializer<Output>()
        ) { request, _ in
            let request = try await ServerRequest(stream: request)
            let output = try await handler(request.message)
            return StreamingServerResponse(single: ServerResponse(message: output))
        }
    }

    private static func resolutionPage(source: String) -> TvosNetPlayer_V1_BilibiliResolutionPage {
        var session = TvosNetPlayer_V1_BilibiliResolutionSession()
        session.id = "loopback-resolution-1"
        session.source = source
        session.title = "Loopback video"
        session.sourceKind = "video"
        session.defaultCandidateToken = "candidate-1"

        var candidate = TvosNetPlayer_V1_BilibiliResolutionCandidate()
        candidate.candidateToken = "candidate-1"
        candidate.title = "Loopback video"
        candidate.sourceKind = "video"
        candidate.index = 1

        var page = TvosNetPlayer_V1_BilibiliResolutionPage()
        page.session = session
        page.candidates = [candidate]
        page.pageInfo.totalSize = 1
        page.pageInfo.snapshotID = "loopback-resolution-snapshot"
        return page
    }

    private static func task(
        state: TvosNetPlayer_V1_TaskState,
        progress: Double,
        outputRevision: UInt64? = nil
    ) -> TvosNetPlayer_V1_Task {
        var task = TvosNetPlayer_V1_Task()
        task.id = "loopback-task-1"
        task.kind = .bilibiliProgressivePlayback
        task.state = state
        task.source = "BV1loopback"
        task.title = "Loopback video"
        task.progress = progress
        task.message = "Synthetic task state."
        if let outputRevision {
            var summary = TvosNetPlayer_V1_TaskOutputSummary()
            summary.revision = outputRevision
            summary.resultCount = 1
            summary.terminalResultCount = 1
            summary.successfulResultCount = 1
            summary.primaryResultID = "loopback-result-1"
            task.outputSummary = summary
        }
        return task
    }

    private static func event(task: TvosNetPlayer_V1_Task) -> TvosNetPlayer_V1_TaskEvent {
        var event = TvosNetPlayer_V1_TaskEvent()
        event.task = task
        return event
    }

    private static func result() -> TvosNetPlayer_V1_TaskResult {
        var result = TvosNetPlayer_V1_TaskResult()
        result.id = "loopback-result-1"
        result.state = .succeeded
        result.title = "Loopback result"
        return result
    }
}
