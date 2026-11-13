import Foundation
import TVOSNetPlayerCacheClient

/// Stable identity for the player, controller, and item that own one reporting context.
public struct HLSPlaybackContextOwner: Equatable, Sendable {
    public let ownerToken: UUID
    public let serverID: String
    public let sessionID: String
    public let sessionGeneration: UInt64

    public init?(
        ownerToken: UUID,
        serverID: String,
        sessionID: String,
        sessionGeneration: UInt64
    ) {
        guard Self.isValidIdentifier(serverID),
            sessionGeneration > 0,
            (try? HLSPlaybackContextOpenRequest(
                sessionID: sessionID,
                expectedSessionGeneration: sessionGeneration
            ).validate()) != nil
        else {
            return nil
        }
        self.ownerToken = ownerToken
        self.serverID = serverID
        self.sessionID = sessionID
        self.sessionGeneration = sessionGeneration
    }

    public static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.ownerToken == rhs.ownerToken
            && lhs.serverID.utf8.elementsEqual(rhs.serverID.utf8)
            && lhs.sessionID.utf8.elementsEqual(rhs.sessionID.utf8)
            && lhs.sessionGeneration == rhs.sessionGeneration
    }

    private static func isValidIdentifier(_ value: String) -> Bool {
        let scalars = value.unicodeScalars
        guard !value.isEmpty, value.utf8.count <= 256,
            let first = scalars.first,
            let last = scalars.last,
            !first.properties.isWhitespace,
            !last.properties.isWhitespace
        else {
            return false
        }
        return !scalars.contains { $0.properties.generalCategory == .control }
    }
}

public enum HLSPlaybackContextReportingError: Error, Equatable, Sendable {
    case unsupportedServer
    case noCurrentOwner
    case ownerChanged
    case operationInProgress
    case playbackNotActive
    case invalidSnapshot
    case snapshotPredatesTicket
    case staleSnapshot
    case staleTicket
    case clockAnomaly
    case invalidTicket
    case requestFailed
}

public enum HLSPlaybackContextWithdrawalFailure: Equatable, Sendable {
    case openInFlightWithoutTicket
    case openOutcomeUnknown
    case invalidContextID
    case closeRejected
    case closeRequestFailed
    case timedOut
    case cancelled
    case anotherContextRetiring
}

public enum HLSPlaybackContextWithdrawalOutcome: Equatable, Sendable {
    case notRequired
    case acknowledged
    case unconfirmed(HLSPlaybackContextWithdrawalFailure)
}

/// Sends timing only when explicitly invoked; it does not poll or control playback.
@MainActor
public final class HLSPlaybackContextReportingService {
    private struct ActiveContext {
        let revision: UUID
        let owner: HLSPlaybackContextOwner
        let ticket: HLSPlaybackContextTicket
        let ticketRequestStartedNanoseconds: UInt64
        let ticketReceivedAtNanoseconds: UInt64
    }

    private struct InFlightOpen {
        let operation: UUID
        let owner: HLSPlaybackContextOwner
        var contextID: String?
        var retirementID: UUID?
    }

    private struct InFlightUpdate {
        let operation: UUID
        let owner: HLSPlaybackContextOwner
        let contextID: String
        var retirementID: UUID?
        var closeAcknowledged = false
    }

    private struct RetirementBarrier {
        let id: UUID
        let owner: HLSPlaybackContextOwner?
        var contextID: String?
        var pendingOpenOperation: UUID?
        var pendingOtherOpenOperations: Set<UUID>
        var pendingOtherOpenCloseContextIDs: [UUID: String]
        var unknownRemoteAuthority: Bool
        var closeAttempted: Bool
        var outcome: HLSPlaybackContextWithdrawalOutcome?
    }

    private struct CloseAttempt {
        let id: UUID
        var retirementID: UUID?
        let contextID: String
        let waiter: HLSPlaybackContextCloseWaiter
        let task: Task<Void, Never>
    }

    private struct LegacyCloseReceipt {
        let contextID: String
        let outcome: HLSPlaybackContextWithdrawalOutcome
    }

    private static let maximumSnapshotAgeNanoseconds: UInt64 = 5_000_000_000
    private static let closeWaitDuration: Duration = .seconds(1)

    private let client: any CacheControlClient
    private let serverSummary: CacheServerSummary
    private let ownerProvider: @MainActor () -> HLSPlaybackContextOwner?
    private let snapshotProvider: @MainActor () -> HLSPlaybackSnapshot?
    private let monotonicNowNanoseconds: @MainActor () -> UInt64?

    private var revision = UUID()
    private var operationID: UUID?
    private var activeContext: ActiveContext?
    private var inFlightOpen: InFlightOpen?
    private var inFlightUpdate: InFlightUpdate?
    private var legacyInFlightOpen: InFlightOpen?
    private var legacyInFlightUpdate: InFlightUpdate?
    private var retirementBarrier: RetirementBarrier?
    private var closeAttempt: CloseAttempt?
    private var legacyCloseReceipt: LegacyCloseReceipt?
    private var legacyCloseDebt: LegacyCloseReceipt?
    private var legacyUnknownOpenOutcome = false
    private var lastMonotonicNowNanoseconds: UInt64?

    public convenience init(
        client: any CacheControlClient,
        serverSummary: CacheServerSummary,
        ownerProvider: @escaping @MainActor () -> HLSPlaybackContextOwner?,
        snapshotProvider: @escaping @MainActor () -> HLSPlaybackSnapshot?
    ) {
        self.init(
            client: client,
            serverSummary: serverSummary,
            ownerProvider: ownerProvider,
            snapshotProvider: snapshotProvider,
            monotonicNowNanosecondsProvider: { HLSPlaybackMonotonicClock.shared.nowNanoseconds() }
        )
    }

    public convenience init(
        client: any CacheControlClient,
        serverSummary: CacheServerSummary,
        ownerProvider: @escaping @MainActor () -> HLSPlaybackContextOwner?,
        snapshotProvider: @escaping @MainActor () -> HLSPlaybackSnapshot?,
        monotonicNowNanoseconds: @escaping @MainActor () -> UInt64
    ) {
        self.init(
            client: client,
            serverSummary: serverSummary,
            ownerProvider: ownerProvider,
            snapshotProvider: snapshotProvider,
            monotonicNowNanosecondsProvider: { monotonicNowNanoseconds() }
        )
    }

    init(
        client: any CacheControlClient,
        serverSummary: CacheServerSummary,
        ownerProvider: @escaping @MainActor () -> HLSPlaybackContextOwner?,
        snapshotProvider: @escaping @MainActor () -> HLSPlaybackSnapshot?,
        monotonicNowNanosecondsProvider: @escaping @MainActor () -> UInt64?
    ) {
        self.client = client
        self.serverSummary = serverSummary
        self.ownerProvider = ownerProvider
        self.snapshotProvider = snapshotProvider
        self.monotonicNowNanoseconds = monotonicNowNanosecondsProvider
    }

    /// Opens a context, captures timing only after the first ticket, then sends its first sample.
    public func start() async throws {
        guard operationID == nil else {
            throw HLSPlaybackContextReportingError.operationInProgress
        }
        guard legacyCloseDebt == nil else {
            throw HLSPlaybackContextReportingError.operationInProgress
        }
        if let legacyCloseReceipt, legacyCloseReceipt.outcome != .acknowledged {
            guard let closeAttempt,
                closeAttempt.contextID.utf8.elementsEqual(legacyCloseReceipt.contextID.utf8)
            else {
                throw HLSPlaybackContextReportingError.operationInProgress
            }
        }
        if let activeContext, let closeAttempt,
            !closeAttempt.contextID.utf8.elementsEqual(activeContext.ticket.contextID.utf8)
        {
            throw HLSPlaybackContextReportingError.operationInProgress
        }
        if let retirementBarrier {
            guard retirementBarrier.outcome == .acknowledged,
                !hasUnresolvedOpenAuthority(retirementBarrier),
                closeAttempt == nil
            else {
                throw HLSPlaybackContextReportingError.operationInProgress
            }
            self.retirementBarrier = nil
        }

        if inFlightOpen != nil || inFlightUpdate != nil {
            guard legacyInFlightOpen == nil, legacyInFlightUpdate == nil else {
                throw HLSPlaybackContextReportingError.operationInProgress
            }
            legacyInFlightOpen = inFlightOpen
            legacyInFlightUpdate = inFlightUpdate
            inFlightOpen = nil
            inFlightUpdate = nil
        } else if legacyInFlightOpen != nil || legacyInFlightUpdate != nil {
            throw HLSPlaybackContextReportingError.operationInProgress
        }

        let operation = UUID()
        revision = operation
        operationID = operation
        defer {
            if operationID == operation {
                operationID = nil
            }
            if inFlightOpen?.operation == operation {
                inFlightOpen = nil
            }
            if inFlightUpdate?.operation == operation {
                inFlightUpdate = nil
            }
            if legacyInFlightOpen?.operation == operation {
                legacyInFlightOpen = nil
            }
            if legacyInFlightUpdate?.operation == operation {
                legacyInFlightUpdate = nil
            }
        }

        if let previousContext = activeContext {
            activeContext = nil
            await closeBestEffort(previousContext.ticket.contextID)
        }
        try Task.checkCancellation()
        _ = try checkedNow()
        guard revision == operation, operationID == operation else {
            throw HLSPlaybackContextReportingError.ownerChanged
        }
        guard serverSummary.supportsHLSPlaybackContext else {
            throw HLSPlaybackContextReportingError.unsupportedServer
        }
        guard let owner = ownerProvider(), ownerMatchesServer(owner) else {
            throw HLSPlaybackContextReportingError.noCurrentOwner
        }

        let openRequest = HLSPlaybackContextOpenRequest(
            sessionID: owner.sessionID,
            expectedSessionGeneration: owner.sessionGeneration,
            httpReaderID: httpReaderID(for: owner)
        )
        try openRequest.validate()
        try requireCurrent(owner, revision: operation, operation: operation)
        let openRequestStarted = try checkedNow()
        inFlightOpen = InFlightOpen(operation: operation, owner: owner, contextID: nil, retirementID: nil)
        var openedTicket: HLSPlaybackContextTicket?

        do {
            let ticket = try await client.openHLSPlaybackContext(openRequest)
            openedTicket = ticket
            recordOpenedTicket(ticket, operation: operation)
            try Task.checkCancellation()
            let ticketReceivedAt = try checkedNow()
            try requireCurrent(owner, revision: operation, operation: operation)
            try ticket.validate(matching: openRequest)
            try requireLive(ticket, requestStarted: openRequestStarted, now: ticketReceivedAt)

            let (snapshot, sample) = try captureFreshSample(
                for: owner,
                revision: operation,
                operation: operation,
                snapshotNotBefore: ticketReceivedAt
            )
            let updateRequest = HLSPlaybackContextUpdateRequest(
                contextID: ticket.contextID,
                challenge: ticket.challenge,
                sessionID: owner.sessionID,
                expectedSessionGeneration: owner.sessionGeneration,
                sequence: ticket.nextSequence,
                sample: sample,
                expectedHTTPReaderID: ticket.httpReaderID
            )
            try updateRequest.validate()

            let updateRequestStarted = try checkedNow()
            try requireCurrent(owner, revision: operation, operation: operation)
            try requireLive(ticket, requestStarted: openRequestStarted, now: updateRequestStarted)
            try requireFresh(snapshot, now: updateRequestStarted, notBefore: ticketReceivedAt)

            inFlightOpen = nil
            inFlightUpdate = InFlightUpdate(
                operation: operation,
                owner: owner,
                contextID: ticket.contextID,
                retirementID: retirementBarrier?.id
            )
            let nextTicket = try await client.updateHLSPlaybackContext(updateRequest)
            try Task.checkCancellation()
            let afterUpdate = try checkedNow()
            try requireCurrent(owner, revision: operation, operation: operation)
            try requireFresh(snapshot, now: afterUpdate, notBefore: ticketReceivedAt)
            try requireLive(ticket, requestStarted: openRequestStarted, now: afterUpdate)
            try nextTicket.validate(matching: updateRequest)
            try requireLive(nextTicket, requestStarted: updateRequestStarted, now: afterUpdate)

            inFlightUpdate = nil
            activeContext = ActiveContext(
                revision: operation,
                owner: owner,
                ticket: nextTicket,
                ticketRequestStartedNanoseconds: updateRequestStarted,
                ticketReceivedAtNanoseconds: afterUpdate
            )
        } catch {
            if let openedTicket {
                let updateWasAlreadyClosed = updateOperation(operation)?.closeAcknowledged == true
                let flightRetirementID = retirementID(for: operation)
                clearInFlightOperation(operation)
                if let flightRetirementID {
                    if retirementBarrier?.id == flightRetirementID {
                        _ = await resolveRetirement(flightRetirementID, allowExplicitRetry: false)
                    }
                } else if !updateWasAlreadyClosed {
                    if revision == operation, operationID == operation {
                        activeContext = nil
                    }
                    await closeBestEffort(openedTicket.contextID)
                }
            } else if let opening = openOperation(operation) {
                clearInFlightOperation(operation)
                if let retirementID = opening.retirementID, retirementBarrier?.id == retirementID {
                    updateRetirement(retirementID) {
                        if $0.pendingOpenOperation == operation {
                            $0.pendingOpenOperation = nil
                        }
                        $0.pendingOtherOpenOperations.remove(operation)
                        $0.pendingOtherOpenCloseContextIDs.removeValue(forKey: operation)
                        $0.unknownRemoteAuthority = true
                        $0.outcome = .unconfirmed(.openOutcomeUnknown)
                    }
                } else if revision == operation, operationID == operation {
                    activeContext = nil
                    let barrier = ensureRetirement(
                        owner: opening.owner,
                        contextID: nil,
                        pendingOpenOperation: nil,
                        unknownRemoteAuthority: true
                    )
                    updateRetirement(barrier.id) {
                        $0.outcome = .unconfirmed(.openOutcomeUnknown)
                    }
                } else {
                    recordUnknownOpenOutcome(operation: operation)
                }
            } else if let failedUpdate = updateOperation(operation) {
                let updateWasAlreadyClosed = failedUpdate.closeAcknowledged
                clearInFlightOperation(operation)
                if let retirementID = failedUpdate.retirementID, retirementBarrier?.id == retirementID {
                    _ = await resolveRetirement(retirementID, allowExplicitRetry: false)
                } else if revision == operation, operationID == operation {
                    activeContext = nil
                    revision = UUID()
                    let barrier = ensureRetirement(
                        owner: failedUpdate.owner,
                        contextID: failedUpdate.contextID,
                        pendingOpenOperation: nil
                    )
                    _ = await resolveRetirement(barrier.id, allowExplicitRetry: false)
                } else if !updateWasAlreadyClosed {
                    await closeBestEffort(failedUpdate.contextID)
                }
            }
            throw sanitized(error)
        }
    }

    /// Sends one fresh sample. Concurrent updates are rejected instead of overlapping sequences.
    public func update() async throws {
        guard operationID == nil else {
            throw HLSPlaybackContextReportingError.operationInProgress
        }
        guard retirementBarrier == nil else {
            throw HLSPlaybackContextReportingError.operationInProgress
        }
        guard let context = activeContext else {
            throw HLSPlaybackContextReportingError.noCurrentOwner
        }

        let operation = UUID()
        operationID = operation
        defer {
            if operationID == operation {
                operationID = nil
            }
            if inFlightUpdate?.operation == operation {
                inFlightUpdate = nil
            }
            if legacyInFlightUpdate?.operation == operation {
                legacyInFlightUpdate = nil
            }
        }

        do {
            try Task.checkCancellation()
            let beforeCapture = try checkedNow()
            try requireCurrent(context.owner, revision: context.revision, operation: operation)
            try requireLive(
                context.ticket,
                requestStarted: context.ticketRequestStartedNanoseconds,
                now: beforeCapture
            )

            let (snapshot, sample) = try captureFreshSample(
                for: context.owner,
                revision: context.revision,
                operation: operation,
                snapshotNotBefore: context.ticketReceivedAtNanoseconds
            )
            let request = HLSPlaybackContextUpdateRequest(
                contextID: context.ticket.contextID,
                challenge: context.ticket.challenge,
                sessionID: context.owner.sessionID,
                expectedSessionGeneration: context.owner.sessionGeneration,
                sequence: context.ticket.nextSequence,
                sample: sample,
                expectedHTTPReaderID: context.ticket.httpReaderID
            )
            try request.validate()

            let requestStarted = try checkedNow()
            try requireCurrent(context.owner, revision: context.revision, operation: operation)
            try requireLive(
                context.ticket,
                requestStarted: context.ticketRequestStartedNanoseconds,
                now: requestStarted
            )
            try requireFresh(
                snapshot,
                now: requestStarted,
                notBefore: context.ticketReceivedAtNanoseconds
            )

            inFlightUpdate = InFlightUpdate(
                operation: operation,
                owner: context.owner,
                contextID: context.ticket.contextID,
                retirementID: nil
            )
            let nextTicket = try await client.updateHLSPlaybackContext(request)
            try Task.checkCancellation()
            let afterUpdate = try checkedNow()
            try requireCurrent(context.owner, revision: context.revision, operation: operation)
            try requireFresh(
                snapshot,
                now: afterUpdate,
                notBefore: context.ticketReceivedAtNanoseconds
            )
            try requireLive(
                context.ticket,
                requestStarted: context.ticketRequestStartedNanoseconds,
                now: afterUpdate
            )
            try nextTicket.validate(matching: request)
            try requireLive(nextTicket, requestStarted: requestStarted, now: afterUpdate)

            inFlightUpdate = nil
            activeContext = ActiveContext(
                revision: context.revision,
                owner: context.owner,
                ticket: nextTicket,
                ticketRequestStartedNanoseconds: requestStarted,
                ticketReceivedAtNanoseconds: afterUpdate
            )
        } catch {
            let failedUpdate = updateOperation(operation)
            let flightRetirementID = failedUpdate?.retirementID
            let updateWasAlreadyClosed = failedUpdate?.closeAcknowledged == true
            let operationStillOwnsContext =
                revision == context.revision && operationID == operation
                && activeContext?.ticket.contextID.utf8.elementsEqual(context.ticket.contextID.utf8) == true
            if operationStillOwnsContext {
                activeContext = nil
                revision = UUID()
            }
            clearInFlightOperation(operation)
            if let flightRetirementID {
                if retirementBarrier?.id == flightRetirementID {
                    _ = await resolveRetirement(flightRetirementID, allowExplicitRetry: false)
                }
            } else if !updateWasAlreadyClosed {
                await closeBestEffort(context.ticket.contextID)
            }
            throw sanitized(error)
        }
    }

    /// Withdraws local authority immediately and makes in-flight results uncommittable.
    public func withdraw() async {
        if let retirementBarrier {
            _ = await resolveRetirement(retirementBarrier.id, allowExplicitRetry: false)
            return
        }

        let previousContext = activeContext
        let opening = openOperation(operationID)
        let updating = updateOperation(operationID)
        revision = UUID()
        operationID = nil
        activeContext = nil

        if let previousContext {
            await closeBestEffort(previousContext.ticket.contextID)
        } else if let updating {
            await closeBestEffort(updating.contextID)
        } else if let opening, let contextID = opening.contextID {
            await closeBestEffort(contextID)
        } else if let closeAttempt {
            await closeBestEffort(closeAttempt.contextID)
        }
    }

    /// Waits briefly for exact-ID server confirmation; a timeout or unknown ID is never an acknowledgment.
    public func withdrawAndConfirmRemoteRemoval() async -> HLSPlaybackContextWithdrawalOutcome {
        let previousContext = activeContext
        let currentOperation = operationID
        let opening =
            openOperation(currentOperation)
            ?? (currentOperation == nil ? inFlightOpen ?? legacyInFlightOpen : nil)
        let updating = updateOperation(currentOperation)
        let targetOpenOperation =
            retirementBarrier?.pendingOpenOperation
            ?? (previousContext == nil ? opening?.operation : nil)
        let otherPendingOpenOperations = Set(
            [inFlightOpen, legacyInFlightOpen].compactMap { candidate -> UUID? in
                guard let candidate, candidate.contextID == nil,
                    candidate.operation != targetOpenOperation
                else {
                    return nil
                }
                return candidate.operation
            }
        )
        let unknownOpenOutcome = legacyUnknownOpenOutcome
        revision = UUID()
        operationID = nil
        activeContext = nil

        if let retirementBarrier {
            if retirementBarrier.outcome == .acknowledged,
                closeAttempt == nil,
                let legacyCloseDebt
            {
                let acknowledgedBarrier = retirementBarrier
                self.retirementBarrier = nil
                let debtBarrier = ensureRetirement(
                    owner: nil,
                    contextID: legacyCloseDebt.contextID,
                    pendingOpenOperation: nil,
                    pendingOtherOpenOperations: acknowledgedBarrier.pendingOtherOpenOperations
                        .union(otherPendingOpenOperations),
                    pendingOtherOpenCloseContextIDs: acknowledgedBarrier
                        .pendingOtherOpenCloseContextIDs,
                    unknownRemoteAuthority: unknownOpenOutcome
                        || acknowledgedBarrier.unknownRemoteAuthority
                )
                let outcome = await resolveRetirement(debtBarrier.id, allowExplicitRetry: true)
                return accountingForKnownCloseDebt(outcome)
            }
            if unknownOpenOutcome {
                updateRetirement(retirementBarrier.id) { $0.unknownRemoteAuthority = true }
            }
            if !otherPendingOpenOperations.isEmpty {
                updateRetirement(retirementBarrier.id) {
                    $0.pendingOtherOpenOperations.formUnion(otherPendingOpenOperations)
                }
            }
            if let opening { setRetirementID(retirementBarrier.id, forOpenOperation: opening.operation) }
            if let updating { setRetirementID(retirementBarrier.id, forUpdateOperation: updating.operation) }
            let outcome = await resolveRetirement(retirementBarrier.id, allowExplicitRetry: true)
            return accountingForKnownCloseDebt(outcome)
        }

        if let previousContext {
            _ = preserveLegacyCloseReceipt(except: previousContext.ticket.contextID)
            let barrier = ensureRetirement(
                owner: previousContext.owner,
                contextID: previousContext.ticket.contextID,
                pendingOpenOperation: nil,
                pendingOtherOpenOperations: otherPendingOpenOperations,
                unknownRemoteAuthority: unknownOpenOutcome
            )
            if let updating,
                updating.contextID.utf8.elementsEqual(previousContext.ticket.contextID.utf8)
            {
                setRetirementID(barrier.id, forUpdateOperation: updating.operation)
            }
            let outcome = await resolveRetirement(barrier.id, allowExplicitRetry: true)
            return accountingForKnownCloseDebt(outcome)
        }

        if let updating {
            _ = preserveLegacyCloseReceipt(except: updating.contextID)
            let barrier = ensureRetirement(
                owner: updating.owner,
                contextID: updating.contextID,
                pendingOpenOperation: nil,
                pendingOtherOpenOperations: otherPendingOpenOperations,
                unknownRemoteAuthority: unknownOpenOutcome
            )
            setRetirementID(barrier.id, forUpdateOperation: updating.operation)
            let outcome = await resolveRetirement(barrier.id, allowExplicitRetry: true)
            return accountingForKnownCloseDebt(outcome)
        }

        if let opening {
            if let contextID = opening.contextID {
                _ = preserveLegacyCloseReceipt(except: contextID)
            }
            let barrier = ensureRetirement(
                owner: opening.owner,
                contextID: opening.contextID,
                pendingOpenOperation: opening.contextID == nil ? opening.operation : nil,
                pendingOtherOpenOperations: otherPendingOpenOperations,
                unknownRemoteAuthority: unknownOpenOutcome
            )
            setRetirementID(barrier.id, forOpenOperation: opening.operation)
            if opening.contextID == nil {
                updateRetirement(barrier.id) {
                    $0.outcome = .unconfirmed(.openInFlightWithoutTicket)
                }
                return .unconfirmed(.openInFlightWithoutTicket)
            }
            let outcome = await resolveRetirement(barrier.id, allowExplicitRetry: true)
            return accountingForKnownCloseDebt(outcome)
        }

        if let legacyCloseReceipt {
            let barrier = ensureRetirement(
                owner: nil,
                contextID: legacyCloseReceipt.contextID,
                pendingOpenOperation: nil,
                pendingOtherOpenOperations: otherPendingOpenOperations,
                unknownRemoteAuthority: unknownOpenOutcome
            )
            if legacyCloseReceipt.outcome == .acknowledged {
                updateRetirement(barrier.id) { $0.outcome = .acknowledged }
            }
            let outcome = await resolveRetirement(barrier.id, allowExplicitRetry: true)
            return accountingForKnownCloseDebt(outcome)
        }

        if let closeAttempt {
            let barrier = ensureRetirement(
                owner: nil,
                contextID: closeAttempt.contextID,
                pendingOpenOperation: nil,
                pendingOtherOpenOperations: otherPendingOpenOperations,
                unknownRemoteAuthority: unknownOpenOutcome
            )
            self.closeAttempt?.retirementID = barrier.id
            let outcome = await resolveRetirement(barrier.id, allowExplicitRetry: true)
            return accountingForKnownCloseDebt(outcome)
        }

        if let legacyCloseDebt {
            let barrier = ensureRetirement(
                owner: nil,
                contextID: legacyCloseDebt.contextID,
                pendingOpenOperation: nil,
                pendingOtherOpenOperations: otherPendingOpenOperations,
                unknownRemoteAuthority: unknownOpenOutcome
            )
            let outcome = await resolveRetirement(barrier.id, allowExplicitRetry: true)
            return accountingForKnownCloseDebt(outcome)
        }

        if unknownOpenOutcome || !otherPendingOpenOperations.isEmpty {
            let barrier = ensureRetirement(
                owner: nil,
                contextID: nil,
                pendingOpenOperation: nil,
                pendingOtherOpenOperations: otherPendingOpenOperations,
                unknownRemoteAuthority: unknownOpenOutcome
            )
            updateRetirement(barrier.id) { $0.outcome = .unconfirmed(.openOutcomeUnknown) }
            return await resolveRetirement(barrier.id, allowExplicitRetry: true)
        }

        return .notRequired
    }

    private func captureFreshSample(
        for owner: HLSPlaybackContextOwner,
        revision expectedRevision: UUID,
        operation expectedOperation: UUID,
        snapshotNotBefore: UInt64
    ) throws -> (HLSPlaybackSnapshot, HLSPlaybackContextTimingSample) {
        try requireCurrent(owner, revision: expectedRevision, operation: expectedOperation)
        guard let snapshot = snapshotProvider() else {
            throw HLSPlaybackContextReportingError.invalidSnapshot
        }
        try requireCurrent(owner, revision: expectedRevision, operation: expectedOperation)
        let now = try checkedNow()
        try requireFresh(snapshot, now: now, notBefore: snapshotNotBefore)
        guard let bufferedEnd = snapshot.contiguousBufferedEndSeconds else {
            throw HLSPlaybackContextReportingError.invalidSnapshot
        }
        let sample = HLSPlaybackContextTimingSample(
            playheadSeconds: snapshot.playheadSeconds,
            continuousBufferedEndSeconds: bufferedEnd,
            playbackRate: snapshot.playbackRate
        )
        do {
            try sample.validate()
        } catch {
            throw HLSPlaybackContextReportingError.invalidSnapshot
        }
        return (snapshot, sample)
    }

    private func requireCurrent(
        _ expectedOwner: HLSPlaybackContextOwner,
        revision expectedRevision: UUID,
        operation expectedOperation: UUID
    ) throws {
        try Task.checkCancellation()
        guard revision == expectedRevision, operationID == expectedOperation,
            let currentOwner = ownerProvider(),
            currentOwner == expectedOwner,
            ownerMatchesServer(currentOwner)
        else {
            throw HLSPlaybackContextReportingError.ownerChanged
        }
    }

    private func requireFresh(
        _ snapshot: HLSPlaybackSnapshot,
        now: UInt64,
        notBefore receiptBarrier: UInt64
    ) throws {
        guard snapshot.isPlaying else {
            throw HLSPlaybackContextReportingError.playbackNotActive
        }
        guard snapshot.playheadSeconds.isFinite,
            snapshot.playheadSeconds >= 0,
            let bufferedEnd = snapshot.contiguousBufferedEndSeconds,
            bufferedEnd.isFinite,
            bufferedEnd >= snapshot.playheadSeconds,
            snapshot.playbackRate.isFinite,
            snapshot.playbackRate > 0,
            snapshot.playbackRate <= 16
        else {
            throw HLSPlaybackContextReportingError.invalidSnapshot
        }
        guard snapshot.observedAtNanoseconds >= receiptBarrier else {
            throw HLSPlaybackContextReportingError.snapshotPredatesTicket
        }
        guard now >= snapshot.observedAtNanoseconds else {
            throw HLSPlaybackContextReportingError.clockAnomaly
        }
        guard now - snapshot.observedAtNanoseconds <= Self.maximumSnapshotAgeNanoseconds else {
            throw HLSPlaybackContextReportingError.staleSnapshot
        }
    }

    private func requireLive(
        _ ticket: HLSPlaybackContextTicket,
        requestStarted: UInt64,
        now: UInt64
    ) throws {
        guard (1...5_000).contains(ticket.validityMillis) else {
            throw HLSPlaybackContextReportingError.invalidTicket
        }
        guard now >= requestStarted else {
            throw HLSPlaybackContextReportingError.clockAnomaly
        }
        let validityNanoseconds = ticket.validityMillis * 1_000_000
        guard now - requestStarted < validityNanoseconds else {
            throw HLSPlaybackContextReportingError.staleTicket
        }
    }

    private func checkedNow() throws -> UInt64 {
        guard let now = monotonicNowNanoseconds() else {
            throw HLSPlaybackContextReportingError.clockAnomaly
        }
        if let previous = lastMonotonicNowNanoseconds, now < previous {
            throw HLSPlaybackContextReportingError.clockAnomaly
        }
        lastMonotonicNowNanoseconds = now
        return now
    }

    private func ownerMatchesServer(_ owner: HLSPlaybackContextOwner) -> Bool {
        owner.serverID.utf8.elementsEqual(serverSummary.id.utf8)
            && owner.sessionGeneration > 0
    }

    private func httpReaderID(for owner: HLSPlaybackContextOwner) -> String? {
        guard serverSummary.supportsHLSPlaybackReaderBinding else { return nil }
        return owner.ownerToken.uuidString.lowercased()
    }

    private func recordOpenedTicket(_ ticket: HLSPlaybackContextTicket, operation: UUID) {
        if var opening = inFlightOpen, opening.operation == operation {
            opening.contextID = ticket.contextID
            inFlightOpen = opening
        } else if var opening = legacyInFlightOpen, opening.operation == operation {
            opening.contextID = ticket.contextID
            legacyInFlightOpen = opening
        }
        updateRetirement(forOpenOperation: operation) {
            if $0.pendingOpenOperation == operation {
                $0.contextID = ticket.contextID
                $0.pendingOpenOperation = nil
                $0.closeAttempted = false
                $0.outcome = nil
            } else if $0.pendingOtherOpenOperations.remove(operation) != nil {
                $0.pendingOtherOpenCloseContextIDs[operation] = ticket.contextID
            }
        }
    }

    private func ensureRetirement(
        owner: HLSPlaybackContextOwner?,
        contextID: String?,
        pendingOpenOperation: UUID?,
        pendingOtherOpenOperations: Set<UUID> = [],
        pendingOtherOpenCloseContextIDs: [UUID: String] = [:],
        unknownRemoteAuthority: Bool = false
    ) -> RetirementBarrier {
        if var existing = retirementBarrier {
            if existing.contextID == nil, let contextID, existing.owner == owner {
                existing.contextID = contextID
                existing.pendingOpenOperation = nil
                existing.closeAttempted = false
                existing.outcome = nil
            }
            let primaryOpenOperation = existing.pendingOpenOperation
            existing.pendingOtherOpenOperations.formUnion(
                pendingOtherOpenOperations.filter { $0 != primaryOpenOperation }
            )
            existing.pendingOtherOpenCloseContextIDs.merge(
                pendingOtherOpenCloseContextIDs,
                uniquingKeysWith: { current, _ in current }
            )
            existing.unknownRemoteAuthority = existing.unknownRemoteAuthority || unknownRemoteAuthority
            retirementBarrier = existing
            return existing
        }

        let id = UUID()
        let initialOutcome: HLSPlaybackContextWithdrawalOutcome?
        if unknownRemoteAuthority {
            initialOutcome = .unconfirmed(.openOutcomeUnknown)
        } else if pendingOpenOperation != nil {
            initialOutcome = .unconfirmed(.openInFlightWithoutTicket)
        } else if contextID == nil {
            initialOutcome = .unconfirmed(.openOutcomeUnknown)
        } else {
            initialOutcome = nil
        }
        let barrier = RetirementBarrier(
            id: id,
            owner: owner,
            contextID: contextID,
            pendingOpenOperation: pendingOpenOperation,
            pendingOtherOpenOperations: pendingOtherOpenOperations,
            pendingOtherOpenCloseContextIDs: pendingOtherOpenCloseContextIDs,
            unknownRemoteAuthority: unknownRemoteAuthority,
            closeAttempted: false,
            outcome: initialOutcome
        )
        retirementBarrier = barrier
        return barrier
    }

    private func updateRetirement(
        _ id: UUID,
        _ update: (inout RetirementBarrier) -> Void
    ) {
        guard var barrier = retirementBarrier, barrier.id == id else { return }
        update(&barrier)
        retirementBarrier = barrier
    }

    private func updateRetirement(
        forOpenOperation operation: UUID,
        _ update: (inout RetirementBarrier) -> Void
    ) {
        guard let barrier = retirementBarrier,
            barrier.pendingOpenOperation == operation
                || barrier.pendingOtherOpenOperations.contains(operation)
        else {
            return
        }
        updateRetirement(barrier.id, update)
    }

    private func setRetirementID(_ retirementID: UUID, forOpenOperation operation: UUID) {
        if inFlightOpen?.operation == operation {
            inFlightOpen?.retirementID = retirementID
        }
        if legacyInFlightOpen?.operation == operation {
            legacyInFlightOpen?.retirementID = retirementID
        }
    }

    private func setRetirementID(_ retirementID: UUID, forUpdateOperation operation: UUID) {
        if inFlightUpdate?.operation == operation {
            inFlightUpdate?.retirementID = retirementID
        }
        if legacyInFlightUpdate?.operation == operation {
            legacyInFlightUpdate?.retirementID = retirementID
        }
    }

    private func recordUnknownOpenOutcome(operation: UUID) {
        if let barrier = retirementBarrier {
            updateRetirement(barrier.id) {
                if $0.pendingOpenOperation == operation {
                    $0.pendingOpenOperation = nil
                }
                $0.pendingOtherOpenOperations.remove(operation)
                $0.pendingOtherOpenCloseContextIDs.removeValue(forKey: operation)
                $0.unknownRemoteAuthority = true
                if $0.contextID == nil {
                    $0.outcome = .unconfirmed(.openOutcomeUnknown)
                }
            }
        } else {
            legacyUnknownOpenOutcome = true
        }
    }

    private func hasUnresolvedOpenAuthority(_ barrier: RetirementBarrier) -> Bool {
        barrier.unknownRemoteAuthority
            || barrier.pendingOpenOperation != nil
            || !barrier.pendingOtherOpenOperations.isEmpty
            || !barrier.pendingOtherOpenCloseContextIDs.isEmpty
    }

    private func clearAcknowledgedOtherOpenClosures(_ contextID: String) {
        guard var barrier = retirementBarrier else { return }
        barrier.pendingOtherOpenCloseContextIDs = barrier.pendingOtherOpenCloseContextIDs.filter {
            !$0.value.utf8.elementsEqual(contextID.utf8)
        }
        retirementBarrier = barrier
    }

    private func markUpdatesClosed(_ contextID: String) {
        if var update = inFlightUpdate,
            update.contextID.utf8.elementsEqual(contextID.utf8)
        {
            update.closeAcknowledged = true
            inFlightUpdate = update
        }
        if var update = legacyInFlightUpdate,
            update.contextID.utf8.elementsEqual(contextID.utf8)
        {
            update.closeAcknowledged = true
            legacyInFlightUpdate = update
        }
    }

    private func preserveLegacyCloseReceipt(except contextID: String) -> Bool {
        guard let receipt = legacyCloseReceipt,
            receipt.outcome != .acknowledged,
            !receipt.contextID.utf8.elementsEqual(contextID.utf8)
        else {
            return true
        }

        if let legacyCloseDebt {
            guard legacyCloseDebt.contextID.utf8.elementsEqual(receipt.contextID.utf8) else {
                return false
            }
        } else {
            legacyCloseDebt = receipt
        }
        legacyCloseReceipt = nil
        return true
    }

    private func recordKnownCloseOutcome(
        contextID: String,
        outcome: HLSPlaybackContextWithdrawalOutcome
    ) {
        if outcome == .acknowledged {
            if legacyCloseDebt?.contextID.utf8.elementsEqual(contextID.utf8) == true {
                legacyCloseDebt = nil
            }
            if legacyCloseReceipt?.contextID.utf8.elementsEqual(contextID.utf8) == true {
                legacyCloseReceipt = LegacyCloseReceipt(contextID: contextID, outcome: outcome)
            }
            return
        }

        if legacyCloseReceipt?.contextID.utf8.elementsEqual(contextID.utf8) == true {
            legacyCloseReceipt = LegacyCloseReceipt(contextID: contextID, outcome: outcome)
        } else if legacyCloseDebt?.contextID.utf8.elementsEqual(contextID.utf8) == true {
            legacyCloseDebt = LegacyCloseReceipt(contextID: contextID, outcome: outcome)
        } else if legacyCloseDebt == nil {
            legacyCloseDebt = LegacyCloseReceipt(contextID: contextID, outcome: outcome)
        }
    }

    private func accountingForKnownCloseDebt(
        _ outcome: HLSPlaybackContextWithdrawalOutcome
    ) -> HLSPlaybackContextWithdrawalOutcome {
        outcome == .acknowledged && legacyCloseDebt != nil
            ? .unconfirmed(.anotherContextRetiring)
            : outcome
    }

    private func clearAcknowledgedRetirement(_ id: UUID) {
        guard let barrier = retirementBarrier,
            barrier.id == id,
            barrier.outcome == .acknowledged,
            !hasUnresolvedOpenAuthority(barrier),
            legacyCloseDebt == nil,
            closeAttempt == nil
        else {
            return
        }
        retirementBarrier = nil
    }

    private func resolveRetirement(
        _ retirementID: UUID,
        allowExplicitRetry: Bool
    ) async -> HLSPlaybackContextWithdrawalOutcome {
        guard var barrier = retirementBarrier, barrier.id == retirementID else {
            return .unconfirmed(.anotherContextRetiring)
        }
        if Task.isCancelled {
            return .unconfirmed(.cancelled)
        }
        if barrier.outcome == .acknowledged {
            if !hasUnresolvedOpenAuthority(barrier) {
                return .acknowledged
            }
            if allowExplicitRetry,
                let (operation, contextID) = barrier.pendingOtherOpenCloseContextIDs.first
            {
                return await resolvePendingOtherOpenClose(
                    retirementID,
                    operation: operation,
                    contextID: contextID
                )
            }
            return .unconfirmed(.openOutcomeUnknown)
        }
        if barrier.unknownRemoteAuthority, barrier.contextID == nil {
            if allowExplicitRetry,
                let (operation, contextID) = barrier.pendingOtherOpenCloseContextIDs.first
            {
                return await resolvePendingOtherOpenClose(
                    retirementID,
                    operation: operation,
                    contextID: contextID
                )
            }
            return .unconfirmed(.openOutcomeUnknown)
        }
        guard let contextID = barrier.contextID else {
            if allowExplicitRetry,
                let (operation, contextID) = barrier.pendingOtherOpenCloseContextIDs.first
            {
                return await resolvePendingOtherOpenClose(
                    retirementID,
                    operation: operation,
                    contextID: contextID
                )
            }
            return barrier.outcome ?? .unconfirmed(.openOutcomeUnknown)
        }

        if let closeAttempt {
            guard closeAttempt.contextID.utf8.elementsEqual(contextID.utf8) else {
                return .unconfirmed(.anotherContextRetiring)
            }
            if closeAttempt.retirementID == nil {
                self.closeAttempt?.retirementID = retirementID
            } else if closeAttempt.retirementID != retirementID {
                return .unconfirmed(.anotherContextRetiring)
            }
            let result = await closeAttempt.waiter.wait(for: Self.closeWaitDuration)
            guard !Task.isCancelled else { return .unconfirmed(.cancelled) }
            return applyingUnknownOpenOutcome(to: result, retirementID: retirementID)
        }

        if barrier.closeAttempted {
            guard allowExplicitRetry else {
                return barrier.outcome ?? .unconfirmed(.closeRequestFailed)
            }
            barrier.closeAttempted = false
            barrier.outcome = nil
        }

        let request = HLSPlaybackContextCloseRequest(contextID: contextID)
        guard (try? request.validate()) != nil else {
            barrier.closeAttempted = true
            let outcome: HLSPlaybackContextWithdrawalOutcome = .unconfirmed(.invalidContextID)
            barrier.outcome = outcome
            retirementBarrier = barrier
            return outcome
        }

        barrier.closeAttempted = true
        barrier.outcome = nil
        retirementBarrier = barrier
        let result = await beginCloseAttempt(contextID: contextID, retirementID: retirementID)
        return applyingUnknownOpenOutcome(to: result, retirementID: retirementID)
    }

    private func resolvePendingOtherOpenClose(
        _ retirementID: UUID,
        operation: UUID,
        contextID: String
    ) async -> HLSPlaybackContextWithdrawalOutcome {
        guard let barrier = retirementBarrier, barrier.id == retirementID,
            barrier.pendingOtherOpenCloseContextIDs[operation]?.utf8.elementsEqual(contextID.utf8)
                == true
        else {
            return .unconfirmed(.anotherContextRetiring)
        }
        if Task.isCancelled {
            return .unconfirmed(.cancelled)
        }
        if let closeAttempt {
            guard closeAttempt.contextID.utf8.elementsEqual(contextID.utf8) else {
                return .unconfirmed(.anotherContextRetiring)
            }
            if closeAttempt.retirementID == nil {
                self.closeAttempt?.retirementID = retirementID
            } else if closeAttempt.retirementID != retirementID {
                return .unconfirmed(.anotherContextRetiring)
            }
            let result = await closeAttempt.waiter.wait(for: Self.closeWaitDuration)
            guard !Task.isCancelled else { return .unconfirmed(.cancelled) }
            return applyingUnknownOpenOutcome(to: result, retirementID: retirementID)
        }
        let result = await beginCloseAttempt(contextID: contextID, retirementID: nil)
        return applyingUnknownOpenOutcome(to: result, retirementID: retirementID)
    }

    private func beginCloseAttempt(
        contextID: String,
        retirementID: UUID?
    ) async -> HLSPlaybackContextWithdrawalOutcome {
        let request = HLSPlaybackContextCloseRequest(contextID: contextID)
        guard (try? request.validate()) != nil else {
            return .unconfirmed(.invalidContextID)
        }
        let attemptID = UUID()
        let waiter = HLSPlaybackContextCloseWaiter()
        let client = self.client

        let task = Task.detached(priority: .utility) { [weak self, waiter, client] in
            let outcome: HLSPlaybackContextWithdrawalOutcome
            do {
                let response = try await client.closeHLSPlaybackContext(request)
                outcome = response.closed ? .acknowledged : .unconfirmed(.closeRejected)
            } catch {
                outcome = .unconfirmed(.closeRequestFailed)
            }
            if let self {
                await self.finishCloseAttempt(
                    attemptID,
                    contextID: contextID,
                    outcome: outcome,
                    waiter: waiter
                )
            } else {
                await waiter.finish(outcome)
            }
        }
        closeAttempt = CloseAttempt(
            id: attemptID,
            retirementID: retirementID,
            contextID: contextID,
            waiter: waiter,
            task: task
        )
        let result = await waiter.wait(for: Self.closeWaitDuration)
        guard !Task.isCancelled else { return .unconfirmed(.cancelled) }
        return result
    }

    private func applyingUnknownOpenOutcome(
        to outcome: HLSPlaybackContextWithdrawalOutcome,
        retirementID: UUID
    ) -> HLSPlaybackContextWithdrawalOutcome {
        guard outcome == .acknowledged,
            retirementBarrier?.id == retirementID,
            let retirementBarrier,
            hasUnresolvedOpenAuthority(retirementBarrier)
        else {
            return outcome
        }
        return .unconfirmed(.openOutcomeUnknown)
    }

    private func finishCloseAttempt(
        _ attemptID: UUID,
        contextID: String,
        outcome: HLSPlaybackContextWithdrawalOutcome,
        waiter: HLSPlaybackContextCloseWaiter
    ) async {
        guard let attempt = closeAttempt, attempt.id == attemptID else {
            await waiter.finish(outcome)
            return
        }
        closeAttempt = nil
        if outcome == .acknowledged {
            markUpdatesClosed(contextID)
            clearAcknowledgedOtherOpenClosures(contextID)
        }
        if let retirementID = attempt.retirementID,
            var barrier = retirementBarrier,
            barrier.id == retirementID,
            barrier.contextID?.utf8.elementsEqual(contextID.utf8) == true
        {
            barrier.outcome = outcome
            retirementBarrier = barrier
        }
        if outcome == .acknowledged,
            var barrier = retirementBarrier,
            barrier.contextID == nil,
            barrier.pendingOpenOperation == nil,
            barrier.pendingOtherOpenOperations.isEmpty,
            barrier.pendingOtherOpenCloseContextIDs.isEmpty,
            !barrier.unknownRemoteAuthority
        {
            barrier.outcome = .acknowledged
            retirementBarrier = barrier
        }
        recordKnownCloseOutcome(contextID: contextID, outcome: outcome)
        await waiter.finish(outcome)
    }

    private func closeBestEffort(_ contextID: String) async {
        if legacyCloseDebt?.contextID.utf8.elementsEqual(contextID.utf8) == true {
            return
        }
        let request = HLSPlaybackContextCloseRequest(contextID: contextID)
        guard (try? request.validate()) != nil else {
            guard preserveLegacyCloseReceipt(except: contextID) else { return }
            legacyCloseReceipt = LegacyCloseReceipt(
                contextID: contextID,
                outcome: .unconfirmed(.invalidContextID)
            )
            return
        }
        if legacyCloseReceipt?.contextID.utf8.elementsEqual(contextID.utf8) == true {
            return
        }
        guard preserveLegacyCloseReceipt(except: contextID) else { return }
        if let closeAttempt {
            guard closeAttempt.contextID.utf8.elementsEqual(contextID.utf8) else {
                legacyCloseReceipt = LegacyCloseReceipt(
                    contextID: contextID,
                    outcome: .unconfirmed(.anotherContextRetiring)
                )
                return
            }
            let outcome = await closeAttempt.waiter.wait(for: Self.closeWaitDuration)
            if closeAttempt.retirementID == nil {
                legacyCloseReceipt = LegacyCloseReceipt(contextID: contextID, outcome: outcome)
            }
            return
        }

        let attemptID = UUID()
        let waiter = HLSPlaybackContextCloseWaiter()
        let client = self.client
        legacyCloseReceipt = LegacyCloseReceipt(contextID: contextID, outcome: .unconfirmed(.timedOut))
        let task = Task.detached(priority: .utility) { [weak self, waiter, client] in
            let outcome: HLSPlaybackContextWithdrawalOutcome
            do {
                let response = try await client.closeHLSPlaybackContext(request)
                outcome = response.closed ? .acknowledged : .unconfirmed(.closeRejected)
            } catch {
                outcome = .unconfirmed(.closeRequestFailed)
            }
            if let self {
                await self.finishCloseAttempt(
                    attemptID,
                    contextID: contextID,
                    outcome: outcome,
                    waiter: waiter
                )
            } else {
                await waiter.finish(outcome)
            }
        }
        closeAttempt = CloseAttempt(
            id: attemptID,
            retirementID: nil,
            contextID: contextID,
            waiter: waiter,
            task: task
        )
        let outcome = await waiter.wait(for: Self.closeWaitDuration)
        if legacyCloseReceipt?.contextID.utf8.elementsEqual(contextID.utf8) == true,
            closeAttempt?.retirementID == nil
        {
            legacyCloseReceipt = LegacyCloseReceipt(contextID: contextID, outcome: outcome)
        }
    }

    private func openOperation(_ operation: UUID?) -> InFlightOpen? {
        guard let operation else { return nil }
        if let current = inFlightOpen, current.operation == operation {
            return current
        }
        if let legacy = legacyInFlightOpen, legacy.operation == operation {
            return legacy
        }
        return nil
    }

    private func updateOperation(_ operation: UUID?) -> InFlightUpdate? {
        guard let operation else { return nil }
        if let current = inFlightUpdate, current.operation == operation {
            return current
        }
        if let legacy = legacyInFlightUpdate, legacy.operation == operation {
            return legacy
        }
        return nil
    }

    private func retirementID(for operation: UUID) -> UUID? {
        openOperation(operation)?.retirementID ?? updateOperation(operation)?.retirementID
    }

    private func clearInFlightOperation(_ operation: UUID) {
        if inFlightOpen?.operation == operation {
            inFlightOpen = nil
        }
        if inFlightUpdate?.operation == operation {
            inFlightUpdate = nil
        }
        if legacyInFlightOpen?.operation == operation {
            legacyInFlightOpen = nil
        }
        if legacyInFlightUpdate?.operation == operation {
            legacyInFlightUpdate = nil
        }
    }

    private func sanitized(_ error: any Error) -> any Error {
        if Task.isCancelled || error is CancellationError {
            return CancellationError()
        }
        if let error = error as? HLSPlaybackContextReportingError {
            return error
        }
        if let error = error as? CacheControlClientUnsupportedOperation,
            error == .hlsPlaybackContext
        {
            return HLSPlaybackContextReportingError.unsupportedServer
        }
        if error is HLSPlaybackContextValidationError {
            return HLSPlaybackContextReportingError.invalidTicket
        }
        return HLSPlaybackContextReportingError.requestFailed
    }
}

private actor HLSPlaybackContextCloseWaiter {
    private var deliveryOutcome: HLSPlaybackContextWithdrawalOutcome?
    private var continuations: [UUID: CheckedContinuation<HLSPlaybackContextWithdrawalOutcome, Never>] = [:]
    private var timeoutTask: Task<Void, Never>?

    func wait(for timeout: Duration) async -> HLSPlaybackContextWithdrawalOutcome {
        let waiterID = UUID()
        return await withTaskCancellationHandler {
            await withCheckedContinuation { continuation in
                if Task.isCancelled {
                    continuation.resume(returning: .unconfirmed(.cancelled))
                    return
                }
                if let deliveryOutcome {
                    continuation.resume(returning: deliveryOutcome)
                    return
                }

                continuations[waiterID] = continuation
                if timeoutTask == nil {
                    timeoutTask = Task.detached { [weak self] in
                        do {
                            try await Task.sleep(for: timeout)
                        } catch {
                            return
                        }
                        await self?.finish(.unconfirmed(.timedOut))
                    }
                }
            }
        } onCancel: {
            Task { await self.cancel(waiterID) }
        }
    }

    func finish(_ outcome: HLSPlaybackContextWithdrawalOutcome) {
        guard deliveryOutcome == nil else { return }
        deliveryOutcome = outcome
        timeoutTask?.cancel()
        timeoutTask = nil
        let pendingContinuations = Array(continuations.values)
        self.continuations.removeAll()
        for continuation in pendingContinuations {
            continuation.resume(returning: outcome)
        }
    }

    private func cancel(_ waiterID: UUID) {
        guard let continuation = continuations.removeValue(forKey: waiterID) else { return }
        continuation.resume(returning: .unconfirmed(.cancelled))
    }
}
