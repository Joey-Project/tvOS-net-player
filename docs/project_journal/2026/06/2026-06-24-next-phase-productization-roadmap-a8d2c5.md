---
id: 20260624-a8d2c5
title: Next Phase Productization Roadmap
status: active
created: 2026-06-24
updated: 2026-06-25
branch: wip/next-phase-roadmap
pr:
supersedes:
  - 20260622-f4a7b2
superseded_by:
---

# Next Phase Productization Roadmap

## Summary

- Carry the next productization phase as sequential PRs after the playback controls, remote endpoint, HLS execution, segment-index splitting, playback-progress UX, and batch-cache/download-options work.
- Keep tvOS as the primary product target and macOS as the validation/operator client.
- Keep the Rust LAN cache server as the only Bilibili/media integration point visible to clients. tvOS/macOS continue to receive server-owned HTTP/HLS/Range playback URLs and do not talk to Bilibili media URLs directly.
- Keep physical Apple TV validation deferred. Use macOS app validation first for credential, restricted-area proxy, HLS, weak-network, and transcoding policy work.
- Move transcoding/ABR policy controls before the Bilibili task options/result schema v2 work. This intentionally keeps the user-approved PR numbering non-linear: execute PR 7 before the deferred/non-sequential PR 6 schema discussion.

## Current State

- PRs through `Batch Cache Finalization And Sidecar Options UX` are complete on the repository default branch advertised by `origin/HEAD`.
- The app supports Bonjour/manual/remote HTTPS cache endpoints, shared tvOS/macOS player controls, macOS validation/operator diagnostics, progressive Bilibili HLS playback, completed-HLS offline cache, quota/watermark eviction, adaptive weak-network policy, playback-position reporting, LAN transcoding execution MVP, segment-index playlist splitting, multi-result Bilibili selection, and complete-download sidecar/options controls.
- The live e2e skill has canonical ordinary video, multi-part video, Bangumi media, Bangumi episode, authenticated page-fetch, and collection/list fixture definitions. Restricted and authenticated cases remain opt-in because they depend on local credentials, proxy availability, and account state.
- `docs/PROJECT_TODO.md` is the cross-workstream backlog entrypoint. This journal is the durable plan for the next PR sequence.

## PR Plan

### PR 0: Tracker Cleanup And Roadmap Note

- Status: completed by this roadmap PR.
- Mark the completed batch-cache/download-options PR as completed in `docs/PROJECT_TODO.md`.
- Add this roadmap journal as the next-phase recovery entrypoint.
- Keep this PR docs-only so future work starts from a clean tracker state.

### PR 1: macOS Validation Operator UX

- Status: completed by the `wip/macos-validation-operator-ux` slice.
- Added shared AppCore diagnostics state for server info, health, capabilities, credential readiness, restricted-area proxy readiness, cache root/quota summary, HLS cache status, weak-network state, transcoding runtime state, playback signal, and live validation readiness without exposing secret paths or secret values.
- Added a macOS diagnostics panel with explicit refresh/recheck controls.
- Preserved tvOS behavior; macOS is the richer validation surface for this slice.

### PR 2: Playback-Position-Aware Segment Scheduling

- Use the existing app-to-server playback progress signal to influence HLS segment fill and prefetch order.
- Prioritize init data and the playable window around active/recent playback position.
- Demote distant segments and stale playback windows under cache pressure.
- Re-prioritize cleanly after seek, playback stop, and playback-source changes.
- Keep protocol changes minimal; consume the existing `ReportPlaybackProgress` and `GetHlsCacheStatus` foundation where possible.

### PR 3: Weak/Offline UX Completion

- Complete shared tvOS/macOS presentation for weak/offline playback states.
- Surface cache-only, partially cached, quota-blocked, upstream-failed, retrying, variant downgraded, and recovery states consistently.
- Make per-item and global cache status understandable from the app without requiring CLI logs.
- Keep this PR focused on UX/state semantics, not scheduler internals.

### PR 4: Credential Profile And Login Foundation

- Add server-owned credential profile/login foundation while keeping secrets local to the Mac mini/server.
- Continue exposing only redacted readiness/status to clients.
- Prefer a control-plane shape that can later support QR/web login sessions, profile selection, and credential refresh without changing media playback.
- If a full QR login flow is not safe or stable in this slice, land the profile/status/session foundation first and document the remaining login handoff.

### PR 5: Authenticated And Restricted Live Validation Expansion

- Expand opt-in live validation through the LAN cache server and macOS validation path.
- Cover authenticated history, watch-later, following/dynamic feeds, restricted Bangumi media/episode, and collection/list cases.
- Classify failures as credential, proxy, account-state, upstream availability/schema, or server bug.
- Keep these cases outside default CI unless they become stable and credential-free.

### PR 7: Transcoding And ABR Policy Controls

- Status: pending; intentionally executed before deferred PR 6.
- Add product-level controls and policy surfaces for transcoding and ABR behavior before schema v2 work.
- Cover automatic/manual transcoding preference, compatible-variant preference, weak-network downgrade/upgrade policy, and conservative AVPlayer-safe defaults.
- Keep expensive quality heuristics and broad policy automation incremental and testable.
- Validate through macOS app and deterministic server tests before considering physical Apple TV validation.

### PR 6: Bilibili Task Options And Result Schema v2

- Status: deferred and non-sequential until explicit schema discussion.
- Discuss schema before implementation.
- Candidate scope includes per-result artifacts, subtitle/danmaku/cover metadata, episode/page identity, library handoff, and result-level task outputs.
- Avoid turning this into a generic task-system rewrite unless the schema discussion shows a concrete need.

## Validation Contract

- Each implementation PR starts from the updated repository default branch advertised by `origin/HEAD` and lands on a focused `wip/<topic>` branch.
- Each PR must pass the full local gate, including `just ci`, plus relevant focused tests or live/macOS validation called out by that PR.
- Each PR must pass GitHub CI and required repository checks.
- Each PR must complete the requested review gates before merge:
  - GitHub `codex/review-gate` when present, required, or explicitly triggered for the PR.
  - `independent-codex-pr-review`.
  - `offline-frozen-diff-review`.
- All actionable PR comments and unresolved conversations must be addressed or resolved before merge.
- After each merge, update the local branch that tracks `origin/HEAD` before branching the next PR.
- Pause before PR 6 and discuss the schema before implementing it.

## Deferred

- Physical Apple TV deployment and real-device playback validation.
- Full LAN control-plane auth/TLS hardening beyond remote endpoint transport needs.
- Bilibili task options/result schema v2 implementation until the schema is explicitly discussed.

## Evidence

- PR 1 focused validation: `swift test --filter CacheServerDiagnosticsViewModelTests`, `scripts/build-macos.sh`, `scripts/test-macos.sh`.
- Previous roadmap: `docs/project_journal/2026/06/2026-06-22-playback-remote-hls-roadmap-f4a7b2.md`
- Current architecture note: `docs/architecture/cache-server.md`
- Current repo state entrypoint: `docs/PROJECT_STATE.md`
- Current backlog entrypoint: `docs/PROJECT_TODO.md`
