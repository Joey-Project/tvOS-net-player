import Combine
@preconcurrency import Foundation
@preconcurrency import Network
import TVOSNetPlayerCacheClient

public struct DiscoveredCacheServer: Identifiable, Equatable, Sendable {
    public let id: String
    public let name: String
    public let endpoint: CacheServerEndpoint
    public let serverID: String?
    public let version: String?

    public init(
        id: String,
        name: String,
        endpoint: CacheServerEndpoint,
        serverID: String? = nil,
        version: String? = nil
    ) {
        self.id = id
        self.name = name
        self.endpoint = endpoint
        self.serverID = serverID
        self.version = version
    }

    public var displayName: String {
        name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? endpoint.displayAddress : name
    }

    public var detailText: String {
        if let version, !version.isEmpty {
            return "\(endpoint.displayAddress) · \(version)"
        }

        return endpoint.displayAddress
    }
}

public struct CacheServerDiscoverySnapshot: Equatable, Sendable {
    public let servers: [DiscoveredCacheServer]
    public let isSearching: Bool
    public let errorMessage: String?

    public init(
        servers: [DiscoveredCacheServer],
        isSearching: Bool,
        errorMessage: String? = nil
    ) {
        self.servers = servers
        self.isSearching = isSearching
        self.errorMessage = errorMessage
    }
}

public protocol CacheServerDiscoveryClient: Sendable {
    func snapshots() -> AsyncStream<CacheServerDiscoverySnapshot>
}

@MainActor
public final class CacheServerDiscoveryViewModel: ObservableObject {
    @Published public private(set) var discoveredServers: [DiscoveredCacheServer] = []
    @Published public private(set) var isSearching = false
    @Published public private(set) var errorMessage: String?
    @Published public private(set) var selectedServerID: String?

    private let discoveryClient: any CacheServerDiscoveryClient
    private var discoveryTask: Task<Void, Never>?

    public init(discoveryClient: any CacheServerDiscoveryClient = BonjourCacheServerDiscoveryClient()) {
        self.discoveryClient = discoveryClient
    }

    deinit {
        discoveryTask?.cancel()
    }

    public var statusMessage: String {
        if let errorMessage {
            return "Discovery failed: \(errorMessage)"
        }
        if isSearching && discoveredServers.isEmpty {
            return "Searching for LAN cache servers..."
        }
        if discoveredServers.isEmpty {
            return "No LAN cache servers discovered."
        }
        if discoveredServers.count == 1 {
            return "Found 1 LAN cache server."
        }

        return "Found \(discoveredServers.count) LAN cache servers."
    }

    public var preferredServer: DiscoveredCacheServer? {
        if let selectedServerID,
            let selected = discoveredServers.first(where: { $0.id == selectedServerID })
        {
            return selected
        }

        return discoveredServers.first
    }

    public func start() {
        guard discoveryTask == nil else {
            return
        }

        isSearching = true
        errorMessage = nil
        let stream = discoveryClient.snapshots()
        discoveryTask = Task { [weak self] in
            for await snapshot in stream {
                self?.apply(snapshot)
            }
            self?.finish()
        }
    }

    public func stop() {
        discoveryTask?.cancel()
        discoveryTask = nil
        isSearching = false
    }

    public func select(_ server: DiscoveredCacheServer) {
        selectedServerID = server.id
    }

    private func apply(_ snapshot: CacheServerDiscoverySnapshot) {
        discoveredServers = snapshot.servers.sorted {
            if $0.displayName.localizedCaseInsensitiveCompare($1.displayName) == .orderedSame {
                return $0.endpoint.displayAddress < $1.endpoint.displayAddress
            }

            return $0.displayName.localizedCaseInsensitiveCompare($1.displayName) == .orderedAscending
        }
        isSearching = snapshot.isSearching
        errorMessage = snapshot.errorMessage
        if let selectedServerID, !discoveredServers.contains(where: { $0.id == selectedServerID }) {
            self.selectedServerID = nil
        }
    }

    private func finish() {
        discoveryTask = nil
        isSearching = false
    }
}

public final class BonjourCacheServerDiscoveryClient: NSObject, CacheServerDiscoveryClient, @unchecked Sendable {
    public static let serviceType = "_tvos-net-player._tcp"
    public static let serviceDomain = "local."

    public override init() {}

    public func snapshots() -> AsyncStream<CacheServerDiscoverySnapshot> {
        let session = BonjourCacheServerDiscoverySession(
            serviceType: Self.serviceType,
            serviceDomain: Self.serviceDomain
        )
        return session.snapshots()
    }
}

private final class BonjourCacheServerDiscoverySession: NSObject, NetServiceDelegate, @unchecked Sendable {
    private static let initialResolveRetryDelay: TimeInterval = 2
    private static let maxResolveRetryDelay: TimeInterval = 30
    private static let browserRestartDelay: TimeInterval = 5

    private let serviceType: String
    private let serviceDomain: String
    private let queue = DispatchQueue(label: "TVOSNetPlayer.CacheServerDiscovery")
    private var browser: NWBrowser?
    private var continuation: AsyncStream<CacheServerDiscoverySnapshot>.Continuation?
    private var activeServiceKeys: Set<BonjourServiceKey> = []
    private var resolvingServices: [BonjourServiceKey: NetService] = [:]
    private var resolvedServers: [BonjourServiceKey: DiscoveredCacheServer] = [:]
    private var resolveRetryCounts: [BonjourServiceKey: Int] = [:]
    private var isSearching = false
    private var isRestartingBrowser = false
    private var errorMessage: String?

    init(serviceType: String, serviceDomain: String) {
        self.serviceType = serviceType
        self.serviceDomain = serviceDomain
    }

    func snapshots() -> AsyncStream<CacheServerDiscoverySnapshot> {
        AsyncStream { continuation in
            self.queue.async {
                self.continuation = continuation
                self.startBrowser()
                self.emitSnapshot()
            }
            continuation.onTermination = { _ in
                self.queue.async {
                    self.cancel()
                }
            }
        }
    }

    private func startBrowser() {
        guard browser == nil else {
            return
        }

        let browser = NWBrowser(
            for: .bonjour(type: serviceType, domain: serviceDomain),
            using: .tcp
        )
        browser.stateUpdateHandler = { [weak self] state in
            self?.handleBrowserState(state)
        }
        browser.browseResultsChangedHandler = { [weak self] results, _ in
            self?.handleBrowseResults(results)
        }
        self.browser = browser
        isSearching = true
        browser.start(queue: queue)
    }

    private func cancel() {
        browser?.cancel()
        browser = nil
        for service in resolvingServices.values {
            let handle = NetServiceHandle(service: service)
            DispatchQueue.main.async {
                handle.service.stop()
            }
        }
        resolvingServices = [:]
        activeServiceKeys = []
        resolvedServers = [:]
        resolveRetryCounts = [:]
        isSearching = false
        isRestartingBrowser = false
        continuation = nil
    }

    private func handleBrowserState(_ state: NWBrowser.State) {
        switch state {
        case .ready:
            isSearching = true
            isRestartingBrowser = false
            errorMessage = nil
        case .failed(let error):
            isSearching = false
            errorMessage = error.localizedDescription
            let failedBrowser = browser
            browser = nil
            failedBrowser?.cancel()
            scheduleBrowserRestart()
        case .cancelled:
            isSearching = false
        default:
            isSearching = true
        }

        emitSnapshot()
    }

    private func scheduleBrowserRestart() {
        guard continuation != nil, !isRestartingBrowser else {
            return
        }

        isRestartingBrowser = true
        queue.asyncAfter(deadline: .now() + Self.browserRestartDelay) { [weak self] in
            guard let self else {
                return
            }

            self.isRestartingBrowser = false
            guard self.continuation != nil, self.browser == nil else {
                return
            }

            self.errorMessage = nil
            self.startBrowser()
            self.emitSnapshot()
        }
    }

    private func handleBrowseResults(_ results: Set<NWBrowser.Result>) {
        var nextKeys: Set<BonjourServiceKey> = []
        var keysToResolve: [BonjourServiceKey] = []
        for result in results {
            guard let key = BonjourServiceKey(endpoint: result.endpoint) else {
                continue
            }
            nextKeys.insert(key)
            if resolvedServers[key] == nil && resolvingServices[key] == nil {
                keysToResolve.append(key)
            }
        }

        let removedKeys = activeServiceKeys.subtracting(nextKeys)
        activeServiceKeys = nextKeys
        for key in removedKeys {
            if let service = resolvingServices.removeValue(forKey: key) {
                let handle = NetServiceHandle(service: service)
                DispatchQueue.main.async {
                    handle.service.stop()
                }
            }
            resolvedServers[key] = nil
            resolveRetryCounts[key] = nil
        }

        for key in keysToResolve where activeServiceKeys.contains(key) {
            resolve(key)
        }
        emitSnapshot()
    }

    private func resolve(_ key: BonjourServiceKey) {
        let service = NetService(domain: key.domain, type: key.type, name: key.name)
        service.delegate = self
        resolvingServices[key] = service
        let handle = NetServiceHandle(service: service)
        DispatchQueue.main.async {
            handle.service.resolve(withTimeout: 5)
        }
    }

    func netServiceDidResolveAddress(_ sender: NetService) {
        let serviceName = sender.name
        let hostName = sender.hostName
        let port = sender.port
        let txtRecordData = sender.txtRecordData()
        let key = BonjourServiceKey(netService: sender)
        queue.async {
            guard let key else {
                return
            }
            self.resolvingServices[key] = nil
            guard self.activeServiceKeys.contains(key) else {
                self.resolvedServers[key] = nil
                self.resolveRetryCounts[key] = nil
                self.emitSnapshot()
                return
            }
            guard let host = Self.normalizedHostName(hostName), port > 0 else {
                self.scheduleResolveRetry(for: key)
                self.emitSnapshot()
                return
            }

            self.resolveRetryCounts[key] = nil
            let txt = Self.txtRecordValues(from: txtRecordData)
            let serverName = txt["server_name"]?.nonEmptyString ?? serviceName
            let server = DiscoveredCacheServer(
                id: key.id,
                name: serverName,
                endpoint: CacheServerEndpoint(host: host, port: port),
                serverID: txt["server_id"]?.nonEmptyString,
                version: txt["version"]?.nonEmptyString
            )
            self.resolvedServers[key] = server
            self.emitSnapshot()
        }
    }

    func netService(_ sender: NetService, didNotResolve errorDict: [String: NSNumber]) {
        let key = BonjourServiceKey(netService: sender)
        queue.async {
            if let key {
                self.resolvingServices[key] = nil
                self.scheduleResolveRetry(for: key)
            }
            self.emitSnapshot()
        }
    }

    private func scheduleResolveRetry(for key: BonjourServiceKey) {
        guard
            activeServiceKeys.contains(key),
            resolvedServers[key] == nil,
            resolvingServices[key] == nil
        else {
            return
        }

        let retryCount = (resolveRetryCounts[key] ?? 0) + 1
        resolveRetryCounts[key] = retryCount
        let delay = min(
            Self.initialResolveRetryDelay * Double(1 << min(retryCount - 1, 4)),
            Self.maxResolveRetryDelay
        )
        queue.asyncAfter(deadline: .now() + delay) { [weak self] in
            guard
                let self,
                self.activeServiceKeys.contains(key),
                self.resolvedServers[key] == nil,
                self.resolvingServices[key] == nil
            else {
                return
            }

            self.resolve(key)
        }
    }

    private func emitSnapshot() {
        let servers = activeServiceKeys.compactMap { resolvedServers[$0] }
        continuation?.yield(
            CacheServerDiscoverySnapshot(
                servers: servers,
                isSearching: isSearching,
                errorMessage: errorMessage
            )
        )
    }

    private static func normalizedHostName(_ value: String?) -> String? {
        let trimmed = (value ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        let withoutTrailingDot = trimmed.hasSuffix(".") ? String(trimmed.dropLast()) : trimmed
        return withoutTrailingDot.isEmpty ? nil : withoutTrailingDot
    }

    private static func txtRecordValues(from data: Data?) -> [String: String] {
        guard let data else {
            return [:]
        }

        return NetService.dictionary(fromTXTRecord: data).reduce(into: [:]) { values, element in
            values[element.key] = String(data: element.value, encoding: .utf8)
        }
    }
}

private struct NetServiceHandle: @unchecked Sendable {
    let service: NetService
}

private struct BonjourServiceKey: Hashable, Sendable {
    let name: String
    let type: String
    let domain: String

    var id: String {
        "\(name).\(type)\(domain)"
    }

    init?(endpoint: NWEndpoint) {
        guard case let .service(name, type, domain, _) = endpoint else {
            return nil
        }
        self.init(name: name, type: type, domain: domain)
    }

    init?(netService: NetService) {
        self.init(name: netService.name, type: netService.type, domain: netService.domain)
    }

    init(name: String, type: String, domain: String) {
        self.name = name
        self.type = Self.normalizedType(type)
        self.domain = Self.normalizedDomain(domain)
    }

    private static func normalizedType(_ value: String) -> String {
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.hasSuffix(".") ? trimmed : "\(trimmed)."
    }

    private static func normalizedDomain(_ value: String) -> String {
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty {
            return "local."
        }

        return trimmed.hasSuffix(".") ? trimmed : "\(trimmed)."
    }
}

private extension String {
    var nonEmptyString: String? {
        let trimmed = trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }
}
