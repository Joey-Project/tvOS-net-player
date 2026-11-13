---
id: 20261113-b46412
title: HLS Playback Urgency Scheduler
status: active
created: 2026-11-13
updated: 2026-11-13
branch: codex/native-navigation-bridge-prototype
pr:
supersedes: []
superseded_by:
---

# HLS Playback Urgency Scheduler

## Summary
- Continue from accepted master `e43562673e131ac3b2dcf7549443cb2aa91b31d6`
  after PR #102. Preserve one player/item, default buffering, full-fill intent,
  media identity validation and the existing strict acceptance budgets.
- Approved ordering is missing near-term video/audio first, eligible high
  preparation before far-future low, and eventual complete fills. Insufficient
  or stale origin evidence holds new high work while low supply continues.

## Current State
- Reader binding, sleep-inclusive freshness and explicit withdrawal outcomes
  are available. The automatic reporter and priority consumer are not enabled.
- The current tvOS `VideoPlayer` and macOS `AVPlayerView` expose native seek
  controls outside AppCore. A time-jump notification can arrive after the
  player starts fetching the new position; local invalidation or a queued
  Close does not establish pre-request ordering.
- The server cannot distinguish a far-future prefetch from a user seek that
  produces the same media request while the old timing context is still valid.
  Missing/expired-context fallback alone does not cover this live-stale case.
  Ambiguous HTTP requests must retain continuity protection.
- Production playback, scheduling and wire schemas remain unchanged. The first
  slice is an isolated native-control diagnostic, not permission to demote.

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
  remaining native-control causality gap. Unknown routes remain protected.

## Next Steps
1. Finish physical mouse-drag and external/system transport coverage, and close
   the missing native-handler-entry observation. Keep broad PR/merge readiness
   separate; a working key-release experiment is not an all-route seek fence.
2. Use measured native-control boundaries to choose the smallest reliable bridge
   or a narrowly scoped custom control. Integrate real exact-ID Close only after
   the input boundary is sound. Do not claim a late callback is a pre-seek barrier.
3. Consume validated SIDX ranges, exact reader/session/generation, fresh safe
   continuous buffers and fresh representation-bound video plus required-audio
   supply. Revalidate before admission and each new origin transfer; preserve
   same-flight foreground promotion and immediate priority cancellation.
4. Require complete local CI, fixed-signed native3 with verified mute, GHA CI/UI,
   independent released artifact checks, current-head GitHub Codex review and
   all resolved conversations before normal SHA-bound protected merge. Do not
   alter helpers, deadlines or assertions to make the candidate pass.
5. Complete PR #98's separate bounded OFF/ON comparison after production wiring;
   a failed preparation is not a measured negative latency result.

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

## Evidence
- [Freshness and withdrawal PR #102](https://github.com/Joey-Project/tvOS-net-player/pull/102).
- [Playback context foundation](2026-11-12-playback-urgency-context-b46407.md).
- [Stable reader binding](2026-11-12-hls-reader-binding-b46408.md).
- `Sources/TVOSNetPlayerCore/AppCore/PlayerViewModel.swift`: managed seek and
  asynchronous time-jump observation.
- `TVOSNetPlayer/ContentView.swift`: native `VideoPlayer` surface.
- `MacOSNetPlayer/ContentView.swift`: native `AVPlayerView` surface.
- Source thread: `019e7fd0-b463-76f2-a5ab-37d7d77620f4`.
