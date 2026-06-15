---
id: 20260615-c8f4d2
title: BBDown 0.3.0 And Product UX Roadmap
status: active
created: 2026-06-15
updated: 2026-06-15
branch: wip/bbdown-030-bump
pr:
supersedes: []
superseded_by:
---

# BBDown 0.3.0 And Product UX Roadmap

## Summary

- Continue from the merged shared AppCore and macOS frontend foundation.
- Upgrade the LAN cache server to `BBDown-rust` `v0.3.0` before building the next client workflows.
- Keep tvOS as the primary product surface and keep macOS feature parity for debugging and light desktop use.
- Defer Bilibili page/episode/all selection and multi-item result schema until the basic task UI has been exercised.

## Current State

- `master` contains the shared AppCore refactor and macOS frontend from PR #13 and PR #14.
- `bbdown-core` is pinned to `BBDown-rust` `v0.3.0` for the LAN cache server.
- The LAN cache server already owns BBDown integration, progressive HLS playback, and offline HLS cache finalization.
- Swift clients already expose `CreateBilibiliPlaybackTask`, `GetTask`, and `WatchTasks`.
- AppCore currently exposes LAN cache server/library/playback state, but not a Bilibili task submission/progress workflow.
- Library browsing still loads a bounded first-page preview of up to 200 items.
- Persistent task state has no retention/cleanup policy yet.

## PR Plan

### PR 0: Bump BBDown-rust 0.3.0

- Status: implemented by this slice.
- Update `bbdown-core` to `BBDown-rust` `v0.3.0`.
- Fix adapter compilation/API compatibility.
- Preserve the existing progressive HLS and complete-download behavior.
- Update architecture and journal docs for the new core version and newly available feed/history/watch-later input parsing.
- Run full local CI, `independent-codex-pr-review`, `offline-frozen-diff-review`, GitHub CI/review-gate, and resolve all PR conversations before merge.

### PR 1: Bilibili Task UI

- Add shared AppCore state for Bilibili URL/BV submission, progress watching, cancellation/retry, and playable-source handoff.
- Wire the same workflow into tvOS and macOS app shells.
- Use the current default selection behavior; do not expand page/episode/all schema in this PR.
- Keep media bytes on the LAN cache server HTTP/HLS plane.

### PR 2: LAN Library Pagination And Search

- Replace the 200-item preview with explicit page/search state in AppCore.
- Add tvOS and macOS search plus load-more controls.
- Keep the existing gRPC page-token contract.

### PR 3: Task Retention And Cleanup

- Add configurable retention for persisted terminal task state.
- Clean up old terminal tasks and handle downloaded output conflicts deterministically.
- Add Rust tests that cover retention, restart recovery, and cleanup boundaries.

### PR 5: Discovery, Cache Management, And Weak-Network UX

- Add Bonjour discovery so clients can find the LAN cache server without manual address entry.
- Add cache management and eviction APIs/UI for local and completed HLS items.
- Improve weak-network/offline playback states and user actions in tvOS and macOS.

## Deferred

- Bilibili task options/result schema for explicit page/episode/all selection and multi-item results.
- LAN-side transcoding policy and UI.
- Apple TV physical-device deployment validation until hardware pairing/signing is available.

## Next Steps

- Continue with PR 1, the shared Bilibili task UI for tvOS and macOS.
- After each PR merge, update `master` and branch the next PR from the new `master`.

## Evidence

- PR #13: https://github.com/Joey-Project/tvOS-net-player/pull/13
- PR #14: https://github.com/Joey-Project/tvOS-net-player/pull/14
- `BBDown-rust` `v0.3.0` tag: `4905a2f06b8038a979cf4d6078c7dd5f40f6a2d8`
- PR 0 focused validation:
  - `cargo update -p bbdown-core`
  - `cargo fmt --all -- --check`
  - `cargo clippy --package tvos-net-player-cache-server --all-targets --locked -- -D warnings`
  - `cargo test --package tvos-net-player-cache-server --locked`
  - `git diff --check`
  - `python3 /Users/joey/.codex/personal-sync/overlays/private/releases/bb9b591d6375c3c11482cb4fa99394132419c816/personal_codex/skills/project-journal/scripts/project_journal.py validate --repo /Users/joey/Program/Codex-workspace/tvOS-net-player`
  - `just ci`
  - `scripts/pre-commit.sh`
