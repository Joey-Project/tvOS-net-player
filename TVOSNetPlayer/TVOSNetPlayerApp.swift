import SwiftUI

@main
struct TVOSNetPlayerApp: App {
    @StateObject private var model = PlayerViewModel()

    var body: some Scene {
        WindowGroup {
            ContentView(model: model)
        }
    }
}
