import Foundation
import GRPCCore
import GRPCNIOTransportHTTP2TransportServices
import SwiftProtobuf

public final class GRPCCacheControlClient: CacheControlClient {
    private let endpoint: CacheServerEndpoint
    private let rpcTimeout: Duration
    private let maxLibraryPages: Int
    private let maxLibraryItems: Int
    private let allowPartialLibraryResults: Bool

    public init(
        endpoint: CacheServerEndpoint,
        rpcTimeout: Duration = .seconds(10),
        maxLibraryPages: Int = 100,
        maxLibraryItems: Int = 5_000,
        allowPartialLibraryResults: Bool = false
    ) {
        self.endpoint = endpoint
        self.rpcTimeout = rpcTimeout
        self.maxLibraryPages = max(1, maxLibraryPages)
        self.maxLibraryItems = max(1, maxLibraryItems)
        self.allowPartialLibraryResults = allowPartialLibraryResults
    }

    public func getServerInfo() async throws -> CacheServerSummary {
        try await withGRPCClient(
            transport: .http2NIOTS(
                target: endpoint.grpcTarget,
                transportSecurity: endpoint.grpcTransportSecurity
            )
        ) { client in
            let service = TvosNetPlayer_V1_ServerService.Client(wrapping: client)
            let response = try await service.getServerInfo(
                TvosNetPlayer_V1_GetServerInfoRequest(),
                options: callOptions
            )
            return CacheServerSummary(response)
        }
    }

    public func getBilibiliCredentialStatus() async throws -> BilibiliCredentialStatus {
        do {
            return try await withGRPCClient(
                transport: .http2NIOTS(
                    target: endpoint.grpcTarget,
                    transportSecurity: endpoint.grpcTransportSecurity
                )
            ) { client in
                let service = TvosNetPlayer_V1_ServerService.Client(wrapping: client)
                let response = try await service.getBilibiliCredentialStatus(
                    TvosNetPlayer_V1_GetBilibiliCredentialStatusRequest(),
                    options: callOptions
                )
                return BilibiliCredentialStatus(response)
            }
        } catch let error as RPCError where error.code == .unimplemented {
            throw CacheControlClientUnsupportedFeature.bilibiliCredentialStatus
        }
    }

    public func listCacheRoots() async throws -> [CacheRoot] {
        try await withGRPCClient(
            transport: .http2NIOTS(
                target: endpoint.grpcTarget,
                transportSecurity: endpoint.grpcTransportSecurity
            )
        ) { client in
            let service = TvosNetPlayer_V1_CacheService.Client(wrapping: client)
            let response = try await service.listCacheRoots(
                TvosNetPlayer_V1_ListCacheRootsRequest(),
                options: callOptions
            )
            return response.roots.map(CacheRoot.init)
        }
    }

    public func getHLSCacheStatus() async throws -> HLSCacheStatus {
        try await withGRPCClient(
            transport: .http2NIOTS(
                target: endpoint.grpcTarget,
                transportSecurity: endpoint.grpcTransportSecurity
            )
        ) { client in
            let service = TvosNetPlayer_V1_CacheService.Client(wrapping: client)
            let response = try await service.getHlsCacheStatus(
                TvosNetPlayer_V1_GetHlsCacheStatusRequest(),
                options: callOptions
            )
            return HLSCacheStatus(response)
        }
    }

    public func reportPlaybackProgress(_ report: PlaybackProgressReport) async throws -> PlaybackProgressReportResult {
        do {
            return try await withGRPCClient(
                transport: .http2NIOTS(
                    target: endpoint.grpcTarget,
                    transportSecurity: endpoint.grpcTransportSecurity
                )
            ) { client in
                let service = TvosNetPlayer_V1_CacheService.Client(wrapping: client)
                var request = TvosNetPlayer_V1_ReportPlaybackProgressRequest()
                request.playbackUri = report.playbackURI
                request.libraryItemID = report.libraryItemID
                request.variantID = report.variantID
                request.positionSeconds = report.positionSeconds
                request.durationSeconds = report.durationSeconds ?? 0
                request.intent = TvosNetPlayer_V1_PlaybackProgressIntent(report.intent)
                let response = try await service.reportPlaybackProgress(request, options: callOptions)
                return PlaybackProgressReportResult(response)
            }
        } catch let error as RPCError where error.code == .unimplemented {
            throw CacheControlClientUnsupportedFeature.playbackProgressReporting
        }
    }

    public func listLibraryItemsPage(
        pageToken: String = "",
        pageSize: Int = 50,
        searchText: String? = nil
    ) async throws -> CacheLibraryItemsPage {
        try await withGRPCClient(
            transport: .http2NIOTS(
                target: endpoint.grpcTarget,
                transportSecurity: endpoint.grpcTransportSecurity
            )
        ) { client in
            let service = TvosNetPlayer_V1_LibraryService.Client(wrapping: client)
            let request = Self.listLibraryItemsRequest(
                pageToken: pageToken,
                pageSize: pageSize,
                searchText: searchText
            )
            let response = try await service.listLibraryItems(request, options: callOptions)
            return CacheLibraryItemsPage(response)
        }
    }

    public func listLibraryItems(pageSize: Int = 50, searchText: String? = nil) async throws -> [CacheLibraryItem] {
        try await withGRPCClient(
            transport: .http2NIOTS(
                target: endpoint.grpcTarget,
                transportSecurity: endpoint.grpcTransportSecurity
            )
        ) { client in
            let service = TvosNetPlayer_V1_LibraryService.Client(wrapping: client)
            return try await collectCacheLibraryItems(
                maxPages: maxLibraryPages,
                maxItems: maxLibraryItems,
                allowPartialResults: allowPartialLibraryResults
            ) { pageToken in
                let request = Self.listLibraryItemsRequest(
                    pageToken: pageToken,
                    pageSize: pageSize,
                    searchText: searchText
                )
                let response = try await service.listLibraryItems(request, options: callOptions)
                return CacheLibraryItemsPage(response)
            }
        }
    }

    public func getPlaybackSource(itemID: String, variantID: String) async throws -> CachePlaybackSource {
        try await withGRPCClient(
            transport: .http2NIOTS(
                target: endpoint.grpcTarget,
                transportSecurity: endpoint.grpcTransportSecurity
            )
        ) { client in
            let service = TvosNetPlayer_V1_LibraryService.Client(wrapping: client)
            var request = TvosNetPlayer_V1_GetPlaybackSourceRequest()
            request.itemID = itemID
            request.variantID = variantID
            let response = try await service.getPlaybackSource(request, options: callOptions)
            return CachePlaybackSource(response)
        }
    }

    public func deleteLibraryItem(id: String) async throws -> Bool {
        try await withGRPCClient(
            transport: .http2NIOTS(
                target: endpoint.grpcTarget,
                transportSecurity: endpoint.grpcTransportSecurity
            )
        ) { client in
            let service = TvosNetPlayer_V1_CacheService.Client(wrapping: client)
            var request = TvosNetPlayer_V1_DeleteLibraryItemRequest()
            request.id = id
            let response = try await service.deleteLibraryItem(request, options: callOptions)
            return response.deleted
        }
    }

    public func resolveBilibiliInput(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions = BilibiliPlaybackTaskOptions()
    ) async throws -> BilibiliResolveResult {
        do {
            return try await withGRPCClient(
                transport: .http2NIOTS(
                    target: endpoint.grpcTarget,
                    transportSecurity: endpoint.grpcTransportSecurity
                )
            ) { client in
                let service = TvosNetPlayer_V1_TaskService.Client(wrapping: client)
                var request = TvosNetPlayer_V1_ResolveBilibiliInputRequest()
                request.urlOrID = urlOrID
                request.options = TvosNetPlayer_V1_BilibiliPlaybackOptions(options)
                let response = try await service.resolveBilibiliInput(request, options: callOptions)
                return BilibiliResolveResult(response)
            }
        } catch let error as RPCError where error.code == .unimplemented {
            throw CacheControlClientUnsupportedFeature.bilibiliResolve
        }
    }

    public func createBilibiliPlaybackTask(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions = BilibiliPlaybackTaskOptions()
    ) async throws -> CacheTask {
        try await createBilibiliPlaybackTask(
            urlOrID: urlOrID,
            selectionID: nil,
            options: options
        )
    }

    public func createBilibiliPlaybackTask(
        urlOrID: String,
        selectionID: String? = nil,
        options: BilibiliPlaybackTaskOptions = BilibiliPlaybackTaskOptions()
    ) async throws -> CacheTask {
        let normalizedSelectionID = selectionID?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        return try await createBilibiliPlaybackTask(
            urlOrID: urlOrID,
            selectionID: normalizedSelectionID,
            selection: nil,
            options: options
        )
    }

    public func createBilibiliPlaybackTask(
        urlOrID: String,
        selection: BilibiliTaskSelection?,
        options: BilibiliPlaybackTaskOptions = BilibiliPlaybackTaskOptions()
    ) async throws -> CacheTask {
        try await createBilibiliPlaybackTask(
            urlOrID: urlOrID,
            selectionID: nil,
            selection: selection,
            options: options
        )
    }

    private func createBilibiliPlaybackTask(
        urlOrID: String,
        selectionID: String?,
        selection: BilibiliTaskSelection?,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask {
        let normalizedSelectionID = selectionID?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        if let requiredCapability = Self.requiredCapabilityForBilibiliPlaybackTask(
            selectionID: normalizedSelectionID,
            selection: selection
        ) {
            let serverInfo = try await getServerInfo()
            guard serverInfo.capabilities.contains(requiredCapability) else {
                throw Self.unsupportedFeature(forMissingCapability: requiredCapability)
            }
        }

        return try await withGRPCClient(
            transport: .http2NIOTS(
                target: endpoint.grpcTarget,
                transportSecurity: endpoint.grpcTransportSecurity
            )
        ) { client in
            let service = TvosNetPlayer_V1_TaskService.Client(wrapping: client)
            var request = TvosNetPlayer_V1_CreateBilibiliPlaybackTaskRequest()
            request.urlOrID = urlOrID
            request.options = TvosNetPlayer_V1_BilibiliPlaybackOptions(options)
            request.selectionID = normalizedSelectionID
            if let selection {
                request.selection = TvosNetPlayer_V1_BilibiliTaskSelection(selection)
            }
            let response = try await service.createBilibiliPlaybackTask(request, options: callOptions)
            return CacheTask(response)
        }
    }

    public func createBilibiliTask(
        urlOrID: String,
        options: BilibiliDownloadTaskOptions = BilibiliDownloadTaskOptions()
    ) async throws -> CacheTask {
        try await withGRPCClient(
            transport: .http2NIOTS(
                target: endpoint.grpcTarget,
                transportSecurity: endpoint.grpcTransportSecurity
            )
        ) { client in
            let service = TvosNetPlayer_V1_TaskService.Client(wrapping: client)
            var request = TvosNetPlayer_V1_CreateBilibiliTaskRequest()
            request.urlOrID = urlOrID
            request.options = TvosNetPlayer_V1_BilibiliDownloadOptions(options)
            let response = try await service.createBilibiliTask(request, options: callOptions)
            return CacheTask(response)
        }
    }

    static func requiredCapabilityForBilibiliPlaybackTask(
        selectionID: String,
        selection: BilibiliTaskSelection?
    ) -> String? {
        if selection != nil {
            return CacheServerCapability.bilibiliTaskSelection
        }
        let normalizedSelectionID = selectionID.trimmingCharacters(in: .whitespacesAndNewlines)
        if !normalizedSelectionID.isEmpty {
            return CacheServerCapability.bilibiliResolve
        }
        return nil
    }

    private static func unsupportedFeature(forMissingCapability capability: String)
        -> CacheControlClientUnsupportedFeature
    {
        if capability == CacheServerCapability.bilibiliTaskSelection {
            return .bilibiliTaskSelection
        }
        return .bilibiliResolve
    }

    public func getTask(id: String) async throws -> CacheTask {
        try await withGRPCClient(
            transport: .http2NIOTS(
                target: endpoint.grpcTarget,
                transportSecurity: endpoint.grpcTransportSecurity
            )
        ) { client in
            let service = TvosNetPlayer_V1_TaskService.Client(wrapping: client)
            var request = TvosNetPlayer_V1_GetTaskRequest()
            request.id = id
            let response = try await service.getTask(request, options: callOptions)
            return CacheTask(response)
        }
    }

    public func cancelTask(id: String) async throws -> CacheTask {
        try await withGRPCClient(
            transport: .http2NIOTS(
                target: endpoint.grpcTarget,
                transportSecurity: endpoint.grpcTransportSecurity
            )
        ) { client in
            let service = TvosNetPlayer_V1_TaskService.Client(wrapping: client)
            var request = TvosNetPlayer_V1_CancelTaskRequest()
            request.id = id
            let response = try await service.cancelTask(request, options: callOptions)
            return CacheTask(response)
        }
    }

    public func watchTasks(ids: [String] = []) async -> AsyncThrowingStream<CacheTask, Error> {
        AsyncThrowingStream { continuation in
            let streamTask = Task {
                do {
                    try await withGRPCClient(
                        transport: .http2NIOTS(
                            target: endpoint.grpcTarget,
                            transportSecurity: endpoint.grpcTransportSecurity
                        )
                    ) { client in
                        let service = TvosNetPlayer_V1_TaskService.Client(wrapping: client)
                        var request = TvosNetPlayer_V1_WatchTasksRequest()
                        request.ids = ids
                        try await service.watchTasks(request, options: streamCallOptions) { response in
                            for try await event in response.messages where event.hasTask {
                                continuation.yield(CacheTask(event.task))
                            }
                        }
                    }
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }

            continuation.onTermination = { _ in
                streamTask.cancel()
            }
        }
    }

    private var callOptions: CallOptions {
        var options = CallOptions.defaults
        options.timeout = rpcTimeout
        return options
    }

    private var streamCallOptions: CallOptions {
        CallOptions.defaults
    }

    private static func listLibraryItemsRequest(
        pageToken: String,
        pageSize: Int,
        searchText: String?
    ) -> TvosNetPlayer_V1_ListLibraryItemsRequest {
        var request = TvosNetPlayer_V1_ListLibraryItemsRequest()
        request.pageSize = Int32(clamping: max(1, min(pageSize, 200)))
        request.pageToken = pageToken

        let trimmedSearchText = searchText?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        if !trimmedSearchText.isEmpty {
            request.filter.searchText = trimmedSearchText
        }

        return request
    }
}

extension CacheServerEndpoint {
    var grpcTransportSecurity: HTTP2ClientTransport.TransportServices.TransportSecurity {
        usesTLS ? .tls : .plaintext
    }

    var grpcTargetKind: CacheServerEndpointGRPCTargetKind {
        if isIPv6Literal && !usesTLS {
            return .ipv6Literal
        }

        return .dns
    }

    var grpcTarget: any ResolvableTarget {
        switch grpcTargetKind {
        case .ipv6Literal:
            return .ipv6(address: host, port: port)
        case .dns:
            return .dns(host: host, port: grpcDNSTargetPort)
        }
    }

    var grpcDNSTargetPort: Int? {
        usesTLS && port == Self.defaultTLSPort ? nil : port
    }
}

enum CacheServerEndpointGRPCTargetKind: Equatable {
    case dns
    case ipv6Literal
}

enum CacheLibraryPaginationError: Error, Equatable {
    case repeatedPageToken(String)
    case exceededPageLimit(Int)
    case exceededItemLimit(Int)
}

func collectCacheLibraryItems(
    maxPages: Int = 100,
    maxItems: Int = 5_000,
    allowPartialResults: Bool = false,
    fetchPage: (String) async throws -> CacheLibraryItemsPage
) async throws -> [CacheLibraryItem] {
    var allItems: [CacheLibraryItem] = []
    var pageToken = ""
    var seenNextPageTokens = Set<String>()
    var pageCount = 0

    while true {
        pageCount += 1
        let page = try await fetchPage(pageToken)
        allItems.append(contentsOf: page.items)

        guard allItems.count <= maxItems else {
            throw CacheLibraryPaginationError.exceededItemLimit(maxItems)
        }

        guard !page.nextPageToken.isEmpty else {
            return allItems
        }

        guard pageCount < maxPages else {
            if allowPartialResults {
                return allItems
            }

            throw CacheLibraryPaginationError.exceededPageLimit(maxPages)
        }

        guard seenNextPageTokens.insert(page.nextPageToken).inserted else {
            throw CacheLibraryPaginationError.repeatedPageToken(page.nextPageToken)
        }

        pageToken = page.nextPageToken
    }
}

extension CacheServerSummary {
    fileprivate init(_ proto: TvosNetPlayer_V1_ServerInfo) {
        self.init(
            id: proto.id,
            name: proto.name,
            version: proto.version,
            mediaBaseURIs: proto.mediaBaseUris,
            capabilities: proto.capabilities.map { String(describing: $0) }
        )
    }
}

extension BilibiliCredentialStatus {
    fileprivate init(_ proto: TvosNetPlayer_V1_BilibiliCredentialStatus) {
        self.init(
            state: String(describing: proto.state),
            message: proto.message,
            credentialPathConfigured: proto.credentialPathConfigured,
            credentialFileLoaded: proto.credentialFileLoaded,
            hasWebCookie: proto.webCookiePresent,
            hasAccessKey: proto.accessKeyPresent,
            hasTVAccessKey: proto.tvAccessKeyPresent,
            restrictedArea: proto.restrictedArea,
            restrictedPlayURLProxyCount: proto.restrictedPlayurlProxyCount,
            restrictedAPIProxyCount: proto.restrictedApiProxyCount,
            checkedAt: proto.hasCheckedAt ? Date(proto.checkedAt) : nil
        )
    }
}

extension CacheRoot {
    fileprivate init(_ proto: TvosNetPlayer_V1_CacheRoot) {
        self.init(
            id: proto.id,
            label: proto.label,
            kind: String(describing: proto.kind),
            path: proto.path,
            writable: proto.writable,
            freeBytes: proto.freeBytes,
            totalBytes: proto.totalBytes
        )
    }
}

extension HLSCacheStatus {
    fileprivate init(_ proto: TvosNetPlayer_V1_HlsCacheStatus) {
        self.init(
            evictionEnabled: proto.evictionEnabled,
            maxBytes: proto.maxBytes,
            highWatermarkPercent: Int(proto.highWatermarkPercent),
            lowWatermarkPercent: Int(proto.lowWatermarkPercent),
            highWatermarkBytes: proto.highWatermarkBytes,
            lowWatermarkBytes: proto.lowWatermarkBytes,
            usedBytes: proto.usedBytes,
            completedSessionCount: Int(proto.completedSessionCount),
            lastEviction: proto.hasLastEviction ? HLSCacheEvictionSummary(proto.lastEviction) : nil,
            weakNetwork: proto.hasWeakNetwork ? HLSWeakNetworkStatus(proto.weakNetwork) : nil,
            transcoding: proto.hasTranscoding ? LanTranscodingStatus(proto.transcoding) : nil,
            playback: proto.hasPlayback ? HLSPlaybackProgressStatus(proto.playback) : nil
        )
    }
}

extension PlaybackProgressReportResult {
    fileprivate init(_ proto: TvosNetPlayer_V1_ReportPlaybackProgressResponse) {
        self.init(
            accepted: proto.accepted,
            sessionID: proto.sessionID,
            message: proto.message
        )
    }
}

extension HLSPlaybackProgressStatus {
    fileprivate init(_ proto: TvosNetPlayer_V1_HlsPlaybackProgressStatus) {
        self.init(
            state: String(describing: proto.state),
            message: proto.message,
            sessionID: proto.sessionID,
            libraryItemID: proto.libraryItemID,
            variantID: proto.variantID,
            playbackURI: proto.playbackUri,
            positionSeconds: proto.positionSeconds,
            durationSeconds: proto.durationSeconds > 0 ? proto.durationSeconds : nil,
            lastIntent: String(describing: proto.lastIntent),
            updatedAt: proto.hasUpdatedAt ? Date(proto.updatedAt) : nil
        )
    }
}

extension LanTranscodingStatus {
    fileprivate init(_ proto: TvosNetPlayer_V1_LanTranscodingStatus) {
        self.init(
            enabled: proto.enabled,
            state: String(describing: proto.state),
            message: proto.message,
            profileID: proto.profileID,
            targetContainer: proto.targetContainer,
            targetVideoCodec: proto.targetVideoCodec,
            targetAudioCodec: proto.targetAudioCodec,
            maxConcurrentJobs: Int(proto.maxConcurrentJobs),
            activeJobCount: Int(proto.activeJobCount)
        )
    }
}

extension HLSWeakNetworkStatus {
    fileprivate init(_ proto: TvosNetPlayer_V1_HlsWeakNetworkStatus) {
        self.init(
            state: String(describing: proto.state),
            message: proto.message,
            degradedSessionCount: Int(proto.degradedSessionCount),
            unhealthyVariantCount: Int(proto.unhealthyVariantCount),
            retryingVariantCount: Int(proto.retryingVariantCount),
            cacheOnlySessionCount: Int(proto.cacheOnlySessionCount),
            lastChangedAt: proto.hasLastChangedAt ? Date(proto.lastChangedAt) : nil
        )
    }
}

extension HLSCacheEvictionSummary {
    fileprivate init(_ proto: TvosNetPlayer_V1_HlsCacheEvictionSummary) {
        self.init(
            reason: proto.reason,
            startedUsedBytes: proto.startedUsedBytes,
            finishedUsedBytes: proto.finishedUsedBytes,
            targetUsedBytes: proto.targetUsedBytes,
            projectedAddedBytes: proto.projectedAddedBytes,
            evictedBytes: proto.evictedBytes,
            evictedSessionIDs: proto.evictedSessionIds,
            targetReached: proto.targetReached,
            completedAt: proto.hasCompletedAt ? Date(proto.completedAt) : nil
        )
    }
}

extension CacheLibraryItem {
    fileprivate init(_ proto: TvosNetPlayer_V1_LibraryItem) {
        self.init(
            id: proto.id,
            title: proto.title,
            subtitle: proto.subtitle,
            source: String(describing: proto.source),
            sourceID: proto.sourceID,
            posterURI: proto.posterUri,
            variants: proto.variants.map(CacheMediaVariant.init)
        )
    }
}

extension CacheLibraryItemsPage {
    fileprivate init(_ proto: TvosNetPlayer_V1_ListLibraryItemsResponse) {
        self.init(
            items: proto.items.map(CacheLibraryItem.init),
            nextPageToken: proto.nextPageToken
        )
    }
}

extension CacheMediaVariant {
    fileprivate init(_ proto: TvosNetPlayer_V1_MediaVariant) {
        self.init(
            id: proto.id,
            label: proto.label,
            playbackProtocol: String(describing: proto.`protocol`),
            container: proto.container,
            videoCodec: proto.videoCodec,
            audioCodec: proto.audioCodec,
            width: Int(proto.width),
            height: Int(proto.height),
            bitrate: proto.bitrate,
            sizeBytes: proto.sizeBytes
        )
    }
}

extension CachePlaybackSource {
    fileprivate init(_ proto: TvosNetPlayer_V1_PlaybackSource) {
        self.init(
            itemID: proto.itemID,
            variantID: proto.variantID,
            playbackProtocol: String(describing: proto.`protocol`),
            uri: proto.uri
        )
    }
}

extension BilibiliResolveResult {
    fileprivate init(_ proto: TvosNetPlayer_V1_BilibiliResolveResult) {
        self.init(
            source: proto.source,
            title: proto.title,
            sourceKind: proto.sourceKind,
            candidates: proto.candidates.map(BilibiliResolvedCandidate.init),
            defaultSelectionID: proto.defaultSelectionID,
            candidatesTruncated: proto.candidatesTruncated
        )
    }
}

extension BilibiliResolvedCandidate {
    fileprivate init(_ proto: TvosNetPlayer_V1_BilibiliResolvedCandidate) {
        self.init(
            selectionID: proto.selectionID,
            title: proto.title,
            subtitle: proto.subtitle,
            sourceKind: proto.sourceKind,
            contentID: proto.contentID,
            index: Int(proto.index),
            durationSeconds: proto.durationSeconds,
            coverURI: proto.coverUri
        )
    }
}

extension BilibiliTaskSelection {
    fileprivate init(_ proto: TvosNetPlayer_V1_BilibiliTaskSelection) {
        self.init(
            mode: String(describing: proto.mode),
            selectionIDs: proto.selectionIds,
            rangeStartIndex: Int(proto.rangeStartIndex),
            rangeEndIndex: Int(proto.rangeEndIndex)
        )
    }
}

extension TvosNetPlayer_V1_BilibiliTaskSelection {
    fileprivate init(_ selection: BilibiliTaskSelection) {
        self.init()
        mode = TvosNetPlayer_V1_BilibiliTaskSelectionMode(selection.mode)
        selectionIds = selection.selectionIDs
        rangeStartIndex = clampedSelectionIndex(selection.rangeStartIndex)
        rangeEndIndex = clampedSelectionIndex(selection.rangeEndIndex)
    }
}

private func clampedSelectionIndex(_ value: Int) -> UInt32 {
    if value <= 0 {
        return 0
    }
    if value >= Int(UInt32.max) {
        return UInt32.max
    }
    return UInt32(value)
}

extension TvosNetPlayer_V1_BilibiliTaskSelectionMode {
    fileprivate init(_ mode: String) {
        switch mode {
        case "default":
            self = .default
        case "current":
            self = .current
        case "single":
            self = .single
        case "multiple":
            self = .multiple
        case "range":
            self = .range
        case "all":
            self = .all
        default:
            self = .unspecified
        }
    }
}

extension BilibiliTaskResultItem {
    fileprivate init(_ proto: TvosNetPlayer_V1_BilibiliTaskResultItem) {
        self.init(
            id: proto.id,
            selectionID: proto.selectionID,
            title: proto.title,
            subtitle: proto.subtitle,
            sourceKind: proto.sourceKind,
            contentID: proto.contentID,
            index: Int(proto.index),
            state: String(describing: proto.state),
            message: proto.message,
            libraryItemID: proto.libraryItemID,
            playbackSource: proto.hasPlaybackSource ? CachePlaybackSource(proto.playbackSource) : nil,
            playbackSession: proto.hasPlaybackSession ? CacheBilibiliPlaybackSession(proto.playbackSession) : nil
        )
    }
}

extension CacheTask {
    init(_ proto: TvosNetPlayer_V1_Task) {
        self.init(
            id: proto.id,
            kind: String(describing: proto.kind),
            state: String(describing: proto.state),
            source: proto.source,
            title: proto.title,
            progress: proto.progress,
            downloadedBytes: proto.downloadedBytes,
            totalBytes: proto.totalBytes,
            message: proto.message,
            libraryItemID: proto.libraryItemID,
            playbackSource: proto.hasPlaybackSource ? CachePlaybackSource(proto.playbackSource) : nil,
            playbackSession: proto.hasPlaybackSession ? CacheBilibiliPlaybackSession(proto.playbackSession) : nil,
            bilibiliSelection: proto.hasBilibiliSelection
                ? BilibiliTaskSelection(proto.bilibiliSelection)
                : nil,
            resultItems: proto.resultItems.map(BilibiliTaskResultItem.init)
        )
    }
}

extension CacheBilibiliPlaybackSession {
    fileprivate init(_ proto: TvosNetPlayer_V1_BilibiliPlaybackSession) {
        self.init(
            id: proto.id,
            title: proto.title,
            contentID: proto.contentID,
            selectedVariantID: proto.selectedVariantID,
            selectedVariant: proto.hasSelectedVariant
                ? CacheBilibiliPlaybackVariant(proto.selectedVariant)
                : nil,
            variants: proto.variants.map(CacheBilibiliPlaybackVariant.init),
            transcodingPlan: proto.hasTranscodingPlan
                ? LanTranscodingPlan(proto.transcodingPlan)
                : nil
        )
    }
}

extension LanTranscodingPlan {
    fileprivate init(_ proto: TvosNetPlayer_V1_LanTranscodingPlan) {
        self.init(
            state: String(describing: proto.state),
            profileID: proto.profileID,
            reason: proto.reason,
            sourceVariantID: proto.sourceVariantID,
            targetContainer: proto.targetContainer,
            targetVideoCodec: proto.targetVideoCodec,
            targetAudioCodec: proto.targetAudioCodec,
            outputProtocol: String(describing: proto.outputProtocol)
        )
    }
}

extension CacheBilibiliPlaybackVariant {
    fileprivate init(_ proto: TvosNetPlayer_V1_BilibiliPlaybackVariant) {
        self.init(
            id: proto.id,
            label: proto.label,
            sourceKind: proto.sourceKind,
            container: proto.container,
            videoCodec: proto.videoCodec,
            audioCodec: proto.audioCodec,
            width: Int(proto.width),
            height: Int(proto.height),
            bitrate: proto.bitrate,
            sizeBytes: proto.sizeBytes
        )
    }
}

extension TvosNetPlayer_V1_BilibiliPlaybackOptions {
    fileprivate init(_ options: BilibiliPlaybackTaskOptions) {
        self.init()
        qualityPreference = options.qualityPreference
        encodingPreference = options.encodingPreference
        audioLanguage = options.audioLanguagePreference
        preferTvApi = options.preferTVAPI
    }
}

extension TvosNetPlayer_V1_BilibiliDownloadOptions {
    init(_ options: BilibiliDownloadTaskOptions) {
        self.init()
        qualityPreference = options.qualityPreference
        encodingPreference = options.encodingPreference
        audioLanguage = options.audioLanguagePreference
        preferTvApi = options.preferTVAPI
        downloadSubtitles = options.downloadSubtitles
        downloadDanmaku = options.downloadDanmaku
        downloadCover = options.downloadCover
        subtitleAiPolicy = TvosNetPlayer_V1_BilibiliSubtitleAiPolicy(options.subtitleAIPolicy)
        danmakuFormats = options.danmakuFormats.map(TvosNetPlayer_V1_BilibiliDanmakuFormat.init)
    }
}

extension TvosNetPlayer_V1_BilibiliSubtitleAiPolicy {
    init(_ policy: BilibiliSubtitleAIPolicy) {
        switch policy {
        case .unspecified:
            self = .unspecified
        case .include:
            self = .include
        case .preferNonAI:
            self = .preferNonAi
        case .excludeAI:
            self = .excludeAi
        case .onlyAI:
            self = .onlyAi
        }
    }
}

extension TvosNetPlayer_V1_BilibiliDanmakuFormat {
    init(_ format: BilibiliDanmakuFormat) {
        switch format {
        case .xml:
            self = .xml
        case .ass:
            self = .ass
        }
    }
}

extension TvosNetPlayer_V1_PlaybackProgressIntent {
    fileprivate init(_ intent: PlaybackProgressIntent) {
        switch intent {
        case .started:
            self = .started
        case .playing:
            self = .playing
        case .seek:
            self = .seek
        case .paused:
            self = .paused
        case .stopped:
            self = .stopped
        }
    }
}

extension Date {
    fileprivate init(_ proto: Google_Protobuf_Timestamp) {
        self.init(
            timeIntervalSince1970: TimeInterval(proto.seconds)
                + TimeInterval(proto.nanos) / 1_000_000_000
        )
    }
}
