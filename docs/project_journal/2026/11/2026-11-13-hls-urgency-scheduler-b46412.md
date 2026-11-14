---
id: 20261113-b46412
title: HLS Playback Urgency Scheduler
status: active
created: 2026-11-13
updated: 2026-11-14
branch: codex/hls-seek-recovery
pr:
supersedes: []
superseded_by:
---

# HLS Playback Urgency Scheduler

## Summary
- Continue from accepted master `ab8ebe3791f229aac6cbe153c294022f109efea9`
  after PR #106. Preserve one player/item, default buffering, full-fill intent,
  media identity validation and the existing strict acceptance budgets.
- Approved ordering is missing near-term video/audio first, eligible high
  preparation before far-future low, and eventual complete fills. Insufficient
  or stale origin evidence holds new high work while low supply continues.

## Current State
- Structured HLS playback uses negotiated reader binding and automatic context
  reporting. Reader IDs affect only the active factory URL, not saved source
  identity. Catalog generations remain authoritative; unsupported metadata
  leaves ordinary playback unchanged.
- Timing-based range priority protects a rate-scaled 20-second wall-time window,
  then eligible preparation before verified far playback reads. Pending spans
  age into continuity service after 2 seconds without progress; later spans can
  yield again after successful progress. Automatic high preparation is not
  enabled by this slice. Content identity and cancellation checks are unchanged.
- The current tvOS `VideoPlayer` and macOS `AVPlayerView` expose native seek
  controls outside AppCore. A time-jump notification can arrive after the
  player starts fetching the new position; local invalidation or a queued
  Close does not establish pre-request ordering.
- The server cannot distinguish a far-future prefetch from a user seek that
  produces the same media request while the old timing context is still valid.
  Missing/expired-context fallback alone does not cover this live-stale case.
  The approved seek-loading decision below changes the latency policy, not
  these measured native-control limits or media correctness requirements.
- PR #106 delivered the isolated diagnostic. Its acceptance did not enable
  production request demotion; the next two slices are defined below.

## Seek-Loading Decision
- Joey explicitly accepts temporary loading after a user/native timeline seek.
  An all-route asynchronous pre-seek barrier is therefore not a prerequisite
  for timing-based quality-of-service scheduling. No such barrier is claimed.
- Normal playback ordering remains missing near-term video/audio, eligible
  high preparation, far-future low reads, then eventual full fills. High work
  requires a safe continuous buffer and fresh representation-bound video plus
  required-audio origin supply. Insufficient supply continues low playback.
- An observed jump retires the prior local timing/preparation plan. New target
  video/audio refilling takes priority until a safe buffer is restored. Native
  seeks themselves do not wait for a remote Close acknowledgment.
- A request can briefly retain its old classification before the observation
  arrives. Bound that delay with request-age promotion and conservative
  fallback, not an unbounded dependence on notification delivery. Missing,
  expired or ambiguous reader/session/generation/range evidence remains
  continuity-first. Existing identity, cancellation and content checks remain.
- Keep withdrawal acknowledgment truthful: a timeout/unknown outcome is not
  remote removal. Late open/update results cannot restore retired authority.
- This concession applies to user seeks, not automatic upshifts. Retain native
  controls, one player/item and default buffering; do not manufacture seeks or
  discard buffered low content to make an upshift test pass.
- Slice 1 wires production context reporting, priority classification and
  bounded seek recovery. Slice 2 starts eligible high preparation and measures
  high-low-high behavior and continuity against a prewarm-disabled baseline.
  Merge each fully tested/current-head-reviewed PR before branching the next.

## Native-Reuse Decision
- Retain native controls where possible. First build a small isolated bridge
  prototype and measure input, native navigation callbacks, time-jump observation
  and actual HLS requests. Do not replace the production control strip first.
- A fixed-signed macOS diagnostic uses native AVPlayerView; an explicit tvOS
  simulator diagnostic uses native AVPlayerViewController. Both use one muted
  player/item, default buffering, and a bounded synthetic loopback fixture.
- An optional key-gating experiment uses a deliberately delayed simulated
  withdrawal acknowledgment. It is not real gRPC Close, does not establish all
  native input routes, and must not grant production scheduler authority.
- Keep mouse scrubbing, keyboard navigation, accessibility and system transport
  coverage separate. A successful representative key case does not close the
  remaining native-control causality gap. The later seek-loading decision uses
  bounded recovery instead of claiming complete input interception.

## Next Steps
1. Integrate high preparation after the reporting and request-urgency slice.
   Keep the diagnostic's unverified input-route coverage explicit. Reuse exact-ID withdrawal
   for context retirement without blocking native seeks or pretending a late
   callback is a pre-seek barrier. Preserve conservative fallback for evidence
   that cannot support safe optional work.
2. Consume validated SIDX ranges, exact reader/session/generation, fresh safe
   continuous buffers and fresh representation-bound video plus required-audio
   supply. Revalidate before admission and each new origin transfer; preserve
   same-flight foreground promotion and immediate priority cancellation.
3. Require complete local CI, fixed-signed native3 with verified mute, GHA CI/UI,
   independent released artifact checks, current-head GitHub Codex review and
   all resolved conversations before normal SHA-bound protected merge. Do not
   alter helpers, deadlines or assertions to make the candidate pass.
4. Complete PR #98's separate bounded OFF/ON comparison after production wiring;
   a failed preparation is not a measured negative latency result.

## Production Slice Coverage
- Shared-player tests exercise negotiated reader URLs, original source identity,
  catalog-capability fallback, native time-jump context rotation, repeated state
  observations, pause/buffering/end, replacement, weak ownership and unconfirmed
  withdrawal. Prepared-resumption fixtures retain the actual server metadata;
  identity and race checks remain strict.
- Range tests cover rate-scaled safety windows, incomplete buffers, unknown
  ranges, unaligned SIDX time origins, stale or ambiguous bindings, same-size
  content-prefix replacement and no-progress promotion across queue polling.
- Real loopback HTTP tests register the map through the production playlist
  path, check exact response bytes and origin Range traces, and exercise actual
  preparation-batch contention. Preparation publishes a verified chunk before
  its next transfer is gated; uncached near video and unmapped audio retain
  foreground access, while a far request eventually ages into service without
  pretending the still-active preparation has completed.
- These controlled model and HTTP integration tests do not by themselves prove
  full macOS AppCore-to-server engagement or faster automatic quality recovery.
  Fixed-signed native media regression acceptance, production high preparation
  and the separate OFF/ON measurement retain their distinct evidence boundaries.

## Public API Boundary
- The Xcode 26.5 macOS `AVPlayerViewDelegate` contract has fullscreen and
  restoration callbacks, but no equivalent pre-seek decision callback. The
  tvOS `timeToSeekAfterUserNavigatedFrom:to:` delegate returns a time
  synchronously and covers documented user navigation, not an asynchronous
  all-route withdrawal acknowledgment.
- Public seek completion handlers, time-jump notifications and remote-command
  registration do not document a guarantee that all native media requests wait
  for application approval. An overridable `AVPlayer` seek method alone does
  not establish that every AVKit or accessibility path calls that override.
- These are documented-contract limits, not proof that every possible bridge
  is impossible. Keep AX, mouse, system transport and keyboard coverage separate.
  Do not add private hooks or replace the production native controls as part of
  this diagnostic slice.
- AppCore observes `.AVPlayerItemTimeJumped` through Combine before scheduling
  MainActor handling. The probe's explicit main-queue notification observer is a
  different observation point; its measured timing is not AppCore sink timing.
- References: Xcode 26.5 `AVPlayerView.h`, `AVPlayerViewController.h`,
  `AVPlayer.h`, and `AVPlayerItem.h`; [Apple navigation delegate contract](https://developer.apple.com/documentation/avkit/avplayerviewcontrollerdelegate).

## Prototype Checks
- Full Python helper discovery in the implementation worktree: 257 tests passed,
  including 39 focused local-build tests covering missing-product recovery.
  This explicit-worktree run supersedes earlier helper counts. Earlier
  restricted-environment process-inspection failures are not counted as passes.
- tvOS diagnostic: Swift formatting, normal build, focused build-for-testing and
  eight unit tests passed on the tvOS 26.5 simulator. The filename fix covers
  bounded plain-ASCII components, including valid 64-character run IDs.
- The macOS diagnostic is a separately pinned signed app bundle under
  `Diagnostics/NativeNavigation/NativeNavigationProbe.app` in the configured
  local-build root. The contained executable, bundle signature, diagnostic
  receipt and self-test passed; the shared legacy receipt remained unchanged.
  The prior 340,800-byte diagnostic bare binary is retained, not used as a
  fallback. `prepare --print-path` returns the bundle; `run` uses its executable.
- Full CI, remote PR review and production native media acceptance are separate
  gates. No production scheduler authority follows from these results.
- The preceding Python/tvOS results cover implementation checkpoint `26a3b7a`.
  The subsequent macOS-only native-notification observer passed Swift 6 strict
  concurrency, warnings-as-errors typecheck and strict Swift formatting. It
  records `native_time_jump` separately from periodic `time_jump`; callback entry
  and the pre-actor-hop position read have separate raw clock markers. No
  production source, buffering or transport schema changed.
- Fixture pacing now schedules bounded shared send slots before socket writes
  instead of accumulating credit from all elapsed idle time. Actual written
  bytes are accounted separately from pacing reservations. Final event/byte
  totals and late log-budget failures are observed after request handlers drain.

## Prototype Commands
- Start `python3 scripts/native-navigation-fixture.py --output-root
  .codex-tmp/native-navigation --run-id navigation-01 --max-media-bytes 33554432
  --max-media-bytes-per-second 131072`. Read its ready file for the loopback URL.
  Interrupt the owned fixture after the experiment; its report must confirm
  media cleanup. Defaults are intentionally slower than the encoded stream.
- Run the fixed-signed diagnostic with `python3 scripts/local_build.py run
  native-navigation-probe -- --url <loopback-url> --events-output
  <new-absolute-path.jsonl> --run-id navigation-01 --duration 120`. Optional
  `--mode keyboard-gate --barrier-delay-ms 500` applies only to candidate
  window keyDown events, not all native inputs.
- A separately built tvOS simulator app accepts `--native-navigation-probe
  --native-navigation-url <loopback-url> --native-navigation-run-id navigation-01`.
  Its bounded trace is under `Library/Caches/NativeNavigationProbe` in the
  diagnostic app's simulator data container. Non-simulator diagnostic launches
  and malformed diagnostic arguments fail visibly without production models.
- Use Computer Use for native UI actions, not programmatic seek or synthesized
  shell input. Verify mute first. Compare same-host raw monotonic clocks;
  fixture observations are request receipt, not client request issue timestamps.

## Native UI Findings
- A credential-free tvOS 26.5 simulator run exercised native pause, forward,
  reverse and resume through Computer Use. The retained trace contains six
  delegate input events and three time-jump observations. Seek pairs were
  71.812 -> 116.382, 140.983 -> 136.860, and 143.980 -> 94.352 seconds.
- Delegate entry preceded main-queue time-jump delivery by approximately 59,
  37 and 34 ms in these cases. These are callback observations, not hardware
  input timestamps. The macOS probe separately labels its periodic position
  jump observations; neither should be mistaken for a pre-request barrier.
- The far-position precheck had already seen segment index 254 (about 508 s),
  including target index 240 (about 480 s). This run therefore establishes
  native-control smoke coverage only, not unbuffered-seek request ordering.
  Previously prefetched requests are not causal seek counterexamples.
- Normal tvOS fixture shutdown reported successful media cleanup. An earlier
  macOS bare-executable attempt was not controllable through Computer Use;
  after forced shutdown it lacked a cleanup report, and its owned media was
  removed separately after process quiescence. It is not a passing UI case.
- An initial bundled macOS attempt built and started, but Computer Use spent
  approximately 71 minutes in host lock detection before reporting that manual
  unlock was required. That attempt has no UI acceptance and is not a successful
  long playback test. The unlocked runs below are separate cases.
- After manual unlock, `mac-observe-03` exercised the native timeline through
  Computer Use at 300 and 480 seconds. Prechecks had not seen the target segment
  indices 150 and 240, respectively; both positions rendered and resumed. The
  window closed normally after approximately 233 seconds. These are specific
  native timeline/AX paths, not proof that every input is intercepted.
- Coordinate drag attempts did not establish successful physical timeline
  dragging. Floating controls were absent from the captured image; OCR entries
  also appeared on keyboard pause and do not establish which region was hit.
  Keep physical mouse dragging explicitly unverified.
- In `mac-gate-01`, Right and Left each obtained their matching simulated fixture
  acknowledgment and were released after approximately 515 and 509 ms. No
  `native_player_view` callback was recorded, so native-handler entry and full
  keyUp/repeat semantics remain unproven. This is not real gRPC Close.
- An AX timeline change to an unrequested 480-second position in keyboard-gate
  mode rendered and resumed without increasing the two keyboard gate requests.
  That entry is outside the measured key-only gate; retain continuity protection
  for unknown/native bypass routes.
- Periodic-position versus HTTP receipt ordering from those first two runs does
  not establish actual AVPlayerItem notification timing. The subsequent
  `mac-native-notification-01` case uses the distinct real-notification observer.
  Before its AX seek to 480 seconds, the precheck had seen only segment index 37
  and had not seen target index 240; the target frame rendered and resumed.
- In that single synthetic loopback case, the target HTTP request receipt
  preceded the probe's main-queue native-notification callback entry by
  16.427 ms, and its periodic position sample by 78.596 ms. These are separate
  measured observation points. They are not native emission timestamps,
  client request-issue timestamps or an all-case ordering guarantee.
- Production AppCore currently uses a Combine notification publisher followed by
  a MainActor task. The diagnostic's explicit main-queue observer is not the
  identical Combine sink or AppCore-handler entry point. No production handler
  timing claim or permission to demote ambiguous requests follows from this test.
- Those historical GUI cases used the earlier elapsed-time limiter, which could
  accumulate idle credit. Their target-not-yet-requested prechecks and measured
  callback clocks remain specific observations, not constrained-bandwidth
  acceptance. The corrected fixture does not retroactively establish missing
  mouse, key-handler or system-transport coverage.

## Evidence
- [Native navigation diagnostics PR #106](https://github.com/Joey-Project/tvOS-net-player/pull/106).
- [Freshness and withdrawal PR #102](https://github.com/Joey-Project/tvOS-net-player/pull/102).
- [Playback context foundation](2026-11-12-playback-urgency-context-b46407.md).
- [Stable reader binding](2026-11-12-hls-reader-binding-b46408.md).
- `Sources/TVOSNetPlayerCore/AppCore/PlayerViewModel.swift`: managed seek and
  asynchronous time-jump observation.
- `TVOSNetPlayer/ContentView.swift`: native `VideoPlayer` surface.
- `MacOSNetPlayer/ContentView.swift`: native `AVPlayerView` surface.
- Source thread: `019e7fd0-b463-76f2-a5ab-37d7d77620f4`.
