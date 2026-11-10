---
id: 20261110-b46398
title: Native Media CI And Regression Gating
status: completed
created: 2026-11-10
updated: 2026-11-10
branch: codex/native-media-ci
pr: https://github.com/Joey-Project/tvOS-net-player/pull/91
supersedes: []
superseded_by:
---

# Native Media CI And Regression Gating

## Approved Scope
- Deliver slice 3B after [native 3A acceptance](2026-11-09-native-abr-recovery-b46397.md) and [PR #90](https://github.com/Joey-Project/tvOS-net-player/pull/90).
- Run the existing credential-free native media suite as an independent macOS GitHub Actions job and include its result in the existing required CI aggregate. Preserve every previous build, lint, test and UI gate.
- Keep automatic recovery-latency optimization after this slice. Do not change player buffering, whole-asset fill, rung selection, production cache behavior or acceptance windows to obtain a CI pass.

## Execution Contract
- The `media-network` job runs on `macos-26`, independently of the Apple and Core jobs. It invokes the existing runner with explicit `--ci-unsigned`, which remains restricted to GitHub Actions; local operator acceptance continues to use the pinned certificate and stable signed products.
- The job has a 70-minute outer limit and a 60-minute absolute work deadline established before checkout. The media step has a 52-minute outer limit around the unchanged 50-minute runner: 25 minutes compilation, 24 minutes execution and one minute finalization. Setup must leave enough work time for those phases; the existing 45-minute generic CI stage wrapper must not enclose the media runner.
- Tool setup, helper regressions, cache operations and artifact upload each have finite limits. A source/toolchain-bound, media-specific Rust cache is optional; missing caches do not skip compilation or acceptance. Only a successful default-branch push may save it.
- The runner retains at most 32 MiB of reports/logs and rejects insufficient free space before generating bounded media. Always attempt diagnostic upload with three-day retention; forced cancellation or a lost runner cannot guarantee retained artifacts.
- No Apple Developer secret, Bilibili account, public proxy, live API or existing server credential profile is required. System mute is set and verified before playback and remains enabled; audio-track evidence does not establish audible audio.

## Acceptance Contract
- Require the exact three existing native tests and strict raw report validation: 600-second future-media high/low/high recovery, 130-second buffered continuity, and executed H.264/AAC transcoding with audio and all controls.
- Require distinct decoded frames and real fault/recovery clocks/traffic. Diagnostic-only cases, skipped/ignored required cases, log-only success and missing reports cannot pass.
- The existing `tvOS build and tests` aggregate passes only if Apple, Core and native media all report success. Failure, cancellation, skipped, empty and unknown outcomes must fail the aggregate.
- Validate workflow shell syntax, actual deadline exports, timeout margins, cache boundaries, upload policy and aggregate outcome combinations through helper regressions included in normal CI.
- Final delivery requires complete local validation, current-head GitHub CI/UI/media reports, current-head GitHub Codex review, all conversations resolved and genuine workflow CODEOWNER approval. Keep transient gate state in PR metadata and private receipts rather than tracked journals.

## Current State
- The independent media job and three-result aggregate are implemented without changing the production playback code or native runner.
- Workflow regressions execute the real deadline export and aggregate shell fragments, covering all 216 success/failure/cancelled/skipped/empty/unknown result combinations. They retain the existing stage/cache/command checks and assert media mode, toolchain identity, budgets and artifact policy.
- The media job explicitly selects Bash so pipeline failures match the regression execution environment. Cache identity includes bounded Apple compiler, Swift, SDK, Clang, OS, Rust and Cargo version output as well as applicable source inputs.

## Next Steps
1. Deliver recovery-latency optimization separately after 3B; retain the historical failed 130-second recovery evidence and the current buffered-content reuse contract.
2. Continue to inspect exact-run native reports during future failures rather than treating a compilation or job label as decoded playback proof. Preserve full App UI, authenticated live, offline, audible-audio and physical-device boundaries.

## Evidence
- [Delivery PR #91](https://github.com/Joey-Project/tvOS-net-player/pull/91).
- [Home LAN roadmap](../10/2026-10-23-home-lan-readiness-roadmap-b46388.md).
- [Native recovery acceptance and historical boundaries](2026-11-09-native-abr-recovery-b46397.md).
- Source thread: `019e7fd0-b463-76f2-a5ab-37d7d77620f4`.
