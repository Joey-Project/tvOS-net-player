import XCTest
import GRPCCore
@testable import TVOSNetPlayerCacheClient

final class GRPCCacheControlClientTests: XCTestCase {
    func testTaskControlErrorMapsSupportedGRPCStatusCodes() {
        let mappings: [(RPCError.Code, CacheTaskControlError)] = [
            (.unavailable, .unavailable),
            (.deadlineExceeded, .deadlineExceeded),
            (.unauthenticated, .unauthenticated),
            (.permissionDenied, .permissionDenied),
            (.notFound, .notFound),
            (.invalidArgument, .invalidArgument),
            (.failedPrecondition, .failedPrecondition),
            (.unimplemented, .unimplemented),
            (.cancelled, .cancelled),
            (.unknown, .unknown),
        ]

        for (code, expected) in mappings {
            let rpcError = RPCError(code: code, message: "PRIVATE_UPSTREAM_DETAIL")
            let mappedError = CacheTaskControlError.mapTransportError(
                rpcError,
                callerIsCancelled: false
            )

            XCTAssertEqual(mappedError as? CacheTaskControlError, expected)
            XCTAssertFalse(mappedError.localizedDescription.contains("PRIVATE_UPSTREAM_DETAIL"))
        }
    }

    func testTaskControlErrorOnlyRetriesObservationForTransientCases() {
        let errors: [(CacheTaskControlError, Bool)] = [
            (.unavailable, true),
            (.deadlineExceeded, true),
            (.unauthenticated, false),
            (.permissionDenied, false),
            (.notFound, false),
            (.invalidArgument, false),
            (.failedPrecondition, false),
            (.unimplemented, false),
            (.cancelled, true),
            (.unknown, true),
        ]

        for (error, canRetry) in errors {
            XCTAssertEqual(error.canRetryObservation, canRetry, "Unexpected retry policy for \(error).")
        }
    }

    func testUnknownTaskControlErrorRedactsUnderlyingTransportText() {
        let diagnosticSentinel = "PRIVATE_UPSTREAM_DETAIL"
        let nonGRPCError = CacheTaskControlError.mapTransportError(
            SentinelTransportError(message: diagnosticSentinel),
            callerIsCancelled: false
        )

        XCTAssertEqual(nonGRPCError as? CacheTaskControlError, .unknown)
        XCTAssertFalse(nonGRPCError.localizedDescription.contains(diagnosticSentinel))
    }

    func testUnclassifiedGRPCStatusCodesBecomeRedactedUnknown() {
        let codes: [RPCError.Code] = [
            .alreadyExists,
            .resourceExhausted,
            .aborted,
            .outOfRange,
            .internalError,
            .dataLoss,
        ]
        let diagnosticSentinel = "PRIVATE_UPSTREAM_DETAIL"

        for code in codes {
            let rpcError = RPCError(code: code, message: diagnosticSentinel)
            let mappedError = CacheTaskControlError.mapTransportError(
                rpcError,
                callerIsCancelled: false
            )

            XCTAssertEqual(mappedError as? CacheTaskControlError, .unknown)
            XCTAssertFalse(mappedError.localizedDescription.contains(diagnosticSentinel))
            XCTAssertTrue((mappedError as? CacheTaskControlError)?.canRetryObservation == true)
        }
    }

    func testLocalCancellationRemainsCancellationErrorAndTakesPrecedence() {
        let cancellation = CacheTaskControlError.mapTransportError(
            CancellationError(),
            callerIsCancelled: false
        )
        XCTAssertTrue(cancellation is CancellationError)

        let callerCancelled = CacheTaskControlError.mapTransportError(
            SentinelTransportError(message: "transport failure"),
            callerIsCancelled: true
        )
        XCTAssertTrue(callerCancelled is CancellationError)
    }

    func testLoginStartDeadlineDefaultsToNinetySeconds() {
        let client = GRPCCacheControlClient(
            endpoint: CacheServerEndpoint(host: "localhost"),
            rpcTimeout: .seconds(10)
        )

        XCTAssertEqual(client.loginStartCallOptions.timeout, .seconds(90))
    }

    func testLoginStartDeadlineDoesNotShortenLongerConfiguredTimeout() {
        let client = GRPCCacheControlClient(
            endpoint: CacheServerEndpoint(host: "localhost"),
            rpcTimeout: .seconds(120)
        )

        XCTAssertEqual(client.loginStartCallOptions.timeout, .seconds(120))
    }

    func testResolutionStartDeadlineHasOneHundredTwentySecondFloor() {
        let client = GRPCCacheControlClient(
            endpoint: CacheServerEndpoint(host: "localhost"),
            rpcTimeout: .seconds(10)
        )

        XCTAssertEqual(client.resolutionStartCallOptions.timeout, .seconds(120))
    }

    func testResolutionStartDeadlinePreservesLongerConfiguredTimeout() {
        let client = GRPCCacheControlClient(
            endpoint: CacheServerEndpoint(host: "localhost"),
            rpcTimeout: .seconds(180)
        )

        XCTAssertEqual(client.resolutionStartCallOptions.timeout, .seconds(180))
    }

    func testCacheFillStatusProgressHandlesUnknownZeroAndOverflowingValues() {
        let unknownTotal = HlsCacheFillStatus(state: .filling, completedBytes: 400)
        XCTAssertNil(unknownTotal.progressFraction)
        XCTAssertNil(unknownTotal.progressPercentLabel)
        XCTAssertNil(unknownTotal.progressBytes)

        let zeroTotal = HlsCacheFillStatus(
            state: .filling,
            completedBytes: 0,
            totalBytes: 0,
            totalBytesKnown: true
        )
        XCTAssertNil(zeroTotal.progressFraction)

        let overflowRatio = HlsCacheFillStatus(
            state: .filling,
            completedBytes: UInt64.max,
            totalBytes: 1,
            totalBytesKnown: true
        )
        XCTAssertEqual(overflowRatio.progressFraction, 1)
        XCTAssertEqual(overflowRatio.progressPercentLabel, "100%")
        XCTAssertEqual(overflowRatio.progressBytes, 1)
    }

    func testTaskAndInlineResultMapCacheFillStatusAndPreserveOlderAbsence() {
        var taskProto = TvosNetPlayer_V1_Task()
        taskProto.hlsCacheFillStatus.state = .blockedQuota
        taskProto.hlsCacheFillStatus.failureKind = .persistence
        taskProto.hlsCacheFillStatus.completedBytes = 512
        taskProto.hlsCacheFillStatus.totalBytes = 2_048
        taskProto.hlsCacheFillStatus.totalBytesKnown = true
        taskProto.hlsCacheFillStatus.representationID = "video-720p"
        taskProto.hlsCacheFillStatus.message = "Quota admission is unavailable."

        var resultProto = TvosNetPlayer_V1_BilibiliTaskResultItem()
        resultProto.id = "result-1"
        resultProto.identity.kind = .videoPage
        resultProto.identity.cid = 42
        resultProto.hlsCacheFillStatus.state = .filling
        resultProto.hlsCacheFillStatus.completedBytes = 128
        resultProto.hlsCacheFillStatus.totalBytes = 512
        resultProto.hlsCacheFillStatus.totalBytesKnown = true
        taskProto.resultItems = [resultProto]

        let mapped = CacheTask(taskProto)
        XCTAssertEqual(mapped.hlsCacheFillStatus?.state, .blockedQuota)
        XCTAssertEqual(mapped.hlsCacheFillStatus?.failureKind, .persistence)
        XCTAssertEqual(mapped.hlsCacheFillStatus?.representationID, "video-720p")
        XCTAssertEqual(mapped.resultItems.first?.identity?.cid, 42)
        XCTAssertEqual(mapped.resultItems.first?.hlsCacheFillStatus?.state, .filling)
        XCTAssertEqual(mapped.resultItems.first?.hlsCacheFillStatus?.progressPercentLabel, "25%")

        XCTAssertNil(CacheTask(TvosNetPlayer_V1_Task()).hlsCacheFillStatus)
        var oldTaskProto = TvosNetPlayer_V1_Task()
        oldTaskProto.resultItems = [TvosNetPlayer_V1_BilibiliTaskResultItem()]
        XCTAssertNil(CacheTask(oldTaskProto).resultItems.first?.hlsCacheFillStatus)
    }

    func testV2ResultDetailsMapStatusAndUnknownFutureEnumSafely() {
        var response = TvosNetPlayer_V1_ListTaskResultsResponse()
        var result = TvosNetPlayer_V1_TaskResult()
        result.id = "v2-result-1"
        result.state = .playable
        result.playbackSource.itemID = "v2-result-1"
        result.playbackSource.variantID = "h264"
        result.playbackSource.`protocol` = .hls
        result.playbackSource.uri = "http://cache.local/hls/v2-result-1/master.m3u8"
        result.providerDetails.bilibili.hlsCacheFillStatus.state = .failed
        result.providerDetails.bilibili.hlsCacheFillStatus.failureKind = .network
        result.providerDetails.bilibili.hlsCacheFillStatus.completedBytes = 900
        result.providerDetails.bilibili.hlsCacheFillStatus.totalBytes = 1_000
        result.providerDetails.bilibili.hlsCacheFillStatus.totalBytesKnown = true
        response.results = [result]

        let mapped = CacheTaskResultsPage(response).results.first
        guard case .bilibili(let details)? = mapped?.providerDetails else {
            return XCTFail("Expected Bilibili result details.")
        }
        XCTAssertEqual(details.hlsCacheFillStatus?.state, .failed)
        XCTAssertEqual(details.hlsCacheFillStatus?.failureKind, .network)
        XCTAssertEqual(details.hlsCacheFillStatus?.progressPercentLabel, "90%")
        XCTAssertEqual(mapped?.playbackSource?.uri, "http://cache.local/hls/v2-result-1/master.m3u8")

        var unknownProto = TvosNetPlayer_V1_HlsCacheFillStatus()
        unknownProto.state = .UNRECOGNIZED(1234)
        unknownProto.failureKind = .UNRECOGNIZED(5678)
        XCTAssertEqual(HlsCacheFillStatus(unknownProto).state, .unspecified)
        XCTAssertEqual(HlsCacheFillStatus(unknownProto).failureKind, .unspecified)
    }

    private struct SentinelTransportError: Error, LocalizedError {
        let message: String

        var errorDescription: String? { message }
    }
}
