import TVOSNetPlayerCore
import XCTest
@testable import MacOSNetPlayer

@MainActor
final class AppShellIntegrationTests: XCTestCase {
    func testContentViewCanRenderWithSharedAppCoreModels() throws {
        let defaultsSuiteName = "MacOSNetPlayerTests-\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: defaultsSuiteName))
        defaults.removePersistentDomain(forName: defaultsSuiteName)
        defer {
            defaults.removePersistentDomain(forName: defaultsSuiteName)
        }

        let playerModel = PlayerViewModel(defaults: defaults, autoplay: false)
        let cacheModel = CacheLibraryViewModel(defaults: defaults)
        let bilibiliModel = BilibiliTaskViewModel()
        let discoveryModel = CacheServerDiscoveryViewModel()
        let diagnosticsModel = CacheServerDiagnosticsViewModel()
        let view = ContentView(
            model: playerModel,
            cacheModel: cacheModel,
            discoveryModel: discoveryModel,
            bilibiliModel: bilibiliModel,
            diagnosticsModel: diagnosticsModel
        )

        _ = view.body

        XCTAssertEqual(playerModel.streamURLText, "")
        XCTAssertEqual(cacheModel.serverAddressText, "")
        XCTAssertEqual(bilibiliModel.sourceText, "")
        XCTAssertEqual(diagnosticsModel.statusMessage, "Diagnostics not loaded.")
    }
}
