---
id: 20261023-b46389
title: CI Stability And Bounded Diagnostics
status: completed
created: 2026-10-23
updated: 2026-10-23
branch: codex/ci-stability-diagnostics
pr:
supersedes: []
superseded_by:
---

# CI Stability And Bounded Diagnostics

## Approved Scope
- Deliver slice 0 of the [home LAN readiness roadmap](2026-10-23-home-lan-readiness-roadmap-b46388.md) from updated master `c825285af0fe3c6b89ad7bf5b09bb49949384f16`.
- Improve the sustainability and diagnosability of existing CI without removing builds, tests, lint, formatting or native UI execution requirements.
- Prefer repository-local helpers and existing scripts. Keep test execution concurrency and assertions unchanged unless a demonstrated test-synchronization defect requires a narrowly justified correction.
- Do not change production playback, task recovery, credentials, schema, dependency versions or review policy in this slice.

## Baseline Evidence
- PR #81 ordinary CI run `39948441867` succeeded in 39m44s under a 45-minute job limit. Its raw log exceeded the bounded 16 MiB acquisition ceiling, so exact remote non-UI test totals were not independently recovered; the complete job/step result metadata did establish success.
- Both native UI jobs succeeded and retained small complete results: macOS and tvOS each executed three named cases with zero failures/skips, plus validated task counters and manual-read ordering.
- An earlier local Rust library batch passed 1,172 tests but timed out in `create_bilibili_playback_task_finalizes_secondary_result_cache` at the unchanged two-second waiter. The exact case passed in 0.57 seconds and one complete normal-parallel recheck passed 1,239 tests with one existing opt-in live case ignored. Resource contention and a production defect remain unproved hypotheses.

## Acceptance Plan
- Keep every existing ordinary CI and both native UI stages. A cache miss or cache restore failure must not turn into a skipped test or a false pass.
- Bind reusable build inputs to the applicable toolchain, platform and dependency state; exclude credentials and runtime/user state from caches.
- Preserve the actual command exit status, enforce bounded diagnostic retention and expose compact stage results. Distinguish successful execution, truncated evidence and unavailable counts explicitly.
- Add focused tests for new helper behavior and any test-waiter diagnostic change. Preserve the original timeout evidence rather than overwriting it with a later pass.
- Run complete relevant local checks and actual current-head GitHub CI/UI checks. Report cold and warm behavior separately; configuration alone cannot prove a speed improvement.
- Require current-head GitHub Codex review, all conversations resolved and genuine workflow CODEOWNER approval before any authorized merge.

## Implementation Decisions
- Add a focused CI stage runner rather than extracting the larger UI helper: ordinary successful build output may exceed a retention ceiling and must still be drained, whereas the UI runner intentionally terminates a producer at its output ceiling. Keep the existing native helper contract unchanged.
- Retain at most 512 KiB of head/tail output per ordinary stage and a small explicit JSON result, under an 8 MiB diagnostic-set ceiling. Preserve actual command status, deadlines and process-group cleanup. This version explicitly records `test_summary: null`; use retained stage logs and native UI result metadata for counts, never infer zero from missing evidence.
- Give ordinary CI the same 60-minute job and first-step 45-minute work window as the native UI jobs, leaving independent cleanup/upload headroom. This complements bounded logging and cache reuse rather than replacing them.
- Separate Rust/Swift cache keys and ordinary/macOS-UI/tvOS-UI writer namespaces. Bind toolchain/platform, dependency manifests/locks and relevant sources. Restore on PR and normal runs; save only on successful pushes to the exact repository default branch. Do not restore arbitrary cross-source/dependency prefixes or skip any command on a hit.
- GitHub documents default/base-branch cache visibility for PRs and merge-ref isolation of PR-written caches in its [dependency caching reference](https://docs.github.com/en/actions/reference/workflows-and-actions/dependency-caching). Cache contents remain build inputs, not trusted test evidence; no credentials or runtime state belong in them.
- No synchronization defect was demonstrated in the Rust waiter. Preserve its two-second deadline, ten-millisecond poll and success predicate; add only bounded, redacted last-observation diagnostics and focused formatting/privacy regressions.
- Add `just test-ci-helpers` to run all repository Python helper regressions and include it in complete local CI. GitHub runs the same test discovery without requiring a new Just installation.

## Delivery State
- The stage runner, cache workflow integration, test-only waiter diagnostics, task-runner entrypoint and full roadmap are implemented. Production runtime behavior and the native UI runner contract are unchanged.
- Focused validation passed 101 Python regressions (30 new and 71 existing), Python syntax, workflow shell syntax and real deadline export, actionlint/ShellCheck for both workflows, Rust formatting, two diagnostic tests and the original exact waiter case. The original case passed in 0.37 seconds; this does not establish a root cause for the earlier timeout.
- This status describes the completed implementation on the squash-merge target branch. Complete current-head local validation, actual GitHub ordinary/native UI execution, remote review and genuine CODEOWNER approval remain mandatory delivery gates recorded in the PR and bounded private receipts.
- No actual warm-cache performance result is claimed by this implementation. Cold fallback and cache key/save policy must remain correct independently of later cache-hit measurements.
- Transient head/check/review state belongs in the PR and ignored task receipts, not the squash-merge target-branch journal.
