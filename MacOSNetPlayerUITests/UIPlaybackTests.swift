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

    func testConnectBrowseAndControlRealPlayback() throws {
        let addressField = app.textFields["cache-server-address"]
        XCTAssertTrue(addressField.waitForExistence(timeout: 15))
        let initialStatus = app.staticTexts["app-status"]
        let refresh = app.buttons["cache-server-refresh"]
        let libraryItems = app.descendants(matching: .any).matching(identifier: "cache-library-item")
        XCTAssertTrue(
            waitUntil(timeout: 10) { status(initialStatus, contains: "Cache server not connected.") },
            "Expected an initially disconnected cache server. Status: \(accessibilityDescription(initialStatus)); "
                + "address: \(accessibilityDescription(addressField)); cached items: \(libraryItems.count)"
        )
        XCTAssertTrue(
            serverAddressIsEmpty(addressField), "Unexpected initial address: \(accessibilityDescription(addressField))"
        )
        XCTAssertTrue(refresh.waitForExistence(timeout: 10))
        XCTAssertFalse(refresh.isEnabled, "Refresh should be disabled until an address is entered.")
        XCTAssertEqual(libraryItems.count, 0, "An isolated launch should not load any cache library items.")

        addressField.click()
        addressField.typeText(serverAddress)
        refresh.click()

        let libraryStatus = app.staticTexts["cache-library-status"]
        XCTAssertTrue(
            waitUntil(timeout: 60) { status(libraryStatus, contains: "Loaded 50 cached item(s)") },
            "The app did not load the first real catalog page. Status: \(accessibilityDescription(libraryStatus))"
        )

        let loadMore = app.buttons["cache-library-load-more"]
        XCTAssertTrue(loadMore.waitForExistence(timeout: 10), "The 55-item catalog should have a second page.")
        loadMore.click()
        XCTAssertTrue(
            waitUntil(timeout: 30) { status(libraryStatus, contains: "Loaded 55 cached item(s)") },
            "The app did not load the second catalog page. Status: \(accessibilityDescription(libraryStatus))"
        )

        let fixture = libraryItem(named: "Fixture 055")
        XCTAssertTrue(fixture.waitForExistence(timeout: 15))
        let libraryList = app.descendants(matching: .any)
            .matching(identifier: "cache-library-list")
            .firstMatch
        XCTAssertTrue(libraryList.waitForExistence(timeout: 10))
        for _ in 0..<12 where !fixture.isHittable {
            libraryList.swipeUp()
        }
        XCTAssertTrue(fixture.isHittable, "Fixture 055 should be reachable by scrolling the real library list.")
        fixture.click()

        let playCached = app.buttons["cache-library-play-cached"]
        XCTAssertTrue(waitUntil(timeout: 10) { playCached.exists && playCached.isEnabled })
        playCached.click()

        let timeline = app.staticTexts["playback-timeline"]
        let state = app.staticTexts["playback-state"]
        XCTAssertTrue(timeline.waitForExistence(timeout: 20))
        XCTAssertTrue(state.waitForExistence(timeout: 20))
        XCTAssertNotNil(waitForTimeline(timeout: 45) { $0 >= 2 }, "The server-backed MP4 timeline never advanced.")
        XCTAssertTrue(waitUntilTimelineState(state, equals: "playing", timeout: 15))

        let toggle = app.buttons["playback-toggle"]
        toggle.click()
        XCTAssertTrue(waitUntilTimelineState(state, equals: "paused", timeout: 10))
        let pausedAt = try XCTUnwrap(timelineSeconds())
        RunLoop.current.run(until: Date().addingTimeInterval(2.2))
        let stillPausedAt = try XCTUnwrap(timelineSeconds())
        XCTAssertLessThanOrEqual(abs(stillPausedAt - pausedAt), 1, "Playback time moved while paused.")

        toggle.click()
        XCTAssertTrue(waitUntilTimelineState(state, equals: "playing", timeout: 10))
        XCTAssertNotNil(waitForTimeline(timeout: 15) { $0 >= pausedAt + 2 })

        let beforeForward = try XCTUnwrap(timelineSeconds())
        app.buttons["playback-skip-forward"].click()
        let afterForward = try XCTUnwrap(
            waitForTimeline(timeout: 10) { $0 >= beforeForward + 8 && $0 <= beforeForward + 16 },
            "The forward control did not seek approximately ten seconds."
        )

        app.buttons["playback-skip-backward"].click()
        XCTAssertNotNil(
            waitForTimeline(timeout: 10) { $0 >= afterForward - 13 && $0 <= afterForward - 7 },
            "The backward control did not seek approximately ten seconds."
        )

        let twoX = app.buttons["2x"]
        XCTAssertTrue(twoX.waitForExistence(timeout: 10))
        twoX.click()
        let speedPicker = app.segmentedControls["playback-speed"]
        XCTAssertTrue(
            waitUntil(timeout: 10) {
                (speedPicker.value as? String) == "2x" || twoX.isSelected
            },
            "The actual speed control did not expose 2x as selected."
        )

        let measurementStart = ProcessInfo.processInfo.systemUptime
        let beforeFastPlayback = try XCTUnwrap(playbackTimeline()?.current)
        while ProcessInfo.processInfo.systemUptime - measurementStart < 5.5 {
            RunLoop.current.run(until: Date().addingTimeInterval(0.15))
        }
        let afterFastPlayback = try XCTUnwrap(playbackTimeline()?.current)
        let measuredDuration = ProcessInfo.processInfo.systemUptime - measurementStart
        let measuredRate = (afterFastPlayback - beforeFastPlayback) / measuredDuration
        XCTAssertGreaterThanOrEqual(measuredRate, 1.4, "2x playback advanced at only \(measuredRate)x.")
        XCTAssertLessThanOrEqual(
            measuredRate, 2.8, "Measured playback rate \(measuredRate)x is outside the 2x tolerance.")

        let naturalPlaybackStart = ProcessInfo.processInfo.systemUptime
        playCached.click()
        let replayStart = try XCTUnwrap(waitForTimeline(timeout: 15) { $0 < 4 })
        XCTAssertLessThanOrEqual(replayStart, 4, "Replay should restart near the beginning of Fixture 055.")
        XCTAssertTrue(waitUntilTimelineState(state, equals: "playing", timeout: 15))

        let naturalEnd = waitForNaturalCompletion(timeout: 90)
        let naturalPlaybackDuration = ProcessInfo.processInfo.systemUptime - naturalPlaybackStart
        XCTAssertNotNil(naturalEnd, "Fixture 055 did not naturally finish within 90 seconds.")
        XCTAssertLessThanOrEqual(naturalPlaybackDuration, 90)
        let terminalTimeline = try XCTUnwrap(naturalEnd)
        XCTAssertGreaterThanOrEqual(terminalTimeline.duration, 90)
        XCTAssertGreaterThanOrEqual(
            terminalTimeline.current,
            terminalTimeline.duration - 1,
            "The finished state must be paired with a terminal timeline."
        )
        XCTAssertEqual(state.value as? String, "finished")

        let completionScreenshot = XCTAttachment(screenshot: app.screenshot())
        completionScreenshot.name = "Natural playback completion"
        completionScreenshot.lifetime = .keepAlways
        add(completionScreenshot)
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

    private func libraryItem(named title: String) -> XCUIElement {
        app.descendants(matching: .any)
            .matching(identifier: "cache-library-item")
            .matching(NSPredicate(format: "label CONTAINS[c] %@", title))
            .firstMatch
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

    private func waitForNaturalCompletion(timeout: TimeInterval) -> (current: Double, duration: Double)? {
        let deadline = ProcessInfo.processInfo.systemUptime + timeout
        repeat {
            if app.staticTexts["playback-state"].value as? String == "finished",
                let timeline = playbackTimeline(), timeline.duration >= 90,
                timeline.current >= timeline.duration - 1
            {
                return timeline
            }
            RunLoop.current.run(until: Date().addingTimeInterval(0.2))
        } while ProcessInfo.processInfo.systemUptime < deadline
        return nil
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
