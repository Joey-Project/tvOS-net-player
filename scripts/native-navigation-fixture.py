#!/usr/bin/env python3
"""Bounded loopback-only HLS fixture for the native navigation prototype."""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit


CLOCK_DOMAIN = "darwin:CLOCK_MONOTONIC_RAW:nanoseconds"
SCHEMA_VERSION = 1
MAX_MEDIA_BYTES = 192 * 1024 * 1024
MAX_FIXTURE_LOG_BYTES = 8 * 1024 * 1024
MAX_GENERATOR_LOG_BYTES = 1 * 1024 * 1024
MAX_TOTAL_BYTES = 256 * 1024 * 1024
MAX_EVENTS = 4096
MAX_WALL_SECONDS = 900
GENERATION_WALL_SECONDS = 180
GENERATION_SECONDS = 600
SEGMENT_SECONDS = 2
SEGMENT_COUNT = GENERATION_SECONDS // SEGMENT_SECONDS
CHUNK_BYTES = 64 * 1024
RUN_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")
REQUEST_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")
SEGMENT_RE = re.compile(r"^segment([0-9]{4})\.m4s$")


class FixtureError(RuntimeError):
    pass


def raw_clock_ns() -> int:
    """Return the shared Darwin raw monotonic clock without epoch fallback."""
    clock_id = getattr(time, "CLOCK_MONOTONIC_RAW", None)
    if clock_id is None or not hasattr(time, "clock_gettime_ns"):
        raise FixtureError("CLOCK_MONOTONIC_RAW nanosecond clock is unavailable")
    try:
        value = time.clock_gettime_ns(clock_id)
    except (OSError, ValueError) as exc:
        raise FixtureError(f"CLOCK_MONOTONIC_RAW failed: {exc}") from exc
    if type(value) is not int or value <= 0:
        raise FixtureError("CLOCK_MONOTONIC_RAW returned an invalid nanosecond value")
    return value


def parse_byte_range(header: str | None, size: int) -> tuple[int, int] | None:
    """Parse one RFC 7233 byte range; None means a full response."""
    if not header:
        return None
    match = re.fullmatch(r"bytes=(\d*)-(\d*)", header.strip())
    if not match or size <= 0:
        raise ValueError("unsatisfiable byte range")
    first, last = match.groups()
    if not first and not last:
        raise ValueError("unsatisfiable byte range")
    if not first:
        suffix = int(last)
        if suffix <= 0:
            raise ValueError("unsatisfiable byte range")
        return max(0, size - suffix), size - 1
    start = int(first)
    end = int(last) if last else size - 1
    if start >= size or end < start:
        raise ValueError("unsatisfiable byte range")
    return start, min(end, size - 1)


class EventLog:
    def __init__(self, path: Path, run_id: str, stop_event: threading.Event):
        self.path = path
        self.run_id = run_id
        self.stop_event = stop_event
        self._lock = threading.Lock()
        self._events = 0
        self._bytes = 0
        self._failure_reason: str | None = None
        self.path.touch(exist_ok=False)

    def emit(self, kind: str, **fields: object) -> None:
        try:
            clock_ns = raw_clock_ns()
        except FixtureError as exc:
            with self._lock:
                self._failure_reason = str(exc)
                self.stop_event.set()
            raise
        event = {
            "schema_version": SCHEMA_VERSION,
            "kind": kind,
            "clock_ns": clock_ns,
            "clock_domain": CLOCK_DOMAIN,
            "run_id": self.run_id,
            **fields,
        }
        encoded = (json.dumps(event, separators=(",", ":"), ensure_ascii=True) + "\n").encode("ascii")
        with self._lock:
            if self._events >= MAX_EVENTS or self._bytes + len(encoded) > MAX_FIXTURE_LOG_BYTES:
                self._failure_reason = "fixture event log budget exceeded"
                self.stop_event.set()
                raise FixtureError(self._failure_reason)
            with self.path.open("ab", buffering=0) as stream:
                stream.write(encoded)
            self._events += 1
            self._bytes += len(encoded)

    @property
    def event_count(self) -> int:
        with self._lock:
            return self._events

    @property
    def failure_reason(self) -> str | None:
        with self._lock:
            return self._failure_reason


class TransferLimiter:
    def __init__(
        self,
        max_bytes: int,
        bytes_per_second: int | None,
        stop_event: threading.Event | None = None,
        *,
        clock=time.monotonic,
    ):
        self.max_bytes = max_bytes
        self.bytes_per_second = bytes_per_second
        self.stop_event = stop_event or threading.Event()
        self._clock = clock
        self._lock = threading.Lock()
        self._reserved = 0
        self._sent = 0
        self._next_send_time = 0.0

    def reserve(self, amount: int) -> bool:
        with self._lock:
            if self._reserved + amount > self.max_bytes:
                return False
            self._reserved += amount
            return True

    def wait_for_slot(self, amount: int) -> bool:
        if self.stop_event.is_set():
            return False
        if self.bytes_per_second is None:
            return True
        with self._lock:
            now = self._clock()
            slot = max(now, self._next_send_time)
            self._next_send_time = slot + amount / self.bytes_per_second
        delay = slot - now
        if delay > 0 and self.stop_event.wait(delay):
            return False
        return not self.stop_event.is_set()

    def account_written(self, amount: int) -> None:
        with self._lock:
            self._sent += amount

    @property
    def written_bytes(self) -> int:
        with self._lock:
            return self._sent


class FixtureState:
    def __init__(
        self,
        media_dir: Path,
        event_log: EventLog,
        stop_event: threading.Event,
        limiter: TransferLimiter,
    ):
        self.media_dir = media_dir.resolve(strict=True)
        self.event_log = event_log
        self.stop_event = stop_event
        self.limiter = limiter

    def media_path(self, url_path: str) -> tuple[Path, str, int | None] | None:
        parsed = urlsplit(url_path)
        if parsed.query or parsed.fragment:
            return None
        if parsed.path == "/master.m3u8":
            basename, segment_index = "master.m3u8", None
        elif parsed.path == "/init.mp4":
            basename, segment_index = "init.mp4", None
        else:
            name = parsed.path.removeprefix("/")
            match = SEGMENT_RE.fullmatch(name)
            if not match:
                return None
            segment_index = int(match.group(1))
            if segment_index >= SEGMENT_COUNT:
                return None
            basename = name
        path = self.media_dir / basename
        try:
            metadata = path.lstat()
        except FileNotFoundError:
            return None
        if not path.is_file() or path.is_symlink() or path.parent.resolve() != self.media_dir:
            return None
        if metadata.st_size <= 0:
            return None
        return path, basename, segment_index


class FixtureHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "NativeNavigationFixture/1"
    sys_version = ""

    @property
    def state(self) -> FixtureState:
        return self.server.fixture_state  # type: ignore[attr-defined]

    def log_message(self, _format: str, *_args: object) -> None:
        return

    def do_HEAD(self) -> None:
        self._serve_media(send_body=False)

    def do_GET(self) -> None:
        self._serve_media(send_body=True)

    def do_POST(self) -> None:
        if self.path != "/barrier":
            self.send_error(404)
            return
        raw_length = self.headers.get("Content-Length")
        if not raw_length or not raw_length.isascii() or not raw_length.isdecimal():
            self.send_error(411)
            return
        length = int(raw_length)
        if length > 4096:
            self.send_error(413)
            return
        if self.headers.get("Transfer-Encoding"):
            self.send_error(400)
            return
        try:
            body = json.loads(self.rfile.read(length))
        except (json.JSONDecodeError, UnicodeDecodeError):
            self.send_error(400)
            return
        if not isinstance(body, dict) or set(body) != {"request_id", "delay_ms"}:
            self.send_error(400)
            return
        request_id, delay_ms = body["request_id"], body["delay_ms"]
        if (
            not isinstance(request_id, str)
            or not REQUEST_ID_RE.fullmatch(request_id)
            or isinstance(delay_ms, bool)
            or not isinstance(delay_ms, int)
            or not 0 <= delay_ms <= 2000
        ):
            self.send_error(400)
            return
        try:
            self.state.event_log.emit("barrier_requested", request_id=request_id, delay_ms=delay_ms)
            if self.state.stop_event.wait(delay_ms / 1000):
                self.send_error(503)
                return
            self.state.event_log.emit("barrier_acknowledged", request_id=request_id, outcome="acknowledged")
        except FixtureError:
            self.send_error(503)
            return
        self._send_json(200, {"request_id": request_id, "outcome": "acknowledged"})

    def _send_json(self, status: int, value: dict[str, object]) -> None:
        body = json.dumps(value, separators=(",", ":")).encode("ascii")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _serve_media(self, *, send_body: bool) -> None:
        entry = self.state.media_path(self.path)
        if entry is None:
            self.send_error(404)
            return
        path, basename, segment_index = entry
        size = path.stat().st_size
        range_header = self.headers.get("Range")
        try:
            byte_range = parse_byte_range(range_header, size)
        except ValueError:
            self.send_response(416)
            self.send_header("Content-Range", f"bytes */{size}")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        start, end = byte_range if byte_range is not None else (0, size - 1)
        length = end - start + 1
        if send_body and not self.state.limiter.reserve(length):
            self.send_response(429)
            self.send_header("Retry-After", "1")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        content_type = "application/vnd.apple.mpegurl" if basename == "master.m3u8" else (
            "video/mp4" if basename == "init.mp4" else "video/iso.segment"
        )
        status = 206 if byte_range is not None else 200
        range_value = f"bytes={start}-{end}" if byte_range is not None else "none"
        try:
            self.state.event_log.emit(
                "request_start", fixture_basename=basename, range=range_value,
                segment_index=segment_index, observation="fixture_received",
            )
        except FixtureError:
            self.send_error(503)
            return
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Length", str(length))
        if byte_range is not None:
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        transferred = 0
        try:
            if send_body:
                with path.open("rb") as source:
                    source.seek(start)
                    remaining = length
                    while remaining:
                        chunk = source.read(min(CHUNK_BYTES, remaining))
                        if not chunk:
                            raise OSError("fixture file changed while serving")
                        if not self.state.limiter.wait_for_slot(len(chunk)):
                            self.close_connection = True
                            break
                        self.wfile.write(chunk)
                        transferred += len(chunk)
                        self.state.limiter.account_written(len(chunk))
                        remaining -= len(chunk)
        except (BrokenPipeError, ConnectionResetError, OSError):
            self.close_connection = True
        finally:
            try:
                self.state.event_log.emit(
                    "request_finish", fixture_basename=basename, range=range_value,
                    segment_index=segment_index, status=status, bytes_written=transferred,
                    observation="fixture_response_finished",
                )
            except FixtureError:
                self.close_connection = True


class FixtureServer(ThreadingHTTPServer):
    address_family = socket.AF_INET
    daemon_threads = False
    block_on_close = True
    allow_reuse_address = False
    max_handlers = 16

    def __init__(self, *args: object, **kwargs: object):
        self._handler_slots = threading.BoundedSemaphore(self.max_handlers)
        super().__init__(*args, **kwargs)

    def get_request(self) -> tuple[socket.socket, tuple[str, int]]:
        request, address = super().get_request()
        request.settimeout(5)
        return request, address

    def process_request(self, request: socket.socket, client_address: tuple[str, int]) -> None:
        if not self._handler_slots.acquire(blocking=False):
            self.shutdown_request(request)
            return
        try:
            super().process_request(request, client_address)
        except BaseException:
            self._handler_slots.release()
            raise

    def process_request_thread(self, request: socket.socket, client_address: tuple[str, int]) -> None:
        try:
            super().process_request_thread(request, client_address)
        finally:
            self._handler_slots.release()


def _walk_bytes(root: Path) -> int:
    total = 0
    for current, _dirs, files in os.walk(root, followlinks=False):
        for name in files:
            path = Path(current) / name
            try:
                if path.is_symlink():
                    raise FixtureError("symlink appeared in fixture output")
                total += path.stat().st_size
            except FileNotFoundError:
                continue
    return total


def _write_json(path: Path, value: dict[str, object]) -> None:
    encoded = (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")
    if len(encoded) > 64 * 1024:
        raise FixtureError("metadata exceeds 64 KiB")
    temporary = path.with_suffix(path.suffix + ".tmp")
    with temporary.open("xb") as stream:
        stream.write(encoded)
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)


def _quiesce_server(server: FixtureServer, server_thread: threading.Thread | None) -> None:
    if server_thread is not None and server_thread.is_alive():
        server.shutdown()
    server.server_close()
    if server_thread is not None:
        server_thread.join()


def _terminate_process(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    if os.name == "posix":
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    else:
        process.terminate()
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        if os.name == "posix":
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        else:
            process.kill()
        process.wait(timeout=2)


def generate_media(media_dir: Path, output_root: Path, *, started: float) -> Path:
    ffmpeg = shutil.which("ffmpeg")
    ffprobe = shutil.which("ffprobe")
    if not ffmpeg or not ffprobe:
        raise FixtureError("ffmpeg and ffprobe are required to generate fixture media")
    playlist = media_dir / "master.m3u8"
    segment_pattern = str(media_dir / "segment%04d.m4s")
    command = [
        ffmpeg, "-hide_banner", "-nostdin", "-y", "-f", "lavfi", "-i",
        "testsrc2=size=640x360:rate=24", "-t", str(GENERATION_SECONDS), "-an",
        "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p",
        "-b:v", "300k", "-minrate", "300k", "-maxrate", "300k", "-bufsize", "300k",
        "-x264-params", "nal-hrd=cbr:filler=1", "-g", "48", "-keyint_min", "48",
        "-sc_threshold", "0", "-f", "hls", "-hls_time", "2",
        "-hls_playlist_type", "vod", "-hls_segment_type", "fmp4",
        "-hls_flags", "independent_segments", "-hls_segment_filename", segment_pattern,
        str(playlist),
    ]
    log_path = output_root / "ffmpeg.log"
    stored = bytearray()
    exceeded_log = threading.Event()

    try:
        process = subprocess.Popen(
            command, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE, start_new_session=(os.name == "posix"),
        )
    except OSError as exc:
        raise FixtureError(f"could not start ffmpeg: {exc}") from exc

    assert process.stderr is not None

    def collect_stderr() -> None:
        while True:
            chunk = process.stderr.read(8192)
            if not chunk:
                return
            if len(stored) + len(chunk) > MAX_GENERATOR_LOG_BYTES:
                remaining = MAX_GENERATOR_LOG_BYTES - len(stored)
                if remaining > 0:
                    stored.extend(chunk[:remaining])
                exceeded_log.set()
                _terminate_process(process)
                return
            stored.extend(chunk)

    collector = threading.Thread(target=collect_stderr, daemon=True)
    collector.start()
    failure: str | None = None
    deadline = min(started + MAX_WALL_SECONDS, time.monotonic() + GENERATION_WALL_SECONDS)
    try:
        while process.poll() is None:
            if exceeded_log.is_set():
                failure = "ffmpeg log exceeded 1 MiB"
                break
            if time.monotonic() >= deadline:
                failure = "ffmpeg generation exceeded 180 seconds or fixture wall limit"
                break
            media_bytes = _walk_bytes(media_dir)
            if media_bytes > MAX_MEDIA_BYTES:
                failure = "generated media exceeded 192 MiB"
                break
            if _walk_bytes(output_root) > MAX_TOTAL_BYTES:
                failure = "aggregate experiment output exceeded 256 MiB"
                break
            time.sleep(0.01)
    except KeyboardInterrupt:
        _terminate_process(process)
        raise
    finally:
        if failure:
            _terminate_process(process)
        return_code = process.wait(timeout=3)
        collector.join(timeout=3)
        with log_path.open("xb") as stream:
            stream.write(stored)
    if failure:
        raise FixtureError(failure)
    if return_code != 0:
        raise FixtureError(f"ffmpeg exited with status {return_code}; see bounded ffmpeg.log")
    if _walk_bytes(media_dir) > MAX_MEDIA_BYTES or time.monotonic() - started > MAX_WALL_SECONDS:
        raise FixtureError("generation exceeded fixture budget")
    probe = subprocess.run(
        [ffprobe, "-v", "error", "-show_entries", "format=duration", "-of", "default=nw=1:nk=1", str(playlist)],
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        timeout=10, check=False,
    )
    if probe.returncode != 0 or len(probe.stdout) > 1024 or len(probe.stderr) > 4096:
        raise FixtureError("ffprobe could not verify the generated playlist")
    try:
        duration = float(probe.stdout.decode("ascii").strip())
    except (UnicodeDecodeError, ValueError) as exc:
        raise FixtureError("ffprobe returned invalid duration") from exc
    if not 599.0 <= duration <= 601.0:
        raise FixtureError(f"generated HLS duration is not 600 seconds: {duration}")
    expected = ["#EXTM3U", "#EXT-X-VERSION:7", "#EXT-X-TARGETDURATION:2"]
    text = playlist.read_text(encoding="ascii")
    if any(line not in text for line in expected) or "#EXT-X-MAP:URI=\"init.mp4\"" not in text:
        raise FixtureError("generated playlist is not the expected fMP4 HLS shape")
    if len(list(media_dir.glob("segment[0-9][0-9][0-9][0-9].m4s"))) != SEGMENT_COUNT:
        raise FixtureError("generated playlist does not contain 300 two-second segments")
    if time.monotonic() - started > MAX_WALL_SECONDS:
        raise FixtureError("fixture exceeded 900-second wall limit during verification")
    return playlist


def _parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-root", required=True, type=Path)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--max-media-bytes", type=int, default=8 * 1024 * 1024)
    parser.add_argument("--max-media-bytes-per-second", type=int, default=16 * 1024)
    args = parser.parse_args(argv)
    if not RUN_ID_RE.fullmatch(args.run_id):
        parser.error("--run-id must be 1-64 safe ASCII characters")
    if not 0 <= args.port <= 65535:
        parser.error("--port must be between 0 and 65535")
    if not 1 <= args.max_media_bytes <= MAX_TOTAL_BYTES:
        parser.error("--max-media-bytes must be between 1 and 268435456")
    if not 0 <= args.max_media_bytes_per_second <= 100 * 1024 * 1024:
        parser.error("--max-media-bytes-per-second must be between 0 and 104857600; 0 disables pacing")
    return args


def run(argv: list[str] | None = None) -> int:
    args = _parse_args(sys.argv[1:] if argv is None else argv)
    started = time.monotonic()
    if sys.platform != "darwin":
        print("native-navigation-fixture: run requires Darwin CLOCK_MONOTONIC_RAW", file=sys.stderr)
        return 1
    try:
        raw_clock_ns()
    except FixtureError as exc:
        print(f"native-navigation-fixture: {exc}", file=sys.stderr)
        return 1
    output_root = args.output_root.expanduser().resolve()
    output_root.mkdir(parents=True, exist_ok=True)
    run_dir = output_root / args.run_id
    run_dir.mkdir(mode=0o700, exist_ok=False)
    ready_path = run_dir / "ready.json"
    event_path = run_dir / "fixture.jsonl"
    report_path = run_dir / "report.json"
    stop_event = threading.Event()
    media_temp: tempfile.TemporaryDirectory[str] | None = None
    media_dir: Path | None = None
    server: FixtureServer | None = None
    server_thread: threading.Thread | None = None
    event_log: EventLog | None = None
    limiter: TransferLimiter | None = None
    error: str | None = None
    shutdown_reason = "requested"
    event_count = 0
    media_bytes_written = 0
    media_cleaned = False
    result_code = 0
    try:
        media_temp = tempfile.TemporaryDirectory(prefix=".native-navigation-media-", dir=run_dir)
        media_dir = Path(media_temp.name)
        playlist = generate_media(media_dir, run_dir, started=started)
        if time.monotonic() - started > MAX_WALL_SECONDS:
            raise FixtureError("fixture exceeded 900-second wall limit before serving")
        event_log = EventLog(event_path, args.run_id, stop_event)
        limiter = TransferLimiter(args.max_media_bytes, args.max_media_bytes_per_second or None, stop_event)
        state = FixtureState(media_dir, event_log, stop_event, limiter)
        server = FixtureServer(("127.0.0.1", args.port), FixtureHandler)
        server.fixture_state = state  # type: ignore[attr-defined]
        port = server.server_address[1]
        artifacts = {
            "events": str(event_path), "ready": str(ready_path), "report": str(report_path),
            "playlist": str(playlist), "media_directory": str(media_dir),
        }
        _write_json(ready_path, {
            "schema_version": SCHEMA_VERSION,
            "run_id": args.run_id,
            "url": f"http://127.0.0.1:{port}/master.m3u8",
            "pid": os.getpid(),
            "clock_domain": CLOCK_DOMAIN,
            "artifacts": artifacts,
        })
        print(json.dumps({"ready": str(ready_path), "url": f"http://127.0.0.1:{port}/master.m3u8"}), flush=True)
        server_thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.05}, daemon=True)
        server_thread.start()
        try:
            while not stop_event.wait(0.1):
                elapsed = time.monotonic() - started
                if elapsed >= MAX_WALL_SECONDS:
                    error = "fixture reached 900-second wall limit"
                    shutdown_reason = "wall_limit"
                    stop_event.set()
                    break
                if _walk_bytes(run_dir) > MAX_TOTAL_BYTES:
                    error = "aggregate experiment output exceeded 256 MiB"
                    shutdown_reason = "aggregate_budget"
                    stop_event.set()
                    break
        except KeyboardInterrupt:
            shutdown_reason = "keyboard_interrupt"
            stop_event.set()
    except KeyboardInterrupt:
        error = "interrupted"
        shutdown_reason = "keyboard_interrupt"
        result_code = 130
        print("native-navigation-fixture: interrupted", file=sys.stderr)
    except (FixtureError, OSError, subprocess.SubprocessError) as exc:
        error = str(exc)
        result_code = 1
        print(f"native-navigation-fixture: {error}", file=sys.stderr)
    finally:
        if server is not None:
            stop_event.set()
            try:
                _quiesce_server(server, server_thread)
            except OSError as exc:
                shutdown_error = f"fixture server shutdown failed: {exc}"
                error = f"{error}; {shutdown_error}" if error else shutdown_error
                result_code = 1
            server = None
        if event_log is not None:
            event_count = event_log.event_count
            failure_reason = event_log.failure_reason
            if failure_reason:
                error = f"{error}; {failure_reason}" if error and failure_reason not in error else failure_reason
                shutdown_reason = "fixture_log_failure"
                result_code = 1
        if limiter is not None:
            media_bytes_written = limiter.written_bytes
        if media_temp is not None:
            try:
                media_temp.cleanup()
                media_cleaned = True
            except OSError as exc:
                cleanup_error = f"media cleanup failed: {exc}"
                error = f"{error}; {cleanup_error}" if error else cleanup_error
                shutdown_reason = "media_cleanup_failure"
                result_code = 1
                print(f"native-navigation-fixture: {cleanup_error}", file=sys.stderr)
        try:
            _write_json(report_path, {
                "schema_version": SCHEMA_VERSION,
                "run_id": args.run_id,
                "clock_domain": CLOCK_DOMAIN,
                "elapsed_seconds": round(time.monotonic() - started, 3),
                "event_count": event_count,
                "media_bytes_written": media_bytes_written,
                "error": error,
                "shutdown_reason": shutdown_reason,
                "media_cleaned": media_cleaned,
            })
        except OSError as exc:
            print(f"native-navigation-fixture: report write failed: {exc}", file=sys.stderr)
            result_code = 1
    return result_code


if __name__ == "__main__":
    raise SystemExit(run())
