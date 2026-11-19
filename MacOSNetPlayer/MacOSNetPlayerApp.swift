import Foundation
import SwiftUI
import TVOSNetPlayerCore

struct InitialFormInputConfiguration: Equatable {
    let serverAddressText: String?
    let bilibiliSourceText: String?

    static func resolve(
        arguments: [String],
        environment: [String: String],
        isDebugBuild: Bool
    ) -> Self {
        let argumentSet = Set(arguments)
        guard isDebugBuild,
            argumentSet.contains("--ui-testing"),
            !argumentSet.contains("--ui-testing-preserve-preferences")
        else {
            return Self(serverAddressText: nil, bilibiliSourceText: nil)
        }
        return Self(
            serverAddressText: environment["UI_E2E_INITIAL_SERVER_ADDRESS"],
            bilibiliSourceText: environment["UI_E2E_INITIAL_BILIBILI_SOURCE"]
        )
    }
}

@main
struct MacOSNetPlayerApp: App {
    @StateObject private var model: PlayerViewModel
    @StateObject private var cacheModel: CacheLibraryViewModel
    @StateObject private var discoveryModel: CacheServerDiscoveryViewModel
    @StateObject private var bilibiliModel: BilibiliTaskViewModel
    @StateObject private var resumptionStore: ClientResumptionStore
    @StateObject private var bilibiliLoginModel: BilibiliLoginViewModel
    @StateObject private var diagnosticsModel: CacheServerDiagnosticsViewModel

    init() {
        let isUITesting = Self.isUITesting
        let preserveUITestingPreferences = Self.preserveUITestingPreferences
        let initialFormInputs = InitialFormInputConfiguration.resolve(
            arguments: ProcessInfo.processInfo.arguments,
            environment: ProcessInfo.processInfo.environment,
            isDebugBuild: Self.isDebugBuild
        )
        let defaults = Self.makeDefaults(
            isUITesting: isUITesting,
            preservePreferences: preserveUITestingPreferences
        )
        let resumptionStore = ClientResumptionStore(defaults: defaults)
        let defaultServerAddressText: String?
        if isUITesting && !preserveUITestingPreferences {
            defaultServerAddressText = initialFormInputs.serverAddressText ?? ""
        } else if defaults.object(forKey: CacheLibraryViewModel.serverAddressDefaultsKey) == nil {
            defaultServerAddressText = resumptionStore.record?.task.endpoint.displayAddress
        } else {
            defaultServerAddressText = nil
        }

        _resumptionStore = StateObject(wrappedValue: resumptionStore)
        _model = StateObject(
            wrappedValue: PlayerViewModel(
                defaultStreamURLText: isUITesting ? "" : nil,
                defaults: defaults,
                resumptionStore: resumptionStore
            )
        )
        _cacheModel = StateObject(
            wrappedValue: CacheLibraryViewModel(
                defaultServerAddressText: defaultServerAddressText,
                defaults: defaults
            )
        )
        _discoveryModel = StateObject(wrappedValue: CacheServerDiscoveryViewModel())
        _bilibiliModel = StateObject(
            wrappedValue: BilibiliTaskViewModel(
                sourceText: initialFormInputs.bilibiliSourceText ?? "",
                defaults: defaults,
                resumptionStore: resumptionStore
            )
        )
        _bilibiliLoginModel = StateObject(wrappedValue: BilibiliLoginViewModel())
        _diagnosticsModel = StateObject(wrappedValue: CacheServerDiagnosticsViewModel())
    }

    var body: some Scene {
        WindowGroup {
            ContentView(
                model: model,
                cacheModel: cacheModel,
                discoveryModel: discoveryModel,
                bilibiliModel: bilibiliModel,
                diagnosticsModel: diagnosticsModel,
                bilibiliLoginModel: bilibiliLoginModel
            )
        }
        .defaultSize(width: 960, height: 600)
        .commands {
            CommandGroup(replacing: .newItem) {}
        }
    }

    private static var isUITesting: Bool {
        #if DEBUG
            ProcessInfo.processInfo.arguments.contains("--ui-testing")
        #else
            false
        #endif
    }

    private static var isDebugBuild: Bool {
        #if DEBUG
            true
        #else
            false
        #endif
    }

    private static var preserveUITestingPreferences: Bool {
        #if DEBUG
            ProcessInfo.processInfo.arguments.contains("--ui-testing-preserve-preferences")
        #else
            false
        #endif
    }

    private static func makeDefaults(isUITesting: Bool, preservePreferences: Bool) -> UserDefaults {
        guard isUITesting else {
            return .standard
        }

        let suiteName = "\(Bundle.main.bundleIdentifier ?? "dev.joey.tvos-net-player.macos").ui-testing"
        guard let defaults = UserDefaults(suiteName: suiteName) else {
            fatalError("Could not create isolated UI test preferences.")
        }
        if !preservePreferences {
            defaults.removePersistentDomain(forName: suiteName)
        }
        return defaults
    }
}
