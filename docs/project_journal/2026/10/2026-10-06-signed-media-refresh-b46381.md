---
id: 20261006-b46381
title: Same-Identity Signed Media Refresh
status: completed
created: 2026-10-06
updated: 2026-10-07
branch: wip/signed-media-url-refresh
pr:
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
- Replace only upstream URL candidates and their request headers. Keep existing range prefix/sampling, representation, object/content, and publication guards authoritative; identity matching and samples do not prove a whole-origin version.
- Trigger bounded replanning for exhausted expiration-like media responses (401/403/404/410), not every transient network failure. Coalesce concurrent requests for one session, impose a timeout and cooldown, and retain cancellation/preemption checks.
- Publish refreshed requests only while the task and accepted session are still current. Do not resurrect deleted/cancelled/completed sessions or let a stale refresh overwrite newer media requests.
- Foreground range reads and background full fill reuse the same refresh coordination. Background retries must use the current session instead of a scheduler's stale queued clone. Preserve the approved newest-playback preemption policy.

## Validation Plan
- Add deterministic local-HTTP coverage for expired URL recovery, unchanged LAN/cache identity, partial-cache retention, unique representation matching, content/codec/length mismatch rejection, failed refresh, concurrent single-flight refresh, cancellation/deletion, cooldown, and restart.
- Run the complete Rust/Swift/native test sets, formatting/lint, macOS/tvOS builds, and compatible-runner simulator XCTest.
- Reuse the four canonical real Bilibili cases and long foreground/full-fill/restart/cache-only validation where available. Keep HTTP and native AVPlayer evidence separate and report actual exercised durations.
- The opt-in full-fill harness can force request refresh only after verified positive partial quiescence: replace URL candidates in the isolated fixture manifest with an unknown loopback LAN route (404), restart, and require real accepted-item replanning plus complete fill. This tests an injected expiration response, not natural provider expiry.
- Use current-head GitHub `@codex review`, passing CI, and a complete conversation gate before squash merge. No local formal review lanes.

## Delivered
- Foreground ranges, MP4 initialization probes, proxy fallback, and background full fill can refresh exhausted expired request candidates once without changing accepted content, representation, resource IDs, or cache keys.
- Session refresh is single-flight, bounded, cancellation/preemption-aware, and protected by current-task/runtime/persisted-session compare-and-replace. Existing publication recovery and ordinary generation invalidation remain authoritative.
- Restored playable tasks retain their playback options and server-owned credential-profile selection, including non-primary results. Complete and legacy sessions do not silently acquire new refresh authority.
- The isolated full-fill harness has an opt-in expired-request injection after verified partial quiescence. It preserves accepted identities and durable files and cannot target an arbitrary deployed cache root. Its filesystem checks protect object identity and content stability, not timestamp stability or directory child-entry counts.

## Validation Results
- Rust: 1,132 passed, zero failed, one opt-in network test ignored in the complete default-parallel and serial suites. Focused refresh, probe, publication/rewrite recovery, and generation-invalidating regressions passed. Release build, formatting, Clippy, ShellCheck, and Swift formatting passed through the repository gate.
- Swift/macOS: 343 Swift tests, one macOS XCTest, and 30 native-probe self-tests passed. macOS and tvOS builds and tvOS build-for-testing passed. Local simulator execution remains unavailable because CoreSimulator service `1051.54` does not match Xcode's `1051.55`; compatible-runner simulator execution is a CI requirement, not a claimed local pass.
- All four canonical real URL smokes passed: ordinary video, multi-part video, Bangumi series, and episode. Restricted Bangumi used a Web-mode Hong Kong reverse proxy; saved credentials were reused and Web cookies were not forwarded to the public proxy.
- Ordinary video: verified 1,572,867 durable partial bytes in three extents before injection; real accepted-item replanning recovered the 404 candidates and completed fill. The 600-second foreground probe made 2,175 rounds, followed by 300-second cache-only validation. Total test duration was 909.87 seconds.
- Bangumi episode: verified 2,621,445 durable partial bytes in five extents; the same injected-expiration recovery and complete fill passed. The 600-second foreground probe made 2,061 rounds, followed by 300-second cache-only validation. Total test duration was 932.09 seconds.
- Separate macOS AVPlayer probes completed their requested 120-second windows: 2,855 ordinary-video frames and 2,852 episode frames, with video/audio tracks present. Successful completion requires the probe's play, pause/resume, forward/backward seek, and 1.25x-rate checks. This is native-probe evidence, not a full app GUI or physical Apple TV test.
- Both full-fill runs verified cache-only reads and stable completed-cache checksums and completed teardown. The corrected runner and consumer have 16 passing credential-free safety tests. Initial runner startup failures were fixed before any live request and are not counted as live passes.

## Boundaries And Next Step
- The expiration test injects loopback 404 responses followed by real planning/download. It does not prove that Bilibili naturally expired a URL during the run, or that small cross-CDN samples prove an entire origin version.
- No BBDown dependency upgrade, client-facing upstream URL, plaintext credential mutation RPC, local formal review, or physical Apple TV validation is introduced.
- Continue the approved credential-lifecycle slice, then authenticated following/dynamic live regression, after updating `master` and creating separate successor branches.
