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
        let libraryStatus = app.staticTexts["cache-library-status"]
        let refresh = app.buttons["cache-server-refresh"]
        let libraryItems = app.buttons.matching(identifier: "cache-library-item")
        XCTAssertTrue(
            waitUntil(timeout: 10) { status(libraryStatus, contains: "Cache server not connected.") },
            "Expected an initially disconnected cache server. Status: \(accessibilityDescription(libraryStatus)); "
                + "address: \(accessibilityDescription(addressField)); cached items: \(libraryItems.count)"
        )
        XCTAssertTrue(
            serverAddressIsEmpty(addressField), "Unexpected initial address: \(accessibilityDescription(addressField))"
        )
        XCTAssertTrue(refresh.waitForExistence(timeout: 10))
        XCTAssertFalse(refresh.isEnabled, "Refresh should be disabled until an address is entered.")
        XCTAssertEqual(libraryItems.count, 0, "An isolated launch should not load any cache library items.")

        remote.press(.select)
        addressField.typeText(serverAddress)
        addressField.typeText("\n")

        let keyboard = app.keyboards.firstMatch
        if keyboard.exists {
            remote.press(.menu)
        }
        XCTAssertTrue(
            waitUntil(timeout: 10) { !keyboard.exists },
            "The address keyboard did not dismiss after submission."
        )
        XCTAssertTrue(
            waitUntil(timeout: 60) { refresh.exists && refresh.isEnabled },
            "Refresh did not become enabled after address submission. Status: \(accessibilityDescription(libraryStatus))"
        )
        XCTAssertTrue(
            waitUntil(timeout: 10) { addressField.hasFocus || refresh.hasFocus },
            "Submission did not return focus to the address controls. \(connectionFocusDescription())"
        )
        XCTAssertTrue(
            focusDown(until: refresh, limit: 3),
            "The refresh control below the address field did not receive remote focus. \(connectionFocusDescription())"
        )
        remote.press(.select)

        XCTAssertTrue(
            waitUntil(timeout: 60) { status(libraryStatus, contains: "Loaded 50 cached item(s)") },
            "The app did not load the first real catalog page. Status: \(accessibilityDescription(libraryStatus))"
        )

        let loadMore = app.buttons["cache-library-load-more"]
        XCTAssertTrue(
            focusDown(until: loadMore, limit: 80),
            "Remote focus could not reach the next-page control in the real library."
        )
        remote.press(.select)
        XCTAssertTrue(
            waitUntil(timeout: 30) { status(libraryStatus, contains: "Loaded 55 cached item(s)") },
            "The app did not load the second catalog page. Status: \(accessibilityDescription(libraryStatus))"
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

    private func status(_ element: XCUIElement, contains expectedText: String) -> Bool {
        guard element.exists else { return false }
        return element.label.contains(expectedText) || (element.value as? String)?.contains(expectedText) == true
    }

    private func serverAddressIsEmpty(_ element: XCUIElement) -> Bool {
        guard let value = element.value as? String else { return false }
        return value.isEmpty || value == element.placeholderValue
    }

    private func accessibilityDescription(_ element: XCUIElement) -> String {
        guard element.exists else { return "exists=false" }
        return "label=\(element.label.debugDescription), value=\(String(describing: element.value))"
    }

    private func connectionFocusDescription() -> String {
        let address = app.textFields["cache-server-address"]
        let refresh = app.buttons["cache-server-refresh"]
        return "addressFocused=\(address.hasFocus), refreshFocused=\(refresh.hasFocus), "
            + "refreshEnabled=\(refresh.isEnabled), keyboardPresent=\(app.keyboards.firstMatch.exists), "
            + "status: \(accessibilityDescription(app.staticTexts["cache-library-status"]))"
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
