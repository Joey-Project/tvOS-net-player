---
id: 20260621-b6f2a9
title: Bilibili Fetch UX Polish
status: completed
created: 2026-06-21
updated: 2026-06-21
branch: wip/bilibili-fetch-ux-polish
pr:
supersedes:
  - 20260621-c9f0a2
superseded_by:
---

# Bilibili Fetch UX Polish

## Summary

- PR 6 adds shared AppCore presentation state for Bilibili fetch notices without changing the server control plane or media pipeline.
- tvOS and macOS now surface the same notice categories for credential-required pages, empty resolved lists, truncated candidate windows, volatile list/feed results, and retryable upstream failures.
- Candidate selection now has explicit re-resolve and clear-selection actions so macOS validation can refresh live Bilibili list/feed pages without clearing the whole task surface.

## Current State

- `BilibiliTaskViewModel.fetchNotice` classifies the major fetch UX states from resolved input metadata, current task state, and error text.
- `BilibiliTaskViewModel.reResolve(serverAddressText:)` refreshes the current resolved input while keeping the user's source/options as the source of truth.
- `BilibiliTaskViewModel.clearResolvedCandidateSelection()` clears the current selection and leaves the resolved candidate list visible for a fresh choice.
- `TVOSNetPlayer` and `MacOSNetPlayer` both render the shared notice and expose `Re-resolve` / `Clear Selection` controls during candidate selection.

## Next Steps

- Continue with PR 7: ABR metadata foundation.
- Keep physical Apple TV validation deferred; use macOS app validation first for this phase.

## Evidence

- `swift test --filter BilibiliTaskViewModelTests`
- `scripts/format.sh`
- `git diff --check`
- `scripts/lint.sh`
- `just ci`
