import Foundation
import SwiftUI
import TVOSNetPlayerCore

@main
struct MacOSNetPlayerApp: App {
    @StateObject private var model: PlayerViewModel
    @StateObject private var cacheModel: CacheLibraryViewModel
    @StateObject private var discoveryModel: CacheServerDiscoveryViewModel
    @StateObject private var bilibiliModel: BilibiliTaskViewModel
    @StateObject private var bilibiliLoginModel: BilibiliLoginViewModel
    @StateObject private var diagnosticsModel: CacheServerDiagnosticsViewModel

    init() {
        let isUITesting = Self.isUITesting
        let defaults = Self.makeDefaults(isUITesting: isUITesting)
        _model = StateObject(
            wrappedValue: PlayerViewModel(
                defaultStreamURLText: isUITesting ? "" : nil,
                defaults: defaults
            )
        )
        _cacheModel = StateObject(
            wrappedValue: CacheLibraryViewModel(
                defaultServerAddressText: isUITesting ? "" : nil,
                defaults: defaults
            )
        )
        _discoveryModel = StateObject(wrappedValue: CacheServerDiscoveryViewModel())
        _bilibiliModel = StateObject(wrappedValue: BilibiliTaskViewModel(defaults: defaults))
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

    private static func makeDefaults(isUITesting: Bool) -> UserDefaults {
        guard isUITesting else {
            return .standard
        }

        let suiteName = "\(Bundle.main.bundleIdentifier ?? "dev.joey.tvos-net-player.macos").ui-testing"
        guard let defaults = UserDefaults(suiteName: suiteName) else {
            fatalError("Could not create isolated UI test preferences.")
        }
        defaults.removePersistentDomain(forName: suiteName)
        return defaults
    }
}
