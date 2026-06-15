import XCTest
import TVOSNetPlayerCore

final class AppCoreIntegrationTests: XCTestCase {
    @MainActor
    func testTVOSAppCanInstantiateSharedAppCoreModels() {
        let playerModel = PlayerViewModel(defaultStreamURLText: "", autoplay: false)
        let cacheModel = CacheLibraryViewModel(defaultServerAddressText: "")
        let bilibiliModel = BilibiliTaskViewModel()

        XCTAssertEqual(playerModel.statusMessage, "Ready for an HTTP or HTTPS stream on your network.")
        XCTAssertEqual(cacheModel.statusMessage, "Cache server not connected.")
        XCTAssertEqual(bilibiliModel.statusMessage, "No Bilibili playback task submitted.")
    }
}
