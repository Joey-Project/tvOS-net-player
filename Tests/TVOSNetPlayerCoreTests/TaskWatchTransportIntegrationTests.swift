import Combine
import Foundation
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

    func testSilentWatchReconcilesUnchangedAndTerminalSnapshotsWithoutResubscribing() async throws {
        let probe = TaskWatchRPCProbe(scenario: .silentReconciliation)
        let service = TaskWatchTestService(probe: probe)

        try await withLoopbackServer(service: service) { endpoint in
            let model = self.makeModel(
                endpoint: endpoint,
                source: "BV1silent-reconciliation",
                watchReconciliationInterval: .milliseconds(100)
            )
            defer { model.clearTask() }

            let watchOpened = self.expectation(description: "silent watch stream is open")
            let runningSnapshot = self.expectation(description: "first reconciliation updates the task")
            let healthySilentWatch = self.expectation(description: "unchanged snapshots keep the watch healthy")
            let completed = self.expectation(description: "terminal reconciliation publishes task output")
            let watchClosed = self.expectation(description: "client cancellation closes the silent watch stream")
            await probe.holdGetTaskResponse(attempt: 3)
            let openWaiter = Task { @MainActor in
                await probe.waitForWatchOpenCount(1)
                watchOpened.fulfill()
            }
            let healthyWatchWaiter = Task { @MainActor in
                await probe.waitForGetTaskCount(3)
                healthySilentWatch.fulfill()
            }
            let closeWaiter = Task { @MainActor in
                await probe.waitForWatchCloseCount(1)
                watchClosed.fulfill()
            }
            defer {
                openWaiter.cancel()
                healthyWatchWaiter.cancel()
                closeWaiter.cancel()
            }

            let runningSnapshotOnce = TaskWatchExpectationOnce(runningSnapshot)
            let taskToken = model.$currentTask.sink { task in
                if task?.id == self.taskID,
                    task?.state.localizedCaseInsensitiveContains("running") == true
                {
                    Task { @MainActor in runningSnapshotOnce.fulfill() }
                }
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
                    expectations: [watchOpened, runningSnapshot, healthySilentWatch, completed, watchClosed]
                )
            else {
                return
            }

            let firstReconciliation = await XCTWaiter.fulfillment(
                of: [watchOpened, runningSnapshot],
                timeout: 20
            )
            XCTAssertEqual(firstReconciliation, .completed)

            let healthyWatchWait = await XCTWaiter.fulfillment(of: [healthySilentWatch], timeout: 20)
            XCTAssertEqual(healthyWatchWait, .completed)
            XCTAssertTrue(model.isWatching)
            XCTAssertFalse(model.isReconnecting)
            XCTAssertNil(model.watchErrorMessage)
            XCTAssertEqual(model.currentTask?.id, self.taskID)
            XCTAssertTrue(model.currentTask?.state.localizedCaseInsensitiveContains("running") == true)

            await probe.releaseGetTaskResponse(attempt: 3)
            let completedWait = await XCTWaiter.fulfillment(of: [completed], timeout: 20)
            XCTAssertEqual(completedWait, .completed)
            XCTAssertEqual(model.currentTask?.id, self.taskID)
            XCTAssertTrue(model.currentTask?.state.localizedCaseInsensitiveContains("succeeded") == true)
            XCTAssertEqual(model.currentTask?.outputSummary?.revision, 42)
            XCTAssertEqual(model.taskResults.map(\.id), ["loopback-result-1"])
            XCTAssertEqual(model.taskResults.first?.title, "Loopback result")
            XCTAssertNil(model.watchErrorMessage)
            XCTAssertFalse(model.isWatching)

            model.clearTask()
            let watchClosedWait = await XCTWaiter.fulfillment(of: [watchClosed], timeout: 20)
            XCTAssertEqual(watchClosedWait, .completed)

            let snapshot = await probe.snapshot()
            XCTAssertEqual(snapshot.resolutionSources, ["BV1silent-reconciliation"])
            XCTAssertEqual(snapshot.createdTaskIDs, [self.taskID])
            XCTAssertEqual(snapshot.watchRequestIDs, [[self.taskID]])
            XCTAssertEqual(snapshot.getTaskIDs, [self.taskID, self.taskID, self.taskID])
            XCTAssertEqual(snapshot.cancelledTaskIDs, [])
            XCTAssertEqual(snapshot.openWatchStreams, 1)
            XCTAssertEqual(snapshot.closedWatchStreams, 1)
            XCTAssertEqual(snapshot.taskResultQueries, [self.taskID])
            XCTAssertEqual(snapshot.resultOutputRevisions, [42])

            _ = [taskToken, completionToken]
        }
    }

    func testSucceededMultiResultTransportRestoresExactSavedPlaybackWithoutResubmission() async throws {
        let suiteName = "TaskWatchTransportIntegrationTests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let store = ClientResumptionStore(defaults: defaults)
        let probe = TaskWatchRPCProbe(scenario: .succeededResumption)
        let service = TaskWatchTestService(probe: probe)

        try await withLoopbackServer(service: service) { endpoint in
            let model = self.makeModel(
                endpoint: endpoint,
                source: SucceededResumptionFixture.source,
                defaults: defaults,
                resumptionStore: store
            )
            defer { model.clearTask() }
            let cached = self.expectation(description: "succeeded transport results are cached and watch is terminal")
            let cachedToken = self.observeCachedSucceededResults(in: model, expectation: cached)

            await model.submit(serverAddressText: endpoint.displayAddress)
            guard await self.assertTaskSubmitted(model: model, probe: probe, expectations: [cached]) else {
                return
            }
            let cachedWait = await XCTWaiter.fulfillment(of: [cached], timeout: 20)
            XCTAssertEqual(cachedWait, .completed, "status=\(model.statusMessage), watching=\(model.isWatching)")
            guard cachedWait == .completed else { return }

            XCTAssertEqual(model.currentTask?.state, "succeeded")
            XCTAssertEqual(model.currentTask?.resultItems.map(\.state), ["succeeded", "succeeded"])
            XCTAssertEqual(
                model.currentTask?.resultItems.map(\.id),
                [SucceededResumptionFixture.decoyResultID, SucceededResumptionFixture.selectedResultID]
            )
            XCTAssertEqual(model.currentTask?.outputSummary?.primaryResultID, SucceededResumptionFixture.decoyResultID)
            XCTAssertEqual(model.statusMessage, "2 Bilibili results are cached for LAN playback.")
            XCTAssertFalse(model.isWatching)
            XCTAssertNil(model.watchErrorMessage)
            XCTAssertTrue(model.taskResults.allSatisfy { $0.isCached && $0.isReady && $0.playbackURL != nil })

            let taskIdentity = try XCTUnwrap(store.record?.task)
            XCTAssertEqual(taskIdentity.endpoint, endpoint)
            XCTAssertEqual(taskIdentity.serverID, "loopback-test-server")
            XCTAssertEqual(taskIdentity.taskID, self.taskID)
            XCTAssertNil(
                store.record?.playback,
                "Normal task acceptance must persist the task before playback begins."
            )

            let selected = try XCTUnwrap(
                model.taskResults.first(where: { $0.id == SucceededResumptionFixture.selectedResultID })
            )
            XCTAssertEqual(selected.playbackLibraryItemID, SucceededResumptionFixture.selectedLibraryID)
            XCTAssertEqual(selected.playbackVariantID, SucceededResumptionFixture.selectedVariantID)
            XCTAssertEqual(selected.playbackURL?.absoluteString, SucceededResumptionFixture.selectedMediaURI)
            let context = try XCTUnwrap(
                model.playbackProgressContext(for: selected, serverAddressText: endpoint.displayAddress)
            )
            let playbackIdentity = try XCTUnwrap(context.resumptionIdentity)
            XCTAssertEqual(
                playbackIdentity,
                ClientPlaybackIdentity(
                    task: taskIdentity,
                    resultID: SucceededResumptionFixture.selectedResultID,
                    libraryItemID: SucceededResumptionFixture.selectedLibraryID,
                    variantID: SucceededResumptionFixture.selectedVariantID
                )
            )
            store.beginPlayback(playbackIdentity)
            store.updatePlayback(
                identity: playbackIdentity,
                positionSeconds: 37.25,
                durationSeconds: 120,
                completed: false
            )
            let savedRecord = try XCTUnwrap(store.record)
            let savedCheckpoint = try XCTUnwrap(savedRecord.playback)
            XCTAssertEqual(savedCheckpoint.positionSeconds, 37.25)
            let beforeRestore = await probe.snapshot()
            XCTAssertEqual(beforeRestore.resolutionSources, [SucceededResumptionFixture.source])
            XCTAssertEqual(beforeRestore.createdTaskIDs, [self.taskID])
            XCTAssertEqual(beforeRestore.watchRequestIDs, [[self.taskID]])
            XCTAssertTrue(beforeRestore.getTaskIDs.isEmpty)
            XCTAssertTrue(beforeRestore.playbackSourceRequests.isEmpty)
            XCTAssertTrue(beforeRestore.cancelledTaskIDs.isEmpty)

            let reopenedStore = ClientResumptionStore(defaults: defaults)
            XCTAssertEqual(reopenedStore.record, savedRecord)
            let restored = self.makeModel(
                endpoint: endpoint,
                source: "",
                defaults: defaults,
                resumptionStore: reopenedStore
            )
            defer { restored.clearTask() }
            let restoredCached = self.expectation(description: "restored succeeded results remain cached and terminal")
            let restoredToken = self.observeCachedSucceededResults(in: restored, expectation: restoredCached)

            await restored.restoreSavedTask(serverAddressText: endpoint.displayAddress)
            let restoredWait = await XCTWaiter.fulfillment(of: [restoredCached], timeout: 20)
            XCTAssertEqual(
                restoredWait, .completed,
                "status=\(restored.statusMessage), resumption=\(restored.resumptionMessage ?? "none")"
            )
            guard restoredWait == .completed else { return }
            XCTAssertEqual(restored.currentTask?.id, self.taskID)
            XCTAssertEqual(restored.currentTask?.state, "succeeded")
            XCTAssertEqual(restored.currentTask?.resultItems.map(\.state), ["succeeded", "succeeded"])
            XCTAssertEqual(restored.statusMessage, "2 Bilibili results are cached for LAN playback.")
            XCTAssertFalse(restored.isWatching)
            XCTAssertFalse(restored.isRestoringTask)
            XCTAssertNil(restored.resumptionMessage)
            XCTAssertTrue(restored.canContinueSavedPlayback)

            let prepared = await restored.prepareSavedPlayback(serverAddressText: endpoint.displayAddress)
            let unwrappedPrepared = try XCTUnwrap(prepared, restored.resumptionMessage ?? "No prepared resumption.")
            XCTAssertEqual(unwrappedPrepared.checkpoint, savedCheckpoint)
            XCTAssertEqual(unwrappedPrepared.progressContext.resumptionIdentity, playbackIdentity)
            XCTAssertEqual(unwrappedPrepared.progressContext.endpoint, endpoint)
            XCTAssertEqual(
                unwrappedPrepared.progressContext.libraryItemID,
                SucceededResumptionFixture.selectedLibraryID
            )
            XCTAssertEqual(
                unwrappedPrepared.progressContext.variantID,
                SucceededResumptionFixture.selectedVariantID
            )
            XCTAssertEqual(unwrappedPrepared.url.absoluteString, SucceededResumptionFixture.selectedMediaURI)
            XCTAssertEqual(reopenedStore.record, savedRecord, "Preparation must not replace the saved checkpoint.")

            let afterPrepare = await probe.snapshot()
            XCTAssertEqual(afterPrepare.resolutionSources, beforeRestore.resolutionSources)
            XCTAssertEqual(afterPrepare.createdTaskIDs, beforeRestore.createdTaskIDs)
            XCTAssertEqual(afterPrepare.cancelledTaskIDs, beforeRestore.cancelledTaskIDs)
            XCTAssertEqual(afterPrepare.watchRequestIDs, beforeRestore.watchRequestIDs)
            XCTAssertEqual(afterPrepare.getTaskIDs, [self.taskID, self.taskID])
            XCTAssertEqual(
                afterPrepare.playbackSourceRequests,
                Array(
                    repeating: TaskWatchRPCProbe.PlaybackSourceRequest(
                        itemID: SucceededResumptionFixture.selectedLibraryID,
                        variantID: SucceededResumptionFixture.selectedVariantID
                    ),
                    count: 2
                )
            )
            XCTAssertGreaterThanOrEqual(
                afterPrepare.taskResultQueries.count,
                beforeRestore.taskResultQueries.count + 2
            )
            XCTAssertTrue(afterPrepare.taskResultQueries.allSatisfy { $0 == self.taskID })
            XCTAssertTrue(afterPrepare.resultOutputRevisions.allSatisfy { $0 == 42 })
            _ = [cachedToken, restoredToken]
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

    private func makeModel(
        endpoint: CacheServerEndpoint,
        source: String,
        defaults: UserDefaults = .standard,
        resumptionStore: ClientResumptionStore? = nil,
        watchReconciliationInterval: Duration = .seconds(30)
    ) -> BilibiliTaskViewModel {
        BilibiliTaskViewModel(
            sourceText: source,
            defaults: defaults,
            resumptionStore: resumptionStore,
            operationTimeout: .seconds(10),
            resolutionTimeout: .seconds(10),
            watchReconciliationInterval: watchReconciliationInterval,
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

    private func observeCachedSucceededResults(
        in model: BilibiliTaskViewModel,
        expectation: XCTestExpectation
    ) -> AnyCancellable {
        let once = TaskWatchExpectationOnce(expectation)
        return model.objectWillChange.sink {
            Task { @MainActor in
                guard model.currentTask?.state == "succeeded",
                    model.currentTask?.outputSummary?.revision == 42,
                    model.currentTask?.outputSummary?.resultCount == 2,
                    !model.isWatching,
                    !model.isLoadingMoreTaskResults,
                    model.taskResults.map(\.id) == [
                        SucceededResumptionFixture.decoyResultID,
                        SucceededResumptionFixture.selectedResultID,
                    ],
                    model.taskResults.allSatisfy({ $0.isCached && $0.isReady && $0.playbackURL != nil }),
                    model.statusMessage == "2 Bilibili results are cached for LAN playback."
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
    case silentReconciliation
    case succeededResumption
}

private enum SucceededResumptionFixture {
    static let source = "BV1xK4y1P7mN"
    static let decoyResultID = "loopback-decoy-result"
    static let selectedResultID = "loopback-selected-result"
    static let selectedLibraryID = "loopback-selected-library"
    static let selectedVariantID = "loopback-selected-mp4"
    static let mediaBaseURI = "http://127.0.0.1:8080"
    static let selectedMediaURI = "\(mediaBaseURI)/media/selected.mp4"
}

private enum TaskWatchFixturePlan: Sendable {
    case unavailableAfterProgress
    case permissionDenied
    case complete
    case remainOpen
}

private enum TaskWatchGetTaskPlan: Sendable {
    case notFound
    case running
    case terminal
}

private actor TaskWatchRPCProbe {
    struct PlaybackSourceRequest: Equatable, Sendable {
        let itemID: String
        let variantID: String
    }

    struct Snapshot: Sendable {
        let serverInfoRequests: Int
        let resolutionSources: [String]
        let createdTaskIDs: [String]
        let watchRequestIDs: [[String]]
        let getTaskIDs: [String]
        let cancelledTaskIDs: [String]
        let openWatchStreams: Int
        let closedWatchStreams: Int
        let taskResultQueries: [String]
        let resultOutputRevisions: [UInt64]
        let playbackSourceRequests: [PlaybackSourceRequest]
    }

    private enum EventKind: Hashable {
        case getTask
        case watchOpen
        case watchClose
    }

    private struct EventWaiter {
        let kind: EventKind
        let target: Int
        let continuation: CheckedContinuation<Void, Never>
    }

    nonisolated let scenario: TaskWatchFixtureScenario
    private var serverInfoRequests = 0
    private var resolutionSources: [String] = []
    private var createdTaskIDs: [String] = []
    private var watchRequestIDs: [[String]] = []
    private var getTaskIDs: [String] = []
    private var cancelledTaskIDs: [String] = []
    private var openWatchStreams = 0
    private var closedWatchStreams = 0
    private var taskResultQueries: [String] = []
    private var resultOutputRevisions: [UInt64] = []
    private var playbackSourceRequests: [PlaybackSourceRequest] = []
    private var eventWaiters: [UUID: EventWaiter] = [:]
    private var heldGetTaskAttempts: Set<Int> = []
    private var releasedGetTaskAttempts: Set<Int> = []
    private var getTaskResponseWaiters: [UUID: (attempt: Int, continuation: CheckedContinuation<Void, Never>)] = [:]

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
        case .silentReconciliation:
            return .remainOpen
        case .succeededResumption:
            return .complete
        }
    }

    func recordGetTask(id: String) -> Int {
        getTaskIDs.append(id)
        resumeEventWaiters(for: .getTask)
        return getTaskIDs.count
    }

    func getTaskPlan(attempt: Int) -> TaskWatchGetTaskPlan {
        switch scenario {
        case .notFoundAfterUnavailable:
            .notFound
        case .silentReconciliation:
            attempt >= 3 ? .terminal : .running
        case .recoverAfterUnavailable, .permissionDenied, .interruptThenComplete:
            .running
        case .succeededResumption:
            .terminal
        }
    }

    func holdGetTaskResponse(attempt: Int) {
        heldGetTaskAttempts.insert(attempt)
    }

    func releaseGetTaskResponse(attempt: Int) {
        releasedGetTaskAttempts.insert(attempt)
        let readyIDs = getTaskResponseWaiters.compactMap { id, waiter in
            waiter.attempt == attempt ? id : nil
        }
        for id in readyIDs {
            getTaskResponseWaiters.removeValue(forKey: id)?.continuation.resume()
        }
    }

    func waitForGetTaskResponseRelease(attempt: Int) async {
        guard heldGetTaskAttempts.contains(attempt), !releasedGetTaskAttempts.contains(attempt) else {
            return
        }

        let id = UUID()
        await withTaskCancellationHandler {
            await withCheckedContinuation { continuation in
                guard !Task.isCancelled, !releasedGetTaskAttempts.contains(attempt) else {
                    continuation.resume()
                    return
                }
                getTaskResponseWaiters[id] = (attempt, continuation)
            }
        } onCancel: {
            Task { await self.cancelGetTaskResponseWaiter(id) }
        }
    }

    func recordCancellation(id: String) {
        cancelledTaskIDs.append(id)
    }

    func recordWatchOpened() {
        openWatchStreams += 1
        resumeEventWaiters(for: .watchOpen)
    }

    func recordWatchClosed() {
        closedWatchStreams += 1
        resumeEventWaiters(for: .watchClose)
    }

    func waitForGetTaskCount(_ count: Int) async {
        await waitForEvent(.getTask, count: count)
    }

    func waitForWatchOpenCount(_ count: Int) async {
        await waitForEvent(.watchOpen, count: count)
    }

    func waitForWatchCloseCount(_ count: Int) async {
        await waitForEvent(.watchClose, count: count)
    }

    private func waitForEvent(_ kind: EventKind, count: Int) async {
        guard eventCount(for: kind) < count else {
            return
        }

        let id = UUID()
        await withTaskCancellationHandler {
            await withCheckedContinuation { continuation in
                guard !Task.isCancelled, eventCount(for: kind) < count else {
                    continuation.resume()
                    return
                }
                eventWaiters[id] = EventWaiter(kind: kind, target: count, continuation: continuation)
            }
        } onCancel: {
            Task { await self.cancelEventWaiter(id) }
        }
    }

    private func eventCount(for kind: EventKind) -> Int {
        switch kind {
        case .getTask:
            getTaskIDs.count
        case .watchOpen:
            openWatchStreams
        case .watchClose:
            closedWatchStreams
        }
    }

    private func resumeEventWaiters(for kind: EventKind) {
        let reached = eventCount(for: kind)
        let readyIDs = eventWaiters.compactMap { id, waiter in
            waiter.kind == kind && waiter.target <= reached ? id : nil
        }
        for id in readyIDs {
            eventWaiters.removeValue(forKey: id)?.continuation.resume()
        }
    }

    private func cancelEventWaiter(_ id: UUID) {
        eventWaiters.removeValue(forKey: id)?.continuation.resume()
    }

    private func cancelGetTaskResponseWaiter(_ id: UUID) {
        getTaskResponseWaiters.removeValue(forKey: id)?.continuation.resume()
    }

    func recordTaskResults(id: String, outputRevision: UInt64) {
        taskResultQueries.append(id)
        resultOutputRevisions.append(outputRevision)
    }

    func recordPlaybackSource(itemID: String, variantID: String) {
        playbackSourceRequests.append(PlaybackSourceRequest(itemID: itemID, variantID: variantID))
    }

    func snapshot() -> Snapshot {
        Snapshot(
            serverInfoRequests: serverInfoRequests,
            resolutionSources: resolutionSources,
            createdTaskIDs: createdTaskIDs,
            watchRequestIDs: watchRequestIDs,
            getTaskIDs: getTaskIDs,
            cancelledTaskIDs: cancelledTaskIDs,
            openWatchStreams: openWatchStreams,
            closedWatchStreams: closedWatchStreams,
            taskResultQueries: taskResultQueries,
            resultOutputRevisions: resultOutputRevisions,
            playbackSourceRequests: playbackSourceRequests
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
        let succeededResumption = probe.scenario == .succeededResumption
        Self.registerUnary(
            TvosNetPlayer_V1_ServerService.Method.GetServerInfo.descriptor,
            with: &router
        ) { (_: TvosNetPlayer_V1_GetServerInfoRequest) in
            await probe.recordServerInfo()
            var info = TvosNetPlayer_V1_ServerInfo()
            info.id = "loopback-test-server"
            info.name = "Loopback test server"
            info.version = "test"
            if succeededResumption {
                info.mediaBaseUris = [SucceededResumptionFixture.mediaBaseURI]
            }
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
            return Self.task(state: .preparing, progress: 0.1, succeededResumption: succeededResumption)
        }
        Self.registerUnary(
            TvosNetPlayer_V1_TaskService.Method.GetTask.descriptor,
            with: &router
        ) { (request: TvosNetPlayer_V1_GetTaskRequest) in
            let attempt = await probe.recordGetTask(id: request.id)
            await probe.waitForGetTaskResponseRelease(attempt: attempt)
            switch await probe.getTaskPlan(attempt: attempt) {
            case .notFound:
                throw RPCError(code: .notFound, message: "Synthetic task not found.")
            case .running:
                return Self.task(state: .running, progress: 0.65)
            case .terminal:
                return Self.task(
                    state: .succeeded,
                    progress: 1,
                    outputRevision: 42,
                    succeededResumption: succeededResumption
                )
            }
        }
        Self.registerUnary(
            TvosNetPlayer_V1_TaskService.Method.CancelTask.descriptor,
            with: &router
        ) { (request: TvosNetPlayer_V1_CancelTaskRequest) in
            await probe.recordCancellation(id: request.id)
            return Self.task(state: .running, progress: 0.65)
        }
        Self.registerUnary(
            TvosNetPlayer_V1_TaskService.Method.ListTaskResults.descriptor,
            with: &router
        ) { (request: TvosNetPlayer_V1_ListTaskResultsRequest) in
            await probe.recordTaskResults(id: request.taskID, outputRevision: 42)
            var response = TvosNetPlayer_V1_ListTaskResultsResponse()
            response.results =
                succeededResumption
                ? [Self.resumptionResult(isDecoy: true), Self.resumptionResult(isDecoy: false)]
                : [Self.result()]
            response.outputRevision = 42
            response.pageInfo.totalSize = succeededResumption ? 2 : 1
            response.pageInfo.snapshotID = "loopback-results-revision-42"
            return response
        }
        Self.registerUnary(
            TvosNetPlayer_V1_LibraryService.Method.GetPlaybackSource.descriptor,
            with: &router
        ) { (request: TvosNetPlayer_V1_GetPlaybackSourceRequest) in
            await probe.recordPlaybackSource(itemID: request.itemID, variantID: request.variantID)
            guard succeededResumption,
                request.itemID == SucceededResumptionFixture.selectedLibraryID,
                request.variantID == SucceededResumptionFixture.selectedVariantID
            else {
                throw RPCError(code: .notFound, message: "The exact selected playback source was not requested.")
            }
            return Self.resumptionPlaybackSource(isDecoy: false)
        }

        router.registerHandler(
            forMethod: TvosNetPlayer_V1_TaskService.Method.WatchTasks.descriptor,
            deserializer: ProtobufDeserializer<TvosNetPlayer_V1_WatchTasksRequest>(),
            serializer: ProtobufSerializer<TvosNetPlayer_V1_TaskEvent>()
        ) { request, context in
            let rpcCancellation = context.cancellation
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
                                outputRevision: 42,
                                succeededResumption: succeededResumption
                            )))
                    return [:]
                case .remainOpen:
                    await probe.recordWatchOpened()
                    do {
                        try await rpcCancellation.cancelled
                    } catch {
                        await probe.recordWatchClosed()
                        throw error
                    }
                    await probe.recordWatchClosed()
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
        outputRevision: UInt64? = nil,
        succeededResumption: Bool = false
    ) -> TvosNetPlayer_V1_Task {
        var task = TvosNetPlayer_V1_Task()
        task.id = "loopback-task-1"
        task.kind = .bilibiliProgressivePlayback
        task.state = state
        task.source = succeededResumption ? SucceededResumptionFixture.source : "BV1loopback"
        task.title = "Loopback video"
        task.progress = progress
        task.message = succeededResumption ? "Relaunch resumption fixture complete" : "Synthetic task state."
        if let outputRevision {
            var summary = TvosNetPlayer_V1_TaskOutputSummary()
            summary.revision = outputRevision
            summary.resultCount = succeededResumption ? 2 : 1
            summary.terminalResultCount = succeededResumption ? 2 : 1
            summary.successfulResultCount = succeededResumption ? 2 : 1
            summary.primaryResultID =
                succeededResumption
                ? SucceededResumptionFixture.decoyResultID : "loopback-result-1"
            task.outputSummary = summary
            if succeededResumption {
                task.resultItems = [resumptionResultItem(isDecoy: true), resumptionResultItem(isDecoy: false)]
            }
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

    private static func resumptionResultItem(isDecoy: Bool) -> TvosNetPlayer_V1_BilibiliTaskResultItem {
        let source = resumptionPlaybackSource(isDecoy: isDecoy)
        var item = TvosNetPlayer_V1_BilibiliTaskResultItem()
        item.id = isDecoy ? SucceededResumptionFixture.decoyResultID : SucceededResumptionFixture.selectedResultID
        item.selectionID = "candidate-1"
        item.title = isDecoy ? "Decoy result" : "Selected result"
        item.sourceKind = "video"
        item.contentID = SucceededResumptionFixture.source
        item.index = isDecoy ? 1 : 2
        item.state = .succeeded
        item.libraryItemID = source.itemID
        item.playbackSource = source
        return item
    }

    private static func resumptionResult(isDecoy: Bool) -> TvosNetPlayer_V1_TaskResult {
        let item = resumptionResultItem(isDecoy: isDecoy)
        var result = TvosNetPlayer_V1_TaskResult()
        result.id = item.id
        result.state = .succeeded
        result.title = item.title
        result.libraryItemID = item.libraryItemID
        result.playbackSource = item.playbackSource
        return result
    }

    private static func resumptionPlaybackSource(isDecoy: Bool) -> TvosNetPlayer_V1_PlaybackSource {
        var source = TvosNetPlayer_V1_PlaybackSource()
        source.itemID = isDecoy ? "loopback-decoy-library" : SucceededResumptionFixture.selectedLibraryID
        source.variantID = isDecoy ? "loopback-decoy-mp4" : SucceededResumptionFixture.selectedVariantID
        source.`protocol` = .httpFile
        source.uri =
            isDecoy
            ? "\(SucceededResumptionFixture.mediaBaseURI)/media/decoy.mp4"
            : SucceededResumptionFixture.selectedMediaURI
        return source
    }
}
