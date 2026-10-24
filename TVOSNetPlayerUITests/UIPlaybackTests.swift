import Foundation
import XCTest

@MainActor
final class UIPlaybackTests: XCTestCase {
    private enum SetupError: Error {
        case missingServerAddress(String)
    }

    private var app: XCUIApplication!
    private var serverAddress = ""
    private var didLaunchApp = false

    override func setUp() async throws {
        try await super.setUp()
        try await MainActor.run {
            continueAfterFailure = false

            let addressVariable =
                name.contains("testRemoteConnectPageSelectAndPlayRealFixture")
                ? "UI_E2E_SERVER_ADDRESS"
                : "UI_E2E_TASK_SERVER_ADDRESS"
            guard
                let address = ProcessInfo.processInfo.environment[addressVariable]?
                    .trimmingCharacters(in: .whitespacesAndNewlines),
                !address.isEmpty
            else {
                throw SetupError.missingServerAddress(addressVariable)
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
        XCTAssertTrue(
            focusDown(until: fixture, limit: 20),
            "Remote navigation did not materialize and focus Fixture 055 after paging. "
                + "Target: \(accessibilityDescription(fixture)); \(libraryFocusDescription())"
        )
        remote.press(.select)

        let timeline = identifiedElement("playback-timeline")
        let state = identifiedElement("playback-state")
        XCTAssertTrue(
            timeline.waitForExistence(timeout: 25),
            "Playback timeline is missing after selecting Fixture 055. \(accessibilityDescription(timeline))"
        )
        XCTAssertTrue(
            state.waitForExistence(timeout: 25),
            "Playback state is missing after selecting Fixture 055. \(accessibilityDescription(state))"
        )
        XCTAssertTrue(
            waitUntilTimelineState(state, equals: "playing", timeout: 30),
            "Fixture 055 did not enter playing state. \(accessibilityDescription(state)); "
                + "timeline: \(accessibilityDescription(timeline))"
        )
        XCTAssertNotNil(
            waitForTimeline(timeout: 45) { $0 >= 2 },
            "The server-backed MP4 timeline never advanced. \(accessibilityDescription(timeline)); "
                + "state: \(accessibilityDescription(state))"
        )
    }

    func testAutomaticTaskWatchRecoversWithoutResubmission() throws {
        executionTimeAllowance = 600
        try prepareTaskWatchScenario(source: "BV1QtjA6BEB8")
        XCUIRemote.shared.press(.select)

        let reconnecting = identifiedElement("bilibili.watch.status")
        XCTAssertTrue(
            waitUntil(timeout: 60) { status(reconnecting, contains: "Reconnecting task updates") },
            "The submitted task did not show reconnecting task updates within 60 seconds. "
                + accessibilityDescription(identifiedElement("bilibili-task-status"))
        )
        XCTAssertTrue(status(reconnecting, contains: "Reconnecting task updates"))
        let taskStatus = identifiedElement("bilibili-task-status")
        XCTAssertTrue(
            waitUntil(timeout: 60) {
                status(taskStatus, contains: "Automatic recovery complete")
                    && status(identifiedElement("bilibili-result-status-ui-e2e-automatic-result"), contains: "Cached")
            },
            "The original task did not reach the fixture's automatic-recovery terminal status. "
                + accessibilityDescription(taskStatus)
        )
        XCTAssertFalse(reconnecting.exists, "The terminal task must stop showing a reconnecting state.")
        XCTAssertFalse(
            identifiedElement("bilibili.watch.error").exists, "A recovered task must not retain a watch error.")
        XCTAssertFalse(identifiedElement("bilibili.watch.retry").exists, "A terminal task must not offer watch retry.")
    }

    func testManualTaskWatchReconnectPreservesPlayback() throws {
        executionTimeAllowance = 600
        try prepareTaskWatchScenario(source: "BV1uW4y1s7zN")
        XCUIRemote.shared.press(.select)

        let watchError = identifiedElement("bilibili.watch.error")
        let reconnect = identifiedElement("bilibili.watch.retry")
        let reconnecting = identifiedElement("bilibili.watch.status")
        XCTAssertTrue(
            waitUntil(timeout: 100) { watchError.exists && reconnect.exists && !reconnecting.exists },
            "Automatic task-watch retries did not stop with an error and manual reconnect control. "
                + accessibilityDescription(identifiedElement("bilibili-task-status"))
        )
        XCTAssertFalse(watchError.label.isEmpty, "The exhausted watch error must expose user-visible text.")
        XCTAssertFalse(reconnecting.exists)

        let addressField = app.textFields["cache-server-address"]
        XCTAssertTrue(
            moveFocus(until: addressField, direction: .up, limit: 40),
            "Remote focus could not recover the cache server address landmark. "
                + taskWatchFocusDescription(target: addressField)
        )
        let play = app.buttons["bilibili-result-play-ui-e2e-manual-result"]
        XCTAssertTrue(
            focusDown(until: play, limit: 80),
            "Remote focus could not reach the visible result Play control. " + taskWatchFocusDescription(target: play)
        )
        XCTAssertTrue(play.isEnabled, "The fixture's task media must be playable while updates are unavailable.")
        XCUIRemote.shared.press(.select)

        let timeline = identifiedElement("playback-timeline")
        let playbackState = identifiedElement("playback-state")
        XCTAssertTrue(timeline.waitForExistence(timeout: 20))
        XCTAssertTrue(waitUntilTimelineState(playbackState, equals: "playing", timeout: 20))
        let beforeOfflineProgress = try XCTUnwrap(waitForTimeline(timeout: 15) { $0 >= 2 })
        XCTAssertNotNil(
            waitForTimeline(timeout: 10) { $0 >= beforeOfflineProgress + 1.5 },
            "The available task media did not advance while task updates remained unavailable. "
                + accessibilityDescription(timeline)
        )
        XCTAssertTrue(watchError.exists, "The watch error should remain visible until manual reconnection.")

        let beforeReconnect = try XCTUnwrap(timelineSeconds())
        XCTAssertTrue(
            moveFocus(until: reconnect, direction: .up, limit: 20),
            "Remote focus could not return to Reconnect Updates from the result Play control. "
                + taskWatchFocusDescription(target: reconnect)
        )
        XCUIRemote.shared.press(.select)
        let taskStatus = identifiedElement("bilibili-task-status")
        XCTAssertTrue(
            waitUntil(timeout: 60) {
                status(taskStatus, contains: "Manual recovery complete")
                    && status(identifiedElement("bilibili-result-status-ui-e2e-manual-result"), contains: "Cached")
            },
            "Manual reconnect did not finish observing the original task. " + accessibilityDescription(taskStatus)
        )
        XCTAssertFalse(watchError.exists, "Successful manual reconnection must clear the stale watch warning.")
        XCTAssertFalse(identifiedElement("bilibili.watch.retry").exists)
        XCTAssertTrue(waitUntilTimelineState(playbackState, equals: "playing", timeout: 10))
        XCTAssertNotNil(
            waitForTimeline(timeout: 10) { $0 >= beforeReconnect + 1 },
            "Playback did not remain progressing across manual task-watch reconnection. "
                + accessibilityDescription(timeline)
        )
    }

    func testSilentTaskWatchReconcilesWithoutInterruptingPlayback() throws {
        executionTimeAllowance = 600
        try prepareTaskWatchScenario(source: "BV1s7ZxqR9Kp")
        XCUIRemote.shared.press(.select)

        let taskStatus = identifiedElement("bilibili-task-status")
        let reconnecting = identifiedElement("bilibili.watch.status")
        let watchError = identifiedElement("bilibili.watch.error")
        let retry = identifiedElement("bilibili.watch.retry")
        let play = app.buttons["bilibili-result-play-ui-e2e-silent-result"]
        var observedFalseWatchState = false
        let playable = waitUntil(timeout: 20) {
            if status(reconnecting, contains: "Reconnecting task updates") || watchError.exists || retry.exists {
                observedFalseWatchState = true
                return true
            }
            return play.exists && play.isEnabled
                && status(taskStatus, contains: "Silent task reconciliation fixture is ready to play.")
        }
        XCTAssertFalse(observedFalseWatchState, "A healthy silent watch exposed reconnect or retry UI.")
        XCTAssertTrue(
            playable && !observedFalseWatchState,
            "The silent task did not expose its initial playable output. " + accessibilityDescription(taskStatus)
        )
        XCTAssertFalse(status(reconnecting, contains: "Reconnecting task updates"))
        XCTAssertFalse(watchError.exists)
        XCTAssertFalse(retry.exists)

        let addressField = app.textFields["cache-server-address"]
        XCTAssertTrue(
            moveFocus(until: addressField, direction: .up, limit: 40),
            "Remote focus could not return to the server address. " + taskWatchFocusDescription(target: addressField)
        )
        XCTAssertTrue(
            focusDown(until: play, limit: 80),
            "Remote focus could not reach the silent task Play control. " + taskWatchFocusDescription(target: play)
        )
        XCTAssertTrue(play.isEnabled, "The silent task media must remain playable while its watch is open.")
        XCUIRemote.shared.press(.select)

        let timeline = identifiedElement("playback-timeline")
        let playbackState = identifiedElement("playback-state")
        XCTAssertTrue(timeline.waitForExistence(timeout: 20))
        XCTAssertTrue(waitUntilTimelineState(playbackState, equals: "playing", timeout: 20))
        let duringSilence = try XCTUnwrap(
            waitForTimeline(timeout: 15) { $0 >= 2 },
            "Silent-task playback did not start. "
                + accessibilityDescription(timeline)
        )
        XCTAssertNotNil(
            waitForTimeline(timeout: 10) { $0 >= duringSilence + 1.5 },
            "Playback did not advance while task updates were silent. " + accessibilityDescription(timeline)
        )
        let duration = try XCTUnwrap(playbackTimeline(), accessibilityDescription(timeline)).duration
        XCTAssertGreaterThanOrEqual(duration, 90, "The task media must cover the bounded reconciliation window.")

        let reconciled = waitUntil(timeout: 75) {
            if status(reconnecting, contains: "Reconnecting task updates") || watchError.exists || retry.exists {
                observedFalseWatchState = true
                return true
            }
            return status(taskStatus, contains: "Silent task reconciliation complete")
                && status(identifiedElement("bilibili-result-status-ui-e2e-silent-result"), contains: "Cached")
        }
        XCTAssertFalse(observedFalseWatchState, "A healthy silent watch exposed reconnect or retry UI.")
        XCTAssertTrue(
            reconciled && !observedFalseWatchState,
            "The original silent task did not reconcile to cached output within 75 seconds. "
                + accessibilityDescription(taskStatus)
        )
        XCTAssertFalse(status(reconnecting, contains: "Reconnecting task updates"))
        XCTAssertFalse(watchError.exists)
        XCTAssertFalse(retry.exists)
        XCTAssertTrue(waitUntilTimelineState(playbackState, equals: "playing", timeout: 10))
        let afterReconciliation = try XCTUnwrap(timelineSeconds(), accessibilityDescription(timeline))
        XCTAssertNotNil(
            waitForTimeline(timeout: 10) { $0 >= afterReconciliation + 1.5 },
            "Playback did not continue after silent task reconciliation. " + accessibilityDescription(timeline)
        )
    }

    func testRelaunchRestoresTaskAndExplicitPlaybackPosition() throws {
        executionTimeAllowance = 600
        let remote = XCUIRemote.shared
        try prepareResumptionScenario()
        remote.press(.select)

        let selectedResultID = "ui-e2e-resumption-result"
        let decoyResultID = "ui-e2e-resumption-decoy-result"
        let taskStatus = identifiedElement("bilibili-task-status")
        let selectedStatus = identifiedElement("bilibili-result-status-\(selectedResultID)")
        let decoyStatus = identifiedElement("bilibili-result-status-\(decoyResultID)")
        let selectedPlay = app.buttons["bilibili-result-play-\(selectedResultID)"]
        let decoyPlay = app.buttons["bilibili-result-play-\(decoyResultID)"]
        XCTAssertTrue(
            waitUntil(timeout: 75) {
                status(taskStatus, contains: "Relaunch resumption fixture complete")
                    && status(selectedStatus, contains: "Cached")
                    && status(decoyStatus, contains: "Cached")
                    && selectedPlay.exists && selectedPlay.isEnabled
                    && decoyPlay.exists && decoyPlay.isEnabled
            },
            "The fixture did not restore both the decoy and exact selected task results. "
                + accessibilityDescription(taskStatus)
        )

        XCTAssertTrue(
            focusDown(until: selectedPlay, limit: 100),
            "Remote focus could not reach the exact second result. " + taskWatchFocusDescription(target: selectedPlay)
        )
        remote.press(.select)
        let timeline = identifiedElement("playback-timeline")
        let playbackState = identifiedElement("playback-state")
        XCTAssertTrue(timeline.waitForExistence(timeout: 20))
        XCTAssertTrue(waitUntilTimelineState(playbackState, equals: "playing", timeout: 20))
        XCTAssertNotNil(
            waitForTimeline(timeout: 20) { $0 >= 5 },
            "The exact selected task result did not reach a nonzero checkpoint. "
                + accessibilityDescription(timeline)
        )

        let pause = app.buttons["playback-toggle"]
        XCTAssertTrue(
            focusDown(until: pause, limit: 100),
            "Remote focus could not reach Pause. " + taskWatchFocusDescription(target: pause)
        )
        remote.press(.select)
        let savedCheckpoint = try pausedCheckpointBaseline(state: playbackState, timeline: timeline)
        XCTAssertGreaterThan(savedCheckpoint, 0, "The inactive checkpoint must be nonzero.")

        XCUIDevice.shared.press(.home)
        XCTAssertTrue(
            waitUntil(timeout: 20) { app.state == .runningBackground || app.state == .notRunning },
            "The player did not leave the foreground before termination. State: \(app.state)"
        )
        app.terminate()
        app.launchArguments = ["--ui-testing", "--ui-testing-preserve-preferences"]
        app.launch()

        let emptyPlayer = identifiedElement("player-empty-state")
        XCTAssertTrue(emptyPlayer.waitForExistence(timeout: 25))
        XCTAssertNotEqual(
            playbackState.value as? String,
            "playing",
            "Playback started automatically before explicit Continue."
        )
        XCTAssertTrue(
            waitUntil(timeout: 75) {
                status(taskStatus, contains: "Relaunch resumption fixture complete")
                    && status(selectedStatus, contains: "Cached")
                    && selectedPlay.exists && selectedPlay.isEnabled
            },
            "The original task/result identity was not restored after relaunch. "
                + accessibilityDescription(taskStatus)
        )
        RunLoop.current.run(until: Date().addingTimeInterval(2.2))
        XCTAssertTrue(emptyPlayer.exists, "Relaunch loaded a player before explicit Continue.")
        XCTAssertNotEqual(playbackState.value as? String, "playing")

        let addressField = app.textFields["cache-server-address"]
        XCTAssertTrue(
            moveFocus(until: addressField, direction: .up, limit: 50),
            "Remote focus could not return to the server address. " + taskWatchFocusDescription(target: addressField)
        )
        let continueButton = app.buttons["bilibili-resumption-continue"]
        XCTAssertTrue(
            focusDown(until: continueButton, limit: 100),
            "Remote focus could not reach Continue. " + taskWatchFocusDescription(target: continueButton)
        )
        XCTAssertTrue(continueButton.isEnabled)
        remote.press(.select)

        XCTAssertTrue(
            waitUntil(timeout: 30) {
                playbackState.value as? String == "playing"
                    && timelineSeconds().map { abs($0 - savedCheckpoint) <= 2 } == true
            },
            "Explicit Continue did not restore the saved position near \(savedCheckpoint). "
                + accessibilityDescription(timeline)
        )
        let restoredAt = try XCTUnwrap(timelineSeconds(), accessibilityDescription(timeline))
        XCTAssertNotNil(
            waitForTimeline(timeout: 15) { $0 >= restoredAt + 1.5 },
            "Playback did not advance after explicit Continue. \(accessibilityDescription(timeline))"
        )
    }

    private func prepareTaskWatchScenario(source: String) throws {
        let remote = XCUIRemote.shared
        let addressField = app.textFields["cache-server-address"]
        XCTAssertTrue(addressField.waitForExistence(timeout: 20))
        XCTAssertTrue(waitUntil(timeout: 10) { addressField.hasFocus })
        XCTAssertTrue(
            serverAddressIsEmpty(addressField), "Unexpected initial address: \(accessibilityDescription(addressField))")
        remote.press(.select)
        addressField.typeText(serverAddress)
        if app.keyboards.firstMatch.exists {
            remote.press(.menu)
        }
        XCTAssertTrue(waitUntil(timeout: 10) { !app.keyboards.firstMatch.exists })

        let sourceField = app.textFields["bilibili-source"]
        XCTAssertTrue(sourceField.waitForExistence(timeout: 15))
        XCTAssertTrue(
            focusDown(until: sourceField, limit: 80),
            "Remote focus could not reach the Bilibili source field. \(accessibilityDescription(sourceField))"
        )
        remote.press(.select)
        sourceField.typeText(source)
        if app.keyboards.firstMatch.exists {
            remote.press(.menu)
        }
        XCTAssertTrue(waitUntil(timeout: 10) { !app.keyboards.firstMatch.exists })

        let submit = app.buttons["bilibili-submit"]
        XCTAssertTrue(
            focusDown(until: submit, limit: 80),
            "Remote focus could not reach the visible Bilibili submit control. \(accessibilityDescription(submit))"
        )
        XCTAssertTrue(submit.isEnabled, "The single-result Bilibili input should be ready for one submission.")
    }

    private func prepareResumptionScenario() throws {
        let remote = XCUIRemote.shared
        let addressField = app.textFields["cache-server-address"]
        XCTAssertTrue(addressField.waitForExistence(timeout: 20))
        XCTAssertTrue(waitUntil(timeout: 10) { addressField.hasFocus })
        XCTAssertTrue(serverAddressIsEmpty(addressField))
        remote.press(.select)
        addressField.typeText(serverAddress)
        if app.keyboards.firstMatch.exists {
            remote.press(.menu)
        }
        XCTAssertTrue(waitUntil(timeout: 10) { !app.keyboards.firstMatch.exists })

        let refresh = app.buttons["cache-server-refresh"]
        XCTAssertTrue(
            focusDown(until: refresh, limit: 3),
            "Remote focus could not reach Connect. " + connectionFocusDescription()
        )
        remote.press(.select)
        let libraryStatus = app.staticTexts["cache-library-status"]
        XCTAssertTrue(
            waitUntil(timeout: 45) { status(libraryStatus, contains: "Loaded 0 cached item(s)") },
            "The production Connect flow did not persist the fixture endpoint. "
                + accessibilityDescription(libraryStatus)
        )

        let sourceField = app.textFields["bilibili-source"]
        XCTAssertTrue(
            focusDown(until: sourceField, limit: 80),
            "Remote focus could not reach the Bilibili source field. " + accessibilityDescription(sourceField)
        )
        remote.press(.select)
        sourceField.typeText("BV1xK4y1P7mN")
        if app.keyboards.firstMatch.exists {
            remote.press(.menu)
        }
        XCTAssertTrue(waitUntil(timeout: 10) { !app.keyboards.firstMatch.exists })

        let submit = app.buttons["bilibili-submit"]
        XCTAssertTrue(
            focusDown(until: submit, limit: 80),
            "Remote focus could not reach the task submit control. " + accessibilityDescription(submit)
        )
        XCTAssertTrue(submit.isEnabled)
    }

    private func pausedCheckpointBaseline(state: XCUIElement, timeline: XCUIElement) throws -> Double {
        XCTAssertTrue(
            waitUntilTimelineState(state, equals: "paused", timeout: 10),
            "Pause did not settle before checkpointing. \(accessibilityDescription(state)); "
                + "timeline: \(accessibilityDescription(timeline))"
        )
        let baseline = try XCTUnwrap(timelineSeconds(), accessibilityDescription(timeline))
        RunLoop.current.run(until: Date().addingTimeInterval(2.2))
        let current = try XCTUnwrap(timelineSeconds(), accessibilityDescription(timeline))
        XCTAssertEqual(state.value as? String, "paused")
        XCTAssertLessThanOrEqual(
            abs(current - baseline),
            1,
            "Playback time moved while paused before the relaunch checkpoint. Baseline: \(baseline); current: \(current)"
        )
        return current
    }

    private func taskWatchFocusDescription(target: XCUIElement) -> String {
        let submit = app.buttons["bilibili-submit"]
        let reconnect = identifiedElement("bilibili.watch.retry")
        let focusedControl = app.descendants(matching: .any)
            .matching(NSPredicate(format: "hasFocus == true"))
            .firstMatch
        let focusedDescription =
            focusedControl.exists
            ? "id=\(focusedControl.identifier.debugDescription), label=\(focusedControl.label.debugDescription)"
            : "none"
        let targetExists = target.exists
        return "targetExists=\(targetExists), targetEnabled=\(targetExists && target.isEnabled), "
            + "targetHittable=\(targetExists && target.isHittable), targetFocused=\(targetExists && target.hasFocus), "
            + "submitFocused=\(submit.exists && submit.hasFocus), "
            + "reconnectFocused=\(reconnect.exists && reconnect.hasFocus), focusedControl={\(focusedDescription)}; "
            + "target: \(accessibilityDescription(target))"
    }

    private func identifiedElement(_ identifier: String) -> XCUIElement {
        app.descendants(matching: .any).matching(identifier: identifier).element
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

    private func libraryFocusDescription() -> String {
        let items = app.buttons.matching(identifier: "cache-library-item").allElementsBoundByIndex
        let focused = items.first(where: { $0.hasFocus }).map { accessibilityDescription($0) } ?? "none"
        return "focusedLibraryItem: \(focused); materializedLibraryItems: \(items.count)"
    }

    private func focusDown(until element: XCUIElement, limit: Int) -> Bool {
        moveFocus(until: element, direction: .down, limit: limit)
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
        let value = identifiedElement("playback-timeline").value
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
