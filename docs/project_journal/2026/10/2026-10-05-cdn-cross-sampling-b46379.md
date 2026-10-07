---
id: 20261005-b46379
title: Bounded CDN Cross-Sampling
status: completed
created: 2026-10-05
updated: 2026-10-05
branch: wip/bangumi-live-validation
pr:
supersedes: []
superseded_by:
---

# Bounded CDN Cross-Sampling

## Decision
- Joey approved a small heuristic cross-sampling policy, with more elaborate version verification deferred unless a real counterexample requires it.
- Different CDN ETags are not a content conflict by themselves. A same-origin validator change triggers content revalidation instead of unconditional rejection.
- Preserve representation identity, exact range/total/body-length checks, prefix hashing, durable extent hashes, object binding, and serialized publication checks.
- Do not add whole-object comparison, a BBDown API or dependency change, a new client setting, or a trusted content-version claim.

## Sampling Policy
- Keep the existing prefix comparison. Add at most three nonoverlapping 16 KiB probes outside that prefix, spread across already durable covered bytes.
- Additional remote sample payload is at most 48 KiB per candidate verification; the existing prefix request is accounted for separately.
- Never sample a sparse-file gap or download an uncached remote interval merely to establish a baseline. With only a durable prefix, retain prefix-only verification and do not delay cold start with a remote sampling sweep.
- Compare the candidate's exact byte intervals with locally hash-validated durable bytes. Require each response to retain the admitted effective origin, correct total length, exact Content-Range, and complete bounded body.
- Only update an origin's current validator after successful content checks. Keep the publication-time validator guard so unverified or concurrently stale responses cannot silently commit.
- A failed sample, short read, unsupported range, or failed local revalidation is not a successful match. Retry eligible candidates without publishing conflicting bytes.

## Interpretation Boundaries
- Matching identity metadata, length, prefix, and sampled bytes is evidence of sampled equivalence, not proof that unobserved bytes match.
- A final locally computed checksum proves retained-byte stability, not independent source-version correctness.
- A live readability pass does not by itself prove that a CDN-switch or validator-change branch ran, nor does it prove native player decoding or full offline completion.
- Preserve existing live-validation edits and local credential profiles. Do not log or commit credentials, signed media URLs, raw ETags, or payloads.

## Delivery Scope
- The lead owns design, journal updates, integration, and delivery decisions.
- A GPT-6 Luna worker owns the Rust cache implementation and embedded regression tests; another GPT-6 Luna worker owns bounded validation execution and ignored receipts.
- No local formal code-review lane. Any later PR must use current-head GitHub `@codex review`, passing CI, and resolved conversations before merge.

## Implementation
- Candidate verification compares the existing prefix and bounded durable samples before admitting a new effective CDN origin or replacing a changed stored strong validator.
- Stable redirects are supported, while a different origin during revalidation is not silently admitted. Remote byte conflicts, transport/status/range failures, and failed local cache revalidation remain separate outcomes.
- Sample HTTP 200 is range-unsupported; other non-206 statuses retain their upstream status. An invalid 206 interval or a byte conflict is not reported as a successful match.
- Fetch returns the verified manifest snapshot for extent publication. Origin binding publication is idempotent for the same proven desired binding and rejects competing distinct validators, stale snapshots, or changed object/content bindings.
- No persisted DTO, cache schema, Swift client, or BBDown dependency changed.

## Deterministic Validation
- Toolchain: repository-selected Rust/Cargo 1.95.0 and Swift 6.3.3.
- Initial frozen implementation SHA-256: `cd8771111ac18d77bd034c20c7cfee3d08a6de6b22b41b9fe60bbbef851b44ed`.
- Focused filters passed: six ETag-related tests and ten sample-related tests. These counts may overlap and are not added to the full-suite total.
- Complete deterministic Rust suite passed: 1,084 tests, zero failures, and one intentionally ignored opt-in live test.
- Workspace formatting, all-target Clippy with `-D warnings`, and repository lint passed.
- The LAN Swift probe compiled and passed 30 self-test cases. Its script requires Bash; Swift module-cache access needed a scoped elevated rerun. These self-tests do not establish live AVPlayer decoding.
- Regressions cover matching content with different tags, nonprefix sampled corruption and backup fallback, exact sample bounds, sparse and prefix-only caches, short bodies, wrong ranges/statuses, weak validators, stable redirects, local cached-byte corruption, and competing stale publication.
- Earlier compilation failures and sandbox/entrypoint failures were corrected or rerun with the required scope; retain their receipts rather than treating them as passing attempts.

## Live Validation
- The single ordinary-video 300-second sustained LAN case passed in 302.66 seconds, using the existing saved test profile; producer and runner exited 0 with confirmed cleanup.
- This result establishes ongoing LAN readability, not full offline completion, native decoding, or proof that a particular live CDN-switch branch executed. Sampler-branch evidence is provided by the deterministic regressions instead.
- The frozen implementation digest remained unchanged throughout the successful formal gates. Every producer was terminated and reaped before reporting completion.
- Scoped loopback access was required for network fixtures; it did not require a new login, credential-store mutation, or public restricted-area proxy requests.

## Artifact Hygiene
- Retain sanitized phase logs, runner files, source fingerprints, and execution receipts under the ignored task directory; 15 phase logs total 115,377 bytes within a fixed 16-file/16-MiB ceiling.
- Remove the generated native-probe binary directory and Python bytecode after all producers finish. Preserve earlier unrelated validation artifacts and working-tree changes.
- The initial local implementation stage did not create a commit or PR, run GitHub CI or formal reviews, build the full Apple apps, or repeat restricted-area/full-fill/native-decode validation. Those are not implied by its successful local gates; the subsequent delivery evidence below is recorded separately.

## Extended Delivery Regression
- The subsequent delivery baseline incorporates target-branch review-gate v2 at `fc8463c2fafca0c0e27ae87777745524f579ff68`; the initial production and live-harness source digests remained unchanged during that fast-forward.
- All four canonical URL smoke cases passed on that baseline in 21.35 seconds, using the saved private test profile and the approved HK Web route for restricted cases.
- The longer ordinary-video full-fill run observed a durable partial checkpoint and restart, then stopped background fill at `origin-binding-rebase`. Its 600-second foreground window, cache-only phase, checksums, and native decoding did not complete and are not passing evidence.
- The task-owned producer and consumer were stopped and reaped; the fixed ready document was absent. Preserve the compact receipt, failed log, and recovery root under `.codex-tmp/playback-delivery-live-20261005/`.
- A deterministic stale-snapshot regression reproduces rejection after another already-validated CDN changes its validator without changing the selected representation, data object, prefix, or durable spans. Correct that metadata-only race while retaining exact target-binding and content-conflict checks, then repeat affected gates on the new source.

## Corrected Publication Boundaries
- Origin-validation publication and extent publication have separate compatibility predicates. Both permit independently verified non-target validator metadata updates while preserving the selected representation, data-object identity, total length, prefix, and prior durable extents.
- Origin-validation publication retains the target binding compare-and-set. Extent publication requires the complete selected-origin binding to match its fetch snapshot and the response ETag to match the current binding, including absent/present transitions.
- A known origin's `None` to strong-ETag transition also triggers content revalidation; missing validators are not a way to bypass the target-publication guard.
- Generic checkpoint publication and the final descriptor/digest reread remain strict. This is a correction to metadata-only concurrency handling, not global permission to accept changed cache content.
- Focused tests passed for the real local fetch/independent-origin update/extent-commit transition, origin-binding rebase, target `Some`/`None` conflicts, newly present validator revalidation, same-origin changed-validator content checks, and stale-response rejection.
- Corrected frozen implementation SHA-256: `2933f986f7eb060162cf40a7a2497efcde64d98fd18aafc7c4a6f1d013b0e884`. The earlier complete-suite and 300-second readability evidence above belongs to the initial digest; the following results use this corrected version.

## Corrected Delivery Validation
- Baseline: `fc8463c2fafca0c0e27ae87777745524f579ff68`. The complete deterministic Rust suite passed 1,088 tests with zero failures and one intentionally ignored live test; Swift package tests passed 343, macOS XCTest passed one, and the native LAN probe passed 30 self-tests.
- Rust release server, macOS app, generic tvOS app, and tvOS build-for-testing succeeded. Repository pre-commit checks passed Rust formatting/all-target Clippy, Swift strict formatting/lint, Bash syntax, and ShellCheck.
- Actual local tvOS Simulator XCTest could not start: CoreSimulator 1051.54.0 is older than Xcode's required 1051.55.0. Generic/build-for-testing success is not a simulator test pass; compatible GitHub CI remains a delivery requirement.
- All four canonical real URL smokes passed on the corrected frozen binary in approximately 18.7 seconds. Restricted cases reused the saved private profile and working HK Web proxy, without a new login or credential-store mutation.
- The ordinary-video full-fill regression passed a 600-second foreground window, durable partial checkpoint/restart, complete selected-variant fill, second same-root restart with Bilibili disabled, and 300 seconds of cache-only validation. It completed in 910.31 seconds with 2,116 foreground rounds and 335 offline resources totaling 86,000,820 bytes; retained-content SHA-256 was `2ac19e0b16bde8dc0927b3994756359a88fe452a386f9000c66c5d59556d3505`.
- Ordinary cache-only AVPlayer validation passed a 120-second window with 2,856 decoded frames, 2,811 distinct display times, nonblank 480x360 pixel buffers, enabled audio-track evidence, pause/resume, forward/back seek, and 1.25x playback rate. This is native probe coverage, not full macOS app UI coverage.
- The restricted episode passed the matching 600-second foreground, durable partial checkpoint/restart, complete fill, second same-root restart, and 300-second cache-only regression in 935.75 seconds. Its 2,068 foreground rounds were followed by 603 offline resources totaling 178,132,517 bytes; retained-content SHA-256 was `c4243de7d79355d55a0d9821566142045cc290958e1c872944db67c59da13ff5`.
- Episode cache-only AVPlayer passed its 120-second window in 120.29 seconds with 2,854 decoded frames, 2,810 distinct display times, nonblank 640x360 pixel buffers, enabled audio-track evidence, and all pause/resume/forward/back seek/1.25x controls. Series and multi-part fixtures have smoke coverage, not separate long native runs.
- Both corrected full-fill producers and native consumers exited zero. Their normal teardown removed the fixed ready document after listener shutdown; the final source and binary fingerprints are retained with the task receipts.
- Formal isolated skill validation, project journal validation, and `git diff --check` passed. Deterministic gate receipts are under `.codex-tmp/playback-delivery-gates-20261005/`; corrected long-live receipts are under `.codex-tmp/playback-delivery-live-r2-20261005/`.

## Evidence
- Content comparison and its limits: `2026-10-04-etag-content-validation-b46378.md`.
- Production scope: `CacheServer/RustCacheServer/src/hls_cache.rs`.
- Canonical live cases: `.agents/skills/bilibili-live-e2e/references/live-cases.json`.
- Current task validation artifacts: `.codex-tmp/bilibili-cross-sampling-20261005/` (ignored).
