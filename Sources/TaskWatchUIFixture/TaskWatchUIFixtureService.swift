import Foundation
import GRPCCore
import GRPCProtobuf
import SwiftProtobuf
import TVOSNetPlayerCacheClient

package struct TaskWatchUIFixtureService: RegistrableRPCService, Sendable {
    private let metrics: FixtureMetricsStore
    private let runtime: FixtureTaskRuntime
    private let mediaURL: String

    package init(metrics: FixtureMetricsStore, mediaURL: URL) {
        self.metrics = metrics
        self.runtime = FixtureTaskRuntime()
        self.mediaURL = mediaURL.absoluteString
    }

    package func registerMethods<Transport: ServerTransport>(with router: inout RPCRouter<Transport>) {
        let metrics = self.metrics
        let runtime = self.runtime
        let mediaURL = self.mediaURL

        Self.registerUnary(TvosNetPlayer_V1_ServerService.Method.GetServerInfo.descriptor, with: &router) {
            (_: TvosNetPlayer_V1_GetServerInfoRequest) in
            try await Self.persistMetrics { try await metrics.recordServerInfo() }
            var response = TvosNetPlayer_V1_ServerInfo()
            response.id = "task-watch-ui-fixture"
            response.name = "Task watch UI fixture"
            response.version = "1"
            response.capabilities = [
                .bilibiliPlaybackPolicy,
                .bilibiliResolutionV2,
                .bilibiliExecutionV2,
                .taskOutputV2,
            ]
            return response
        }

        Self.registerUnary(TvosNetPlayer_V1_ServerService.Method.CheckHealth.descriptor, with: &router) {
            (_: TvosNetPlayer_V1_CheckHealthRequest) in
            var response = TvosNetPlayer_V1_HealthStatus()
            response.state = .serving
            response.message = "Fixture is serving."
            return response
        }

        Self.registerUnary(TvosNetPlayer_V1_LibraryService.Method.ListLibraryItems.descriptor, with: &router) {
            (_: TvosNetPlayer_V1_ListLibraryItemsRequest) in
            try await Self.persistMetrics { try await metrics.recordLibraryList() }
            return TvosNetPlayer_V1_ListLibraryItemsResponse()
        }

        Self.registerUnary(
            TvosNetPlayer_V1_TaskService.Method.StartBilibiliResolution.descriptor,
            with: &router
        ) { (request: TvosNetPlayer_V1_StartBilibiliResolutionRequest) in
            guard
                let scenario = try await Self.resolve(
                    request.urlOrID,
                    metrics: metrics
                )
            else {
                throw RPCError(code: .invalidArgument, message: "Unsupported fixture input.")
            }
            _ = try await Self.record(.startResolution, scenario: scenario, metrics: metrics)
            return Self.resolutionPage(for: scenario)
        }

        Self.registerUnary(
            TvosNetPlayer_V1_TaskService.Method.ListBilibiliResolutionCandidates.descriptor,
            with: &router
        ) { (request: TvosNetPlayer_V1_ListBilibiliResolutionCandidatesRequest) in
            guard let scenario = Self.scenario(forSessionID: request.sessionID) else {
                try await Self.reject(metrics)
                throw RPCError(code: .notFound, message: "Resolution session not found.")
            }
            _ = try await Self.record(.listCandidates, scenario: scenario, metrics: metrics)
            return Self.resolutionPage(for: scenario)
        }

        Self.registerUnary(
            TvosNetPlayer_V1_TaskService.Method.CreateBilibiliTaskV2.descriptor,
            with: &router
        ) { (request: TvosNetPlayer_V1_CreateBilibiliTaskV2Request) in
            return try await Self.createTask(
                scenario: Self.scenario(forSessionID: request.sessionID),
                selectionIsValid: request.selection.mode == .single
                    && request.selection.candidateTokens == [
                        Self.scenario(forSessionID: request.sessionID)?.candidateToken ?? ""
                    ],
                executionIsPlayback: Self.isPlaybackExecution(request),
                metrics: metrics,
                runtime: runtime,
                mediaURL: mediaURL
            )
        }

        Self.registerUnary(
            TvosNetPlayer_V1_TaskService.Method.CreateBilibiliPlaybackTaskV2.descriptor,
            with: &router
        ) { (request: TvosNetPlayer_V1_CreateBilibiliPlaybackTaskV2Request) in
            let scenario = Self.scenario(forSessionID: request.sessionID)
            return try await Self.createTask(
                scenario: scenario,
                selectionIsValid: request.selection.mode == .single
                    && request.selection.candidateTokens == [scenario?.candidateToken ?? ""],
                executionIsPlayback: true,
                metrics: metrics,
                runtime: runtime,
                mediaURL: mediaURL
            )
        }

        Self.registerUnary(TvosNetPlayer_V1_TaskService.Method.GetTask.descriptor, with: &router) {
            (request: TvosNetPlayer_V1_GetTaskRequest) in
            guard let scenario = Self.scenario(forTaskID: request.id) else {
                try await Self.reject(metrics)
                throw RPCError(code: .notFound, message: "Task not found.")
            }
            try await Self.record(.getTask, scenario: scenario, metrics: metrics)
            guard await runtime.hasTask(scenario) else {
                throw RPCError(code: .notFound, message: "Task not found.")
            }
            let observation = await runtime.observeGetTask(for: scenario)
            if observation.countsForSilentReconciliation {
                try await Self.recordGetTaskObservation(
                    scenario: scenario,
                    completed: observation.state == .completed,
                    metrics: metrics
                )
            }
            return Self.task(
                for: scenario,
                runtimeState: observation.state,
                mediaURL: mediaURL
            )
        }

        Self.registerUnary(
            TvosNetPlayer_V1_TaskService.Method.ListTaskResults.descriptor,
            with: &router
        ) { (request: TvosNetPlayer_V1_ListTaskResultsRequest) in
            guard let scenario = Self.scenario(forTaskID: request.taskID) else {
                try await Self.reject(metrics)
                throw RPCError(code: .notFound, message: "Task output not found.")
            }
            try await Self.record(.output, scenario: scenario, metrics: metrics)
            guard await runtime.hasTask(scenario) else {
                throw RPCError(code: .notFound, message: "Task output not found.")
            }
            return Self.outputPage(
                for: scenario,
                runtimeState: await runtime.state(for: scenario),
                mediaURL: mediaURL
            )
        }

        Self.registerUnary(TvosNetPlayer_V1_TaskService.Method.CancelTask.descriptor, with: &router) {
            (request: TvosNetPlayer_V1_CancelTaskRequest) in
            guard let scenario = Self.scenario(forTaskID: request.id) else {
                try await Self.reject(metrics)
                throw RPCError(code: .notFound, message: "Task not found.")
            }
            try await Self.record(.cancel, scenario: scenario, metrics: metrics)
            guard await runtime.cancel(scenario) else {
                throw RPCError(code: .notFound, message: "Task not found.")
            }
            return Self.task(for: scenario, runtimeState: .cancelled, mediaURL: mediaURL)
        }

        router.registerHandler(
            forMethod: TvosNetPlayer_V1_TaskService.Method.WatchTasks.descriptor,
            deserializer: ProtobufDeserializer<TvosNetPlayer_V1_WatchTasksRequest>(),
            serializer: ProtobufSerializer<TvosNetPlayer_V1_TaskEvent>()
        ) { request, context in
            let request = try await ServerRequest(stream: request)
            guard request.message.ids.count == 1,
                let scenario = Self.scenario(forTaskID: request.message.ids[0])
            else {
                try await Self.reject(metrics)
                throw RPCError(code: .invalidArgument, message: "A recognized task ID is required.")
            }

            let attempt = try await Self.record(.watch, scenario: scenario, metrics: metrics)
            guard await runtime.hasTask(scenario) else {
                throw RPCError(code: .notFound, message: "Task not found.")
            }

            let cancellation = context.cancellation
            return StreamingServerResponse(of: TvosNetPlayer_V1_TaskEvent.self) { writer in
                if scenario == .silent {
                    let watchID = try await runtime.openSilentWatch()
                    do {
                        try await Self.recordWatchStreamOpen(scenario: scenario, metrics: metrics)
                        var event = TvosNetPlayer_V1_TaskEvent()
                        event.task = Self.task(
                            for: scenario,
                            runtimeState: await runtime.state(for: scenario),
                            mediaURL: mediaURL
                        )
                        try await writer.write(event)
                        // RPC cancellation does not necessarily cancel the producer's Swift task.
                        try await cancellation.cancelled
                    } catch {
                        await runtime.cancelSilentWatch(id: watchID)
                        throw error
                    }
                    await runtime.cancelSilentWatch(id: watchID)
                    return [:]
                }

                try await Self.recordWatchStreamOpen(scenario: scenario, metrics: metrics)
                let failureLimit = scenario == .automatic ? 3 : 7
                guard attempt > failureLimit else {
                    throw RPCError(code: .unavailable, message: "Task updates are temporarily unavailable.")
                }

                await runtime.complete(scenario)
                var event = TvosNetPlayer_V1_TaskEvent()
                event.task = Self.task(for: scenario, runtimeState: .completed, mediaURL: mediaURL)
                try await writer.write(event)
                return [:]
            }
        }
    }

    private static func createTask(
        scenario: FixtureScenario?,
        selectionIsValid: Bool,
        executionIsPlayback: Bool,
        metrics: FixtureMetricsStore,
        runtime: FixtureTaskRuntime,
        mediaURL: String
    ) async throws -> TvosNetPlayer_V1_Task {
        guard let scenario else {
            try await reject(metrics)
            throw RPCError(code: .invalidArgument, message: "Resolution session not found.")
        }

        let createCount = try await record(.create, scenario: scenario, metrics: metrics)
        guard createCount == 1 else {
            throw RPCError(code: .alreadyExists, message: "Fixture task was already created.")
        }
        guard selectionIsValid, executionIsPlayback else {
            throw RPCError(code: .invalidArgument, message: "Fixture requires its single playback candidate.")
        }
        await runtime.create(scenario)
        return task(for: scenario, runtimeState: .active, mediaURL: mediaURL)
    }

    private static func isPlaybackExecution(
        _ request: TvosNetPlayer_V1_CreateBilibiliTaskV2Request
    ) -> Bool {
        guard case .some(.playback(_)) = request.execution else {
            return false
        }
        return true
    }

    private static func resolve(
        _ input: String,
        metrics: FixtureMetricsStore
    ) async throws -> FixtureScenario? {
        do {
            return try FixtureSourceResolver.scenario(for: input)
        } catch {
            try await reject(metrics)
            return nil
        }
    }

    private static func record(
        _ rpc: FixtureRPC,
        scenario: FixtureScenario,
        metrics: FixtureMetricsStore
    ) async throws -> UInt64 {
        do {
            return try await metrics.record(rpc, for: scenario)
        } catch {
            throw RPCError(code: .internalError, message: "Fixture metrics could not be persisted.")
        }
    }

    private static func reject(_ metrics: FixtureMetricsStore) async throws {
        do {
            try await metrics.recordRejectedRequest()
        } catch {
            throw RPCError(code: .internalError, message: "Fixture metrics could not be persisted.")
        }
    }

    private static func recordWatchStreamOpen(
        scenario: FixtureScenario,
        metrics: FixtureMetricsStore
    ) async throws {
        do {
            try await metrics.recordWatchStreamOpen(for: scenario)
        } catch {
            throw RPCError(code: .internalError, message: "Fixture metrics could not be persisted.")
        }
    }

    private static func recordGetTaskObservation(
        scenario: FixtureScenario,
        completed: Bool,
        metrics: FixtureMetricsStore
    ) async throws {
        do {
            try await metrics.recordGetTaskObservation(for: scenario, completed: completed)
        } catch {
            throw RPCError(code: .internalError, message: "Fixture metrics could not be persisted.")
        }
    }

    private static func persistMetrics(_ operation: @Sendable () async throws -> Void) async throws {
        do {
            try await operation()
        } catch {
            throw RPCError(code: .internalError, message: "Fixture metrics could not be persisted.")
        }
    }

    private static func resolutionPage(for scenario: FixtureScenario) -> TvosNetPlayer_V1_BilibiliResolutionPage {
        var session = TvosNetPlayer_V1_BilibiliResolutionSession()
        session.id = scenario.resolutionSessionID
        session.source = scenario.source
        session.title = scenario.title
        session.sourceKind = "video"
        session.defaultCandidateToken = scenario.candidateToken

        var identity = TvosNetPlayer_V1_BilibiliContentIdentity()
        identity.kind = .videoPage
        identity.bvid = scenario.source

        var candidate = TvosNetPlayer_V1_BilibiliResolutionCandidate()
        candidate.candidateToken = scenario.candidateToken
        candidate.title = scenario.title
        candidate.sourceKind = "video"
        candidate.identity = identity
        candidate.index = 1

        var response = TvosNetPlayer_V1_BilibiliResolutionPage()
        response.session = session
        response.candidates = [candidate]
        response.pageInfo.totalSize = 1
        response.pageInfo.snapshotID = "\(scenario.taskID)-resolution-v1"
        return response
    }

    private static func task(
        for scenario: FixtureScenario,
        runtimeState: FixtureTaskRuntime.State,
        mediaURL: String
    ) -> TvosNetPlayer_V1_Task {
        let finished = runtimeState == .completed || runtimeState == .cancelled
        var playbackSource = TvosNetPlayer_V1_PlaybackSource()
        playbackSource.itemID = scenario.taskID
        playbackSource.variantID = "\(scenario.taskID)-mp4"
        playbackSource.`protocol` = .httpFile
        playbackSource.uri = mediaURL

        var task = TvosNetPlayer_V1_Task()
        task.id = scenario.taskID
        task.kind = .bilibiliProgressivePlayback
        task.state = runtimeState == .completed ? .succeeded : (runtimeState == .cancelled ? .cancelled : .playable)
        task.source = scenario.source
        task.title = scenario.title
        task.progress = 0.42
        if runtimeState == .completed {
            task.progress = 1
        }
        if runtimeState == .completed {
            switch scenario {
            case .automatic:
                task.message = "Automatic recovery complete"
            case .manual:
                task.message = "Manual recovery complete"
            case .silent:
                task.message = "Silent task reconciliation complete"
            }
        } else {
            task.message = "Playable fixture output is available."
        }
        task.libraryItemID = scenario.taskID
        task.playbackSource = playbackSource
        task.resultItems = [resultItem(for: scenario, runtimeState: runtimeState, mediaURL: mediaURL)]

        var summary = TvosNetPlayer_V1_TaskOutputSummary()
        summary.revision = finished ? 2 : 1
        summary.resultCount = 1
        summary.terminalResultCount = finished ? 1 : 0
        summary.successfulResultCount = runtimeState == .completed ? 1 : 0
        summary.cancelledResultCount = runtimeState == .cancelled ? 1 : 0
        summary.primaryResultID = scenario.resultID
        task.outputSummary = summary
        return task
    }

    private static func outputPage(
        for scenario: FixtureScenario,
        runtimeState: FixtureTaskRuntime.State,
        mediaURL: String
    ) -> TvosNetPlayer_V1_ListTaskResultsResponse {
        let finished = runtimeState == .completed || runtimeState == .cancelled
        var response = TvosNetPlayer_V1_ListTaskResultsResponse()
        response.results = [result(for: scenario, runtimeState: runtimeState, mediaURL: mediaURL)]
        response.outputRevision = finished ? 2 : 1
        response.pageInfo.totalSize = 1
        response.pageInfo.snapshotID = "\(scenario.taskID)-output-v\(finished ? 2 : 1)"
        return response
    }

    private static func resultItem(
        for scenario: FixtureScenario,
        runtimeState: FixtureTaskRuntime.State,
        mediaURL: String
    ) -> TvosNetPlayer_V1_BilibiliTaskResultItem {
        var source = TvosNetPlayer_V1_PlaybackSource()
        source.itemID = scenario.resultID
        source.variantID = "\(scenario.taskID)-mp4"
        source.`protocol` = .httpFile
        source.uri = mediaURL

        var item = TvosNetPlayer_V1_BilibiliTaskResultItem()
        item.id = scenario.resultID
        item.selectionID = scenario.candidateToken
        item.title = scenario.title
        item.sourceKind = "video"
        item.contentID = scenario.source
        item.index = 1
        item.state = runtimeState == .completed ? .succeeded : (runtimeState == .cancelled ? .cancelled : .playable)
        item.libraryItemID = scenario.resultID
        item.playbackSource = source
        return item
    }

    private static func result(
        for scenario: FixtureScenario,
        runtimeState: FixtureTaskRuntime.State,
        mediaURL: String
    ) -> TvosNetPlayer_V1_TaskResult {
        var source = TvosNetPlayer_V1_PlaybackSource()
        source.itemID = scenario.resultID
        source.variantID = "\(scenario.taskID)-mp4"
        source.`protocol` = .httpFile
        source.uri = mediaURL

        var result = TvosNetPlayer_V1_TaskResult()
        result.id = scenario.resultID
        result.state = runtimeState == .completed ? .succeeded : (runtimeState == .cancelled ? .cancelled : .playable)
        result.title = scenario.title
        result.libraryItemID = scenario.resultID
        result.playbackSource = source
        var artifact = TvosNetPlayer_V1_TaskArtifact()
        artifact.id = scenario.resultID + "-media"
        artifact.kind = .media
        artifact.state = .available
        artifact.title = scenario.title
        artifact.format = "mp4"
        artifact.libraryItemID = scenario.resultID
        artifact.resource.id = artifact.id
        artifact.resource.uri = mediaURL
        artifact.resource.contentType = "video/mp4"
        artifact.resource.supportsByteRanges = true
        result.artifacts = [artifact]
        return result
    }

    private static func scenario(forSessionID sessionID: String) -> FixtureScenario? {
        FixtureScenario.allCases.first { $0.resolutionSessionID == sessionID }
    }

    private static func scenario(forTaskID taskID: String) -> FixtureScenario? {
        FixtureScenario.allCases.first { $0.taskID == taskID }
    }

    private static func registerUnary<Transport, Input, Output>(
        _ descriptor: MethodDescriptor,
        with router: inout RPCRouter<Transport>,
        handler: @escaping @Sendable (Input) async throws -> Output
    )
    where
        Transport: ServerTransport,
        Input: SwiftProtobuf.Message & Sendable,
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
}

private actor FixtureTaskRuntime {
    package enum State: Equatable, Sendable {
        case absent
        case active
        case completed
        case cancelled
    }

    private var states: [FixtureScenario: State] = [:]
    private var silentWatchID: UUID?
    private var silentWatchDidObserveUnchangedTask = false

    func create(_ scenario: FixtureScenario) {
        states[scenario] = .active
    }

    func hasTask(_ scenario: FixtureScenario) -> Bool {
        states[scenario] != nil
    }

    func state(for scenario: FixtureScenario) -> State {
        states[scenario] ?? .absent
    }

    func openSilentWatch() throws -> UUID {
        guard silentWatchID == nil else {
            throw RPCError(code: .alreadyExists, message: "A silent task watch is already open.")
        }
        let id = UUID()
        silentWatchID = id
        return id
    }

    func observeGetTask(for scenario: FixtureScenario) -> (
        state: State,
        countsForSilentReconciliation: Bool
    ) {
        let currentState = states[scenario] ?? .absent
        guard scenario == .silent, silentWatchID != nil, currentState == .active else {
            return (currentState, false)
        }

        if !silentWatchDidObserveUnchangedTask {
            silentWatchDidObserveUnchangedTask = true
            return (.active, true)
        }

        states[scenario] = .completed
        return (.completed, true)
    }

    func cancelSilentWatch(id: UUID) {
        guard silentWatchID == id else { return }
        silentWatchID = nil
    }

    func complete(_ scenario: FixtureScenario) {
        if states[scenario] == .active {
            states[scenario] = .completed
        }
    }

    func cancel(_ scenario: FixtureScenario) -> Bool {
        guard states[scenario] == .active else {
            return false
        }
        states[scenario] = .cancelled
        return true
    }
}
