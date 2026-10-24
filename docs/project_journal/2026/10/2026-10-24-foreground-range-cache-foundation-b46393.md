---
id: 20261024-b46393
title: Foreground Range Cache Foundation
status: completed
created: 2026-10-24
updated: 2026-10-24
branch: codex/cache-foreground-ranges
pr:
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
- Both paths share the same verified cache. Reuse existing durable coverage and fetch missing bytes; simultaneous small and large reads must not overwrite conflicting data or spuriously fail merely because their ranges overlap.
- Retain the 512 KiB startup identity, resource/total-length binding, per-origin validators and cross-CDN sampling. A small response must pass the existing complete-response checks before becoming durable or being emitted as cached media.
- Preserve object identity, verified content stability and access-policy checks. Compare old extent bytes against their original hashes before using them in newly hashed merged extents; do not bless tampered data by recomputing a fresh hash.
- Preserve existing cancellation, foreground preemption, bounded permits, quota reservations and guarded fallback policies. Do not wait for a blocked background parent range to complete before a foreground small request can progress.
- Keep schema v1 and the gRPC API unchanged. Bound metadata to the existing 8192 extents and 2 MiB manifest; bounded coalescing must not create giant extents or bypass the 64-origin limit. Historical binary rollback compatibility requires separate proof, not an assumption from the schema number.

## Implementation And Acceptance
1. Add small foreground planning without changing background logical chunk sizing or reducing startup identity checks.
2. Handle both concurrent publication orders. Verify matching overlap and write only missing bytes; reject conflicting overlap without replacing already committed bytes. Preserve rebase and checkpoint hardening.
3. Coalesce only verified contiguous coverage under bounded size and concurrency rules. Preserve old published extent identities while pending writers rely on them, or supply an equally strong bounded content proof.
4. Add deterministic tests for exact bytes/headers, early verified payload, background preemption, overlap success and conflict, truncation, cancellation/retry, quota and extent bounds, restore and completion. No global serialization or arbitrary sleeps as a concurrency substitute.
5. Run the complete normal test suite, formatting/lint, current-head GitHub CI and GitHub Codex review. Resolve all conversations before a normal head-bound squash merge.
6. Update master after merging, then integrate the preserved media acceptance work on a new branch and rerun unchanged native ABR/transcoding gates. Do not claim that follow-up result in this PR.

## Evidence
- Foreground continuation planning and the LAN media stream share a 64 KiB bound; the 512 KiB startup identity and large background logical parents remain unchanged. Background planning requests only missing gaps.
- Matching overlap verifies old extent hashes and incoming bytes before writing gaps. Bounded parent-local coalescing requires sole production-flight ownership under the publication lock; new claims use the same lock, and exact old extent tuples remain mandatory for pending snapshot rebases. Metadata capacity is admitted before data writes.
- `cargo fmt --all`, `git diff --check` and normal `just pre-commit` passed. Full normal `cargo test --lib`, without filters or concurrency overrides, passed 1188 tests with zero failures or ignored tests in 36.15 seconds. All 117 `just test-ci-helpers` tests passed.
- Core regressions cover both overlap completion orders, conflicting overlap, same-object content tampering before merge, failed checkpoints, truncation, cancellation/retry, single-permit background preemption, exact-flight promotion, metadata limits and partial-cache restore. Concurrent full-range reads retain exact coverage and all sixteen expected 64 KiB origin subrequests.
- The real loopback LAN HTTP regression covers declared and unknown resource lengths, exact response headers and bytes, first-payload deadlines, durable small-range publication and a blocked large background request. Its valid MP4 fixture has an exact logical-parent length so a separate small background tail cannot contaminate the request-count proof. Truncated small responses do not publish or deliver valid cached media.
- An earlier full run hit the unchanged two-second secondary-result task waiter. Its single focused diagnostic passed in 0.33 seconds, followed by the successful normal full suite above. The earlier failure was retained; no timeout or assertion was weakened, and the underlying timing cause is not established.
- Delivery additionally requires complete normal `just ci`, both existing native UI E2E jobs, current-head GitHub Codex review and resolved conversations. The associated PR carries those head-bound receipts. System output remained muted; these tests do not establish audible-audio validation.
- Native automatic quality downgrade/recovery, the preserved progressive/transcoding acceptance work, authenticated live playback and historical-binary rollback compatibility are not established by this foundation. Roadmap slice 3 remains pending.
