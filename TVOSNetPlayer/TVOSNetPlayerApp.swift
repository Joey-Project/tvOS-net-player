import SwiftUI

@main
struct TVOSNetPlayerApp: App {
    @StateObject private var model = PlayerViewModel()
    @StateObject private var cacheModel = CacheLibraryViewModel()

    var body: some Scene {
        WindowGroup {
            ContentView(model: model, cacheModel: cacheModel)
        }
    }
}
