import SwiftUI
import TVOSNetPlayerCore

@main
struct MacOSNetPlayerApp: App {
    @StateObject private var model = PlayerViewModel()
    @StateObject private var cacheModel = CacheLibraryViewModel()

    var body: some Scene {
        WindowGroup {
            ContentView(model: model, cacheModel: cacheModel)
        }
        .commands {
            CommandGroup(replacing: .newItem) {}
        }
    }
}
