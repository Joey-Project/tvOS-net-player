import Foundation

public struct HLSRenditionCatalogRequest: Equatable, Sendable {
    public let sessionID: String

    public init(sessionID: String) throws {
        self.sessionID = sessionID
        try validate()
    }

    public func validate() throws {
        guard HLSRenditionCatalogValidation.isValidIdentifier(sessionID) else {
            throw HLSRenditionCatalogValidationError.invalidRequest
        }
    }

    public static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.sessionID.utf8.elementsEqual(rhs.sessionID.utf8)
    }
}

public struct HLSRenditionDescriptor: Equatable, Sendable {
    public let variantID: String
    public let advertisedBandwidthBitsPerSecond: UInt64
    public let videoResourceBandwidthBitsPerSecond: UInt64?
    public let audioResourceBandwidthBitsPerSecond: UInt64?
    public let hasSeparateAudio: Bool
    public let videoResourceCodecs: String?
    public let audioResourceCodecs: String?
    public let videoResourceMimeType: String?
    public let audioResourceMimeType: String?
    public let advertisedWidth: UInt32?
    public let advertisedHeight: UInt32?

    public init(
        variantID: String,
        advertisedBandwidthBitsPerSecond: UInt64,
        videoResourceBandwidthBitsPerSecond: UInt64? = nil,
        audioResourceBandwidthBitsPerSecond: UInt64? = nil,
        hasSeparateAudio: Bool,
        videoResourceCodecs: String? = nil,
        audioResourceCodecs: String? = nil,
        videoResourceMimeType: String? = nil,
        audioResourceMimeType: String? = nil,
        advertisedWidth: UInt32? = nil,
        advertisedHeight: UInt32? = nil
    ) throws {
        self.variantID = variantID
        self.advertisedBandwidthBitsPerSecond = advertisedBandwidthBitsPerSecond
        self.videoResourceBandwidthBitsPerSecond = videoResourceBandwidthBitsPerSecond
        self.audioResourceBandwidthBitsPerSecond = audioResourceBandwidthBitsPerSecond
        self.hasSeparateAudio = hasSeparateAudio
        self.videoResourceCodecs = videoResourceCodecs
        self.audioResourceCodecs = audioResourceCodecs
        self.videoResourceMimeType = videoResourceMimeType
        self.audioResourceMimeType = audioResourceMimeType
        self.advertisedWidth = advertisedWidth
        self.advertisedHeight = advertisedHeight
        try validate()
    }

    public func validate() throws {
        guard
            HLSRenditionCatalogValidation.isValidIdentifier(variantID),
            advertisedBandwidthBitsPerSecond > 0,
            videoResourceBandwidthBitsPerSecond.map({ $0 > 0 }) ?? true,
            audioResourceBandwidthBitsPerSecond.map({ $0 > 0 }) ?? true,
            HLSRenditionCatalogValidation.isValidCodecHint(videoResourceCodecs),
            HLSRenditionCatalogValidation.isValidCodecHint(audioResourceCodecs),
            HLSRenditionCatalogValidation.isValidMIMEHint(videoResourceMimeType),
            HLSRenditionCatalogValidation.isValidMIMEHint(audioResourceMimeType),
            (advertisedWidth == nil) == (advertisedHeight == nil),
            advertisedWidth.map({ $0 > 0 }) ?? true,
            advertisedHeight.map({ $0 > 0 }) ?? true
        else {
            throw HLSRenditionCatalogValidationError.invalidResponse
        }

        if !hasSeparateAudio,
            audioResourceBandwidthBitsPerSecond != nil
                || audioResourceCodecs != nil
                || audioResourceMimeType != nil
        {
            throw HLSRenditionCatalogValidationError.invalidResponse
        }
    }

    public static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.variantID.utf8.elementsEqual(rhs.variantID.utf8)
            && lhs.advertisedBandwidthBitsPerSecond == rhs.advertisedBandwidthBitsPerSecond
            && lhs.videoResourceBandwidthBitsPerSecond == rhs.videoResourceBandwidthBitsPerSecond
            && lhs.audioResourceBandwidthBitsPerSecond == rhs.audioResourceBandwidthBitsPerSecond
            && lhs.hasSeparateAudio == rhs.hasSeparateAudio
            && lhs.videoResourceCodecs == rhs.videoResourceCodecs
            && lhs.audioResourceCodecs == rhs.audioResourceCodecs
            && lhs.videoResourceMimeType == rhs.videoResourceMimeType
            && lhs.audioResourceMimeType == rhs.audioResourceMimeType
            && lhs.advertisedWidth == rhs.advertisedWidth
            && lhs.advertisedHeight == rhs.advertisedHeight
    }

    init(_ proto: TvosNetPlayer_V1_HlsRenditionDescriptor) throws {
        try self.init(
            variantID: proto.variantID,
            advertisedBandwidthBitsPerSecond: proto.advertisedBandwidthBitsPerSecond,
            videoResourceBandwidthBitsPerSecond: proto.hasVideoResourceBandwidthBitsPerSecond
                ? proto.videoResourceBandwidthBitsPerSecond : nil,
            audioResourceBandwidthBitsPerSecond: proto.hasAudioResourceBandwidthBitsPerSecond
                ? proto.audioResourceBandwidthBitsPerSecond : nil,
            hasSeparateAudio: proto.hasSeparateAudio_p,
            videoResourceCodecs: proto.hasVideoResourceCodecs ? proto.videoResourceCodecs : nil,
            audioResourceCodecs: proto.hasAudioResourceCodecs ? proto.audioResourceCodecs : nil,
            videoResourceMimeType: proto.hasVideoResourceMimeType ? proto.videoResourceMimeType : nil,
            audioResourceMimeType: proto.hasAudioResourceMimeType ? proto.audioResourceMimeType : nil,
            advertisedWidth: proto.hasAdvertisedWidth ? proto.advertisedWidth : nil,
            advertisedHeight: proto.hasAdvertisedHeight ? proto.advertisedHeight : nil
        )
    }
}

public struct HLSRenditionCatalog: Equatable, Sendable {
    public static let maximumRenditions = 128
    public static let maximumEncodedSizeBytes = 64 * 1024

    public let sessionID: String
    public let sessionGeneration: UInt64
    public let renditions: [HLSRenditionDescriptor]

    public init(sessionID: String, sessionGeneration: UInt64, renditions: [HLSRenditionDescriptor]) throws {
        self.sessionID = sessionID
        self.sessionGeneration = sessionGeneration
        self.renditions = renditions
        try validate()
    }

    public func validate() throws {
        guard
            HLSRenditionCatalogValidation.isValidIdentifier(sessionID),
            sessionGeneration > 0,
            (1...Self.maximumRenditions).contains(renditions.count)
        else {
            throw HLSRenditionCatalogValidationError.invalidResponse
        }

        var seenIDs = Set<Data>()
        for rendition in renditions {
            try rendition.validate()
            guard seenIDs.insert(Data(rendition.variantID.utf8)).inserted else {
                throw HLSRenditionCatalogValidationError.invalidResponse
            }
        }

        var proto = TvosNetPlayer_V1_HlsRenditionCatalog()
        proto.sessionID = sessionID
        proto.sessionGeneration = sessionGeneration
        proto.renditions = renditions.map(\.proto)
        guard try proto.serializedData().count <= Self.maximumEncodedSizeBytes else {
            throw HLSRenditionCatalogValidationError.invalidResponse
        }
    }

    public func rendition(variantID: String) -> HLSRenditionDescriptor? {
        renditions.first { $0.variantID.utf8.elementsEqual(variantID.utf8) }
    }

    public static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.sessionID.utf8.elementsEqual(rhs.sessionID.utf8)
            && lhs.sessionGeneration == rhs.sessionGeneration
            && lhs.renditions == rhs.renditions
    }

    init(
        _ proto: TvosNetPlayer_V1_HlsRenditionCatalog,
        matching request: HLSRenditionCatalogRequest
    ) throws {
        try request.validate()
        guard proto.sessionID.utf8.elementsEqual(request.sessionID.utf8) else {
            throw HLSRenditionCatalogValidationError.invalidResponse
        }
        guard
            (1...Self.maximumRenditions).contains(proto.renditions.count),
            try proto.serializedData().count <= Self.maximumEncodedSizeBytes
        else {
            throw HLSRenditionCatalogValidationError.invalidResponse
        }
        let decodedRenditions = try proto.renditions.map(HLSRenditionDescriptor.init)
        try self.init(
            sessionID: proto.sessionID,
            sessionGeneration: proto.sessionGeneration,
            renditions: decodedRenditions
        )
    }
}

public enum HLSRenditionCatalogValidationError: Error, Equatable, Sendable {
    case invalidRequest
    case invalidResponse
}

private enum HLSRenditionCatalogValidation {
    static func isValidIdentifier(_ value: String) -> Bool {
        let scalars = value.unicodeScalars
        guard
            !value.isEmpty,
            value.utf8.count <= 256,
            let first = scalars.first,
            let last = scalars.last,
            !first.properties.isWhitespace,
            !last.properties.isWhitespace
        else {
            return false
        }
        return !scalars.contains { $0.properties.generalCategory == .control }
    }

    static func isValidCodecHint(_ value: String?) -> Bool {
        guard let value else { return true }
        guard
            !value.isEmpty,
            value.utf8.count <= 256,
            value.utf8.first != UInt8(ascii: " "),
            value.utf8.last != UInt8(ascii: " ")
        else {
            return false
        }

        let pieces = value.split(separator: ",", omittingEmptySubsequences: false)
        guard !pieces.isEmpty else { return false }
        return pieces.allSatisfy { piece in
            let token = piece.trimmingCharacters(in: CharacterSet(charactersIn: " "))
            guard !token.isEmpty else { return false }
            return token.utf8.allSatisfy { byte in
                isASCIIAlphaNumeric(byte) || byte == 46 || byte == 95 || byte == 45
            }
        }
    }

    static func isValidMIMEHint(_ value: String?) -> Bool {
        guard let value else { return true }
        guard !value.isEmpty, value.utf8.count <= 128 else { return false }
        let components = value.split(separator: "/", omittingEmptySubsequences: false)
        guard components.count == 2 else { return false }
        let allowed = Set("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!#$&^_.+-".utf8)
        return components.allSatisfy { !$0.isEmpty && $0.utf8.allSatisfy(allowed.contains) }
    }

    private static func isASCIIAlphaNumeric(_ byte: UInt8) -> Bool {
        (65...90).contains(byte) || (97...122).contains(byte) || (48...57).contains(byte)
    }
}

extension HLSRenditionDescriptor {
    var proto: TvosNetPlayer_V1_HlsRenditionDescriptor {
        var result = TvosNetPlayer_V1_HlsRenditionDescriptor()
        result.variantID = variantID
        result.advertisedBandwidthBitsPerSecond = advertisedBandwidthBitsPerSecond
        if let videoResourceBandwidthBitsPerSecond {
            result.videoResourceBandwidthBitsPerSecond = videoResourceBandwidthBitsPerSecond
        }
        if let audioResourceBandwidthBitsPerSecond {
            result.audioResourceBandwidthBitsPerSecond = audioResourceBandwidthBitsPerSecond
        }
        result.hasSeparateAudio_p = hasSeparateAudio
        if let videoResourceCodecs { result.videoResourceCodecs = videoResourceCodecs }
        if let audioResourceCodecs { result.audioResourceCodecs = audioResourceCodecs }
        if let videoResourceMimeType { result.videoResourceMimeType = videoResourceMimeType }
        if let audioResourceMimeType { result.audioResourceMimeType = audioResourceMimeType }
        if let advertisedWidth { result.advertisedWidth = advertisedWidth }
        if let advertisedHeight { result.advertisedHeight = advertisedHeight }
        return result
    }
}
