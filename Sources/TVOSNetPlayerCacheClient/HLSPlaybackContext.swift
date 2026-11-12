import Foundation

public struct HLSPlaybackContextOpenRequest: Equatable, Sendable {
    public let sessionID: String
    public let expectedSessionGeneration: UInt64

    public init(sessionID: String, expectedSessionGeneration: UInt64) {
        self.sessionID = sessionID
        self.expectedSessionGeneration = expectedSessionGeneration
    }

    public func validate() throws {
        guard Self.isValidIdentifier(sessionID), expectedSessionGeneration > 0 else {
            throw HLSPlaybackContextValidationError.invalidRequest
        }
    }

    static func isValidIdentifier(_ value: String) -> Bool {
        let scalars = value.unicodeScalars
        guard !value.isEmpty, value.utf8.count <= 256,
            let first = scalars.first,
            let last = scalars.last
        else {
            return false
        }
        guard !first.properties.isWhitespace, !last.properties.isWhitespace else {
            return false
        }
        return !scalars.contains { $0.properties.generalCategory == .control }
    }
}

public struct HLSPlaybackContextTimingSample: Equatable, Sendable {
    public let playheadSeconds: Double
    public let continuousBufferedEndSeconds: Double
    public let playbackRate: Double

    public init(
        playheadSeconds: Double,
        continuousBufferedEndSeconds: Double,
        playbackRate: Double
    ) {
        self.playheadSeconds = playheadSeconds
        self.continuousBufferedEndSeconds = continuousBufferedEndSeconds
        self.playbackRate = playbackRate
    }

    public func validate() throws {
        guard playheadSeconds.isFinite,
            playheadSeconds >= 0,
            continuousBufferedEndSeconds.isFinite,
            continuousBufferedEndSeconds >= playheadSeconds,
            playbackRate.isFinite,
            playbackRate > 0,
            playbackRate <= 16
        else {
            throw HLSPlaybackContextValidationError.invalidRequest
        }
    }
}

public struct HLSPlaybackContextUpdateRequest: Equatable, Sendable {
    public let contextID: String
    public let challenge: String
    public let sessionID: String
    public let expectedSessionGeneration: UInt64
    public let sequence: UInt64
    public let sample: HLSPlaybackContextTimingSample

    public init(
        contextID: String,
        challenge: String,
        sessionID: String,
        expectedSessionGeneration: UInt64,
        sequence: UInt64,
        sample: HLSPlaybackContextTimingSample
    ) {
        self.contextID = contextID
        self.challenge = challenge
        self.sessionID = sessionID
        self.expectedSessionGeneration = expectedSessionGeneration
        self.sequence = sequence
        self.sample = sample
    }

    public func validate() throws {
        guard HLSPlaybackContextUUID.isVersion4(contextID),
            HLSPlaybackContextUUID.isVersion4(challenge),
            HLSPlaybackContextOpenRequest.isValidIdentifier(sessionID),
            expectedSessionGeneration > 0,
            sequence > 0,
            sequence < UInt64.max
        else {
            throw HLSPlaybackContextValidationError.invalidRequest
        }
        try sample.validate()
    }
}

public struct HLSPlaybackContextCloseRequest: Equatable, Sendable {
    public let contextID: String

    public init(contextID: String) {
        self.contextID = contextID
    }

    public func validate() throws {
        guard HLSPlaybackContextUUID.isSyntacticallyValid(contextID) else {
            throw HLSPlaybackContextValidationError.invalidRequest
        }
    }
}

public struct HLSPlaybackContextTicket: Equatable, Sendable {
    public let contextID: String
    public let challenge: String
    public let sessionID: String
    public let sessionGeneration: UInt64
    public let nextSequence: UInt64
    public let validityMillis: UInt64

    public init(
        contextID: String,
        challenge: String,
        sessionID: String,
        sessionGeneration: UInt64,
        nextSequence: UInt64,
        validityMillis: UInt64
    ) {
        self.contextID = contextID
        self.challenge = challenge
        self.sessionID = sessionID
        self.sessionGeneration = sessionGeneration
        self.nextSequence = nextSequence
        self.validityMillis = validityMillis
    }

    public func validate(matching request: HLSPlaybackContextOpenRequest) throws {
        try request.validate()
        guard HLSPlaybackContextUUID.isVersion4(contextID),
            HLSPlaybackContextUUID.isVersion4(challenge),
            sessionID.utf8.elementsEqual(request.sessionID.utf8),
            sessionGeneration == request.expectedSessionGeneration,
            nextSequence == 1,
            (1...5_000).contains(validityMillis)
        else {
            throw HLSPlaybackContextValidationError.invalidResponse
        }
    }

    public func validate(matching request: HLSPlaybackContextUpdateRequest) throws {
        try request.validate()
        let (expectedSequence, overflow) = request.sequence.addingReportingOverflow(1)
        guard !overflow,
            HLSPlaybackContextUUID.isVersion4(contextID),
            HLSPlaybackContextUUID.isVersion4(challenge),
            contextID.utf8.elementsEqual(request.contextID.utf8),
            UUID(uuidString: challenge) != UUID(uuidString: request.challenge),
            sessionID.utf8.elementsEqual(request.sessionID.utf8),
            sessionGeneration == request.expectedSessionGeneration,
            nextSequence == expectedSequence,
            (1...5_000).contains(validityMillis)
        else {
            throw HLSPlaybackContextValidationError.invalidResponse
        }
    }

    init(_ proto: TvosNetPlayer_V1_HlsPlaybackContextTicket) {
        self.init(
            contextID: proto.contextID,
            challenge: proto.challenge,
            sessionID: proto.sessionID,
            sessionGeneration: proto.sessionGeneration,
            nextSequence: proto.nextSequence,
            validityMillis: proto.validityMillis
        )
    }
}

public struct HLSPlaybackContextCloseResponse: Equatable, Sendable {
    public let closed: Bool

    public init(closed: Bool) {
        self.closed = closed
    }

    init(_ proto: TvosNetPlayer_V1_CloseHlsPlaybackContextResponse) {
        self.init(closed: proto.closed)
    }
}

public enum HLSPlaybackContextValidationError: Error, Equatable, Sendable {
    case invalidRequest
    case invalidResponse
}

private enum HLSPlaybackContextUUID {
    static func isSyntacticallyValid(_ value: String) -> Bool {
        guard hasCanonicalShape(value), UUID(uuidString: value) != nil else { return false }
        return true
    }

    static func isVersion4(_ value: String) -> Bool {
        guard isSyntacticallyValid(value) else { return false }
        let bytes = Array(value.utf8)
        guard let version = hexValue(bytes[14]),
            let variant = hexValue(bytes[19])
        else {
            return false
        }
        return version == 4 && (variant & 0b1100) == 0b1000
    }

    private static func hasCanonicalShape(_ value: String) -> Bool {
        let bytes = Array(value.utf8)
        guard bytes.count == 36 else { return false }
        for index in [8, 13, 18, 23] where bytes[index] != 45 {
            return false
        }
        for index in 0..<bytes.count where ![8, 13, 18, 23].contains(index) {
            guard hexValue(bytes[index]) != nil else { return false }
        }
        return true
    }

    private static func hexValue(_ byte: UInt8) -> UInt8? {
        switch byte {
        case 48...57: byte - 48
        case 65...70: byte - 65 + 10
        case 97...102: byte - 97 + 10
        default: nil
        }
    }
}

extension TvosNetPlayer_V1_OpenHlsPlaybackContextRequest {
    init(_ request: HLSPlaybackContextOpenRequest) {
        self.init()
        sessionID = request.sessionID
        expectedSessionGeneration = request.expectedSessionGeneration
    }
}

extension TvosNetPlayer_V1_HlsPlaybackTimingSample {
    init(_ sample: HLSPlaybackContextTimingSample) {
        self.init()
        playheadSeconds = sample.playheadSeconds
        continuousBufferedEndSeconds = sample.continuousBufferedEndSeconds
        playbackRate = sample.playbackRate
    }
}

extension TvosNetPlayer_V1_UpdateHlsPlaybackContextRequest {
    init(_ request: HLSPlaybackContextUpdateRequest) {
        self.init()
        contextID = request.contextID
        challenge = request.challenge
        sequence = request.sequence
        sample = TvosNetPlayer_V1_HlsPlaybackTimingSample(request.sample)
    }
}

extension TvosNetPlayer_V1_CloseHlsPlaybackContextRequest {
    init(_ request: HLSPlaybackContextCloseRequest) {
        self.init()
        contextID = request.contextID
    }
}
