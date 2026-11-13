import Foundation
import TVOSNetPlayerCacheClient

public enum HLSPlaybackReaderURLBinding {
    public static func applying(
        to url: URL,
        owner: HLSPlaybackContextOwner,
        server: CacheServerSummary
    ) -> URL? {
        guard server.supportsHLSPlaybackReaderBinding else { return url }

        let readerID = owner.ownerToken.uuidString.lowercased()
        guard owner.serverID.utf8.elementsEqual(server.id.utf8),
            (try? HLSPlaybackContextOpenRequest(
                sessionID: owner.sessionID,
                expectedSessionGeneration: owner.sessionGeneration,
                httpReaderID: readerID
            ).validate()) != nil,
            var components = URLComponents(url: url, resolvingAgainstBaseURL: false),
            let scheme = components.scheme?.lowercased(),
            scheme == "http" || scheme == "https",
            let host = components.host,
            !host.isEmpty,
            components.user == nil,
            components.password == nil,
            components.fragment == nil,
            matchesMasterPath(components.percentEncodedPath, sessionID: owner.sessionID)
        else {
            return nil
        }

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
