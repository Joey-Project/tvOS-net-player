import AVFoundation
import XCTest
import TVOSNetPlayerCacheClient
@testable import TVOSNetPlayerCore

@MainActor
final class HLSPlaybackContextReportingServiceTests: XCTestCase {
    private let contextA = "00000000-0000-4000-8000-000000000011"
    private let contextB = "00000000-0000-4000-8000-000000000012"

    func testUnsupportedOrMissingOwnerProducesZeroActions() async throws {
        let client = PlaybackContextClientProbe()
        let now: UInt64 = 100
        var snapshotCalls = 0
        let owner = try makeOwner(sessionID: "session-A", generation: 8)

        let unsupported = makeService(
            client: client,
            summary: serverSummary(supportsContext: false),
            ownerProvider: { owner },
            snapshotProvider: {
                snapshotCalls += 1
                return self.snapshot(observedAt: now)
            },
            now: { now }
        )
        do {
            try await unsupported.start()
            XCTFail("Expected an unsupported server to be rejected.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .unsupportedServer)
        }

        let noOwner = makeService(
            client: client,
            ownerProvider: { nil },
            snapshotProvider: {
                snapshotCalls += 1
                return self.snapshot(observedAt: now)
            },
            now: { now }
        )
        do {
            try await noOwner.start()
            XCTFail("Expected a missing owner to be rejected.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .noCurrentOwner)
        }

        let counts = await client.counts()
        XCTAssertEqual(counts.openCount, 0)
        XCTAssertEqual(counts.updateCount, 0)
        XCTAssertEqual(counts.closeCount, 0)
        XCTAssertEqual(snapshotCalls, 0)
    }

    func testFreshWithdrawalIsNotRequiredAndDoesNotBlockLaterStart() async throws {
        let client = PlaybackContextClientProbe()
        let owner = try makeOwner(sessionID: "session-A", generation: 8)
        let service = makeService(
            client: client,
            ownerProvider: { owner },
            snapshotProvider: { self.snapshot(observedAt: 100) },
            now: { 100 }
        )

        let noContextOutcome = await service.withdrawAndConfirmRemoteRemoval()
        XCTAssertEqual(noContextOutcome, .notRequired)
        try await service.start()
        let counts = await client.counts()
        XCTAssertEqual(counts.openCount, 1)
        XCTAssertEqual(counts.closeCount, 0)
    }

    func testLegacyCloseReceiptIsAcknowledgedRatherThanReportedNotRequired() async throws {
        let client = PlaybackContextClientProbe()
        let owner = try makeOwner(sessionID: "session-A", generation: 8)
        let service = makeService(
            client: client,
            ownerProvider: { owner },
            snapshotProvider: { self.snapshot(observedAt: 100) },
            now: { 100 }
        )
        try await service.start()
        await service.withdraw()

        let outcome = await service.withdrawAndConfirmRemoteRemoval()
        XCTAssertEqual(outcome, .acknowledged)
    }

    func testDefaultReporterClockSharesContinuousSnapshotEpoch() async throws {
        let client = PlaybackContextClientProbe()
        let owner = try makeOwner(sessionID: "session-A", generation: 8)
        let service = HLSPlaybackContextReportingService(
            client: client,
            serverSummary: serverSummary(supportsContext: true),
            ownerProvider: { owner },
            snapshotProvider: {
                guard let now = HLSPlaybackMonotonicClock.shared.nowNanoseconds() else { return nil }
                return self.snapshot(observedAt: now)
            }
        )

        try await service.start()
        let outcome = await service.withdrawAndConfirmRemoteRemoval()
        XCTAssertEqual(outcome, .acknowledged)
    }

    func testMissingReporterClockFailsClosedBeforeOpening() async throws {
        let client = PlaybackContextClientProbe()
        let owner = try makeOwner(sessionID: "session-A", generation: 8)
        let service = HLSPlaybackContextReportingService(
            client: client,
            serverSummary: serverSummary(supportsContext: true),
            ownerProvider: { owner },
            snapshotProvider: { self.snapshot(observedAt: 100) },
            monotonicNowNanosecondsProvider: { nil }
        )

        do {
            try await service.start()
            XCTFail("Expected a missing clock reading to fail closed.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .clockAnomaly)
        }
        let counts = await client.counts()
        XCTAssertEqual(counts.openCount, 0)
        let outcome = await service.withdrawAndConfirmRemoteRemoval()
        XCTAssertEqual(outcome, .notRequired)
    }

    func testLostOpenWithoutTicketIsUnconfirmedAndBlocksAnotherOpen() async throws {
        let client = PlaybackContextClientProbe()
        let owner = try makeOwner(sessionID: "session-A", generation: 8)
        let service = makeService(
            client: client,
            ownerProvider: { owner },
            snapshotProvider: { self.snapshot(observedAt: 100) },
            now: { 100 }
        )
        await client.failNextOpen()

        do {
            try await service.start()
            XCTFail("Expected the simulated lost Open response.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .requestFailed)
        }

        let outcome = await service.withdrawAndConfirmRemoteRemoval()
        XCTAssertEqual(outcome, .unconfirmed(.openOutcomeUnknown))
        do {
            try await service.start()
            XCTFail("An unknown remote context must block another Open.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .operationInProgress)
        }
        let counts = await client.counts()
        XCTAssertEqual(counts.openCount, 1)
        XCTAssertEqual(counts.closeCount, 0)
    }

    func testLostOpenAfterLegacyWithdrawalStaysUnknownWithoutBlockingUntilConfirmation() async throws {
        let client = PlaybackContextClientProbe()
        let ownerA = try makeOwner(sessionID: "session-A", generation: 8)
        let ownerB = try makeOwner(sessionID: "session-B", generation: 11)
        var currentOwner: HLSPlaybackContextOwner? = ownerA
        let service = makeService(
            client: client,
            ownerProvider: { currentOwner },
            snapshotProvider: { self.snapshot(observedAt: 100) },
            now: { 100 }
        )
        await client.holdOpen(for: ownerA.sessionID)

        let startA = Task { @MainActor in try await service.start() }
        await client.waitForOpenCount(1)
        await service.withdraw()
        await client.failHeldOpen(for: ownerA.sessionID)
        await client.releaseOpen(for: ownerA.sessionID)

        do {
            try await startA.value
            XCTFail("Expected the withdrawn Open to lose its response.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .requestFailed)
        }

        currentOwner = ownerB
        try await service.start()
        let outcome = await service.withdrawAndConfirmRemoteRemoval()
        XCTAssertEqual(outcome, .unconfirmed(.openOutcomeUnknown))
        do {
            try await service.start()
            XCTFail("Explicit confirmation must retain the unresolved Open evidence.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .operationInProgress)
        }
        let counts = await client.counts()
        XCTAssertEqual(counts.closeContextIDs, [contextB])
    }

    func testLateLegacyOpenCloseAcknowledgmentRecoversInEitherCloseOrder() async throws {
        for closeBFirst in [true, false] {
            let client = PlaybackContextClientProbe()
            let ownerA = try makeOwner(sessionID: "session-A", generation: 8)
            let ownerB = try makeOwner(sessionID: "session-B", generation: 11)
            var currentOwner: HLSPlaybackContextOwner? = ownerA
            let service = makeService(
                client: client,
                ownerProvider: { currentOwner },
                snapshotProvider: { self.snapshot(observedAt: 100) },
                now: { 100 }
            )
            await client.holdOpen(for: ownerA.sessionID)

            let startA = Task { @MainActor in try await service.start() }
            await client.waitForOpenCount(1)
            await service.withdraw()
            currentOwner = ownerB
            try await service.start()

            if closeBFirst {
                let closeB = await service.withdrawAndConfirmRemoteRemoval()
                XCTAssertEqual(closeB, .unconfirmed(.openOutcomeUnknown))
                await client.releaseOpen(for: ownerA.sessionID)
                do {
                    try await startA.value
                    XCTFail("The withdrawn A Open cannot restore local authority.")
                } catch let error as HLSPlaybackContextReportingError {
                    XCTAssertEqual(error, .ownerChanged)
                }
            } else {
                await client.releaseOpen(for: ownerA.sessionID)
                do {
                    try await startA.value
                    XCTFail("The withdrawn A Open cannot restore local authority.")
                } catch let error as HLSPlaybackContextReportingError {
                    XCTAssertEqual(error, .ownerChanged)
                }
                let closeB = await service.withdrawAndConfirmRemoteRemoval()
                XCTAssertEqual(closeB, .acknowledged)
            }

            let countsBeforeRecovery = await client.counts()
            XCTAssertEqual(
                countsBeforeRecovery.closeContextIDs,
                closeBFirst ? [contextB, contextA] : [contextA, contextB]
            )
            if closeBFirst {
                let recovered = await service.withdrawAndConfirmRemoteRemoval()
                XCTAssertEqual(recovered, .acknowledged)
            }

            currentOwner = ownerA
            try await service.start()
            let counts = await client.counts()
            XCTAssertEqual(counts.openSessionIDs, [ownerA.sessionID, ownerB.sessionID, ownerA.sessionID])
        }
    }

    func testLateOpenCloseAcknowledgmentDoesNotClearAnotherLostOpen() async throws {
        let client = PlaybackContextClientProbe()
        let ownerA = try makeOwner(sessionID: "session-A", generation: 8)
        let ownerB = try makeOwner(sessionID: "session-B", generation: 11)
        var currentOwner: HLSPlaybackContextOwner? = ownerA
        let service = makeService(
            client: client,
            ownerProvider: { currentOwner },
            snapshotProvider: { self.snapshot(observedAt: 100) },
            now: { 100 }
        )
        await client.holdOpen(for: ownerA.sessionID)
        await client.holdOpen(for: ownerB.sessionID)

        let startA = Task { @MainActor in try await service.start() }
        await client.waitForOpenCount(1)
        await service.withdraw()
        currentOwner = ownerB
        let startB = Task { @MainActor in try await service.start() }
        await client.waitForOpenCount(2)

        let firstOutcome = await service.withdrawAndConfirmRemoteRemoval()
        XCTAssertEqual(firstOutcome, .unconfirmed(.openInFlightWithoutTicket))
        await client.failHeldOpen(for: ownerB.sessionID)
        await client.releaseOpen(for: ownerB.sessionID)
        do {
            try await startB.value
            XCTFail("B's lost Open response must remain unknown.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .requestFailed)
        }

        await client.releaseOpen(for: ownerA.sessionID)
        do {
            try await startA.value
            XCTFail("The withdrawn A Open cannot restore local authority.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .ownerChanged)
        }

        let outcome = await service.withdrawAndConfirmRemoteRemoval()
        XCTAssertEqual(outcome, .unconfirmed(.openOutcomeUnknown))
        do {
            try await service.start()
            XCTFail("A's acknowledged close cannot resolve B's lost Open.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .operationInProgress)
        }
        let counts = await client.counts()
        XCTAssertEqual(counts.closeContextIDs, [contextA])
    }

    func testLateOpenCloseAcknowledgmentDoesNotClearBKnownCloseDebtInABAOrder() async throws {
        let client = PlaybackContextClientProbe()
        let ownerA = try makeOwner(sessionID: "session-A", generation: 8)
        let ownerB = try makeOwner(sessionID: "session-B", generation: 11)
        var currentOwner: HLSPlaybackContextOwner? = ownerA
        let service = makeService(
            client: client,
            ownerProvider: { currentOwner },
            snapshotProvider: { self.snapshot(observedAt: 100) },
            now: { 100 }
        )
        await client.holdOpen(for: ownerA.sessionID)

        let startA = Task { @MainActor in try await service.start() }
        await client.waitForOpenCount(1)
        await service.withdraw()
        currentOwner = ownerB
        try await service.start()
        await client.setNextCloseResponse(false)

        let failedCloseB = await service.withdrawAndConfirmRemoteRemoval()
        XCTAssertEqual(failedCloseB, .unconfirmed(.closeRejected))
        await client.releaseOpen(for: ownerA.sessionID)
        do {
            try await startA.value
            XCTFail("The withdrawn A Open cannot restore local authority.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .ownerChanged)
        }

        currentOwner = ownerA
        do {
            try await service.start()
            XCTFail("A's acknowledged close cannot discard B's known close debt.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .operationInProgress)
        }
        let retriedCloseB = await service.withdrawAndConfirmRemoteRemoval()
        XCTAssertEqual(retriedCloseB, .acknowledged)
        try await service.start()

        let counts = await client.counts()
        XCTAssertEqual(counts.closeContextIDs, [contextB, contextA, contextB])
        XCTAssertEqual(counts.openSessionIDs, [ownerA.sessionID, ownerB.sessionID, ownerA.sessionID])
    }

    func testStrictWithdrawalKeepsLostOpenUnknownAfterLateFailure() async throws {
        let client = PlaybackContextClientProbe()
        let owner = try makeOwner(sessionID: "session-A", generation: 8)
        let service = makeService(
            client: client,
            ownerProvider: { owner },
            snapshotProvider: { self.snapshot(observedAt: 100) },
            now: { 100 }
        )
        await client.holdOpen(for: owner.sessionID)

        let start = Task { @MainActor in try await service.start() }
        await client.waitForOpenCount(1)
        let firstOutcome = await service.withdrawAndConfirmRemoteRemoval()
        XCTAssertEqual(firstOutcome, .unconfirmed(.openInFlightWithoutTicket))
        await client.failHeldOpen(for: owner.sessionID)
        await client.releaseOpen(for: owner.sessionID)

        do {
            try await start.value
            XCTFail("Expected the retired Open to lose its response.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .requestFailed)
        }
        let lateOutcome = await service.withdrawAndConfirmRemoteRemoval()
        XCTAssertEqual(lateOutcome, .unconfirmed(.openOutcomeUnknown))
        let counts = await client.counts()
        XCTAssertEqual(counts.closeCount, 0)
    }

    func testWithdrawalTracksInitialUpdateBeforeAwaitWhileNoContextIsActive() async throws {
        let client = PlaybackContextClientProbe()
        let owner = try makeOwner(sessionID: "session-A", generation: 8)
        let service = makeService(
            client: client,
            ownerProvider: { owner },
            snapshotProvider: { self.snapshot(observedAt: 100) },
            now: { 100 }
        )
        await client.holdNextUpdate()

        let start = Task { @MainActor in try await service.start() }
        await client.waitForUpdateCount(1)
        let withdrawal = await service.withdrawAndConfirmRemoteRemoval()
        XCTAssertEqual(withdrawal, .acknowledged)
        await client.releaseHeldUpdate()

        do {
            try await start.value
            XCTFail("The late initial Update must not restore local authority.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .ownerChanged)
        }
        let counts = await client.counts()
        XCTAssertEqual(counts.closeContextIDs, [contextA])
        XCTAssertEqual(counts.updateCount, 1)
        XCTAssertEqual(counts.openCount, 1)
    }

    func testUpdateResponseBeforeCloseStillSharesOneRetirementBarrier() async throws {
        let client = PlaybackContextClientProbe()
        let owner = try makeOwner(sessionID: "session-A", generation: 8)
        let service = makeService(
            client: client,
            ownerProvider: { owner },
            snapshotProvider: { self.snapshot(observedAt: 100) },
            now: { 100 }
        )
        await client.holdNextUpdate()
        await client.holdClose(for: contextA)

        let start = Task { @MainActor in try await service.start() }
        await client.waitForUpdateCount(1)
        let withdrawal = Task { @MainActor in await service.withdrawAndConfirmRemoteRemoval() }
        let closeReachedClient = await client.waitForCloseCount(1, timeout: .seconds(2))
        XCTAssertTrue(closeReachedClient)
        await client.releaseHeldUpdate()
        await client.waitForCompletedUpdateCount(1)
        await client.releaseClose(for: contextA)

        let withdrawalOutcome = await withdrawal.value
        XCTAssertEqual(withdrawalOutcome, .acknowledged)
        do {
            try await start.value
            XCTFail("The Update response cannot commit after withdrawal.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .ownerChanged)
        }
        let counts = await client.counts()
        XCTAssertEqual(counts.closeContextIDs, [contextA])
    }

    func testAcknowledgedLegacyUpdateDoesNotPoisonBRegardlessOfCompletionOrder() async throws {
        for completeUpdateBeforeBWithdrawal in [true, false] {
            let client = PlaybackContextClientProbe()
            let ownerA = try makeOwner(sessionID: "session-A", generation: 8)
            let ownerB = try makeOwner(sessionID: "session-B", generation: 11)
            var currentOwner: HLSPlaybackContextOwner? = ownerA
            let service = makeService(
                client: client,
                ownerProvider: { currentOwner },
                snapshotProvider: { self.snapshot(observedAt: 100) },
                now: { 100 }
            )
            try await service.start()
            await client.holdNextUpdate()

            let updateA = Task { @MainActor in try await service.update() }
            await client.waitForUpdateCount(2)
            let closeA = await service.withdrawAndConfirmRemoteRemoval()
            XCTAssertEqual(closeA, .acknowledged)

            currentOwner = ownerB
            try await service.start()
            if completeUpdateBeforeBWithdrawal {
                await client.releaseHeldUpdate()
                do {
                    try await updateA.value
                    XCTFail("The retired Update cannot commit after its Close acknowledgment.")
                } catch let error as HLSPlaybackContextReportingError {
                    XCTAssertEqual(error, .ownerChanged)
                }
            }

            let closeB = await service.withdrawAndConfirmRemoteRemoval()
            XCTAssertEqual(closeB, .acknowledged)
            if !completeUpdateBeforeBWithdrawal {
                await client.releaseHeldUpdate()
                do {
                    try await updateA.value
                    XCTFail("The retired Update cannot commit after B is closed.")
                } catch let error as HLSPlaybackContextReportingError {
                    XCTAssertEqual(error, .ownerChanged)
                }
            }

            try await service.start()
            let counts = await client.counts()
            XCTAssertEqual(counts.closeContextIDs, [contextA, contextB])
            XCTAssertEqual(counts.openSessionIDs, [ownerA.sessionID, ownerB.sessionID, ownerB.sessionID])
        }
    }

    func testRepeatedWithdrawalsCoalesceOneCloseRPC() async throws {
        let client = PlaybackContextClientProbe()
        let owner = try makeOwner(sessionID: "session-A", generation: 8)
        let service = makeService(
            client: client,
            ownerProvider: { owner },
            snapshotProvider: { self.snapshot(observedAt: 100) },
            now: { 100 }
        )
        try await service.start()
        await client.holdClose(for: contextA)

        let first = Task { @MainActor in await service.withdrawAndConfirmRemoteRemoval() }
        let closeReachedClient = await client.waitForCloseCount(1, timeout: .seconds(2))
        XCTAssertTrue(closeReachedClient)
        let second = Task { @MainActor in await service.withdrawAndConfirmRemoteRemoval() }
        try await Task.sleep(for: .milliseconds(20))
        let countsWhileHeld = await client.counts()
        XCTAssertEqual(countsWhileHeld.closeCount, 1)
        await client.releaseClose(for: contextA)

        let firstOutcome = await first.value
        let secondOutcome = await second.value
        XCTAssertEqual(firstOutcome, .acknowledged)
        XCTAssertEqual(secondOutcome, .acknowledged)
        let finalCounts = await client.counts()
        XCTAssertEqual(finalCounts.closeCount, 1)
    }

    func testFalseAndMalformedCloseResponsesNeverAcknowledgeOrAutoRetry() async throws {
        let falseClient = PlaybackContextClientProbe()
        let owner = try makeOwner(sessionID: "session-A", generation: 8)
        let falseService = makeService(
            client: falseClient,
            ownerProvider: { owner },
            snapshotProvider: { self.snapshot(observedAt: 100) },
            now: { 100 }
        )
        try await falseService.start()
        await falseClient.setNextCloseResponse(false)
        let falseOutcome = await falseService.withdrawAndConfirmRemoteRemoval()
        XCTAssertEqual(falseOutcome, .unconfirmed(.closeRejected))
        let falseCounts = await falseClient.counts()
        XCTAssertEqual(falseCounts.closeCount, 1)

        let retryOutcome = await falseService.withdrawAndConfirmRemoteRemoval()
        XCTAssertEqual(retryOutcome, .acknowledged)
        let retryCounts = await falseClient.counts()
        XCTAssertEqual(retryCounts.closeCount, 2)

        let malformedClient = PlaybackContextClientProbe()
        let malformedService = makeService(
            client: malformedClient,
            ownerProvider: { owner },
            snapshotProvider: { self.snapshot(observedAt: 100) },
            now: { 100 }
        )
        try await malformedService.start()
        await malformedClient.failNextClose()
        let malformedOutcome = await malformedService.withdrawAndConfirmRemoteRemoval()
        XCTAssertEqual(malformedOutcome, .unconfirmed(.closeRequestFailed))
        let malformedCounts = await malformedClient.counts()
        XCTAssertEqual(malformedCounts.closeCount, 1)
    }

    func testCancelledConfirmationIsUnconfirmedWhileCloseContinues() async throws {
        let client = PlaybackContextClientProbe()
        let owner = try makeOwner(sessionID: "session-A", generation: 8)
        let service = makeService(
            client: client,
            ownerProvider: { owner },
            snapshotProvider: { self.snapshot(observedAt: 100) },
            now: { 100 }
        )
        try await service.start()
        await client.holdClose(for: contextA)

        let withdrawal = Task { @MainActor in await service.withdrawAndConfirmRemoteRemoval() }
        let closeReachedClient = await client.waitForCloseCount(1, timeout: .seconds(2))
        XCTAssertTrue(closeReachedClient)
        withdrawal.cancel()
        let cancelledOutcome = await withdrawal.value
        XCTAssertEqual(cancelledOutcome, .unconfirmed(.cancelled))
        do {
            try await service.start()
            XCTFail("Cancellation cannot release an unresolved explicit barrier.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .operationInProgress)
        }

        await client.releaseClose(for: contextA)
        var lateOutcome = await service.withdrawAndConfirmRemoteRemoval()
        for _ in 0..<100 where lateOutcome != .acknowledged {
            try await Task.sleep(for: .milliseconds(10))
            lateOutcome = await service.withdrawAndConfirmRemoteRemoval()
        }
        XCTAssertEqual(lateOutcome, .acknowledged)
        await client.waitForCompletedCloseCount(1)
    }

    func testStartOpensBeforeCapturingAndSendsOneExplicitInitialSample() async throws {
        let client = PlaybackContextClientProbe()
        let owner = try makeOwner(sessionID: "session-A", generation: 8)
        let now: UInt64 = 10_000
        var snapshotCalls = 0
        let service = makeService(
            client: client,
            ownerProvider: { owner },
            snapshotProvider: {
                snapshotCalls += 1
                return self.snapshot(observedAt: now)
            },
            now: { now }
        )

        try await service.start()

        let counts = await client.counts()
        XCTAssertEqual(counts.openCount, 1)
        XCTAssertEqual(counts.updateCount, 1)
        XCTAssertEqual(counts.closeCount, 0)
        XCTAssertEqual(counts.openHTTPReaderIDs, [nil])
        XCTAssertEqual(counts.updateTicketHTTPReaderIDs, [nil])
        XCTAssertEqual(snapshotCalls, 1)
        XCTAssertEqual(counts.openSessionIDs, [owner.sessionID])
        XCTAssertEqual(counts.updateContextIDs, [contextA])
        XCTAssertEqual(counts.updateSequences, [1])
        XCTAssertEqual(counts.updateSamples.count, 1)
        XCTAssertEqual(counts.updateSamples[0].playheadSeconds, 5, accuracy: 0.01)
        XCTAssertGreaterThanOrEqual(counts.updateSamples[0].continuousBufferedEndSeconds, 5)
        XCTAssertEqual(counts.updateSamples[0].playbackRate, 1)
    }

    func testCapability22KeepsOwnerReaderIDAcrossContextRotationsAndOwnerChanges() async throws {
        let ownerTokenA = try XCTUnwrap(UUID(uuidString: "00000000-0000-4000-8000-0000000000a1"))
        let ownerTokenB = try XCTUnwrap(UUID(uuidString: "00000000-0000-4000-8000-0000000000a2"))
        let ownerA = try makeOwner(sessionID: "session-A", generation: 8, ownerToken: ownerTokenA)
        let ownerB = try makeOwner(sessionID: "session-B", generation: 11, ownerToken: ownerTokenB)
        var currentOwner: HLSPlaybackContextOwner? = ownerA
        let now: UInt64 = 10_000
        let client = PlaybackContextClientProbe()
        let service = makeService(
            client: client,
            summary: serverSummary(supportsContext: true, supportsReaderBinding: true),
            ownerProvider: { currentOwner },
            snapshotProvider: { self.snapshot(observedAt: now) },
            now: { now }
        )

        try await service.start()
        try await service.update()
        try await service.start()
        currentOwner = ownerB
        try await service.start()

        let counts = await client.counts()
        let readerA = ownerTokenA.uuidString.lowercased()
        let readerB = ownerTokenB.uuidString.lowercased()
        XCTAssertEqual(counts.openHTTPReaderIDs, [readerA, readerA, readerB])
        XCTAssertEqual(counts.updateTicketHTTPReaderIDs, [readerA, readerA, readerA, readerB])
    }

    func testCapability21KeepsExplicitReportsUnbound() async throws {
        let owner = try makeOwner(
            sessionID: "session-A",
            generation: 8,
            ownerToken: try XCTUnwrap(UUID(uuidString: "00000000-0000-4000-8000-0000000000a1"))
        )
        let client = PlaybackContextClientProbe()
        let now: UInt64 = 10_000
        let service = makeService(
            client: client,
            summary: serverSummary(supportsContext: true),
            ownerProvider: { owner },
            snapshotProvider: { self.snapshot(observedAt: now) },
            now: { now }
        )

        try await service.start()

        let counts = await client.counts()
        XCTAssertEqual(counts.openHTTPReaderIDs, [nil])
        XCTAssertEqual(counts.updateTicketHTTPReaderIDs, [nil])
    }

    func testOpenTicketIsAgedFromRequestStartAndClockRollbackIsRejected() async throws {
        let owner = try makeOwner(sessionID: "session-A", generation: 8)
        let client = PlaybackContextClientProbe()
        await client.holdOpen(for: owner.sessionID)
        let clock = PlaybackContextTestClock(value: 100)
        var snapshotCalls = 0
        let service = makeService(
            client: client,
            ownerProvider: { owner },
            snapshotProvider: {
                snapshotCalls += 1
                return self.snapshot(observedAt: clock.value)
            },
            now: { clock.value }
        )

        let task = Task { @MainActor in try await service.start() }
        await client.waitForOpenCount(1)
        clock.value += 5_000_000_000
        await client.releaseOpen(for: owner.sessionID)
        do {
            try await task.value
            XCTFail("Expected a ticket expired from request start to be rejected.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .staleTicket)
        }

        let expiredCounts = await client.counts()
        XCTAssertEqual(snapshotCalls, 0)
        XCTAssertEqual(expiredCounts.updateCount, 0)
        XCTAssertEqual(expiredCounts.closeContextIDs, [contextA])

        let rollbackClient = PlaybackContextClientProbe()
        await rollbackClient.holdOpen(for: owner.sessionID)
        clock.value = 500
        let rollbackService = makeService(
            client: rollbackClient,
            ownerProvider: { owner },
            snapshotProvider: {
                snapshotCalls += 1
                return self.snapshot(observedAt: clock.value)
            },
            now: { clock.value }
        )
        let rollbackTask = Task { @MainActor in try await rollbackService.start() }
        await rollbackClient.waitForOpenCount(1)
        clock.value = 499
        await rollbackClient.releaseOpen(for: owner.sessionID)
        do {
            try await rollbackTask.value
            XCTFail("Expected a monotonic clock rollback to be rejected.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .clockAnomaly)
        }
        let rollbackCounts = await rollbackClient.counts()
        XCTAssertEqual(rollbackCounts.updateCount, 0)
        XCTAssertEqual(rollbackCounts.closeContextIDs, [contextA])
    }

    func testStartRejectsSnapshotCapturedBeforeTicketReceiptEvenWhenFresh() async throws {
        let owner = try makeOwner(sessionID: "session-A", generation: 8)
        let client = PlaybackContextClientProbe()
        await client.holdOpen(for: owner.sessionID)
        let clock = PlaybackContextTestClock(value: 100)
        let cachedSnapshotObservedAt = clock.value
        let service = makeService(
            client: client,
            ownerProvider: { owner },
            snapshotProvider: { self.snapshot(observedAt: cachedSnapshotObservedAt) },
            now: { clock.value }
        )

        let task = Task { @MainActor in try await service.start() }
        await client.waitForOpenCount(1)
        clock.value += 1_000_000
        await client.releaseOpen(for: owner.sessionID)
        do {
            try await task.value
            XCTFail("Expected a fresh-but-pre-ticket cached snapshot to be rejected.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .snapshotPredatesTicket)
        }

        let counts = await client.counts()
        XCTAssertEqual(counts.updateCount, 0)
        XCTAssertEqual(counts.closeContextIDs, [contextA])
    }

    func testReverseOpenCompletionClosesOldOwnerWithoutReplacingNewContext() async throws {
        let client = PlaybackContextClientProbe()
        let ownerA = try makeOwner(sessionID: "session-A", generation: 8)
        let ownerB = try makeOwner(sessionID: "session-B", generation: 11)
        var currentOwner: HLSPlaybackContextOwner? = ownerA
        let now: UInt64 = 1_000
        let service = makeService(
            client: client,
            ownerProvider: { currentOwner },
            snapshotProvider: { self.snapshot(observedAt: now) },
            now: { now }
        )
        await client.holdOpen(for: ownerA.sessionID)

        let oldStart = Task { @MainActor in try await service.start() }
        await client.waitForOpenCount(1)
        let openWithdrawal = await service.withdrawAndConfirmRemoteRemoval()
        XCTAssertEqual(openWithdrawal, .unconfirmed(.openInFlightWithoutTicket))
        currentOwner = ownerB
        do {
            try await service.start()
            XCTFail("A late open must be retired before another owner can open a context.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .operationInProgress)
        }
        await client.releaseOpen(for: ownerA.sessionID)

        do {
            try await oldStart.value
            XCTFail("Expected the late old-owner open to be invalidated.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .ownerChanged)
        }
        let openRetirement = await service.withdrawAndConfirmRemoteRemoval()
        XCTAssertEqual(openRetirement, .acknowledged)
        try await service.start()
        try await service.update()

        let counts = await client.counts()
        XCTAssertEqual(counts.openSessionIDs, [ownerA.sessionID, ownerB.sessionID])
        XCTAssertEqual(counts.updateContextIDs, [contextB, contextB])
        XCTAssertEqual(counts.closeContextIDs, [contextA])
        XCTAssertEqual(counts.updateSequences, [1, 2])
    }

    func testIgnoredCancellationCannotReviveWithdrawnContextAndUpdatesAreSerial() async throws {
        let client = PlaybackContextClientProbe()
        let owner = try makeOwner(sessionID: "session-A", generation: 8)
        let now: UInt64 = 2_000
        let service = makeService(
            client: client,
            ownerProvider: { owner },
            snapshotProvider: { self.snapshot(observedAt: now) },
            now: { now }
        )
        try await service.start()
        await client.holdNextUpdate()

        let pendingUpdate = Task { @MainActor in try await service.update() }
        await client.waitForUpdateCount(2)
        do {
            try await service.update()
            XCTFail("Expected overlapping updates to be rejected.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .operationInProgress)
        }

        pendingUpdate.cancel()
        await service.withdraw()
        await client.releaseHeldUpdate()
        do {
            try await pendingUpdate.value
            XCTFail("Expected cancellation to win over a non-cooperative late update.")
        } catch is CancellationError {
        }
        do {
            try await service.update()
            XCTFail("A withdrawn context must not be revived by a late response.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .noCurrentOwner)
        }

        let counts = await client.counts()
        XCTAssertEqual(counts.updateCount, 2)
        XCTAssertEqual(counts.closeContextIDs, [contextA])
    }

    func testLateCloseForOldOwnerCannotClearNewOwnerContext() async throws {
        let client = PlaybackContextClientProbe()
        let ownerA = try makeOwner(sessionID: "session-A", generation: 8)
        let ownerB = try makeOwner(sessionID: "session-B", generation: 11)
        var currentOwner: HLSPlaybackContextOwner? = ownerA
        let now: UInt64 = 3_000
        let service = makeService(
            client: client,
            ownerProvider: { currentOwner },
            snapshotProvider: { self.snapshot(observedAt: now) },
            now: { now }
        )
        try await service.start()
        await client.holdClose(for: contextA)

        let withdrawal = Task { @MainActor in await service.withdraw() }
        let closeReachedClient = await client.waitForCloseCount(1, timeout: .seconds(2))
        XCTAssertTrue(closeReachedClient, "Expected the old context close to start.")
        currentOwner = ownerB
        try await service.start()
        await client.releaseClose(for: contextA)
        await withdrawal.value
        try await service.update()

        let counts = await client.counts()
        XCTAssertEqual(counts.openSessionIDs, [ownerA.sessionID, ownerB.sessionID])
        XCTAssertEqual(counts.updateContextIDs, [contextA, contextB, contextB])
        XCTAssertEqual(counts.closeContextIDs, [contextA])
    }

    func testLegacyWithdrawTimeoutRemainsBestEffortForNewOwner() async throws {
        let client = PlaybackContextClientProbe()
        let ownerA = try makeOwner(sessionID: "session-A", generation: 8)
        let ownerB = try makeOwner(sessionID: "session-B", generation: 11)
        var currentOwner: HLSPlaybackContextOwner? = ownerA
        let now: UInt64 = 3_250
        let service = makeService(
            client: client,
            ownerProvider: { currentOwner },
            snapshotProvider: { self.snapshot(observedAt: now) },
            now: { now }
        )
        try await service.start()
        await client.holdClose(for: contextA)

        let withdrawal = Task { @MainActor in await service.withdraw() }
        let closeReachedClient = await client.waitForCloseCount(1, timeout: .seconds(2))
        XCTAssertTrue(closeReachedClient)
        try await Task.sleep(for: .milliseconds(1_500))
        await withdrawal.value

        currentOwner = ownerB
        try await service.start()
        try await service.update()
        let countsWhileCloseIsHeld = await client.counts()
        XCTAssertEqual(countsWhileCloseIsHeld.openSessionIDs, [ownerA.sessionID, ownerB.sessionID])
        XCTAssertEqual(countsWhileCloseIsHeld.closeContextIDs, [contextA])

        await client.releaseClose(for: contextA)
        await client.waitForCompletedCloseCount(1)
    }

    func testOlderCloseCannotAcknowledgeBWhileItsReceiptIsUnresolved() async throws {
        let client = PlaybackContextClientProbe()
        let ownerA = try makeOwner(sessionID: "session-A", generation: 8)
        let ownerB = try makeOwner(sessionID: "session-B", generation: 11)
        var currentOwner: HLSPlaybackContextOwner? = ownerA
        let service = makeService(
            client: client,
            ownerProvider: { currentOwner },
            snapshotProvider: { self.snapshot(observedAt: 100) },
            now: { 100 }
        )
        try await service.start()
        await client.holdClose(for: contextA)

        let withdrawA = Task { @MainActor in await service.withdraw() }
        await client.waitForCloseCount(1)
        currentOwner = ownerB
        try await service.start()
        await service.withdraw()

        let unresolvedB = await service.withdrawAndConfirmRemoteRemoval()
        XCTAssertEqual(unresolvedB, .unconfirmed(.anotherContextRetiring))
        await client.releaseClose(for: contextA)
        await withdrawA.value

        let confirmedB = await service.withdrawAndConfirmRemoteRemoval()
        XCTAssertEqual(confirmedB, .acknowledged)
        let counts = await client.counts()
        XCTAssertEqual(counts.closeContextIDs, [contextA, contextB])
    }

    func testFailedLegacyUpdateCloseDebtSurvivesBAndLateUpdateCompletion() async throws {
        let client = PlaybackContextClientProbe()
        let ownerA = try makeOwner(sessionID: "session-A", generation: 8)
        let ownerB = try makeOwner(sessionID: "session-B", generation: 11)
        var currentOwner: HLSPlaybackContextOwner? = ownerA
        let service = makeService(
            client: client,
            ownerProvider: { currentOwner },
            snapshotProvider: { self.snapshot(observedAt: 100) },
            now: { 100 }
        )
        try await service.start()
        await client.holdNextUpdate()
        let updateA = Task { @MainActor in try await service.update() }
        await client.waitForUpdateCount(2)
        await client.holdClose(for: contextA)

        let withdrawA = Task { @MainActor in await service.withdraw() }
        await client.waitForCloseCount(1)
        currentOwner = ownerB
        try await service.start()
        await service.withdraw()

        let unresolvedB = await service.withdrawAndConfirmRemoteRemoval()
        XCTAssertEqual(unresolvedB, .unconfirmed(.anotherContextRetiring))
        await client.releaseClose(for: contextA, closed: false)
        await withdrawA.value

        let bCloseWithAStillOwed = await service.withdrawAndConfirmRemoteRemoval()
        XCTAssertEqual(bCloseWithAStillOwed, .unconfirmed(.anotherContextRetiring))
        do {
            try await service.start()
            XCTFail("B's acknowledgment cannot discard A's failed known close debt.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .operationInProgress)
        }

        let retryA = await service.withdrawAndConfirmRemoteRemoval()
        XCTAssertEqual(retryA, .acknowledged)
        await client.releaseHeldUpdate()
        do {
            try await updateA.value
            XCTFail("The late Update cannot restore the closed A context.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .ownerChanged)
        }
        try await service.start()

        let counts = await client.counts()
        XCTAssertEqual(counts.closeContextIDs, [contextA, contextB, contextA])
        XCTAssertEqual(counts.openSessionIDs, [ownerA.sessionID, ownerB.sessionID, ownerB.sessionID])
    }

    func testBoundedCloseTimeoutKeepsNewOwnerBlockedUntilLateAck() async throws {
        let client = PlaybackContextClientProbe()
        let ownerA = try makeOwner(sessionID: "session-A", generation: 8)
        let ownerB = try makeOwner(sessionID: "session-B", generation: 11)
        var currentOwner: HLSPlaybackContextOwner? = ownerA
        let now: UInt64 = 3_500
        let service = makeService(
            client: client,
            ownerProvider: { currentOwner },
            snapshotProvider: { self.snapshot(observedAt: now) },
            now: { now }
        )
        try await service.start()
        await client.holdClose(for: contextA)

        let withdrawal = Task { @MainActor in
            await service.withdrawAndConfirmRemoteRemoval()
        }
        let closeReachedClient = await client.waitForCloseCount(1, timeout: .seconds(2))
        XCTAssertTrue(closeReachedClient, "Expected the old context close to start.")
        let timeoutOutcome = await withdrawal.value
        XCTAssertEqual(timeoutOutcome, .unconfirmed(.timedOut))

        currentOwner = ownerB
        do {
            try await service.start()
            XCTFail("A timeout is not a remote close acknowledgment.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .operationInProgress)
        }
        let blockedCounts = await client.counts()
        XCTAssertEqual(blockedCounts.openCount, 1)
        await client.releaseClose(for: contextA)
        var lateOutcome = await service.withdrawAndConfirmRemoteRemoval()
        for _ in 0..<100 where lateOutcome != .acknowledged {
            try await Task.sleep(for: .milliseconds(10))
            lateOutcome = await service.withdrawAndConfirmRemoteRemoval()
        }
        XCTAssertEqual(lateOutcome, .acknowledged)
        try await service.start()
        try await service.update()

        let counts = await client.counts()
        XCTAssertEqual(counts.openSessionIDs, [ownerA.sessionID, ownerB.sessionID])
        XCTAssertEqual(counts.updateContextIDs, [contextA, contextB, contextB])
        XCTAssertEqual(counts.closeContextIDs, [contextA])
    }

    func testDelayedUpdateRechecksSnapshotFreshnessAfterAwait() async throws {
        let client = PlaybackContextClientProbe()
        let owner = try makeOwner(sessionID: "session-A", generation: 8)
        let clock = PlaybackContextTestClock(value: 4_000)
        let service = makeService(
            client: client,
            ownerProvider: { owner },
            snapshotProvider: { self.snapshot(observedAt: clock.value) },
            now: { clock.value }
        )
        try await service.start()
        await client.holdNextUpdate()

        let pendingUpdate = Task { @MainActor in try await service.update() }
        await client.waitForUpdateCount(2)
        clock.value += 5_000_000_001
        await client.releaseHeldUpdate()
        do {
            try await pendingUpdate.value
            XCTFail("Expected timing to expire while the update response was delayed.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .staleSnapshot)
        }

        let counts = await client.counts()
        XCTAssertEqual(counts.closeContextIDs, [contextA])
        do {
            try await service.update()
            XCTFail("A failed late update must not leave a usable challenge locally.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .noCurrentOwner)
        }
    }

    func testLateSuccessRejectsExpiredConsumedTicketDespiteFreshSuccessor() async throws {
        let client = PlaybackContextClientProbe()
        let owner = try makeOwner(sessionID: "session-A", generation: 8)
        let clock = PlaybackContextTestClock(value: 5_000)
        let service = makeService(
            client: client,
            ownerProvider: { owner },
            snapshotProvider: { self.snapshot(observedAt: clock.value) },
            now: { clock.value }
        )
        await client.setUpdateTicketValidity(1)
        try await service.start()
        await client.setUpdateTicketValidity(5_000)
        await client.holdNextUpdate()

        let pendingUpdate = Task { @MainActor in try await service.update() }
        await client.waitForUpdateCount(2)
        clock.value += 2_000_000
        await client.releaseHeldUpdate()
        do {
            try await pendingUpdate.value
            XCTFail("A fresh successor must not revive the expired consumed challenge.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .staleTicket)
        }

        let counts = await client.counts()
        XCTAssertEqual(counts.updateCount, 2)
        XCTAssertEqual(counts.closeContextIDs, [contextA])
        do {
            try await service.update()
            XCTFail("The expired context must not remain locally usable.")
        } catch let error as HLSPlaybackContextReportingError {
            XCTAssertEqual(error, .noCurrentOwner)
        }
    }

    private func makeOwner(
        sessionID: String,
        generation: UInt64,
        ownerToken: UUID = UUID()
    ) throws -> HLSPlaybackContextOwner {
        try XCTUnwrap(
            HLSPlaybackContextOwner(
                ownerToken: ownerToken,
                serverID: "server",
                sessionID: sessionID,
                sessionGeneration: generation
            )
        )
    }

    private func makeService(
        client: PlaybackContextClientProbe,
        summary: CacheServerSummary? = nil,
        ownerProvider: @escaping @MainActor () -> HLSPlaybackContextOwner?,
        snapshotProvider: @escaping @MainActor () -> HLSPlaybackSnapshot?,
        now: @escaping @MainActor () -> UInt64
    ) -> HLSPlaybackContextReportingService {
        HLSPlaybackContextReportingService(
            client: client,
            serverSummary: summary ?? serverSummary(supportsContext: true),
            ownerProvider: ownerProvider,
            snapshotProvider: snapshotProvider,
            monotonicNowNanoseconds: now
        )
    }

    private func serverSummary(supportsContext: Bool, supportsReaderBinding: Bool = false) -> CacheServerSummary {
        var capabilities: [String] = []
        if supportsContext {
            capabilities.append(CacheServerCapability.hlsPlaybackContext)
        }
        if supportsReaderBinding {
            capabilities.append(CacheServerCapability.hlsPlaybackReaderBinding)
        }
        return CacheServerSummary(
            id: "server",
            name: "Cache",
            version: "1",
            mediaBaseURIs: [],
            capabilities: capabilities
        )
    }

    private func snapshot(observedAt: UInt64) -> HLSPlaybackSnapshot? {
        HLSPlaybackSnapshot.capture(
            timeControlStatus: .playing,
            playbackRate: 1,
            currentTime: CMTime(seconds: 5, preferredTimescale: 600),
            loadedTimeRanges: [
                CMTimeRange(
                    start: .zero,
                    duration: CMTime(seconds: 20, preferredTimescale: 600)
                )
            ],
            observedAtNanoseconds: observedAt
        )
    }
}

@MainActor
private final class PlaybackContextTestClock {
    var value: UInt64

    init(value: UInt64) {
        self.value = value
    }
}

private actor PlaybackContextClientProbe: CacheControlClient {
    struct Counts: Sendable {
        let openCount: Int
        let updateCount: Int
        let completedUpdateCount: Int
        let closeCount: Int
        let completedCloseCount: Int
        let openSessionIDs: [String]
        let openHTTPReaderIDs: [String?]
        let updateContextIDs: [String]
        let updateSequences: [UInt64]
        let updateSamples: [HLSPlaybackContextTimingSample]
        let updateTicketHTTPReaderIDs: [String?]
        let closeContextIDs: [String]
    }

    private struct ContextRecord {
        let sessionID: String
        let generation: UInt64
        let httpReaderID: String?
    }

    private var openRequests: [HLSPlaybackContextOpenRequest] = []
    private var updateRequests: [HLSPlaybackContextUpdateRequest] = []
    private var updateTicketHTTPReaderIDs: [String?] = []
    private var closeRequests: [HLSPlaybackContextCloseRequest] = []
    private var contexts: [String: ContextRecord] = [:]
    private var heldOpenSessions: Set<String> = []
    private var heldOpenFailureSessions: Set<String> = []
    private var heldOpenRequests: [String: HLSPlaybackContextOpenRequest] = [:]
    private var heldOpenContinuations: [String: CheckedContinuation<HLSPlaybackContextTicket, Error>] = [:]
    private var openCountWaiters: [(Int, CheckedContinuation<Void, Never>)] = []
    private var failNextOpenResponse = false
    private var holdUpdate = false
    private var heldUpdateRequest: HLSPlaybackContextUpdateRequest?
    private var heldUpdateContinuation: CheckedContinuation<HLSPlaybackContextTicket, Error>?
    private var updateCountWaiters: [(Int, CheckedContinuation<Void, Never>)] = []
    private var completedUpdateCount = 0
    private var completedUpdateCountWaiters: [(Int, CheckedContinuation<Void, Never>)] = []
    private var updateTicketValidityMillis: UInt64 = 5_000
    private var failNextCloseResponse = false
    private var nextCloseResponse: Bool?
    private var heldCloseIDs: Set<String> = []
    private var heldCloseContinuations: [String: CheckedContinuation<HLSPlaybackContextCloseResponse, Error>] = [:]
    private var closeCountWaiters: [(Int, CheckedContinuation<Void, Never>)] = []
    private var completedCloseCount = 0
    private var completedCloseCountWaiters: [(Int, CheckedContinuation<Void, Never>)] = []

    func getServerInfo() async throws -> CacheServerSummary { throw PlaybackContextClientError.notImplemented }
    func listCacheRoots() async throws -> [CacheRoot] { throw PlaybackContextClientError.notImplemented }
    func listLibraryItemsPage(pageToken: String, pageSize: Int, searchText: String?) async throws
        -> CacheLibraryItemsPage
    {
        throw PlaybackContextClientError.notImplemented
    }
    func getPlaybackSource(itemID: String, variantID: String) async throws -> CachePlaybackSource {
        throw PlaybackContextClientError.notImplemented
    }
    func deleteLibraryItem(id: String) async throws -> Bool { throw PlaybackContextClientError.notImplemented }
    func getTask(id: String) async throws -> CacheTask { throw PlaybackContextClientError.notImplemented }
    func watchTasks(ids: [String]) async -> AsyncThrowingStream<CacheTask, Error> {
        AsyncThrowingStream { $0.finish() }
    }
    func cancelTask(id: String) async throws -> CacheTask { throw PlaybackContextClientError.notImplemented }
    func createBilibiliPlaybackTask(urlOrID: String, options: BilibiliPlaybackTaskOptions) async throws -> CacheTask {
        throw PlaybackContextClientError.notImplemented
    }

    func openHLSPlaybackContext(_ request: HLSPlaybackContextOpenRequest) async throws -> HLSPlaybackContextTicket {
        openRequests.append(request)
        resumeWaiters(&openCountWaiters, reached: openRequests.count)
        if failNextOpenResponse {
            failNextOpenResponse = false
            _ = makeOpenTicket(request)
            throw PlaybackContextClientError.openFailed
        }
        if heldOpenSessions.contains(request.sessionID) {
            heldOpenRequests[request.sessionID] = request
            return try await withCheckedThrowingContinuation {
                heldOpenContinuations[request.sessionID] = $0
            }
        }
        return makeOpenTicket(request)
    }

    func updateHLSPlaybackContext(_ request: HLSPlaybackContextUpdateRequest) async throws -> HLSPlaybackContextTicket {
        updateRequests.append(request)
        resumeWaiters(&updateCountWaiters, reached: updateRequests.count)
        if holdUpdate {
            holdUpdate = false
            heldUpdateRequest = request
            let ticket = try await withCheckedThrowingContinuation { heldUpdateContinuation = $0 }
            completedUpdateCount += 1
            resumeWaiters(&completedUpdateCountWaiters, reached: completedUpdateCount)
            return ticket
        }
        let ticket = makeUpdateTicket(request)
        completedUpdateCount += 1
        resumeWaiters(&completedUpdateCountWaiters, reached: completedUpdateCount)
        return ticket
    }

    func closeHLSPlaybackContext(_ request: HLSPlaybackContextCloseRequest) async throws
        -> HLSPlaybackContextCloseResponse
    {
        closeRequests.append(request)
        resumeWaiters(&closeCountWaiters, reached: closeRequests.count)
        if failNextCloseResponse {
            failNextCloseResponse = false
            recordCompletedClose()
            throw PlaybackContextClientError.closeFailed
        }
        if let nextCloseResponse {
            self.nextCloseResponse = nil
            recordCompletedClose()
            return HLSPlaybackContextCloseResponse(closed: nextCloseResponse)
        }
        if heldCloseIDs.contains(request.contextID) {
            let response = try await withCheckedThrowingContinuation {
                heldCloseContinuations[request.contextID] = $0
            }
            recordCompletedClose()
            return response
        }
        recordCompletedClose()
        return HLSPlaybackContextCloseResponse(closed: true)
    }

    func holdOpen(for sessionID: String) {
        heldOpenSessions.insert(sessionID)
    }

    func failNextOpen() {
        failNextOpenResponse = true
    }

    func failHeldOpen(for sessionID: String) {
        heldOpenFailureSessions.insert(sessionID)
    }

    func waitForOpenCount(_ count: Int) async {
        guard openRequests.count < count else { return }
        await withCheckedContinuation { openCountWaiters.append((count, $0)) }
    }

    func releaseOpen(for sessionID: String) {
        heldOpenSessions.remove(sessionID)
        guard let continuation = heldOpenContinuations.removeValue(forKey: sessionID),
            let request = heldOpenRequests.removeValue(forKey: sessionID)
        else {
            return
        }
        if heldOpenFailureSessions.remove(sessionID) != nil {
            continuation.resume(throwing: PlaybackContextClientError.openFailed)
        } else {
            continuation.resume(returning: makeOpenTicket(request))
        }
    }

    func holdNextUpdate() {
        holdUpdate = true
    }

    func setUpdateTicketValidity(_ validityMillis: UInt64) {
        updateTicketValidityMillis = validityMillis
    }

    func waitForUpdateCount(_ count: Int) async {
        guard updateRequests.count < count else { return }
        await withCheckedContinuation { updateCountWaiters.append((count, $0)) }
    }

    func waitForCompletedUpdateCount(_ count: Int) async {
        guard completedUpdateCount < count else { return }
        await withCheckedContinuation { completedUpdateCountWaiters.append((count, $0)) }
    }

    func releaseHeldUpdate() {
        guard let continuation = heldUpdateContinuation,
            let request = heldUpdateRequest
        else {
            return
        }
        heldUpdateContinuation = nil
        heldUpdateRequest = nil
        continuation.resume(returning: makeUpdateTicket(request))
    }

    func holdClose(for contextID: String) {
        heldCloseIDs.insert(contextID)
    }

    func setNextCloseResponse(_ closed: Bool) {
        nextCloseResponse = closed
    }

    func failNextClose() {
        failNextCloseResponse = true
    }

    func waitForCloseCount(_ count: Int) async {
        guard closeRequests.count < count else { return }
        await withCheckedContinuation { closeCountWaiters.append((count, $0)) }
    }

    func waitForCompletedCloseCount(_ count: Int) async {
        guard completedCloseCount < count else { return }
        await withCheckedContinuation { completedCloseCountWaiters.append((count, $0)) }
    }

    func waitForCloseCount(_ count: Int, timeout: Duration) async -> Bool {
        let clock = ContinuousClock()
        let deadline = clock.now.advanced(by: timeout)
        while closeRequests.count < count, clock.now < deadline {
            try? await Task.sleep(for: .milliseconds(10))
        }
        return closeRequests.count >= count
    }

    func releaseClose(for contextID: String, closed: Bool = true) {
        heldCloseIDs.remove(contextID)
        guard let continuation = heldCloseContinuations.removeValue(forKey: contextID) else { return }
        continuation.resume(returning: HLSPlaybackContextCloseResponse(closed: closed))
    }

    func counts() -> Counts {
        Counts(
            openCount: openRequests.count,
            updateCount: updateRequests.count,
            completedUpdateCount: completedUpdateCount,
            closeCount: closeRequests.count,
            completedCloseCount: completedCloseCount,
            openSessionIDs: openRequests.map(\.sessionID),
            openHTTPReaderIDs: openRequests.map(\.httpReaderID),
            updateContextIDs: updateRequests.map(\.contextID),
            updateSequences: updateRequests.map(\.sequence),
            updateSamples: updateRequests.map(\.sample),
            updateTicketHTTPReaderIDs: updateTicketHTTPReaderIDs,
            closeContextIDs: closeRequests.map(\.contextID)
        )
    }

    private func makeOpenTicket(_ request: HLSPlaybackContextOpenRequest) -> HLSPlaybackContextTicket {
        let isB = request.sessionID.utf8.elementsEqual("session-B".utf8)
        let contextID = isB ? "00000000-0000-4000-8000-000000000012" : "00000000-0000-4000-8000-000000000011"
        let challenge = isB ? "00000000-0000-4000-8000-000000000022" : "00000000-0000-4000-8000-000000000021"
        contexts[contextID] = ContextRecord(
            sessionID: request.sessionID,
            generation: request.expectedSessionGeneration,
            httpReaderID: request.httpReaderID
        )
        return HLSPlaybackContextTicket(
            contextID: contextID,
            challenge: challenge,
            sessionID: request.sessionID,
            sessionGeneration: request.expectedSessionGeneration,
            nextSequence: 1,
            validityMillis: 5_000,
            httpReaderID: request.httpReaderID
        )
    }

    private func makeUpdateTicket(_ request: HLSPlaybackContextUpdateRequest) -> HLSPlaybackContextTicket {
        let context =
            contexts[request.contextID]
            ?? ContextRecord(
                sessionID: request.sessionID,
                generation: request.expectedSessionGeneration,
                httpReaderID: request.expectedHTTPReaderID
            )
        let nextSequence = request.sequence + 1
        let suffix = String(format: "%012llx", nextSequence + 100)
        updateTicketHTTPReaderIDs.append(context.httpReaderID)
        return HLSPlaybackContextTicket(
            contextID: request.contextID,
            challenge: "00000000-0000-4000-8000-\(suffix)",
            sessionID: context.sessionID,
            sessionGeneration: context.generation,
            nextSequence: nextSequence,
            validityMillis: updateTicketValidityMillis,
            httpReaderID: context.httpReaderID
        )
    }

    private func resumeWaiters(
        _ waiters: inout [(Int, CheckedContinuation<Void, Never>)],
        reached count: Int
    ) {
        var ready: [CheckedContinuation<Void, Never>] = []
        var pending: [(Int, CheckedContinuation<Void, Never>)] = []
        for (threshold, continuation) in waiters {
            if threshold <= count {
                ready.append(continuation)
            } else {
                pending.append((threshold, continuation))
            }
        }
        waiters = pending
        ready.forEach { $0.resume() }
    }

    private func recordCompletedClose() {
        completedCloseCount += 1
        resumeWaiters(&completedCloseCountWaiters, reached: completedCloseCount)
    }
}

private enum PlaybackContextClientError: Error {
    case notImplemented
    case openFailed
    case closeFailed
}
