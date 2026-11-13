---
id: 20261113-b46411
title: Native Harness Diagnostics Prerequisite
status: active
created: 2026-11-13
updated: 2026-11-13
branch: codex/native-harness-diagnostics
pr: https://github.com/Joey-Project/tvOS-net-player/pull/105
supersedes: []
superseded_by:
---

# Native Harness Diagnostics Prerequisite

## Summary
- Add bounded failure attribution to the existing media-acceptance harness before the separate PR #104 work. This slice diagnoses the earlier timeout; it does not claim to fix its root cause.

## Current State
- The prior `ProgressiveAbr` run failed before the probe: fixture, preflight, and probe fields were unset and request count was zero. A later standalone FFmpeg plus six ffprobe sequence passed in about 74.7 seconds, but that does not identify which child failed in the harness.
- Diagnostics now label fixture generation, high/low/audio container and metadata ffprobe boundaries, transcoded-output inspection, and native probe. Child errors include elapsed/configured deadline, executable basename, bounded sanitized stderr, wait outcome, kill result, reap status, and separate stdout/stderr drain state.
- An owned-child guard retains cancellation cleanup while the child is still owned and unreaped. A successful wait disables further drop signaling; a wait error immediately marks identity unverified before any further await, preventing both explicit kill/reap retries and implicit drop signals. The existing timeout kill/reap grace and outer case/probe timeout/abort guards remain; outer abort reports inner cleanup as unverified.
- Focused tests cover phase attribution, bounded diagnostics, timeout/reap outcomes, and owned-child cancellation transitions. Complete current-head delivery evidence is recorded in the associated PR; these focused tests alone do not establish native playback or a root-cause fix.
- Unused standalone diagnostic MP4s are removed after retaining hashes, metadata and bounded logs. Required failed-run evidence and independent validator releases remain separate from disposable fixture data.

## Next Steps
1. Use the stage-labelled ABR error/report and bounded logs to identify any recurring timeout's exact child. Do not infer FFmpeg as the cause from the prior standalone pass; preserve all existing media assertions and deadlines.
2. Apply any fixture/harness correction only from demonstrated failure evidence, separately from cleanup of disposable generated media.
3. Refresh master and renew the independent helper migration in PR #104 before returning to PR #102. Historical failed acceptance is not replaced by a standalone fixture success.

## Evidence
- Source base: `fee2d0cc5ba1cf6ae048303209bf63a32bc86f0a`.
- `cargo test --locked -p tvos-net-player-cache-server media_acceptance_tests::child_process_diagnostic_tests -- --test-threads=1`: 7 passed, 0 failed; 1,350 filtered.
- `cargo fmt --manifest-path CacheServer/RustCacheServer/Cargo.toml -- --check`: passed.
- `cargo clippy --locked -p tvos-net-player-cache-server --all-targets -- -D warnings`: passed.
