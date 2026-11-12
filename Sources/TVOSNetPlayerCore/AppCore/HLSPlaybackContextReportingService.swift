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

    private static let maximumSnapshotAgeNanoseconds: UInt64 = 5_000_000_000

    private let client: any CacheControlClient
    private let serverSummary: CacheServerSummary
    private let ownerProvider: @MainActor () -> HLSPlaybackContextOwner?
    private let snapshotProvider: @MainActor () -> HLSPlaybackSnapshot?
    private let monotonicNowNanoseconds: @MainActor () -> UInt64

    private var revision = UUID()
    private var operationID: UUID?
    private var activeContext: ActiveContext?
    private var lastMonotonicNowNanoseconds: UInt64?

    public init(
        client: any CacheControlClient,
        serverSummary: CacheServerSummary,
        ownerProvider: @escaping @MainActor () -> HLSPlaybackContextOwner?,
        snapshotProvider: @escaping @MainActor () -> HLSPlaybackSnapshot?,
        monotonicNowNanoseconds: @escaping @MainActor () -> UInt64 = {
            DispatchTime.now().uptimeNanoseconds
        }
    ) {
        self.client = client
        self.serverSummary = serverSummary
        self.ownerProvider = ownerProvider
        self.snapshotProvider = snapshotProvider
        self.monotonicNowNanoseconds = monotonicNowNanoseconds
    }

    /// Opens a context, captures timing only after the first ticket, then sends its first sample.
    public func start() async throws {
        let operation = UUID()
        revision = operation
        operationID = operation
        let previousContext = activeContext
        activeContext = nil
        defer {
            if operationID == operation {
                operationID = nil
            }
        }

        if let previousContext {
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
            expectedSessionGeneration: owner.sessionGeneration
        )
        try openRequest.validate()
        try requireCurrent(owner, revision: operation, operation: operation)
        let openRequestStarted = try checkedNow()
        var openedTicket: HLSPlaybackContextTicket?

        do {
            let ticket = try await client.openHLSPlaybackContext(openRequest)
            openedTicket = ticket
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
                sample: sample
            )
            try updateRequest.validate()

            let updateRequestStarted = try checkedNow()
            try requireCurrent(owner, revision: operation, operation: operation)
            try requireLive(ticket, requestStarted: openRequestStarted, now: updateRequestStarted)
            try requireFresh(snapshot, now: updateRequestStarted, notBefore: ticketReceivedAt)

            let nextTicket = try await client.updateHLSPlaybackContext(updateRequest)
            try Task.checkCancellation()
            let afterUpdate = try checkedNow()
            try requireCurrent(owner, revision: operation, operation: operation)
            try requireFresh(snapshot, now: afterUpdate, notBefore: ticketReceivedAt)
            try requireLive(ticket, requestStarted: openRequestStarted, now: afterUpdate)
            try nextTicket.validate(matching: updateRequest)
            try requireLive(nextTicket, requestStarted: updateRequestStarted, now: afterUpdate)

            activeContext = ActiveContext(
                revision: operation,
                owner: owner,
                ticket: nextTicket,
                ticketRequestStartedNanoseconds: updateRequestStarted,
                ticketReceivedAtNanoseconds: afterUpdate
            )
        } catch {
            if revision == operation {
                activeContext = nil
            }
            if let openedTicket {
                await closeBestEffort(openedTicket.contextID)
            }
            throw sanitized(error)
        }
    }

    /// Sends one fresh sample. Concurrent updates are rejected instead of overlapping sequences.
    public func update() async throws {
        guard operationID == nil else {
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
                sample: sample
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

            activeContext = ActiveContext(
                revision: context.revision,
                owner: context.owner,
                ticket: nextTicket,
                ticketRequestStartedNanoseconds: requestStarted,
                ticketReceivedAtNanoseconds: afterUpdate
            )
        } catch {
            if revision == context.revision, operationID == operation,
                activeContext?.ticket.contextID.utf8.elementsEqual(context.ticket.contextID.utf8) == true
            {
                activeContext = nil
                revision = UUID()
            }
            await closeBestEffort(context.ticket.contextID)
            throw sanitized(error)
        }
    }

    /// Withdraws local authority immediately and best-effort closes only the saved context ID.
    public func withdraw() async {
        let previousContext = activeContext
        revision = UUID()
        operationID = nil
        activeContext = nil
        if let previousContext {
            await closeBestEffort(previousContext.ticket.contextID)
        }
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
        let now = monotonicNowNanoseconds()
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

    private func closeBestEffort(_ contextID: String) async {
        let request = HLSPlaybackContextCloseRequest(contextID: contextID)
        guard (try? request.validate()) != nil else { return }
        _ = try? await client.closeHLSPlaybackContext(request)
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
