import Foundation
import XCTest

@MainActor
final class UIPlaybackTests: XCTestCase {
    private enum SetupError: Error {
        case missingServerAddress
    }

    private var app: XCUIApplication!
    private var serverAddress = ""
    private var didLaunchApp = false

    override func setUp() async throws {
        try await super.setUp()
        try await MainActor.run {
            continueAfterFailure = false

            guard
                let address = ProcessInfo.processInfo.environment["UI_E2E_SERVER_ADDRESS"]?
                    .trimmingCharacters(in: .whitespacesAndNewlines),
                !address.isEmpty
            else {
                throw SetupError.missingServerAddress
            }

            serverAddress = address
            app = XCUIApplication()
            app.launchArguments = ["--ui-testing"]
            app.launch()
            didLaunchApp = true
        }
    }

    override func tearDown() async throws {
        await MainActor.run {
            if didLaunchApp {
                if testRun?.failureCount ?? 0 > 0 {
                    let screenshot = XCTAttachment(screenshot: app.screenshot())
                    screenshot.name = "Failure screenshot"
                    screenshot.lifetime = .keepAlways
                    add(screenshot)

                    let accessibilityTree = XCTAttachment(string: app.debugDescription)
                    accessibilityTree.name = "Failure accessibility tree"
                    accessibilityTree.lifetime = .keepAlways
                    add(accessibilityTree)
                }

                app.terminate()
                didLaunchApp = false
            }
        }
        try await super.tearDown()
    }

    func testRemoteConnectPageSelectAndPlayRealFixture() throws {
        let remote = XCUIRemote.shared
        let addressField = app.textFields["cache-server-address"]
        XCTAssertTrue(addressField.waitForExistence(timeout: 20))
        XCTAssertTrue(waitUntil(timeout: 10) { addressField.hasFocus })
        XCTAssertTrue(app.staticTexts["cache-library-status"].label.contains("Cache server not connected"))

        remote.press(.select)
        addressField.typeText(serverAddress)
        addressField.typeText("\n")

        let refresh = app.buttons["cache-server-refresh"]
        XCTAssertTrue(focusDown(until: refresh, limit: 12), "The refresh control did not receive remote focus.")
        remote.press(.select)

        let libraryStatus = app.staticTexts["cache-library-status"]
        XCTAssertTrue(
            waitUntil(timeout: 60) { libraryStatus.label.contains("Loaded 50 cached item(s)") },
            "The app did not load the first real catalog page. Status: \(libraryStatus.label)"
        )

        let loadMore = app.buttons["cache-library-load-more"]
        XCTAssertTrue(
            focusDown(until: loadMore, limit: 80),
            "Remote focus could not reach the next-page control in the real library."
        )
        remote.press(.select)
        XCTAssertTrue(
            waitUntil(timeout: 30) { libraryStatus.label.contains("Loaded 55 cached item(s)") },
            "The app did not load the second catalog page. Status: \(libraryStatus.label)"
        )

        let fixture = libraryItem(named: "Fixture 055")
        XCTAssertTrue(fixture.waitForExistence(timeout: 20))
        XCTAssertTrue(focusUp(until: fixture, limit: 8), "Fixture 055 did not receive remote focus after paging.")
        remote.press(.select)

        let timeline = app.staticTexts["playback-timeline"]
        let state = app.staticTexts["playback-state"]
        XCTAssertTrue(timeline.waitForExistence(timeout: 25))
        XCTAssertTrue(state.waitForExistence(timeout: 25))
        XCTAssertTrue(waitUntilTimelineState(state, equals: "playing", timeout: 30))
        XCTAssertNotNil(waitForTimeline(timeout: 45) { $0 >= 2 }, "The server-backed MP4 timeline never advanced.")
    }

    private func libraryItem(named title: String) -> XCUIElement {
        app.buttons
            .matching(identifier: "cache-library-item")
            .matching(NSPredicate(format: "label CONTAINS[c] %@", title))
            .firstMatch
    }

    private func focusDown(until element: XCUIElement, limit: Int) -> Bool {
        moveFocus(until: element, direction: .down, limit: limit)
    }

    private func focusUp(until element: XCUIElement, limit: Int) -> Bool {
        moveFocus(until: element, direction: .up, limit: limit)
    }

    private func moveFocus(
        until element: XCUIElement,
        direction: XCUIRemote.Button,
        limit: Int
    ) -> Bool {
        for _ in 0..<limit {
            if element.exists && element.hasFocus {
                return true
            }
            XCUIRemote.shared.press(direction)
            RunLoop.current.run(until: Date().addingTimeInterval(0.15))
        }
        return element.exists && element.hasFocus
    }

    private func playbackTimeline() -> (current: Double, duration: Double)? {
        let value = app.staticTexts["playback-timeline"].value
        guard
            let string = value as? String,
            let separator = string.firstIndex(of: "/"),
            let current = Double(string[..<separator]),
            let duration = Double(string[string.index(after: separator)...])
        else { return nil }
        return (current, duration)
    }

    private func timelineSeconds() -> Double? {
        playbackTimeline()?.current
    }

    private func waitForTimeline(timeout: TimeInterval, matching predicate: (Double) -> Bool) -> Double? {
        let deadline = Date().addingTimeInterval(timeout)
        repeat {
            if let seconds = timelineSeconds(), predicate(seconds) {
                return seconds
            }
            RunLoop.current.run(until: Date().addingTimeInterval(0.2))
        } while Date() < deadline
        return nil
    }

    private func waitUntilTimelineState(
        _ element: XCUIElement,
        equals expectedValue: String,
        timeout: TimeInterval
    ) -> Bool {
        waitUntil(timeout: timeout) { element.value as? String == expectedValue }
    }

    private func waitUntil(timeout: TimeInterval, condition: () -> Bool) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        repeat {
            if condition() {
                return true
            }
            RunLoop.current.run(until: Date().addingTimeInterval(0.2))
        } while Date() < deadline
        return condition()
    }

}
