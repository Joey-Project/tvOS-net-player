import SwiftUI
import TVOSNetPlayerCore

@main
struct MacOSNetPlayerApp: App {
    @StateObject private var model = PlayerViewModel()
    @StateObject private var cacheModel = CacheLibraryViewModel()
    @StateObject private var discoveryModel = CacheServerDiscoveryViewModel()
    @StateObject private var bilibiliModel = BilibiliTaskViewModel()
    @StateObject private var bilibiliLoginModel = BilibiliLoginViewModel()
    @StateObject private var diagnosticsModel = CacheServerDiagnosticsViewModel()

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
        .commands {
            CommandGroup(replacing: .newItem) {}
        }
    }
}
