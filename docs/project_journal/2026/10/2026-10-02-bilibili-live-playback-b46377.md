---
id: 20261002-b46377
title: Bilibili Live Playback Validation
status: completed
created: 2026-10-02
updated: 2026-10-04
branch: wip/bangumi-live-validation
pr:
supersedes: []
superseded_by:
---

# Bilibili Live Playback Validation

## Scope
- Reuse the new-account credential profile recorded in local, ignored repository preferences; preserve the previous account and the global default.
- Validate the four canonical real URLs through V2 resolution, result pagination, the Rust LAN cache server, and LAN-only HLS sources.
- Keep public restricted proxies in Web mode. Explicit access-key probing consent does not authorize forwarding Web cookies.
- Extend only the opt-in integration harness to admit canonical single-result Bangumi full-fill validation. Production server and client behavior is unchanged.

## Checked Results
- Ordinary playlist and multi-part video smoke checks passed without a restricted proxy.
- Both Bangumi fixtures failed on the initial official-route attempt with the safe `server_bug` diagnostic marker. Joey clarified on 2026-10-04 that direct-route failure is expected for these restricted fixtures; the marker is not evidence of a product defect or an expired credential. Successful Web-proxy resolution and LAN playback are the acceptance criteria.
- Both Bangumi fixtures passed smoke validation through the Hong Kong Web route at `https://bili.lli.cx`.
- Sustained LAN HLS reads passed for 300 seconds per Bangumi case: 1,104 rounds for the media-series first candidate and 1,071 rounds for the episode. The combined run exited successfully in 609.50 seconds with normal harness teardown.
- Ordinary sustained reads completed 300 seconds and 1,158 rounds before the supervisor interrupted a broader three-case run. That run is incomplete, not a Bangumi success. Its log also recorded two `chunk-origin-etag-change` warnings; ordinary full-fill completion was not tested by that run.
- The updated integration target passed 56 deterministic tests with the real-network test ignored. Four initial sandbox-only failures were TCP-bind permission failures; the narrowly approved rerun passed all 56 tests.
- The first focused Clippy run rejected an introduced nested conditional. It was replaced by the behavior-equivalent `Option::filter` form; a fresh Cargo-built deterministic rerun passed all 56 tests with one ignored live test.
- Whole-workspace Rust formatting and Clippy with `--all-targets -D warnings` passed after that correction. The repository live-test skill passed the skill validator using the existing offline PyYAML runtime. Journal validation and `git diff --check` passed.
- Native probe preparation passed 30 self-tests; the task-scoped ready-file consumer passed 10 tests. These are not live decoding evidence.

## Full-Fill and Native Playback
- The real episode full-fill run observed a positive durable partial checkpoint: 1,048,576 completed bytes, 1,048,578 durable bytes, and two extents. It restarted the same root and completed its 300-second foreground HLS probe with 1,099 rounds.
- The selected variant filled completely, then survived a second same-root restart into cache-only playback. The 300-second offline phase read 603 media resources and 180,343,942 aggregate bytes; cached-file checksums remained unchanged. The full integration case passed in 607.91 seconds.
- The separate macOS AVPlayer probe consumed the fresh cache-only LAN source and passed its 120-second playback window in 123.24 seconds. It decoded 2,808 frames with 2,767 distinct presentation times at 640x360, passed pixel-buffer and audio-track evidence, and completed playback, pause, resume, forward/backward seek, and 1.25x rate controls.
- Native playback was validated for the episode fixture. The series fixture has smoke and 300-second HTTP-read evidence, not a separate native decode run. No player used an upstream Bilibili media URL.
- The native consumer and full-fill supervisor exited successfully and reaped their children. The normal producer teardown removed its ready file; final test and lint supervisors also reported terminal, reaped children.

## Follow-Up Boundaries
- Direct-route failure for these restricted Bangumi fixtures is expected and is not a repair item or delivery blocker. Keep validation on the configured Web-proxy route.
- Ordinary-video ETag validation is tracked separately in `2026-10-04-etag-content-validation-b46378.md`; the accepted restricted-route behavior does not explain those warnings.
- This validation does not exercise the complete macOS app UI, physical Apple TV, higher-quality variants, or every episode in a series.
- No local formal review, GitHub CI, PR creation, or merge is part of this validation run.

## Evidence
- Canonical inputs: `.agents/skills/bilibili-live-e2e/references/live-cases.json`.
- Full-fill procedure: `.agents/skills/bilibili-live-e2e/references/full-fill-validation.md`.
- Credential operation: `2026-10-02-bilibili-account-credentials-b46376.md`.
- Private, ignored test artifacts: `.codex-tmp/bilibili-playback-20261002/`.
- The interrupted three-case run's recovery root remains at `.codex-tmp/bilibili-playback-20261002/server/.tmpkoyelD` (119,280 KiB, approximately 116 MiB). Its persisted state was not printed; retain it for the ETag investigation rather than claiming the interrupted run completed orderly teardown.
- Compact server logs, the deterministic integration log, native consumer source/tests, and the native probe binary remain for reproduction. The native compilation module cache and Python bytecode cache were removed with `codex-clean-tmp` after the consumer finished.
