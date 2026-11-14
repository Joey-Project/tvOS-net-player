---
id: 20261113-b46412
title: HLS Playback Urgency Scheduler
status: active
created: 2026-11-13
updated: 2026-11-14
branch: codex/hls-high-preparation
pr:
supersedes: []
superseded_by:
---

# HLS Playback Urgency Scheduler

## Summary
- Continue from accepted master `cbcd297b5f6b9950d957e258901f3b4f147b0808`
  after PR #107. Preserve one player/item, default buffering, full-fill intent,
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
  yield again after successful progress. PR #107 delivered this foundation;
  accepted context updates now invoke the bounded high-preparation coordinator
  through the separately specified admission path below. Readiness and catalog
  queries remain read-only. Content identity and cancellation checks are unchanged.
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
1. Broaden controlled source/buffer conditions and use eligible OFF/ON results
   to guide further recovery-latency work. PR #98's earlier failed preparation
   is not a negative latency result for the current reader-bound path. Missing
   supply and ineligible preparation remain explicit, not successful comparisons.
2. Validate the complete shared AppCore-to-service reporting loop separately
   from the isolated harness's in-process generated service calls. Keep native
   controls, unverified input-route coverage and the lack of an all-route
   pre-seek barrier explicit; preserve exact-ID retirement and conservative fallback.
3. Continue visible-upshift optimization only from measured evidence. Retain
   one player/item, default buffering and useful low-quality buffers; do not
   infer faster decoding from source requests or prepared cache bytes alone.
4. Require complete local CI, fixed-signed native3 with verified mute, GHA CI/UI,
   independent released artifact checks, current-head GitHub Codex review and
   all resolved conversations for follow-up changes before normal SHA-bound
   protected merge. Do not alter accepted helpers, deadlines or assertions to
   make a candidate pass.

## High Preparation Slice Design
- Continue from PR #107's production reporting and bounded request urgency.
  Accepted context updates are the production trigger; catalog and readiness
  queries remain read-only. No new control-plane or persistence schema is needed.
- Target only the explicitly selected compatible primary variant under the
  effective playback policy. The reader context does not identify the currently
  decoded rung; media GETs must not be presented as decoded-quality evidence.
- Require an exact live reader/context/session generation, safe continuous
  buffer, validated per-resource SIDX maps and fresh target-video plus required
  audio origin supply. Missing or insufficient evidence defers optional work;
  there is no implicit unbounded probe to manufacture fresh capacity evidence.
- Optional preparation uses 1.5-times nominal rate-scaled supply headroom,
  distinct from the request classifier's minimum continuity-supply threshold.
  Honor the actual master advertisement and effective network policy rather
  than enabling only a nondefault client preference or bypassing degradation.
- Normal HTTP 206 supply observations capture suspend-inclusive request start,
  first delivered body byte and body completion. Successful evidence is
  published only after the existing content/control/durable-write checks;
  publication queueing does not refresh the captured completion time. A
  completed range transfer is not whole-resource cache completion. Discovery,
  cache hits, truncation and cancellation do not mint fresh successful supply.
- Protect 20 wall-clock seconds scaled by the actual playback rate. Select a
  separate 20-media-second logical preparation window after the greater of that
  boundary and the measured continuous buffered end. Keep existing low buffers.
- Select whole overlapping fragments independently for video and audio so they
  cover the same logical interval. Their SIDX boundaries need not coincide;
  this fills cache bytes and does not perform a client seek or a stream handoff.
- Keep the existing 15-second, 8-MiB logical, 16-MiB expanded/attempted and
  128-request batch bounds. Bound single-flight state and task count, rate-limit
  retries, and revalidate live ownership and supply before new origin transfers.
  Closing, expiration, seeking, session replacement or loss of a safe buffer
  withdraws optional work without delaying native navigation.
- The new opt-in native comparison must use challenge-followed actual player
  timing, real reader propagation and the production coordinator. OFF disables
  only the new coordinator, not the reporting or request-urgency baseline. Each
  mode gets a fresh player/cache/fixture; failed preparation is inconclusive.
- The isolated harness may invoke the actual generated context-service methods
  in process. This exercises their validation and production post-Update hook,
  not external gRPC transport or the full shared AppCore reporting loop. Keep
  those boundaries explicit; do not describe this as full macOS GUI acceptance.
- Use identical existing origin-failure stimulus in both arms, with no direct
  network primer. If a real native high request first restores fresh supply,
  later preparation cannot explain that earlier request. Recovery-phase newly
  readable preparation is needed for a valid recovery comparison; initial-only
  or ineligible work does not produce a latency conclusion.
- Report prepared/readable high bytes, first high player request and decoded
  high frames separately. Preserve the original strict native3 regression
  cases and accepted producer helpers. No quality forcing, buffer cap, seek,
  second player or media-identity substitution may create the measured upshift.

## Production Slice Coverage
- Shared-player tests exercise negotiated reader URLs, original source identity,
  catalog-capability fallback, native time-jump context rotation, repeated state
  observations, pause/buffering/end, replacement, weak ownership and unconfirmed
  withdrawal. Prepared-resumption fixtures retain the actual server metadata;
  identity and race checks remain strict.
- Range tests cover rate-scaled safety windows, incomplete buffers, unknown
  ranges, unaligned SIDX time origins, stale or ambiguous bindings, same-size
  content-prefix replacement and no-progress promotion across queue polling.
- Concurrent foreground resolver waiters keep a persistent semaphore queue
  position while they remain foreground, and revalidate priority and control
  after admission. Cancellation or demotion withdraws the pending acquisition.
  Regressions exhaust all permits before queuing multiple readers, then verify
  progress, cancellation cleanup and foreground precedence after demotion.
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

## Native Comparison Contract
- `just test-media-high-preparation` is an opt-in, credential-free comparison
  with fresh sequential OFF and ON fixture/cache/player instances. The existing
  capacity/failure stimulus and 600-second automatic phase are identical; OFF
  disables only the preparation coordinator, not context or reader scheduling.
- Real generated service methods run in process and consume a new native
  challenge-followed timing sample for every update. This does not establish
  external gRPC transport, the complete AppCore reporter loop, GUI interaction,
  audible audio, or the absence of visible black frames.
- Reader-bound playlist preflight preserves the legacy canonical validation.
  It separately checks every selected playlist URI and range against that
  canonical content, allowing only the exact expected reader association.
  Validation-only normalization never rewrites the served or player URL;
  missing, duplicated, mismatched or unrelated query fields remain failures.
- Eligibility requires a completed recovery-phase preparation, matching
  reader/context/session generation and target window, actual missing-to-readable
  cache transitions, completed source-range overlap and known player-request
  overlap. Initial-only, already-readable, failed, stale or ambiguous cases are
  inconclusive, not a successful comparison or evidence of slower preparation.
- Capture the original before-state when a matching plan is admitted, even if
  its spawned task has not recorded an origin request yet. Keep that snapshot
  as the plan progresses and the native buffered end advances; admission alone
  never satisfies completion, readability or transfer-overlap eligibility.
- Retain a bounded, sanitized category for the first failed control update.
  Diagnostic snapshots taken afterward are not the original failing sample;
  unknown metadata stays unknown. Diagnostics never make a rejected update
  acceptable or replace the required complete OFF/ON comparison.
- A structured cache read returning missing does not prove zero physical byte
  coverage. A stable exact range changing from missing to fully readable proves
  a positive lower bound on newly readable bytes, not an exact downloaded-byte
  delta. Foreground requests may share the same range flight; exclusive
  attribution remains unproven. Logical resource counts are not origin attempts.
- Retain first recovered-high request, completed source body, prepared window
  and decoded-high observations separately. Preparation after an already-issued
  high request cannot explain that request. No faster-selection claim follows
  solely from a completed plan or cache-readability change.
- Keep pinned signing, stable executables, verified system/player mute, bounded
  output and deadlines. Standard native3 and accepted producer helpers remain
  unchanged; the new comparison does not replace those regression gates.
- Continuous-mode verification captures actual player mute and automatic-waiting
  properties before cleanup, then serializes that snapshot. The signed probe
  self-test exercises capture, release and serialization; a missing capture
  cannot report either property as verified. The comparison preflight requires
  this lifecycle regression before starting native media work.
- Control publication uses a fully written, current-owner regular file with
  exact `0600` permissions before atomic replacement in the private directory.
  The publisher self-test covers both `022` and `077` umasks, and comparison
  preflight requires it. The strict reader contract remains unchanged.
- The opt-in comparison entry is outside the legacy native regression module's
  test-name prefix. Discovery preflight checks the exact opt-in selector and
  unchanged three-case legacy ignored-test selection before playback. Missing
  opt-in configuration still fails; ordinary regression runs never select it.
- Each opt-in arm allows 240 seconds for its FFmpeg fixture-generation child,
  as explicitly approved after a 120-second pre-playback fixture timeout.
  Both arms use the same limit and report it. Legacy long-fixture generation
  stays at 120 seconds; inspection, playback, output and overall budgets remain
  unchanged. A fixture timeout is not an ABR measurement.
- macOS UI terminal-seek setup anchors at the head and navigates while paused.
  It does not depend on a play/pause round trip finishing inside a ten-second
  media tail at 2x speed. Natural completion, paused terminal clamping, Resume
  and speed assertions remain required; production player behavior is unchanged.
- The nested tvOS task-result scroll viewport has an explicit height so it
  cannot collapse under its vertically scrolling parent. Relaunch validation
  checks viewport geometry before normal remote navigation; explicit result
  selection and Continue assertions remain unchanged.

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
