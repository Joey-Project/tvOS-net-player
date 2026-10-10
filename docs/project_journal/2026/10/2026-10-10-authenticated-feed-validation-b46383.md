---
id: 20261010-b46383
title: Authenticated Following And Dynamic Validation
status: blocked
created: 2026-10-10
updated: 2026-10-10
branch: codex/authenticated-feed-validation
pr:
supersedes: []
superseded_by:
---

# Authenticated Following And Dynamic Validation

## Approved Scope
- Deliver the fourth slice of [Playback Reliability Follow-Ups](2026-10-05-playback-reliability-followups-b46380.md), after PR #75 merged as `3bcc928ea5d57895095971b02e7bd3753b8de544`, local `master` was fast-forwarded, and this independent branch was created.
- Reuse the explicitly selected saved Web credential profile. Preserve all profiles and the global default; request login only if the necessary credential is missing or rejected.
- Validate `authenticated-following-feed` and `authenticated-space-dynamic` through V2 resolution, opaque one-item pagination, task creation, result pagination, and server-owned LAN HLS playback. Keep candidate/result snapshots stable and require a playable archive.
- Classify credential rejection, empty account state, upstream availability/schema failure, and server bugs without exposing cookies, keys, signed URLs, private account metadata, or raw upstream errors.
- Fix demonstrated compatibility or validation gaps only. No account-switching API, client-side media fetch, physical Apple TV validation, or TLS rollout.

## Execution Plan
1. Run the existing focused authenticated live cases under finite process/output bounds, with the saved profile and no login or credential-store write.
2. On success, exercise at least 600 seconds of sustained LAN HLS for each selected case and a separate native macOS AVPlayer decoding/control probe when the existing handoff supports it. Distinguish HTTP readability from native decoding and app-UI evidence.
3. If an account/feed is empty, report that condition as an incomplete live acceptance gate and use an account-relevant uploader URL only with evidence or user-supplied test data; never lower the minimum candidate count to manufacture success.
4. Add focused deterministic coverage for any changes, then run complete Rust/Swift formatter, linter, tests, Apple builds, and available native integration gates. Compatible-runner tvOS XCTest remains a required CI gate.
5. Freeze a signed head, open a focused PR, require current-head GitHub CI and Codex review, resolve every conversation, and perform the final stable reread before an authorized normal squash merge.

## Delivery Ownership
- GPT-6.1 Sol leads coordination, integration, documentation, and PR/merge decisions. GPT-6 Luna receives bounded implementation/test tasks with disjoint write scopes.
- No local formal code review or Claude lane. Live test artifacts stay ignored and contain only safe summaries.

## Completed Validation Slice
- Add the two authenticated feed IDs to the opt-in full-fill allowlist for first-candidate selection only. Preserve default authentication skips, explicit inclusion/filter routes, the single-result assertions, cache budget, restart identity checks, and opaque pagination contracts.
- Two deterministic tests cover first selection, filter-only and inclusion-only admission, full-fill without authentication opt-in, and rejection of every non-first selection. Synchronize the repo-local live-e2e skill, full-fill reference, and Chinese README.

## Evidence
- Predecessor PR #75 passed 1,225 Rust tests, 359 Swift tests, macOS XCTest, native self-tests, both Apple builds, and current-head GitHub CI including executed tvOS XCTest. All fourteen conversations were resolved before normal squash merge.
- Its final-head ordinary/Bangumi complete-fill regressions ran for about 31 minutes combined, with 600-second sustained HLS, 300-second cache-only phases, and separate 120-second native probes. These predecessor results do not establish authenticated following/dynamic coverage.
- Final changed-source local validation passed Rust formatting, strict all-target Clippy, workspace check, **1,227 Rust tests with zero failures and one ignored opt-in live test**, doc tests, and release build. All 67 Rust/control-plane inputs matched before/after and again after live validation.
- Apple validation passed strict Swift formatting, **359 Swift tests**, macOS XCTest (1/1), macOS/tvOS builds, tvOS build-for-testing, shell syntax/ShellCheck for 20 scripts, and native probe self-tests (30/30). All 150 Apple inputs matched before/after and after live validation. Local CoreSimulator remains below the selected Xcode requirement; actual tvOS XCTest execution is a future current-head CI gate, not a local pass.
- Following passed the initial authenticated V2 pagination/task/result/HLS smoke and 30-second sustained probe (70 rounds). Its final changed-source full-fill run passed in 1,107.583 seconds, including a durable partial checkpoint and same-root restart, **600 seconds sustained LAN HLS (2,155 rounds)**, and **300 seconds cache-only** with unchanged cache checksums.
- The completed selected representation reported **8,490,698 bytes**. The offline walk validated **64 resources and 72,206,489 aggregate payload bytes**; that read-volume metric is not unique on-disk cache size.
- The separate native macOS AVPlayer probe passed for **120 seconds**, decoded **3,380 frames / 3,339 unique display times**, and evidenced one enabled video and audio track. All nine named control steps completed in the retained sanitized result, including pause/resume, forward/backward seeks and 1.25x playback. This is native probe evidence, not full app-UI, independent audible-audio, or physical Apple TV validation.
- The two early long-driver setup attempts did not produce a completed live result; only the corrected producer/consumer exit-zero run counts. Both final processes were reaped and the ready file disappeared after producer teardown. No forced signed-URL expiration was injected in this authenticated run.
- An intermediate full-library run hit the existing two-second finalizer wait timeout. The final whole suite passed, and five predefined isolated repetitions of that exact test passed (0.34-0.87 seconds). No source repair is claimed for the earlier intermittent failure; its root cause remains unproven.

## Remaining Blocker
- The default public `/2/dynamic` sample failed before candidates were available. A second sample, [the public uploader of the committed ordinary-video fixture](https://space.bilibili.com/18698687/dynamic), also failed resolution. Both retain `upstream_schema_or_availability`; neither is counted as success or an empty-account result.
- One bounded authenticated diagnostic using the exact pinned core returned **HTTP 412** from the official space-feed endpoint before processing entries. It exposed only the typed `http_error` category and status, with no provider code. Following succeeded with the saved Web credential; these observations do not establish expired credentials or justify asking for another login.
- The inspected following and space request paths share their user-agent, referer and cookie construction. The evidence does not establish a production parser/signing bug or the precise cause of the HTTP rejection, so no speculative core/server patch or public-cookie proxy was introduced.

## Next Steps
- Choose whether to deliver the validated harness slice while keeping dynamic blocked, or provide a trusted network route to the official API before completing both live cases. No timeout selects that preference automatically.
- Rerun dynamic with a current public uploader URL and the saved server-side profile once the external condition changes; require the original candidate count, V2 pagination, LAN HLS and long native/cache checks without weakening acceptance.
- Complete current-head GitHub CI, Codex review and every conversation before any subsequent PR merge. Keep this larger workstream incomplete while dynamic live acceptance is blocked.
- Follow up the intermittent two-second finalizer test timeout if it recurs; the isolated repetitions do not prove a root cause.
