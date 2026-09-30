import SwiftUI
import TVOSNetPlayerCore

@main
struct TVOSNetPlayerApp: App {
    @StateObject private var model = PlayerViewModel()
    @StateObject private var cacheModel = CacheLibraryViewModel()
    @StateObject private var discoveryModel = CacheServerDiscoveryViewModel()
    @StateObject private var bilibiliModel = BilibiliTaskViewModel()
    @StateObject private var bilibiliLoginModel = BilibiliLoginViewModel()

    var body: some Scene {
        WindowGroup {
            ContentView(
                model: model,
                cacheModel: cacheModel,
                discoveryModel: discoveryModel,
                bilibiliModel: bilibiliModel,
                bilibiliLoginModel: bilibiliLoginModel
            )
        }
    }
}
