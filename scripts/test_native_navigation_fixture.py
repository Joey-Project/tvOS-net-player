from __future__ import annotations

import importlib.util
import io
import json
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
from contextlib import ExitStack
from pathlib import Path
from unittest import mock


SCRIPT = Path(__file__).with_name("native-navigation-fixture.py")
SPEC = importlib.util.spec_from_file_location("native_navigation_fixture", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
fixture = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(fixture)


class FakeClock:
    def __init__(self, now: float = 0.0):
        self.now = now

    def __call__(self) -> float:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += seconds


class RecordingStopEvent:
    def __init__(self, *, cancel: bool = False):
        self.cancel = cancel
        self.waits: list[float] = []

    def is_set(self) -> bool:
        return self.cancel

    def wait(self, timeout: float | None = None) -> bool:
        self.waits.append(timeout or 0.0)
        return self.cancel


class CancelDuringWait(RecordingStopEvent):
    def wait(self, timeout: float | None = None) -> bool:
        self.waits.append(timeout or 0.0)
        self.cancel = True
        return True


class FixtureTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.media = self.root / "media"
        self.media.mkdir()
        (self.media / "master.m3u8").write_text(
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:2\n"
            "#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:2.0,\nsegment0000.m4s\n#EXT-X-ENDLIST\n",
            encoding="ascii",
        )
        (self.media / "init.mp4").write_bytes(b"init-data")
        (self.media / "segment0000.m4s").write_bytes(bytes(range(64)))
        self.stop_event = threading.Event()
        self.events_path = self.root / "events.jsonl"
        self.log = fixture.EventLog(self.events_path, "unit-run", self.stop_event)
        self.limiter = fixture.TransferLimiter(1024, None)
        self.state = fixture.FixtureState(self.media, self.log, self.stop_event, self.limiter)

    def start_server(self, *, max_bytes: int = 1024):
        limiter = fixture.TransferLimiter(max_bytes, None)
        self.state = fixture.FixtureState(self.media, self.log, self.stop_event, limiter)
        server = fixture.FixtureServer(("127.0.0.1", 0), fixture.FixtureHandler)
        server.fixture_state = self.state
        thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.01})
        thread.start()
        self.addCleanup(server.server_close)
        self.addCleanup(thread.join, 2)
        self.addCleanup(server.shutdown)
        return f"http://127.0.0.1:{server.server_address[1]}"

    def events(self) -> list[dict[str, object]]:
        lines = self.events_path.read_text(encoding="ascii").splitlines()
        return [json.loads(line) for line in lines]

    def test_raw_clock_fails_closed_when_darwin_clock_is_unavailable(self) -> None:
        with mock.patch.object(fixture.time, "CLOCK_MONOTONIC_RAW", None, create=True):
            with self.assertRaisesRegex(fixture.FixtureError, "CLOCK_MONOTONIC_RAW"):
                fixture.raw_clock_ns()

    def test_raw_clock_uses_only_explicit_clock_id(self) -> None:
        with mock.patch.object(fixture.time, "CLOCK_MONOTONIC_RAW", 987, create=True):
            with mock.patch.object(fixture.time, "clock_gettime_ns", return_value=123456) as clock:
                self.assertEqual(fixture.raw_clock_ns(), 123456)
        clock.assert_called_once_with(987)

    def test_raw_clock_rejects_nonpositive_or_noninteger_values(self) -> None:
        for value in (0, -1, True, 1.5, "123"):
            with self.subTest(value=value):
                with mock.patch.object(fixture.time, "CLOCK_MONOTONIC_RAW", 987, create=True):
                    with mock.patch.object(fixture.time, "clock_gettime_ns", return_value=value):
                        with self.assertRaisesRegex(fixture.FixtureError, "invalid nanosecond"):
                            fixture.raw_clock_ns()

    def test_run_rejects_non_darwin_before_creating_output(self) -> None:
        output = self.root / "output"
        with mock.patch.object(fixture.sys, "platform", "linux"):
            with mock.patch.object(fixture, "raw_clock_ns") as clock:
                result = fixture.run(["--output-root", str(output), "--run-id", "unit"])
        self.assertEqual(result, 1)
        self.assertFalse(output.exists())
        clock.assert_not_called()

    def test_run_rejects_invalid_clock_before_creating_output(self) -> None:
        output = self.root / "output"
        with mock.patch.object(fixture.sys, "platform", "darwin"):
            with mock.patch.object(fixture, "raw_clock_ns", side_effect=fixture.FixtureError("invalid clock")):
                result = fixture.run(["--output-root", str(output), "--run-id", "unit"])
        self.assertEqual(result, 1)
        self.assertFalse(output.exists())

    def test_run_reports_cleanup_success_and_failure_truthfully(self) -> None:
        for cleanup_fails in (False, True):
            with self.subTest(cleanup_fails=cleanup_fails):
                output = self.root / f"output-{cleanup_fails}"
                output.mkdir()
                owned_temp = tempfile.TemporaryDirectory(prefix="test-media-")
                real_cleanup = owned_temp.cleanup
                media_directory = Path(owned_temp.name)
                cleanup_calls: list[None] = []

                def cleanup() -> None:
                    cleanup_calls.append(None)
                    self.assertFalse((output / "unit" / "report.json").exists())
                    if cleanup_fails:
                        raise OSError("synthetic cleanup failure")
                    real_cleanup()

                owned_temp.cleanup = cleanup  # type: ignore[method-assign]

                def fail_generation(*_args, **_kwargs):
                    raise fixture.FixtureError("synthetic generation failure")

                with mock.patch.object(fixture.sys, "platform", "darwin"):
                    with mock.patch.object(fixture, "raw_clock_ns", return_value=1):
                        with mock.patch.object(fixture.tempfile, "TemporaryDirectory", return_value=owned_temp):
                            with mock.patch.object(fixture, "generate_media", side_effect=fail_generation):
                                result = fixture.run(["--output-root", str(output), "--run-id", "unit"])

                try:
                    report = json.loads((output / "unit" / "report.json").read_text(encoding="ascii"))
                    self.assertEqual(result, 1)
                    self.assertEqual(cleanup_calls, [None])
                    self.assertEqual(report["media_cleaned"], not cleanup_fails)
                    self.assertIn("synthetic generation failure", report["error"])
                    self.assertEqual("synthetic cleanup failure" in report["error"], cleanup_fails)
                    self.assertEqual(report["shutdown_reason"], "media_cleanup_failure" if cleanup_fails else "requested")
                    if not cleanup_fails:
                        self.assertFalse(media_directory.exists())
                finally:
                    real_cleanup()

    def test_run_budget_failures_stop_serving_and_clean_media(self) -> None:
        for budget in ("wall_limit", "aggregate_budget"):
            with self.subTest(budget=budget):
                output = self.root / f"budget-{budget}"
                output.mkdir()
                ready = threading.Event()
                ready_path = output / "budget" / "ready.json"
                servers: list[fixture.FixtureServer] = []
                media_temps: list[tempfile.TemporaryDirectory[str]] = []
                cleanup_calls: list[None] = []
                run_results: list[int] = []
                expected_error = (
                    "fixture reached 900-second wall limit"
                    if budget == "wall_limit"
                    else "aggregate experiment output exceeded 256 MiB"
                )

                class CapturedServer(fixture.FixtureServer):
                    def __init__(self, *args, **kwargs):
                        super().__init__(*args, **kwargs)
                        servers.append(self)

                real_temporary_directory = tempfile.TemporaryDirectory

                def capture_media_temp(*args, **kwargs):
                    media_temp = real_temporary_directory(*args, **kwargs)
                    media_temps.append(media_temp)
                    cleanup = media_temp.cleanup

                    def tracked_cleanup() -> None:
                        cleanup_calls.append(None)
                        cleanup()

                    media_temp.cleanup = tracked_cleanup  # type: ignore[method-assign]
                    return media_temp

                def capture_print(*args, **kwargs) -> None:
                    if kwargs.get("file", sys.stdout) is not sys.stdout:
                        return
                    try:
                        payload = json.loads(" ".join(str(arg) for arg in args))
                    except (json.JSONDecodeError, TypeError):
                        return
                    if isinstance(payload, dict) and payload.get("ready") == str(ready_path):
                        ready.set()

                patches = [
                    mock.patch.object(fixture.sys, "platform", "darwin"),
                    mock.patch.object(fixture, "raw_clock_ns", return_value=123),
                    mock.patch.object(fixture, "generate_media", return_value=Path("mock-media.m3u8")),
                    mock.patch.object(fixture, "FixtureServer", CapturedServer),
                    mock.patch.object(
                        fixture.tempfile, "TemporaryDirectory", side_effect=capture_media_temp
                    ),
                    mock.patch("builtins.print", side_effect=capture_print),
                ]
                if budget == "wall_limit":
                    patches.extend([
                        mock.patch.object(fixture, "MAX_WALL_SECONDS", 10),
                        mock.patch.object(
                            fixture.time, "monotonic", side_effect=[100.0, 100.0, 111.0, 111.0]
                        ),
                    ])
                else:
                    patches.append(
                        mock.patch.object(fixture, "_walk_bytes", return_value=fixture.MAX_TOTAL_BYTES + 1)
                    )

                run_thread = threading.Thread(
                    target=lambda: run_results.append(
                        fixture.run(["--output-root", str(output), "--run-id", "budget"])
                    )
                )
                with ExitStack() as stack:
                    for patcher in patches:
                        stack.enter_context(patcher)
                    run_thread.start()
                    try:
                        self.assertTrue(ready.wait(2))
                    finally:
                        if servers and not ready.is_set():
                            servers[0].fixture_state.stop_event.set()
                        run_thread.join(3)

                try:
                    self.assertFalse(run_thread.is_alive())
                    self.assertEqual(run_results, [1])
                    self.assertEqual(cleanup_calls, [None])
                    self.assertEqual(len(media_temps), 1)
                    self.assertFalse(Path(media_temps[0].name).exists())
                    self.assertTrue(servers[0]._BaseServer__is_shut_down.is_set())
                    report = json.loads((output / "budget" / "report.json").read_text(encoding="ascii"))
                    self.assertEqual(report["error"], expected_error)
                    self.assertEqual(report["shutdown_reason"], budget)
                    self.assertTrue(report["media_cleaned"])
                finally:
                    for media_temp in media_temps:
                        media_temp.cleanup()

    def test_range_parser_accepts_single_byte_ranges(self) -> None:
        self.assertIsNone(fixture.parse_byte_range(None, 64))
        self.assertEqual(fixture.parse_byte_range("bytes=4-9", 64), (4, 9))
        self.assertEqual(fixture.parse_byte_range("bytes=60-", 64), (60, 63))
        self.assertEqual(fixture.parse_byte_range("bytes=-8", 64), (56, 63))
        self.assertEqual(fixture.parse_byte_range("bytes=-100", 64), (0, 63))

    def test_range_parser_rejects_multiple_or_unsatisfiable_ranges(self) -> None:
        for value in ("bytes=1-2,4-5", "bytes=-0", "bytes=64-", "bytes=10-2", "items=1-2"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                fixture.parse_byte_range(value, 64)

    def test_event_log_enforces_event_and_byte_budgets(self) -> None:
        with mock.patch.object(fixture, "raw_clock_ns", return_value=5):
            with mock.patch.object(fixture, "MAX_EVENTS", 1):
                self.log.emit("request_start", fixture_basename="segment0000.m4s", range="none", segment_index=0)
                with self.assertRaisesRegex(fixture.FixtureError, "event log budget"):
                    self.log.emit("request_finish", fixture_basename="segment0000.m4s", range="none", segment_index=0)
        self.assertTrue(self.stop_event.is_set())
        self.assertEqual(self.log.event_count, 1)
        self.assertEqual(self.log.failure_reason, "fixture event log budget exceeded")
        row = self.events()[0]
        self.assertEqual(row["clock_ns"], 5)
        self.assertEqual(row["clock_domain"], fixture.CLOCK_DOMAIN)
        self.assertEqual(row["schema_version"], 1)

    def test_event_log_byte_cap_does_not_write_over_budget(self) -> None:
        path = self.root / "tiny.jsonl"
        stop_event = threading.Event()
        log = fixture.EventLog(path, "unit-run", stop_event)
        with mock.patch.object(fixture, "raw_clock_ns", return_value=5):
            with mock.patch.object(fixture, "MAX_FIXTURE_LOG_BYTES", 8):
                with self.assertRaisesRegex(fixture.FixtureError, "event log budget"):
                    log.emit("request_start", fixture_basename="segment0000.m4s", range="none", segment_index=0)
        self.assertEqual(path.stat().st_size, 0)
        self.assertTrue(stop_event.is_set())

    def test_media_allowlist_rejects_external_and_traversal_paths(self) -> None:
        for value in ("/../secret", "/%2e%2e/secret", "http://example.test/a", "/segment9999.m4s", "/master.m3u8?url=x"):
            with self.subTest(value=value):
                self.assertIsNone(self.state.media_path(value))
        self.assertEqual(self.state.media_path("/segment0000.m4s")[1:], ("segment0000.m4s", 0))

    def test_http_get_head_single_range_and_416(self) -> None:
        base = self.start_server()
        with mock.patch.object(fixture, "raw_clock_ns", return_value=77):
            with urllib.request.urlopen(base + "/segment0000.m4s", timeout=2) as response:
                self.assertEqual(response.status, 200)
                self.assertEqual(response.headers.get_content_type(), "video/iso.segment")
                self.assertEqual(response.read(), bytes(range(64)))
            request = urllib.request.Request(base + "/segment0000.m4s", method="HEAD")
            with urllib.request.urlopen(request, timeout=2) as response:
                self.assertEqual(response.status, 200)
                self.assertEqual(response.headers["Content-Length"], "64")
            request = urllib.request.Request(base + "/segment0000.m4s", headers={"Range": "bytes=3-7"})
            with urllib.request.urlopen(request, timeout=2) as response:
                self.assertEqual(response.status, 206)
                self.assertEqual(response.headers["Content-Range"], "bytes 3-7/64")
                self.assertEqual(response.read(), bytes(range(3, 8)))
            request = urllib.request.Request(base + "/segment0000.m4s", headers={"Range": "bytes=64-"})
            with self.assertRaises(urllib.error.HTTPError) as caught:
                urllib.request.urlopen(request, timeout=2)
            self.assertEqual(caught.exception.code, 416)
            self.assertEqual(caught.exception.headers["Content-Range"], "bytes */64")
            caught.exception.close()
        finishes = [event for event in self.events() if event["kind"] == "request_finish"]
        self.assertEqual([event["status"] for event in finishes], [200, 200, 206])
        starts = [event for event in self.events() if event["kind"] == "request_start"]
        self.assertTrue(all(event["observation"] == "fixture_received" for event in starts))
        self.assertTrue(all(event["observation"] == "fixture_response_finished" for event in finishes))
        self.assertTrue(all("bytes_written" in event and "bytes_sent" not in event for event in finishes))
        self.assertTrue(all(event["clock_ns"] == 77 for event in self.events()))

    def test_transfer_limit_rejects_prefetch_after_byte_budget(self) -> None:
        base = self.start_server(max_bytes=16)
        with mock.patch.object(fixture, "raw_clock_ns", return_value=88):
            request = urllib.request.Request(base + "/segment0000.m4s", headers={"Range": "bytes=0-15"})
            with urllib.request.urlopen(request, timeout=2) as response:
                self.assertEqual(len(response.read()), 16)
            with self.assertRaises(urllib.error.HTTPError) as caught:
                urllib.request.urlopen(base + "/segment0000.m4s", timeout=2)
            self.assertEqual(caught.exception.code, 429)
            caught.exception.close()

    def test_barrier_acknowledges_matching_id_after_simulated_delay(self) -> None:
        base = self.start_server()
        payload = json.dumps({"request_id": "jump-1", "delay_ms": 30}).encode("ascii")
        request = urllib.request.Request(
            base + "/barrier", data=payload,
            headers={"Content-Type": "application/json"}, method="POST",
        )
        began = time.monotonic()
        with mock.patch.object(fixture, "raw_clock_ns", return_value=99):
            with urllib.request.urlopen(request, timeout=2) as response:
                ack = json.loads(response.read())
        self.assertGreaterEqual(time.monotonic() - began, 0.02)
        self.assertEqual(ack, {"request_id": "jump-1", "outcome": "acknowledged"})
        self.assertEqual([event["kind"] for event in self.events()], ["barrier_requested", "barrier_acknowledged"])

    def test_barrier_rejects_invalid_payload(self) -> None:
        base = self.start_server()
        payload = json.dumps({"request_id": "bad", "delay_ms": 2001}).encode("ascii")
        request = urllib.request.Request(base + "/barrier", data=payload, method="POST")
        with mock.patch.object(fixture, "raw_clock_ns", return_value=101):
            with self.assertRaises(urllib.error.HTTPError) as caught:
                urllib.request.urlopen(request, timeout=2)
        self.assertEqual(caught.exception.code, 400)
        caught.exception.close()
        self.assertEqual(self.log.event_count, 0)

    def test_cli_defaults_keep_far_seek_media_bounded(self) -> None:
        options = fixture._parse_args(["--output-root", str(self.root), "--run-id", "unit"])
        self.assertEqual(options.port, 0)
        self.assertEqual(options.max_media_bytes, 8 * 1024 * 1024)
        self.assertEqual(options.max_media_bytes_per_second, 16 * 1024)
        self.assertLess(options.max_media_bytes_per_second * 8, 300_000)
        self.assertLess(options.max_media_bytes, 600 * 300_000 // 8)

    def test_transfer_pacing_does_not_bank_initial_or_midstream_idle_time(self) -> None:
        clock = FakeClock()
        stop_event = RecordingStopEvent()
        limiter = fixture.TransferLimiter(1024 * 1024, 16 * 1024, stop_event, clock=clock)
        clock.advance(120)

        self.assertTrue(limiter.wait_for_slot(fixture.CHUNK_BYTES))
        self.assertTrue(limiter.wait_for_slot(fixture.CHUNK_BYTES))
        clock.advance(100)
        self.assertTrue(limiter.wait_for_slot(fixture.CHUNK_BYTES))
        self.assertTrue(limiter.wait_for_slot(fixture.CHUNK_BYTES))

        self.assertEqual(stop_event.waits, [4.0, 4.0])

    def test_transfer_pacing_serializes_aggregate_concurrent_chunks(self) -> None:
        clock = FakeClock()
        stop_event = RecordingStopEvent()
        limiter = fixture.TransferLimiter(1024 * 1024, 16 * 1024, stop_event, clock=clock)
        start = threading.Barrier(5)
        results: list[bool] = []
        result_lock = threading.Lock()

        def reserve_chunk() -> None:
            start.wait()
            result = limiter.wait_for_slot(fixture.CHUNK_BYTES)
            with result_lock:
                results.append(result)

        threads = [threading.Thread(target=reserve_chunk) for _ in range(4)]
        for thread in threads:
            thread.start()
        start.wait()
        for thread in threads:
            thread.join(2)

        self.assertTrue(all(not thread.is_alive() for thread in threads))
        self.assertEqual(results, [True] * 4)
        self.assertEqual(sorted(stop_event.waits), [4.0, 8.0, 12.0])
        self.assertEqual(limiter.written_bytes, 0)

    def test_transfer_cancellation_does_not_count_reserved_bytes_as_written(self) -> None:
        clock = FakeClock()
        stop_event = CancelDuringWait()
        limiter = fixture.TransferLimiter(1024, 16 * 1024, stop_event, clock=clock)

        self.assertTrue(limiter.wait_for_slot(fixture.CHUNK_BYTES))
        limiter.account_written(fixture.CHUNK_BYTES)
        self.assertFalse(limiter.wait_for_slot(1024))
        self.assertEqual(stop_event.waits, [4.0])
        self.assertEqual(limiter.written_bytes, fixture.CHUNK_BYTES)

    def test_run_drains_http_handlers_before_final_counters_and_failure(self) -> None:
        for event_limit, trigger_exception in ((2, False), (1, False), (2, True), (1, True)):
            expected_count = event_limit
            expected_failure = event_limit == 1
            with self.subTest(event_limit=event_limit, trigger_exception=trigger_exception):
                output = self.root / f"drain-{event_limit}-{trigger_exception}"
                output.mkdir()
                finish_entered = threading.Event()
                release_finish = threading.Event()
                shutdown_entered = threading.Event()
                fail_loop = threading.Event()
                servers = []
                run_results: list[int] = []

                class CapturedServer(fixture.FixtureServer):
                    def __init__(self, *args, **kwargs):
                        super().__init__(*args, **kwargs)
                        servers.append(self)

                    def shutdown(self):
                        shutdown_entered.set()
                        return super().shutdown()

                def generate(media_dir: Path, _output_root: Path, *, started: float) -> Path:
                    del started
                    playlist = media_dir / "master.m3u8"
                    playlist.write_text(
                        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:2\n"
                        "#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:2.0,\n"
                        "segment0000.m4s\n#EXT-X-ENDLIST\n",
                        encoding="ascii",
                    )
                    (media_dir / "init.mp4").write_bytes(b"init")
                    (media_dir / "segment0000.m4s").write_bytes(b"segment")
                    return playlist

                original_emit = fixture.EventLog.emit

                def delayed_finish(log, kind: str, **fields: object) -> None:
                    if kind == "request_finish":
                        finish_entered.set()
                        if not release_finish.wait(2):
                            raise AssertionError("test did not release request handler")
                    original_emit(log, kind, **fields)

                original_walk_bytes = fixture._walk_bytes

                def fail_during_run(root: Path) -> int:
                    if fail_loop.is_set() and root == output / "drain":
                        raise OSError("synthetic serving-loop failure")
                    return original_walk_bytes(root)

                ready = threading.Event()
                ready_payloads: list[dict[str, object]] = []
                ready_path = output / "drain" / "ready.json"

                def capture_print(*args, **kwargs) -> None:
                    if kwargs.get("file", sys.stdout) is not sys.stdout:
                        return
                    try:
                        payload = json.loads(" ".join(str(arg) for arg in args))
                    except (json.JSONDecodeError, TypeError):
                        return
                    if not isinstance(payload, dict) or payload.get("ready") != str(ready_path):
                        return
                    if not isinstance(payload.get("url"), str):
                        return
                    ready_payloads.append(payload)
                    ready.set()

                run_thread = threading.Thread(
                    target=lambda: run_results.append(
                        fixture.run(["--output-root", str(output), "--run-id", "drain"])
                    )
                )
                request_thread: threading.Thread | None = None
                request_result: list[bytes] = []
                with mock.patch.object(fixture.sys, "platform", "darwin"), \
                    mock.patch.object(fixture, "raw_clock_ns", return_value=123), \
                    mock.patch.object(fixture, "generate_media", side_effect=generate), \
                    mock.patch.object(fixture, "FixtureServer", CapturedServer), \
                    mock.patch.object(fixture, "_walk_bytes", side_effect=fail_during_run), \
                    mock.patch.object(fixture, "MAX_EVENTS", event_limit), \
                    mock.patch.object(fixture.EventLog, "emit", delayed_finish), \
                    mock.patch("builtins.print", side_effect=capture_print):
                    run_thread.start()
                    try:
                        self.assertTrue(ready.wait(2))
                        ready_value = ready_payloads[0]

                        def request_segment() -> None:
                            with urllib.request.urlopen(
                                ready_value["url"].removesuffix("master.m3u8") + "segment0000.m4s",
                                timeout=2,
                            ) as response:
                                request_result.append(response.read())

                        request_thread = threading.Thread(target=request_segment)
                        request_thread.start()
                        self.assertTrue(finish_entered.wait(2))
                        if trigger_exception:
                            fail_loop.set()
                        else:
                            servers[0].fixture_state.stop_event.set()
                        self.assertTrue(shutdown_entered.wait(2))
                    finally:
                        release_finish.set()
                        if servers:
                            servers[0].fixture_state.stop_event.set()
                        if request_thread is not None:
                            request_thread.join(2)
                        run_thread.join(2)

                self.assertFalse(request_thread is not None and request_thread.is_alive())
                self.assertFalse(run_thread.is_alive())
                self.assertEqual(request_result, [b"segment"])
                self.assertEqual(run_results, [1 if expected_failure or trigger_exception else 0])
                report = json.loads((output / "drain" / "report.json").read_text(encoding="ascii"))
                self.assertEqual(report["event_count"], expected_count)
                self.assertEqual(report["media_bytes_written"], len(b"segment"))
                self.assertEqual(
                    bool(report["error"] and "event log budget exceeded" in report["error"]),
                    expected_failure,
                )
                self.assertEqual(
                    bool(report["error"] and "synthetic serving-loop failure" in report["error"]),
                    trigger_exception,
                )
                self.assertEqual(report["shutdown_reason"], "fixture_log_failure" if expected_failure else "requested")

    def test_generation_stops_when_media_growth_exceeds_cap(self) -> None:
        output = self.root / "output"
        output.mkdir()

        class FinishedProducer:
            pid = 321
            stderr = io.BytesIO()

            def poll(self):
                return 0

            def wait(self, timeout=None):
                return 0

        def produce(command, **_kwargs):
            Path(command[-1]).write_bytes(b"x" * 9)
            return FinishedProducer()

        with mock.patch.object(fixture.shutil, "which", side_effect=["ffmpeg", "ffprobe"]):
            with mock.patch.object(fixture.subprocess, "Popen", side_effect=produce):
                with mock.patch.object(fixture, "MAX_MEDIA_BYTES", 8):
                    with self.assertRaisesRegex(fixture.FixtureError, "generation exceeded"):
                        fixture.generate_media(self.media, output, started=time.monotonic())

if __name__ == "__main__":
    unittest.main()
