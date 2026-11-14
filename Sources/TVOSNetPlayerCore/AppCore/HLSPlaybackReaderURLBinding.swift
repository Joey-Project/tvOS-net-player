import Foundation
import TVOSNetPlayerCacheClient

public enum HLSPlaybackReaderURLBinding {
    public static func applying(
        to url: URL,
        owner: HLSPlaybackContextOwner,
        server: CacheServerSummary
    ) -> URL? {
        guard server.supportsHLSPlaybackReaderBinding else { return url }
        guard owner.serverID.utf8.elementsEqual(server.id.utf8),
            (try? HLSPlaybackContextOpenRequest(
                sessionID: owner.sessionID,
                expectedSessionGeneration: owner.sessionGeneration,
                httpReaderID: owner.ownerToken.uuidString.lowercased()
            ).validate()) != nil,
            let boundURL = applying(
                to: url,
                server: server,
                expectedServerID: owner.serverID,
                sessionID: owner.sessionID,
                readerID: owner.ownerToken
            )
        else {
            return nil
        }
        return boundURL
    }

    public static func applying(
        to url: URL,
        server: CacheServerSummary,
        expectedServerID: String,
        sessionID: String,
        readerID: UUID
    ) -> URL? {
        guard server.supportsHLSPlaybackReaderBinding else { return url }
        guard server.id.utf8.elementsEqual(expectedServerID.utf8),
            (try? HLSRenditionCatalogRequest(sessionID: sessionID).validate()) != nil,
            let components = URLComponents(url: url, resolvingAgainstBaseURL: false),
            let scheme = components.scheme?.lowercased(),
            scheme == "http" || scheme == "https",
            let host = components.host,
            !host.isEmpty,
            components.user == nil,
            components.password == nil,
            components.fragment == nil,
            matchesMasterPath(components.percentEncodedPath, sessionID: sessionID)
        else {
            return nil
        }

        guard let canonicalReaderID = canonicalReaderID(readerID) else { return nil }
        return appendingReaderID(canonicalReaderID, to: url)
    }

    private static func canonicalReaderID(_ readerID: UUID) -> String? {
        let value = readerID.uuidString.lowercased()
        let bytes = Array(value.utf8)
        guard bytes.count == 36,
            bytes[14] == 52,
            [56, 57, 97, 98].contains(bytes[19])
        else {
            return nil
        }
        return value
    }

    private static func appendingReaderID(_ readerID: String, to url: URL) -> URL? {
        guard var components = URLComponents(url: url, resolvingAgainstBaseURL: false) else { return nil }

        let existingReaderItems = components.queryItems?.filter { $0.name == "playback_reader" } ?? []
        guard existingReaderItems.isEmpty else {
            if existingReaderItems.count == 1, existingReaderItems[0].value == readerID {
                return url
            }
            return nil
        }

        var queryItems = components.percentEncodedQueryItems ?? []
        queryItems.append(URLQueryItem(name: "playback_reader", value: readerID))
        components.percentEncodedQueryItems = queryItems
        return components.url
    }

    private static func matchesMasterPath(_ percentEncodedPath: String, sessionID: String) -> Bool {
        let pathSegments = percentEncodedPath.split(separator: "/", omittingEmptySubsequences: false)
        guard pathSegments.count >= 3 else { return false }
        let route = Array(pathSegments.suffix(3))
        guard route.count == 3,
            let marker = String(route[0]).removingPercentEncoding,
            let routeSessionID = String(route[1]).removingPercentEncoding,
            let masterName = String(route[2]).removingPercentEncoding
        else {
            return false
        }
        return marker.utf8.elementsEqual("hls".utf8)
            && routeSessionID.utf8.elementsEqual(sessionID.utf8)
            && masterName.utf8.elementsEqual("master.m3u8".utf8)
    }
}
