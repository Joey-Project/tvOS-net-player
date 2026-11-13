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
1. After manual host unlock, complete the macOS mouse/keyboard experiments and
   unbuffered-seek ordering coverage. Keep broad PR/merge readiness separate;
   do not silently promote native-control smoke coverage to a scheduling gate.
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
- The subsequent bundled macOS app built and started, but Computer Use spent
  approximately 71 minutes in host lock detection before reporting that manual
  unlock was required. No mouse, unbuffered-seek or keyboard-gate UI acceptance
  is claimed. Do not retry locked UI access or interpret the stalled call as a
  successful long playback test; keep the remaining work explicitly pending.

## Evidence
- [Freshness and withdrawal PR #102](https://github.com/Joey-Project/tvOS-net-player/pull/102).
- [Playback context foundation](2026-11-12-playback-urgency-context-b46407.md).
- [Stable reader binding](2026-11-12-hls-reader-binding-b46408.md).
- `Sources/TVOSNetPlayerCore/AppCore/PlayerViewModel.swift`: managed seek and
  asynchronous time-jump observation.
- `TVOSNetPlayer/ContentView.swift`: native `VideoPlayer` surface.
- `MacOSNetPlayer/ContentView.swift`: native `AVPlayerView` surface.
- Source thread: `019e7fd0-b463-76f2-a5ab-37d7d77620f4`.
