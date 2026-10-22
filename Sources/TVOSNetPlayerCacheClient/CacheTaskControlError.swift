import Foundation
import GRPCCore

public enum CacheTaskControlError: Error, Equatable, Sendable, LocalizedError {
    case unavailable
    case deadlineExceeded
    case unauthenticated
    case permissionDenied
    case notFound
    case invalidArgument
    case failedPrecondition
    case unimplemented
    case cancelled
    case unknown

    public var canRetryObservation: Bool {
        switch self {
        case .unavailable, .deadlineExceeded, .cancelled, .unknown:
            true
        case .unauthenticated, .permissionDenied, .notFound, .invalidArgument,
            .failedPrecondition, .unimplemented:
            false
        }
    }

    public var errorDescription: String? {
        switch self {
        case .unavailable:
            "The cache server is temporarily unavailable."
        case .deadlineExceeded:
            "The cache server did not respond before the deadline."
        case .unauthenticated:
            "Authentication with the cache server failed."
        case .permissionDenied:
            "The cache server denied this operation."
        case .notFound:
            "The requested task was not found."
        case .invalidArgument:
            "The task request is invalid."
        case .failedPrecondition:
            "The task cannot be changed in its current state."
        case .unimplemented:
            "The cache server does not support this task operation."
        case .cancelled:
            "The task-control request was cancelled."
        case .unknown:
            "The cache server returned an unexpected task-control error."
        }
    }

    static func fromRPCCode(_ code: RPCError.Code) -> Self {
        switch code {
        case .cancelled: .cancelled
        case .unknown: .unknown
        case .invalidArgument: .invalidArgument
        case .deadlineExceeded: .deadlineExceeded
        case .notFound: .notFound
        case .permissionDenied: .permissionDenied
        case .failedPrecondition: .failedPrecondition
        case .unimplemented: .unimplemented
        case .unavailable: .unavailable
        case .unauthenticated: .unauthenticated
        default: .unknown
        }
    }

    static func mapTransportError(
        _ error: any Error,
        callerIsCancelled: Bool
    ) -> any Error {
        guard !callerIsCancelled, !(error is CancellationError) else {
            return CancellationError()
        }
        guard let rpcError = error as? RPCError else {
            return Self.unknown
        }
        return Self.fromRPCCode(rpcError.code)
    }
}
