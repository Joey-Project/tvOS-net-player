import XCTest
@testable import TVOSNetPlayer

final class NativeNavigationProbeTests: XCTestCase {
    func testNoDiagnosticArgumentsSelectProduction() {
        XCTAssertEqual(
            NativeNavigationProbeLaunch.parse(arguments: ["TVOSNetPlayer"], isSimulator: true),
            .production
        )
    }

    func testValidLoopbackOptionsSelectSimulatorDiagnostic() {
        let launch = NativeNavigationProbeLaunch.parse(
            arguments: [
                "TVOSNetPlayer",
                "--native-navigation-probe",
                "--native-navigation-url",
                "http://127.0.0.1:8123/master.m3u8",
                "--native-navigation-run-id",
                "sim-run_01",
            ],
            isSimulator: true
        )

        guard case let .diagnostic(configuration) = launch else {
            return XCTFail("Expected a simulator diagnostic configuration")
        }
        XCTAssertEqual(configuration.url.absoluteString, "http://127.0.0.1:8123/master.m3u8")
        XCTAssertEqual(configuration.runID, "sim-run_01")
    }

    func testInvalidDiagnosticArgumentsFailClosed() {
        let cases: [[String]] = [
            ["TVOSNetPlayer", "--native-navigation-probe"],
            ["TVOSNetPlayer", "--native-navigation-url", "http://127.0.0.1:8123/master.m3u8"],
            [
                "TVOSNetPlayer", "--native-navigation-probe", "--native-navigation-url",
                "http://example.com/master.m3u8", "--native-navigation-run-id", "run1",
            ],
            [
                "TVOSNetPlayer", "--native-navigation-probe", "--native-navigation-url",
                "http://user:pass@127.0.0.1:8123/master.m3u8", "--native-navigation-run-id", "run1",
            ],
            [
                "TVOSNetPlayer", "--native-navigation-probe", "--native-navigation-url",
                "http://127.0.0.1:8123/master.m3u8?token=x", "--native-navigation-run-id", "run1",
            ],
            [
                "TVOSNetPlayer", "--native-navigation-probe", "--native-navigation-url",
                "http://127.0.0.1:8123/master.m3u8", "--native-navigation-run-id", "bad id",
            ],
            [
                "TVOSNetPlayer", "--native-navigation-probe", "--native-navigation-probe", "--native-navigation-url",
                "http://127.0.0.1:8123/master.m3u8", "--native-navigation-run-id", "run1",
            ],
            [
                "TVOSNetPlayer", "--native-navigation-probe", "--native-navigation-url",
                "http://127.0.0.1:8123/master.m3u8", "--native-navigation-run-id", "run1", "--unexpected",
            ],
            ["TVOSNetPlayer", "--native-navigation-unknown"],
            ["TVOSNetPlayer", "--native-navigation-probe=typo"],
            [
                "TVOSNetPlayer", "--native-navigation-probe", "--native-navigation-url",
                "http://127.0.0.1:8123/master.m3u8", "--native-navigation-run-id", "run1",
                "--native-navigation-unknown",
            ],
        ]

        for arguments in cases {
            guard case .invalid = NativeNavigationProbeLaunch.parse(arguments: arguments, isSimulator: true) else {
                XCTFail("Expected invalid launch for arguments: \(arguments)")
                continue
            }
        }
    }

    func testValidOptionsAreRejectedOutsideSimulator() {
        let launch = NativeNavigationProbeLaunch.parse(
            arguments: [
                "TVOSNetPlayer",
                "--native-navigation-probe",
                "--native-navigation-url",
                "http://127.0.0.1:8123/master.m3u8",
                "--native-navigation-run-id",
                "device-check",
            ],
            isSimulator: false
        )

        guard case let .invalid(message) = launch else {
            return XCTFail("Expected device launch to fail closed")
        }
        XCTAssertTrue(message.contains("Simulator"))
    }

    @MainActor
    func testDiagnosticBoundsMatchSharedContract() {
        XCTAssertEqual(NativeNavigationProbe.maximumEvents, 4_096)
        XCTAssertEqual(NativeNavigationProbe.maximumTraceBytes, 8 * 1_024 * 1_024)
        XCTAssertEqual(NativeNavigationProbe.maximumLifetimeSeconds, 600)
    }

    @MainActor
    func testTraceFilenameComponentKeepsValid64CharacterRunIDShortAndPlain() {
        let runID = String(repeating: "a", count: 64)
        let component = NativeNavigationProbe.safeTraceFilenameComponent(runID: runID)

        XCTAssertEqual(component, runID)
        XCTAssertEqual(component.utf8.count, 64)
        XCTAssertTrue(component.utf8.allSatisfy { (0x61...0x7A).contains($0) })
    }

    @MainActor
    func testTraceFilenameComponentSanitizesUnsafeCharacters() {
        XCTAssertEqual(
            NativeNavigationProbe.safeTraceFilenameComponent(runID: "../bad/name?x"),
            "___bad_name_x"
        )
    }

    func testClockNanosecondsConversionRejectsInvalidAndOverflowValues() {
        XCTAssertEqual(NativeNavigationClock.nanoseconds(seconds: 2, subsecondNanoseconds: 3), 2_000_000_003)
        XCTAssertNil(NativeNavigationClock.nanoseconds(seconds: 0, subsecondNanoseconds: 1))
        XCTAssertNil(NativeNavigationClock.nanoseconds(seconds: -1, subsecondNanoseconds: 1))
        XCTAssertNil(NativeNavigationClock.nanoseconds(seconds: 1, subsecondNanoseconds: -1))
        XCTAssertNil(NativeNavigationClock.nanoseconds(seconds: 1, subsecondNanoseconds: 1_000_000_000))
        XCTAssertNil(NativeNavigationClock.nanoseconds(seconds: Int64.max, subsecondNanoseconds: 0))
    }
}
