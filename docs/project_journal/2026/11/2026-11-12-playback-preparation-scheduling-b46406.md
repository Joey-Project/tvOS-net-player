---
id: 20261112-b46406
title: Playback Preparation Scheduling
status: completed
created: 2026-11-12
updated: 2026-11-12
branch: codex/playback-prewarm-scheduling
pr: https://github.com/Joey-Project/tvOS-net-player/pull/99
supersedes: []
superseded_by:
---

# Playback Preparation Scheduling

## Decision
- Split an internal cache-scheduling prerequisite from the opt-in [single-player comparison PR #98](https://github.com/Joey-Project/tvOS-net-player/pull/98). Start from master without importing that PR's diagnostics or treating its failed preparation as a valid negative result.
- Order supply work as current foreground playback, bounded preparation for that playback, then whole-asset and demoted old-task fills. Preparation may borrow spare transfer capacity during ongoing foreground activity; global foreground quiescence is not a prerequisite.
- Limit newly admitted preparation to one origin transfer per cache store. With at least two configured transfer slots, preserve at least one foreground-owned or free slot and keep preparation out of the foreground semaphore queue. Unrelated preparation must yield when foreground admission is waiting. At parallelism one, allow idle preparation but yield to unrelated foreground demand. Existing shared flights retain their content ownership; only foreground interest protects a background-owned flight from foreground pressure.
- A reserved concurrency slot is not reserved network bandwidth or a promise of zero delay. Origin contention, CPU/disk work and actual player continuity still require measurement; a production controller's supply estimate and safety policy remain separate work.
- Keep preparation internal, session-bound, cancellable and finite in time, logical bytes, expanded transfer spans and attempts. Preserve the fifteen-second absolute deadline, eight-MiB logical selection, sixteen-MiB cumulative attempted-span budget and 128-attempt ceiling used by the diagnostic. These are cache span-accounting bounds, not a claim about all wire bytes used for identity/CDN verification.
- Reuse existing content identity, ETag/cross-origin sampling, quota reservations, atomic commit, truncation, removal, drain and flight-coalescing guarantees. Preparation must not use an unbounded Full GET fallback.
- Keep Background whole-asset work lowest and retain the existing new-playback preemption/demoted fairness policy. Do not change the task worker, public gRPC/config/persistence schema, BBDown dependency, HLS media, client buffer policy or number of players.

## Validation
- Prove preparation progresses with sustained unrelated foreground activity when capacity is spare, and prove current foreground admission retains capacity under preparation pressure.
- Cover concurrent preparation admission, exact-flight promotion, parallelism one, cancellation/drop, background preemption/recovery, removal/drain and preserved content/quota failures with deterministic unit and real loopback-HTTP tests.
- Run complete repository checks and the unchanged three-case native media gate with fixed signed executables and verified system mute. Keep current-head GitHub CI/UI/native artifacts, Codex review and all conversation resolution as separate delivery gates.
- This foundation does not prove faster decoded upshift or visible/audible continuity. Re-run a fresh bounded OFF/ON comparison only after integrating the prerequisite into #98; do not reuse old-head positive evidence or relax the existing long high-low-high case.

## Current State
- The internal `prepare_playback_batch` entrypoint accepts a single session and borrowed resource/range requests under one deadline and shared logical/expanded-span/attempt budgets. Deadline, limit and ordinary range failures are distinct; no public control-plane contract changes.
- Preparation uses existing startup validation and fine-grained range fetching, not an unbounded Full GET. Non-FIFO admission tracks foreground waiting, preparation transfers and shared-flight interest with cancellation-safe guards. Foreground admission still checks control/session state before its free-permit fast path.
- Deterministic scheduling and loopback HTTP regression cases cover the internal path. Exact-head execution evidence belongs to the delivery PR and is not implied by the presence of test code. No automatic production preparation controller is enabled by this slice.

## Next Steps
1. Integrate the bounded caller into #98 without rewriting either PR's history, retain its failed comparison evidence, and run a fresh paired long comparison.
2. Evaluate fresh paired decoded-frame evidence before designing automatic preparation/retention or changing media-delivery policy.

## Evidence
- [Playback preparation prerequisite PR #99](https://github.com/Joey-Project/tvOS-net-player/pull/99).
- [Single-player comparison PR #98](https://github.com/Joey-Project/tvOS-net-player/pull/98), failed preparation at reviewed candidate `b0211ec0e58832613130f8a083b25806ae5feb8a`.
- [Recovery-latency workstream](2026-11-10-native-abr-latency-b46399.md).
- Source thread: `019e7fd0-b463-76f2-a5ab-37d7d77620f4`.
