---
id: 20261006-b46381
title: Same-Identity Signed Media Refresh
status: completed
created: 2026-10-06
updated: 2026-10-07
branch: wip/signed-media-url-refresh
pr: https://github.com/Joey-Project/tvOS-net-player/pull/72
supersedes: []
superseded_by:
---

# Same-Identity Signed Media Refresh

## Scope
- Continue the approved [Playback Reliability Follow-Ups](2026-10-05-playback-reliability-followups-b46380.md) after PR #71, from `master` commit `c766c4951455d3169d6d632ed200fba5e8aea38d`.
- Refresh expired media requests for the already accepted video page or episode, not the original mutable collection position. Preserve the server-owned LAN playback URI, session/resource IDs, cache keys, and verified durable extents.
- Keep credentials on the server. Use the persisted task's options and server-owned credential-profile selection; never return fresh upstream URLs or credential values to clients.

## Design
- The pinned BBDown core `0.6.0` already exposes content resolution and playback planning. Add a narrow server-side planner refresh contract; do not change the upstream dependency for this slice.
- Persist the accepted AID/BVID/CID/EPID identity with the HLS session. Older sessions without a verifiable accepted identity remain readable but cannot silently opt into refresh.
- Resolve a canonical direct video/episode using that identity. For multi-part video, resolve the current page index from the accepted CID and validate the complete returned identity; do not reuse an old list index.
- Match only one exact accepted DASH representation, independently of URL-derived variant IDs and source hashes. Preserve codec, stream ID, MIME, bandwidth, dimensions, frame rate, duration, and declared size. Unknown declared sizes still require the existing exact HTTP total-length and durable-content verification before additional bytes become durable.
- Replace only upstream URL candidates and the complete request-header list, including added or removed header names. Keep existing range prefix/sampling, representation, cache identity, authorization policy, object/content, and publication guards authoritative; identity matching and samples do not prove a whole-origin version. Ordinary queued saves cannot overwrite newly persisted request candidates or headers with a stale clone.
- Trigger bounded replanning for exhausted expiration-like media responses (401/403/404/410), not every transient network failure. Keep session-level single flight, timeout, cooldown, and cancellation/preemption checks. A concurrent refresh is reusable only when it updates the particular failed resource; a video-only request change does not establish recovery for expired audio or an alternate resource.
- Refresh planning shares the configured playback-planning semaphore with initial planning. Count queued and running refreshes in planning activity, include permit waiting and actual replanning in one 30-second timeout, and release both permit and activity through cancellation, preemption, failure, and timeout. Per-session single flight alone does not limit concurrent work across different sessions.
- Publish refreshed requests only while the exact accepted session is still authorized and current. A completed parent task can retain refresh context for a genuinely online secondary result, including after restart; it does not grant refresh authority to fully cached completed results. Do not resurrect deleted/cancelled/completed cache sessions or let a stale refresh overwrite newer media requests.
- Foreground range reads, initialization probes, HEAD/GET proxy fallback, and background full fill reuse the same refresh coordination. HEAD refresh retries remain bodyless. Background retries must use the current session instead of a scheduler's stale queued clone. Preserve the approved newest-playback preemption policy.

## Validation Plan
- Add deterministic local-HTTP coverage for expired URL recovery, bodyless HEAD recovery, complete header-list replacement, unchanged LAN/cache identity, partial-cache retention, unique representation matching, content/codec/length mismatch rejection, failed refresh, resource-specific concurrent single-flight recovery, cancellation/deletion, cooldown, and restart. Verify an online secondary result remains refreshable after primary cache completion, while fully cached results remain ineligible.
- Verify cross-session refreshes and initial planning share the same concurrency budget, queued refreshes keep background work non-idle, and blocked permit acquisition observes cancellation, preemption, and the overall timeout without leaking permits or activity.
- Run the complete Rust/Swift/native test sets, formatting/lint, macOS/tvOS builds, and compatible-runner simulator XCTest.
- Reuse the four canonical real Bilibili cases and long foreground/full-fill/restart/cache-only validation where available. Keep HTTP and native AVPlayer evidence separate and report actual exercised durations.
- The opt-in full-fill harness can force request refresh only after verified positive partial quiescence: replace URL candidates in the isolated fixture manifest with an unknown loopback LAN route (404), restart, and require real accepted-item replanning plus complete fill. This tests an injected expiration response, not natural provider expiry.
- Use current-head GitHub `@codex review`, passing CI, and a complete conversation gate before squash merge. No local formal review lanes.

## Delivered
- Foreground ranges, MP4 initialization probes, HEAD/GET proxy fallback, and background full fill can refresh exhausted expired request candidates once without changing accepted content, representation, resource IDs, or cache keys. HEAD remains HEAD and does not consume upstream response bodies; unsuccessful refresh retains the original expiration response and permitted headers.
- Session refresh remains single-flight, bounded, cancellation/preemption-aware, and protected by current-task/runtime/persisted-session compare-and-replace. Each failed media resource has its own bounded cooldown and reuse test; unknown-resource callers require every media request to have changed. Publication preserves other resources updated since the failed snapshot, including complete header-list replacement. Existing publication recovery and ordinary generation invalidation remain authoritative.
- Refreshes across distinct sessions share the configured initial-planning semaphore. Permit acquisition and resolver execution use the existing joint 30-second budget, while the planning activity guard spans queued work, active resolution, and publication. Deterministic activity-count barriers verify cross-session serialization, queued cancellation and preemption, immediate retry after preemption, and timeout cleanup.
- Restored tasks retain their playback options and server-owned credential-profile selection while an exact secondary result remains online, even when primary cache completion has completed the parent task. Fully cached, deleted, cancelled, and identity-incomplete sessions do not acquire new refresh authority; finishing the last online result releases the retained refresh context.
- Extent-publication rebase failures have bounded static diagnostics for object identity, representation/content bindings, generation regression, existing extents, and target-origin validator changes. These diagnostics expose no upstream URL, header value, or credential and do not relax any publication guard.
- The isolated full-fill harness has an opt-in expired-request injection after verified partial quiescence. It preserves accepted identities and durable files and cannot target an arbitrary deployed cache root. Its filesystem checks protect object identity and content stability, not timestamp stability or directory child-entry counts.

## Validation Results
- Rust: 1,150 passed, zero failed, one opt-in network test ignored in the complete default-parallel and serial suites. Focused header-list, bodyless HEAD, resource-specific concurrency, completed-parent restoration, publication/rewrite recovery, generation-invalidating, and shared-planning-budget regressions passed. Release build, formatting, Clippy, ShellCheck, and Swift formatting passed through the repository gate. Loopback fixtures required the permitted local-network execution environment; sandbox bind failures are not counted as passes.
- Swift/macOS: 343 Swift tests, one macOS XCTest, and 30 native-probe self-tests passed. macOS and tvOS builds and tvOS build-for-testing passed. Local simulator execution remains unavailable because CoreSimulator service `1051.54` does not match Xcode's `1051.55`; compatible-runner simulator execution is a CI requirement, not a claimed local pass.
- All four canonical real URL smokes passed: ordinary video, multi-part video, Bangumi series, and episode. Restricted Bangumi used a Web-mode Hong Kong reverse proxy; saved credentials were reused and Web cookies were not forwarded to the public proxy.
- Ordinary video: verified 2,621,445 durable partial bytes in five extents, with 1,048,576 completed bytes, before injection; real accepted-item replanning recovered the 404 candidates and completed fill. The 600-second foreground probe made 2,131 rounds, followed by 300-second cache-only validation of 335 resources and 85,425,918 cumulative bytes. Total test duration was 910.28 seconds.
- Bangumi episode: a 2,097,156-byte, four-extent partial checkpoint, with 1,048,576 completed bytes, and injected-expiration recovery passed. The 600-second foreground probe made 2,025 rounds, followed by 300-second cache-only validation of 603 resources and 178,132,517 cumulative bytes. Total test duration was 931.36 seconds.
- Separate macOS AVPlayer probes completed their requested 120-second windows: 2,853 ordinary-video frames in 122.88 seconds and 2,853 episode frames in 122.60 seconds, with video/audio tracks present. Successful completion requires the probe's play, pause/resume, forward/backward seek, and 1.25x-rate checks. This is native-probe evidence, not a full app GUI, physical Apple TV, or audibility test.
- Both full-fill runs verified cache-only reads and stable completed-cache checksums and completed teardown. The corrected runner and consumer have 18 passing credential-free safety tests, including the fixed ready-path contract and bounded, case-insensitive fatal detection across long output chunks. The final passing run binds frozen source manifest SHA-256 `2f5fca1bbbde4592a3ea678736c90ee4bba76f5923b61f23d817aa3935f55b6d` against base `828e28ec3919c225b061a132535aa0862d4149aa`. An earlier passing run used manifest `655734c809f0f645782af8fe49888f73cef42326ac3b61ba70ffe5d02a97dc52`; its evidence is historical after the shared-planning-budget fix. Failed bootstrap attempts are retained separately and are not counted as passes.
- An earlier ordinary-video full-fill run refused `extent-publish-rebase` after its upstream ETag/length/prefix probes passed. Those probes do not prove unchanged expected/current cache objects or bindings. The exact failed field was not logged then; the new static diagnostics leave all guards unchanged. The refusal did not recur in the two complete passing runs, but its original cause remains unconfirmed and is not claimed fixed.

## Boundaries And Next Step
- The expiration test injects loopback 404 responses followed by real planning/download. It does not prove that Bilibili naturally expired a URL during the run, or that small cross-CDN samples prove an entire origin version.
- No BBDown dependency upgrade, client-facing upstream URL, plaintext credential mutation RPC, local formal review, or physical Apple TV validation is introduced.
- Preserve fail-closed cache publication. If the earlier rebase refusal recurs, use its static reason to distinguish a safe bounded refetch from object replacement or content mutation before designing automatic background recovery; do not infer whole-origin equivalence from successful small samples.
- Continue the approved credential-lifecycle slice, then authenticated following/dynamic live regression, after updating `master` and creating separate successor branches.
