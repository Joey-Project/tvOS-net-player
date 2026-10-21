---
id: 20261021-01a0dd39
title: BBDown 0.11 Space Dynamic V2 Acceptance
status: completed
created: 2026-10-21
updated: 2026-10-21
branch: codex/space-dynamic-v2-acceptance
pr:
supersedes: []
superseded_by:
---

# BBDown 0.11 Space Dynamic V2 Acceptance

## Scope
- Upgrade the Rust adapter from BBDown 0.10 to official BBDown 0.11 for the existing authenticated space-dynamic V2 path.
- Keep this a compatibility update. Do not route consumer searches through V2 or change the existing Space Dynamic flow.
- Handle new video-search kinds explicitly: they must not silently become `Latest`, V2 search remains unsupported, and collection kind labels remain exhaustive.

## Acceptance
- Use the existing opt-in `authenticated-space-dynamic` live case with the canonical fixture `https://space.bilibili.com/441167301/dynamic`.
- Confirm the path resolves V2 playback candidates, selects the explicit first candidate, creates a V2 task, lists task results, and retrieves the LAN HLS master playlist.
- Run from the repository's supported Rust toolchain with `BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PATH` and `BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PROFILE` supplied by the operator, and `BILIBILI_LIVE_E2E_CASES=authenticated-space-dynamic` for `test-bilibili-live`. Never record credential locations, profile names, or values here.

## Boundaries
- The harness uses temporary services bound to `127.0.0.1`. The live acceptance uses the Web cookie only for `api.bilibili.com`; media requests use the configured media user-agent and Referer without a cookie. It does not use proxies, refresh credentials, or write the source credential store.
- The harness may fetch media initialization or index bytes while planning candidates. This acceptance does not establish native decoding, audio playback, full-fill, sustained playback, or offline playback.
- Muting was verified before any test that could play audio; keep system audio muted throughout that test.
- No deployment was performed.

## Current State
- Rust 1.95.0 formatting passed. Focused adapter tests for `Search` handling, default selection, rejection, and kind mapping passed.
- The nonsandbox package suite passed: 1,241 tests total (1,175 library, 60 deterministic live-harness, and 6 integration), with zero failures and one ignored test. Package all-target Clippy with `--locked -D warnings` passed, as did the normal pre-commit shell, Swift-format, Rust-format, and workspace Clippy checks.
- The authorized `authenticated-space-dynamic` live case passed once at consumer source `de48b1abad3ca4104041c5596bd21348f7e0101a` (exit 0; 48.135 seconds total, 47.40 seconds in the harness; zero failures or ignored cases). Its assertions covered V2 resolution and candidate selection, task creation, playable HLS/master retrieval, and task results. The result supports at least one candidate; it does not establish a pagination count or task-result count. Later test-only changes were not live-tested again.
- The dedicated login-start timeout regression now separates a 50 ms login-start deadline, a cancellation-aware 2 second mock response, and a 5 second ordinary operation timeout. It retains failure-state, empty-QR, and request-count assertions and requires return within 1 second. The focused case passed in 0.076 seconds. Full `scripts/test.sh` passed 370 Swift package tests with no failures (2.068 seconds of test execution after a 123.80 second build), using Apple Swift 6.3.3 and Xcode 26.6 (17F113). Native Xcode suites remain separate from this Swift package gate.
- Harness shutdown completed after listener, task, and HLS quiescence. No recovery root remained, and before/after integrity checks for source files and the credential store matched.
- An earlier sandboxed suite attempt exposed one assertion, which was fixed, plus eight `dev/fd` or race failures in the sandbox; the nonsandbox rerun passed. One live startup was blocked by Cargo cache locking before the harness began. A subsequent startup was rejected before process launch; the later user-approved bounded attempt passed.

## Evidence
- Adapter: `CacheServer/RustCacheServer/src/bbdown_adapter.rs`.
- Dependency pin: `CacheServer/RustCacheServer/Cargo.toml` and workspace `Cargo.lock`.
- Existing live harness: `CacheServer/RustCacheServer/tests/bilibili_live_e2e.rs` and `.agents/skills/bilibili-live-e2e/SKILL.md`.
- Merge model: squash merge; this note describes the target-branch state after this compatibility acceptance is complete.
