---
id: 20260805-c7a4e2
title: Transcoding And ABR Policy Controls
status: completed
created: 2026-08-05
updated: 2026-08-05
branch: wip/transcoding-abr-policy-controls
pr:
supersedes: []
superseded_by:
---

# Transcoding And ABR Policy Controls

## Summary

- PR 7 adds per-playback policy controls for LAN transcoding, compatible variant selection, and server-assisted weak-network ABR behavior.
- The Rust LAN cache server remains the only Bilibili integration point. tvOS and macOS continue to receive only server-owned HLS URLs.
- The protobuf change is additive within v1 and does not change the deferred Bilibili task result/artifact schema.

## Decisions

- Server startup configuration owns hard execution bounds: ffmpeg availability, whether LAN transcoding is allowed, and its concurrency limit. A playback request cannot override those bounds.
- Each progressive playback request carries a policy with three independent choices:
  - transcoding: automatic when needed, never, or force;
  - variant selection: prefer the conservative compatible profile or preserve requested/default ranking;
  - weak network: adaptive downgrade and recovery, hold a downgrade until the session ends, or leave ABR management to AVPlayer.
- Unspecified wire values normalize to automatic transcoding, compatible-first selection, and adaptive weak-network recovery. The playback session reports normalized effective values.
- Explicit codec input retains precedence over compatible-first selection. The compatible-first path, HLS alternate filtering, and transcoding planning share one conservative H.264/AAC HLS compatibility predicate.
- Requested options remain part of persisted task state, and effective policy remains part of persisted HLS session state. Legacy snapshots and manifests recover through safe defaults without a schema-version bump.
- Non-default client policies require the new playback-policy server capability so an older server cannot silently ignore an explicit user choice.

## Client UX

- Shared AppCore owns the policy selection state and persists it through injected `UserDefaults`.
- tvOS and macOS expose the same three playback-only Picker controls. Complete-download options remain unchanged.
- Existing cache diagnostics continue to show runtime transcoding and weak-network state; current task metadata exposes the effective policy and transcoding decision.
- HLS weak-network observations are generation-scoped and serialized with session removal, so a stale recorder cannot mutate a replacement session that reuses the same ID.
- Playback-only policy changes preserve an already resolved multi-candidate selection and are applied when the task is created without repeating page resolution.

## Non-Goals

- Bilibili task result/artifact schema v2.
- Mutable server settings RPCs or remote changes to ffmpeg/concurrency configuration.
- Expensive content-aware quality heuristics or broad automatic transcoding ladders.
- Physical Apple TV validation or control-plane TLS/authentication hardening.

## Next Steps

- Pause before deferred/non-sequential PR 6 and discuss the Bilibili task options/result schema v2.
- Track richer quality heuristics only after real macOS usage identifies a concrete need.

## Evidence

- Roadmap: `docs/project_journal/2026/06/2026-06-24-next-phase-productization-roadmap-a8d2c5.md`
- Cache-server architecture: `docs/architecture/cache-server.md`
- `just lint`: passed after the final Rust media-handler refactor and policy tests.
- `scripts/test.sh`: 268 Swift package tests passed with no failures, including policy changes after multi-candidate resolution.
- `scripts/test-cache-server.sh`: 498 Rust unit tests, 34 default live-e2e support tests, and 6 integration tests passed; the opt-in real-network live test remained ignored by default. The suite includes stale-generation and session-removal serialization regressions.
- `just build-cache-server`: the optimized Rust LAN cache server build passed.
- `just build`: the generic tvOS Simulator app build passed.
- `just build-for-testing`: the generic tvOS Simulator test bundle build passed.
- `just build-macos`: the macOS app build passed for the generic macOS destination.
- `just test-macos`: the macOS app-shell integration test passed.
- `just ci`: lint and all build prerequisites completed through the tvOS test bundle build. The run was stopped during `scripts/test-tvos-simulator.sh` because `simctl list devices available` blocked in `xcodebuild -runFirstLaunch`; the host has CoreSimulator `1051.54.0`, while Xcode 26.6 requires `1051.55.0`.
- `git diff --check`: passed.
