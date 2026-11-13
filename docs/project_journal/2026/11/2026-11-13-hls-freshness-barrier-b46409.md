---
id: 20261113-b46409
title: Sleep-Aware HLS Freshness and Withdrawal
status: completed
created: 2026-11-13
updated: 2026-11-13
branch: codex/hls-freshness-barrier
pr: https://github.com/Joey-Project/tvOS-net-player/pull/102
supersedes: []
superseded_by:
---

# Sleep-Aware HLS Freshness and Withdrawal

## Decision
- Continue from stable reader binding PR #101 on updated master. Before a
  scheduler consumes timing, make context, snapshot and recent-origin evidence
  age conservatively across system sleep. Origin transfer measurements that
  span sleep must not become optimistic new bandwidth evidence after wake.
- Keep the same clock domain for snapshot capture and AppCore freshness checks.
  Missing, backward, unavailable or otherwise invalid readings cannot authorize
  speculative priority changes.
- Invalidate fresh-origin confidence for the current in-memory history instance
  after a detected clock fault. The fault latch and fresh measurements are not
  persisted; reinitialization requires new measurements, not old ranking data.
  This conservative clock-fault policy is separate from network-failure recovery
  and does not disable ordinary media delivery or historical CDN ranking.
- Distinguish local withdrawal, queued cleanup, server-acknowledged removal and
  proved expiration. A queued Close or bounded wait timeout is not a remote
  acknowledgment. Cover in-flight Open/Update and delayed cleanup completions,
  retaining bounded cleanup concurrency and exact ownership.
- Expose a bounded explicit outcome: no remote operation required when that
  absence is proved, acknowledged only after a valid exact-ID Close response,
  or unconfirmed with a reason. A lost Open response is not proof of absence.
  Do not fabricate a TTL fallback or let an old completion confirm a new owner.
- A future consumer must establish its remote operation barrier before allowing
  new-playhead media requests, or independently prove a conservative server
  request rule. Local revision changes alone cannot make a seek causally safe.
  This slice does not change native seek/pause behavior.
- Preserve one player/item, default buffering, full-fill intent, media identity
  validation and existing compatibility. This prerequisite slice does not yet
  enable automatic reporting, far-low demotion or quality switching.

## Scope
- Rust server context freshness and fresh-origin measurement/age contracts.
- Shared Swift AppCore clock coherence and explicit withdrawal outcome.
- Focused deterministic tests with injected clocks and controlled RPC ordering;
  no actual system suspend or authenticated Bilibili test is required.

## Validation
- Cover sleep-spanning/expired/backward readings, stale evidence, independent
  readers/generations, cancellation and in-flight retirement races.
- Retain lost-Open uncertainty after legacy withdrawal. A known retired Update
  must not discard a newer context's cleanup identity; an older Close must not
  acknowledge a newer unresolved context. Cover both completion orderings.
- Require complete format/lint/local CI, fixed-signed native3 with verified
  system mute, current-head GitHub CI/UI and GitHub Codex review, plus every
  conversation resolved before normal SHA-bound protected squash merge.
- Keep strict automatic ABR and PR #98's separate complete OFF/ON comparison,
  recovery-owned deadline, transfer budgets and readability checks unchanged.
  Failed preparation is not a valid negative latency result.

## Next Steps
- Consume validated indexes and reader ownership in a later scheduler slice:
  missing near-low/audio first; eligible high before far-low only with a safe
  continuous buffer and fresh adequate origin supply. Inadequate origin keeps
  low downloading and holds new high until reevaluation on recovery.
- Complete PR #98's independent OFF/ON acceptance after production wiring;
  neither prerequisite foundation claims faster upshift.

## Evidence
- [Sleep-aware freshness and withdrawal PR #102](https://github.com/Joey-Project/tvOS-net-player/pull/102).
- [Stable reader binding PR #101](https://github.com/Joey-Project/tvOS-net-player/pull/101).
- [Playback context PR #100](https://github.com/Joey-Project/tvOS-net-player/pull/100).
- [Reader binding decisions](2026-11-12-hls-reader-binding-b46408.md).
- Source thread: `019e7fd0-b463-76f2-a5ab-37d7d77620f4`.
