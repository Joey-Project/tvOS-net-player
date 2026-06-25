---
id: 20260625-c3e9a1
title: macOS Validation Operator UX
status: completed
created: 2026-06-25
updated: 2026-06-25
branch: wip/macos-validation-operator-ux
pr:
supersedes:
superseded_by:
---

# macOS Validation Operator UX

## Summary

- Completed Next PR 1 from the next-phase productization roadmap.
- Added a shared AppCore diagnostics model that checks the LAN cache server and turns operator-facing state into stable presentation rows.
- Added a macOS diagnostics panel so macOS can be used as the primary validation/operator client before physical Apple TV validation resumes.
- Kept secrets server-owned: the client shows readiness and counts, not credential paths or secret values.

## Delivered

- Swift cache client now exposes the existing `CheckHealth` server RPC as `checkHealth()`.
- Shared diagnostics cover server identity, health, capabilities, Bilibili credential readiness, restricted-area proxy readiness, live validation readiness, cache storage, HLS cache quota/watermarks, weak-network state, transcoding runtime, and active/recent playback signal.
- Optional diagnostics fail per row instead of failing the whole panel, so older servers still show useful server-level information.
- macOS app refreshes diagnostics after successful cache refresh and also exposes an explicit `Refresh Diagnostics` action.

## Validation

- `swift test --filter CacheServerDiagnosticsViewModelTests`
- `scripts/build-macos.sh`
- `scripts/test-macos.sh`

## Next

- Continue with Next PR 2: playback-position-aware segment scheduling.
