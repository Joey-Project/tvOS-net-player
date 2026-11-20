import AVKit
import AppKit
import SwiftUI
import TVOSNetPlayerCore
import XCTest
@testable import MacOSNetPlayer

@MainActor
final class AppShellIntegrationTests: XCTestCase {
    func testInitialFormInputsRequireFreshDebugUITestingLaunch() {
        let environment = [
            "UI_E2E_INITIAL_SERVER_ADDRESS": "https://cache.example.test:50051/path?x=1&y=%2F#frag.",
            "UI_E2E_INITIAL_BILIBILI_SOURCE": "https://www.bilibili.com/video/BV1xx411c7mD?p=2&x=%2F.",
        ]

        XCTAssertEqual(
            InitialFormInputConfiguration.resolve(
                arguments: ["app", "--ui-testing"],
                environment: environment,
                isDebugBuild: true
            ),
            InitialFormInputConfiguration(
                serverAddressText: environment["UI_E2E_INITIAL_SERVER_ADDRESS"],
                bilibiliSourceText: environment["UI_E2E_INITIAL_BILIBILI_SOURCE"]
            )
        )
        XCTAssertEqual(
            InitialFormInputConfiguration.resolve(
                arguments: ["app"],
                environment: environment,
                isDebugBuild: true
            ),
            InitialFormInputConfiguration(serverAddressText: nil, bilibiliSourceText: nil)
        )
        XCTAssertEqual(
            InitialFormInputConfiguration.resolve(
                arguments: ["app", "--ui-testing", "--ui-testing-preserve-preferences"],
                environment: environment,
                isDebugBuild: true
            ),
            InitialFormInputConfiguration(serverAddressText: nil, bilibiliSourceText: nil)
        )
        XCTAssertEqual(
            InitialFormInputConfiguration.resolve(
                arguments: ["app", "--ui-testing"],
                environment: environment,
                isDebugBuild: false
            ),
            InitialFormInputConfiguration(serverAddressText: nil, bilibiliSourceText: nil)
        )
    }

    func testInitialFormInputsOnlyInitializeModelsWithoutPersistenceOrSubmission() throws {
        let defaultsSuiteName = "MacOSNetPlayerTests-InitialInputs-\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: defaultsSuiteName))
        defaults.removePersistentDomain(forName: defaultsSuiteName)
        defer { defaults.removePersistentDomain(forName: defaultsSuiteName) }

        let serverAddress = "https://cache.example.test:50051/path?x=1&y=%2F#frag."
        let source = "https://www.bilibili.com/video/BV1xx411c7mD?p=2&x=%2F."
        let cacheModel = CacheLibraryViewModel(
            defaultServerAddressText: serverAddress,
            defaults: defaults
        )
        let bilibiliModel = BilibiliTaskViewModel(sourceText: source, defaults: defaults)

        XCTAssertEqual(cacheModel.serverAddressText, serverAddress)
        XCTAssertEqual(bilibiliModel.sourceText, source)
        XCTAssertEqual(cacheModel.statusMessage, "Cache server not connected.")
        XCTAssertFalse(cacheModel.isLoading)
        XCTAssertNil(bilibiliModel.currentTask)
        XCTAssertFalse(bilibiliModel.isSubmitting)
        XCTAssertFalse(bilibiliModel.isResolving)
        XCTAssertFalse(bilibiliModel.isWatching)
        XCTAssertNil(defaults.object(forKey: CacheLibraryViewModel.serverAddressDefaultsKey))
    }

    func testModelWithoutInitialServerAddressPreservesSavedEndpoint() throws {
        let defaultsSuiteName = "MacOSNetPlayerTests-SavedEndpoint-\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: defaultsSuiteName))
        defaults.removePersistentDomain(forName: defaultsSuiteName)
        defer { defaults.removePersistentDomain(forName: defaultsSuiteName) }
        defaults.set("saved-cache.example.test:50051", forKey: CacheLibraryViewModel.serverAddressDefaultsKey)

        let model = CacheLibraryViewModel(defaults: defaults)

        XCTAssertEqual(model.serverAddressText, "saved-cache.example.test:50051")
        XCTAssertEqual(
            defaults.string(forKey: CacheLibraryViewModel.serverAddressDefaultsKey),
            "saved-cache.example.test:50051"
        )
        XCTAssertEqual(model.statusMessage, "Cache server not connected.")
    }

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
        let bilibiliLoginModel = BilibiliLoginViewModel()
        let view = ContentView(
            model: playerModel,
            cacheModel: cacheModel,
            discoveryModel: discoveryModel,
            bilibiliModel: bilibiliModel,
            diagnosticsModel: diagnosticsModel,
            bilibiliLoginModel: bilibiliLoginModel
        )

        _ = view.body

        XCTAssertEqual(playerModel.streamURLText, "")
        XCTAssertEqual(cacheModel.serverAddressText, "")
        XCTAssertEqual(bilibiliModel.sourceText, "")
        XCTAssertEqual(diagnosticsModel.statusMessage, "Diagnostics not loaded.")
        XCTAssertEqual(bilibiliLoginModel.status, .disconnected)
    }

    func testNativePlayerViewKeepsPlayerIdentityAcrossHostedUpdates() throws {
        let firstPlayer = AVPlayer()
        let replacementPlayer = AVPlayer()
        let hostingView = NSHostingView(rootView: MacOSNativePlayerView(player: firstPlayer))
        hostingView.frame = NSRect(x: 0, y: 0, width: 640, height: 360)
        let window = NSWindow(
            contentRect: hostingView.frame,
            styleMask: .borderless,
            backing: .buffered,
            defer: false
        )
        window.isReleasedWhenClosed = false
        window.contentView = hostingView
        defer { window.close() }

        hostingView.layoutSubtreeIfNeeded()
        hostingView.displayIfNeeded()

        let playerView = try XCTUnwrap(descendant(of: AVPlayerView.self, in: hostingView))
        XCTAssertTrue(playerView.player === firstPlayer)

        hostingView.rootView = MacOSNativePlayerView(player: replacementPlayer)
        hostingView.layoutSubtreeIfNeeded()
        XCTAssertTrue(playerView.player === replacementPlayer)

        hostingView.rootView = MacOSNativePlayerView(player: nil)
        hostingView.layoutSubtreeIfNeeded()
        XCTAssertNil(playerView.player)
    }

    func testNativeControlScrollViewKeepsScrollableGeometryAcrossHostedUpdates() throws {
        let hostedView = NSHostingView(
            rootView: nativeControlScrollTestView(rowCount: 20)
        )
        hostedView.frame = NSRect(x: 0, y: 0, width: 360, height: 240)
        let window = NSWindow(
            contentRect: hostedView.frame,
            styleMask: .borderless,
            backing: .buffered,
            defer: false
        )
        window.isReleasedWhenClosed = false
        window.contentView = hostedView
        defer { window.close() }

        hostedView.layoutSubtreeIfNeeded()

        let scrollView = try XCTUnwrap(descendant(of: NSScrollView.self, in: hostedView))
        let documentView = try XCTUnwrap(scrollView.documentView)
        let initialDocumentHeight = documentView.frame.height
        XCTAssertGreaterThan(scrollView.contentView.bounds.height, 0)
        XCTAssertGreaterThan(scrollView.contentView.bounds.width, 0)
        XCTAssertGreaterThan(initialDocumentHeight, scrollView.contentView.bounds.height)
        XCTAssertEqual(documentView.frame.width, scrollView.contentView.bounds.width, accuracy: 1)
        XCTAssertEqual(scrollView.contentView.bounds.origin.y, 0, accuracy: 1)

        scrollView.contentView.scroll(to: NSPoint(x: 0, y: 128))
        scrollView.reflectScrolledClipView(scrollView.contentView)
        let previousScrollY = scrollView.contentView.bounds.origin.y
        XCTAssertGreaterThan(previousScrollY, 0)

        window.setContentSize(NSSize(width: 480, height: 240))
        hostedView.frame = NSRect(x: 0, y: 0, width: 480, height: 240)
        hostedView.rootView = nativeControlScrollTestView(rowCount: 32, viewportWidth: 480)
        hostedView.layoutSubtreeIfNeeded()
        scrollView.layoutSubtreeIfNeeded()
        documentView.layoutSubtreeIfNeeded()

        XCTAssertGreaterThan(documentView.frame.height, initialDocumentHeight)
        XCTAssertGreaterThan(scrollView.contentView.bounds.height, 0)
        XCTAssertGreaterThan(documentView.frame.height, scrollView.contentView.bounds.height)
        XCTAssertEqual(documentView.frame.width, scrollView.contentView.bounds.width, accuracy: 1)
        XCTAssertEqual(scrollView.contentView.bounds.origin.y, previousScrollY, accuracy: 1)
    }

    private func nativeControlScrollTestView(rowCount: Int, viewportWidth: CGFloat = 360) -> some View {
        MacOSNativeControlScrollView {
            VStack(alignment: .leading, spacing: 0) {
                ForEach(0..<rowCount, id: \.self) { index in
                    Text("Control \(index)")
                        .frame(maxWidth: .infinity, minHeight: 32, alignment: .leading)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .frame(width: viewportWidth, height: 240)
    }

    private func descendant<ViewType: NSView>(of type: ViewType.Type, in view: NSView) -> ViewType? {
        if let view = view as? ViewType {
            return view
        }
        for subview in view.subviews {
            if let match = descendant(of: type, in: subview) {
                return match
            }
        }
        return nil
    }
}
