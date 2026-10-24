---
id: 20261024-b46393
title: Foreground Range Cache Foundation
status: completed
created: 2026-10-24
updated: 2026-10-24
branch: codex/cache-foreground-ranges
pr: https://github.com/Joey-Project/tvOS-net-player/pull/86
supersedes: []
superseded_by:
---

# Foreground Range Cache Foundation

## Goal And Placement
- Deliver a focused prerequisite to slice 3 of the [home LAN readiness roadmap](2026-10-23-home-lan-readiness-roadmap-b46388.md), before returning to controlled native ABR acceptance.
- Start from master `7f13162f600d96f8f830f5b39f961ed680d05aaf`. Preserve the separate, uncommitted media acceptance work; do not import its progressive-index, probe, fixture or workflow changes into this PR.
- Separate foreground delivery granularity from background full-fill blocks. This is a cache transport foundation, not evidence that native automatic quality switching has passed.

## Accepted Contract
- Foreground playback uses bounded small Range fetches, approximately 64 KiB maximum per origin request after the existing startup identity is established. Background download retains its current large logical blocks and full-fill goal.
- Keep origin-fetch granularity separate from cache-to-client delivery. A verified contiguous cached prefix may use a larger bounded delivery window, without waiting for that larger amount from an uncached origin or rehashing a large extent for every 64 KiB body chunk.
- Both paths share the same verified cache. Reuse existing durable coverage and fetch missing bytes; simultaneous small and large reads must not overwrite conflicting data or spuriously fail merely because their ranges overlap.
- Retain the 512 KiB startup identity, resource/total-length binding, per-origin validators and cross-CDN sampling. A small response must pass the existing complete-response checks before becoming durable or being emitted as cached media.
- Preserve object identity, verified content stability and access-policy checks. Compare old extent bytes against their original hashes before using them in newly hashed merged extents; do not bless tampered data by recomputing a fresh hash.
- Preserve existing cancellation, foreground preemption, bounded permits, quota reservations and guarded fallback policies. Do not wait for a blocked background parent range to complete before a foreground small request can progress.
- Keep schema v1 and the gRPC API unchanged. Bound metadata to the existing 8192 extents and 2 MiB manifest; bounded coalescing must not create giant extents or bypass the 64-origin limit. Historical binary rollback compatibility requires separate proof, not an assumption from the schema number.
- Derive only intersecting foreground parents. Defer normal coalescing until complete-parent coverage and reclaim fragmented metadata after concurrent snapshots drain, including before capacity exhaustion. Preserve exact-snapshot safety and avoid quadratic whole-parent hashing on successive small appends.

## Implementation And Acceptance
1. Add small foreground planning without changing background logical chunk sizing or reducing startup identity checks.
2. Handle both concurrent publication orders. Verify matching overlap and write only missing bytes; reject conflicting overlap without replacing already committed bytes. Preserve rebase and checkpoint hardening.
3. Coalesce only verified contiguous coverage under bounded size and concurrency rules. Preserve old published extent identities while pending writers rely on them, or supply an equally strong bounded content proof.
4. Add deterministic tests for exact bytes/headers, early verified payload, background preemption, overlap success and conflict, truncation, cancellation/retry, quota and extent bounds, restore and completion. No global serialization or arbitrary sleeps as a concurrency substitute.
5. Run the complete normal test suite, formatting/lint, current-head GitHub CI and GitHub Codex review. Resolve all conversations before a normal head-bound squash merge.
6. Update master after merging, then integrate the preserved media acceptance work on a new branch and rerun unchanged native ABR/transcoding gates. Do not claim that follow-up result in this PR.

## Evidence
- Foreground continuation planning and LAN media cache-miss fetches share a 64 KiB bound; verified cached delivery has a separate 8 MiB maximum. The 512 KiB startup identity and large background logical parents remain unchanged. Background planning requests only missing gaps.
- Matching overlap verifies old extent hashes and incoming bytes before writing gaps. Resource-local shared admission retains concurrent flight snapshots; exclusive maintenance waits for them to drain before changing extent tuples. Each pass handles at most one 8 MiB parent, with bounded control-aware retries and lock release between passes. Exact old extent tuples remain mandatory for rebases; capacity is admitted before data writes.
- Normal coalescing waits for complete-parent coverage instead of rehashing a growing parent on every small append. Merged hashes consume the same byte stream whose original extent hashes were verified. Foreground planning derives only intersecting parents, and cache-only delivery uses a separately bounded verified contiguous prefix. Control checks cover reads and prepared body delivery.
- Before the fixture-budget adjustment, `cargo fmt --all`, `git diff --check` and normal `just pre-commit` passed. Full normal `cargo test --lib`, without filters or concurrency overrides, passed 1195 tests with zero failures or ignored tests in 39.76 seconds. All 117 `just test-ci-helpers` tests passed in 2.676 seconds. Current-head delivery receipts are recorded in the associated PR.
- Core regressions cover both overlap completion orders, conflicting overlap, same-object content tampering before merge, failed checkpoints, truncation, cancellation/retry, single-permit background preemption, exact-flight promotion, metadata limits and partial-cache restore. Concurrent full-range reads retain exact coverage and all sixteen expected 64 KiB origin subrequests.
- The real loopback LAN HTTP regression covers declared and unknown resource lengths, exact response headers and bytes, first-payload deadlines, durable small-range publication and a blocked large background request. Its valid MP4 fixture has an exact logical-parent length so a separate small background tail cannot contaminate the request-count proof. Truncated small responses do not publish or deliver valid cached media.
- Review regressions prove at most two visited parents for a 64 KiB read, exactly 8 MiB of data hashing for one warm 8 MiB prefix, and at most 32 MiB of data hashing for 128 sequential 64 KiB appends into one 8 MiB parent. A sparse maximum-size fixture exercises metadata-capacity recovery without downloading or allocating the whole resource. Drain/fairness, sparse capacity, linear hashing and prefix integrity also passed individually before the full suite.
- Session retirement/unpublished admission, original content hashes, object replacement and stale snapshots remain negative checks. The path selector now distinguishes an observed replacement from genuine absence while retaining legitimate final-path relocation. The strict replacement assertion was preserved; the separate absence assertion proves `NotFound` instead of conflating the two cases.
- Mixed full-suite runs repeatedly exposed two-second functional task-state and cleanup fixture waits. A controlled baseline passed 1175 library tests; on the candidate, the original 1175 tests passed together in 35.87 seconds and the 20 additions passed together in 6.82 seconds. These filtered comparisons are diagnostic only and do not establish a specific resource cause or replace full-suite acceptance.
- The approved adjustment gives only the shared test task-state waiter and the restored-output cleanup test a ten-second synchronization budget. Server runtime timeouts, cleanup retry intervals, media/ABR performance deadlines, all functional assertions and full-suite parallelism remain unchanged. Earlier negative runs remain retained; the change does not claim a production timing root cause.
- Delivery additionally requires complete normal `just ci`, both existing native UI E2E jobs, current-head GitHub Codex review and resolved conversations. The associated PR carries those head-bound receipts. System output remained muted; these tests do not establish audible-audio validation.
- Native automatic quality downgrade/recovery, the preserved progressive/transcoding acceptance work, authenticated live playback and historical-binary rollback compatibility are not established by this foundation. Roadmap slice 3 remains pending.
