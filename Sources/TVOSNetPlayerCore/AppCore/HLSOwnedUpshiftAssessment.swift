import Foundation
import TVOSNetPlayerCacheClient

/// Identifies the HLS session reported by the structured playback source.
public struct PlayerHLSPlaybackSessionContext: Equatable, Sendable {
    public let serverID: String
    public let sessionID: String

    /// Returns `nil` when either identifier is invalid or absent.
    public init?(serverID: String, sessionID: String) {
        guard !serverID.utf8.isEmpty, (try? HLSRenditionCatalogRequest(sessionID: sessionID)) != nil else {
            return nil
        }
        self.serverID = serverID
        self.sessionID = sessionID
    }

    public static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.serverID.utf8.elementsEqual(rhs.serverID.utf8)
            && lhs.sessionID.utf8.elementsEqual(rhs.sessionID.utf8)
    }
}

/// A recommendation bound to the playback snapshot and local player owner that produced it.
public struct HLSOwnedUpshiftAssessment: Sendable {
    public let snapshot: HLSPlaybackSnapshot
    public let rendition: HLSRenditionDescriptor
    public let recommendation: HLSUpshiftRecommendation
    let ownerToken: UUID

    init(
        snapshot: HLSPlaybackSnapshot,
        rendition: HLSRenditionDescriptor,
        recommendation: HLSUpshiftRecommendation,
        ownerToken: UUID
    ) {
        self.snapshot = snapshot
        self.rendition = rendition
        self.recommendation = recommendation
        self.ownerToken = ownerToken
    }
}
