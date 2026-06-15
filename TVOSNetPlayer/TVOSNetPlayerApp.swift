import SwiftUI
import TVOSNetPlayerCore

@main
struct TVOSNetPlayerApp: App {
    @StateObject private var model = PlayerViewModel()
    @StateObject private var cacheModel = CacheLibraryViewModel()
    @StateObject private var bilibiliModel = BilibiliTaskViewModel()

    var body: some Scene {
        WindowGroup {
            ContentView(model: model, cacheModel: cacheModel, bilibiliModel: bilibiliModel)
        }
    }
}
