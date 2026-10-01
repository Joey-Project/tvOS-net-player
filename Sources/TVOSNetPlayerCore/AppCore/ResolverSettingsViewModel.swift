import Combine
import Foundation
import TVOSNetPlayerCacheClient

@MainActor
public final class ResolverSettingsViewModel: ObservableObject {
    public static let securityWarning =
        "The server control API currently has no authentication. Any client that can reach it can change resolver settings and redirect proxy requests. The shared access_key may be sent to any enabled third-party resolver, including bundled built-ins and custom resolvers."

    @Published public private(set) var snapshot: ResolverSettingsSnapshot?
    @Published public private(set) var disabledBuiltinHostIDs: Set<String> = []
    @Published public private(set) var customEndpoints: [ResolverCustomEndpoint] = []
    @Published public private(set) var isLoading = false
    @Published public private(set) var isSaving = false
    @Published public private(set) var isAvailable = false
    @Published public private(set) var errorMessage: String?
    @Published public private(set) var statusMessage: String?

    private let client: any CacheControlClient
    private var loadRequestID = 0
    private var draftRevision = 0

    public init(client: any CacheControlClient) {
        self.client = client
    }

    public var builtinEndpoints: [ResolverEndpoint] {
        snapshot?.builtin ?? []
    }

    public var hasChanges: Bool {
        guard let snapshot else { return false }
        return disabledBuiltinHostIDs != Set(snapshot.disabledBuiltinHostIDs)
            || customEndpoints != snapshot.custom
    }

    public func load() async {
        loadRequestID += 1
        let requestID = loadRequestID
        let startingDraftRevision = draftRevision
        isLoading = true
        errorMessage = nil
        do {
            let server = try await client.getServerInfo()
            guard requestID == loadRequestID else { return }
            guard server.supportsResolverSettingsWrite else {
                isAvailable = false
                snapshot = nil
                errorMessage = "This cache server does not support resolver settings."
                isLoading = false
                return
            }
            let settings = try await client.getResolverSettings()
            guard requestID == loadRequestID else { return }
            isAvailable = true
            apply(settings, preservingDraft: draftRevision != startingDraftRevision)
            statusMessage = "Resolver settings loaded."
        } catch {
            guard requestID == loadRequestID else { return }
            isAvailable = false
            errorMessage = Self.message(for: error)
        }
        isLoading = false
    }

    public func reload() async {
        await load()
    }

    public func setBuiltin(_ endpoint: ResolverEndpoint, enabled: Bool) {
        if enabled {
            if disabledBuiltinHostIDs.remove(endpoint.hostID) != nil {
                draftRevision += 1
            }
        } else {
            if disabledBuiltinHostIDs.insert(endpoint.hostID).inserted {
                draftRevision += 1
            }
        }
    }

    public func addCustom(name: String, origin: String, regions: [ResolverRegion]) throws {
        guard customEndpoints.count < 32 else {
            throw ResolverSettingsValidationError.tooManyEntries
        }
        let entry = try validatedCustom(name: name, origin: origin, regions: regions)
        guard !customEndpoints.contains(where: { hostID(for: $0.origin) == hostID(for: entry.origin) }) else {
            throw ResolverSettingsValidationError.duplicateHost
        }
        customEndpoints.append(entry)
        draftRevision += 1
        errorMessage = nil
    }

    public func updateCustom(
        at index: Int,
        name: String,
        origin: String,
        regions: [ResolverRegion],
        enabled: Bool
    ) throws {
        guard customEndpoints.indices.contains(index) else {
            throw ResolverSettingsValidationError.missingEntry
        }
        let validated = try validatedCustom(name: name, origin: origin, regions: regions)
        let normalizedHostID = hostID(for: validated.origin)
        guard
            !builtinEndpoints.contains(where: {
                $0.hostID.caseInsensitiveCompare(normalizedHostID) == .orderedSame
            })
        else {
            throw ResolverSettingsValidationError.duplicateHost
        }
        guard
            !customEndpoints.enumerated().contains(where: {
                $0.offset != index && hostID(for: $0.element.origin) == normalizedHostID
            })
        else {
            throw ResolverSettingsValidationError.duplicateHost
        }
        customEndpoints[index] = ResolverCustomEndpoint(
            name: validated.name,
            origin: validated.origin,
            regions: validated.regions,
            enabled: enabled
        )
        draftRevision += 1
        errorMessage = nil
    }

    public func removeCustom(at index: Int) {
        guard customEndpoints.indices.contains(index) else { return }
        customEndpoints.remove(at: index)
        draftRevision += 1
    }

    public func save() async {
        guard let snapshot, hasChanges, !isSaving else { return }
        isSaving = true
        errorMessage = nil
        statusMessage = nil
        let startingDraftRevision = draftRevision
        let request = UpdateResolverSettingsRequest(
            disabledBuiltinHostIDs: disabledBuiltinHostIDs.sorted(),
            custom: customEndpoints,
            expectedRevision: snapshot.revision
        )
        do {
            let saved = try await client.updateResolverSettings(request)
            apply(saved, preservingDraft: draftRevision != startingDraftRevision)
            statusMessage = "Resolver settings saved."
        } catch is CacheControlClientRevisionConflict {
            switch await reloadAfterConflict(startingDraftRevision: startingDraftRevision) {
            case .reloaded(preservingNewerEdits: true):
                errorMessage =
                    "Settings changed on the server. The submitted draft was rejected, but newer edits made while saving were preserved. Review them before saving again."
            case .reloaded(preservingNewerEdits: false):
                errorMessage =
                    "Settings changed on the server. Your unsaved edits were discarded and the latest values were reloaded. Review them before saving again."
            case .failed:
                break
            }
        } catch {
            errorMessage = Self.message(for: error)
        }
        isSaving = false
    }

    private func reloadAfterConflict(startingDraftRevision: Int) async -> ConflictReloadResult {
        do {
            let latest = try await client.getResolverSettings()
            let preservedNewerEdits = draftRevision != startingDraftRevision
            apply(latest, preservingDraft: preservedNewerEdits)
            return .reloaded(preservingNewerEdits: preservedNewerEdits)
        } catch {
            errorMessage =
                "Settings changed on the server, and the latest values could not be loaded: \(Self.message(for: error))"
            return .failed
        }
    }

    private func apply(_ value: ResolverSettingsSnapshot, preservingDraft: Bool = false) {
        snapshot = value
        if !preservingDraft {
            disabledBuiltinHostIDs = Set(value.disabledBuiltinHostIDs)
            customEndpoints = value.custom
            draftRevision += 1
        }
    }

    private func validatedCustom(
        name: String,
        origin: String,
        regions: [ResolverRegion]
    ) throws -> ResolverCustomEndpoint {
        let cleanName = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard
            !cleanName.isEmpty,
            cleanName.utf8.count <= 128,
            !cleanName.unicodeScalars.contains(where: { $0.value < 0x20 || $0.value == 0x7F })
        else {
            throw ResolverSettingsValidationError.invalidName
        }
        guard !regions.isEmpty, Set(regions).count == regions.count else {
            throw ResolverSettingsValidationError.invalidRegions
        }
        guard let normalizedOrigin = Self.normalizedHTTPSOrigin(origin) else {
            throw ResolverSettingsValidationError.invalidOrigin
        }
        let candidateHostID = hostID(for: normalizedOrigin)
        guard
            !builtinEndpoints.contains(where: {
                $0.hostID.caseInsensitiveCompare(candidateHostID) == .orderedSame
            })
        else {
            throw ResolverSettingsValidationError.duplicateHost
        }
        return ResolverCustomEndpoint(
            name: cleanName,
            origin: normalizedOrigin,
            regions: regions,
            enabled: true
        )
    }

    private static func normalizedHTTPSOrigin(_ text: String) -> String? {
        let raw = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard
            let components = URLComponents(string: raw),
            components.url != nil,
            components.scheme?.lowercased() == "https",
            let host = components.host,
            !host.isEmpty,
            components.user == nil,
            components.password == nil,
            components.query == nil,
            components.fragment == nil,
            components.path.isEmpty || components.path == "/",
            !host.contains(":"),
            !isIPv4Literal(host)
        else {
            return nil
        }
        if let port = components.port, !(1...65_535).contains(port) {
            return nil
        }
        let hostID = host.lowercased()
        let portSuffix = components.port.map { $0 == 443 ? "" : ":\($0)" } ?? ""
        return "https://\(hostID)\(portSuffix)/"
    }

    private static func isIPv4Literal(_ host: String) -> Bool {
        let parts = host.split(separator: ".", omittingEmptySubsequences: false)
        guard parts.count == 4 else { return false }
        return parts.allSatisfy { part in
            !part.isEmpty
                && part.utf8.allSatisfy { $0 >= UInt8(ascii: "0") && $0 <= UInt8(ascii: "9") }
                && (Int(part).map { (0...255).contains($0) } ?? false)
        }
    }

    private func hostID(for origin: String) -> String {
        guard let components = URLComponents(string: origin), let host = components.host else {
            return origin.lowercased()
        }
        let normalizedHost = host.lowercased()
        guard let port = components.port, port != 443 else { return normalizedHost }
        return "\(normalizedHost):\(port)"
    }

    private static func message(for error: Error) -> String {
        if error is CacheControlClientUnsupportedFeature {
            return "This cache server does not support resolver settings."
        }
        return error.localizedDescription
    }
}

private enum ConflictReloadResult {
    case reloaded(preservingNewerEdits: Bool)
    case failed
}

public enum ResolverSettingsValidationError: Error, Equatable, Sendable {
    case invalidName
    case invalidOrigin
    case invalidRegions
    case duplicateHost
    case missingEntry
    case tooManyEntries
}
