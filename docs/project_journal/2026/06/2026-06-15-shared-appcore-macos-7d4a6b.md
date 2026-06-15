---
id: 20260615-7d4a6b
title: Shared AppCore And macOS Frontend
status: active
created: 2026-06-15
updated: 2026-06-15
branch: wip/shared-appcore-refactor
pr:
supersedes: []
superseded_by:
---

# Shared AppCore And macOS Frontend

## Summary

- tvOS remains the primary product surface.
- macOS should reach feature parity with tvOS for LAN cache browsing, playback, and Bilibili task workflows, mainly as a desktop debugging and light-use client.
- Shared behavior should move into a platform-neutral AppCore layer before adding the macOS app target.

## Current State

- `TVOSNetPlayerCore` contains `StreamURLNormalizer` plus shared AppCore playback and cache-library view models.
- `TVOSNetPlayerCacheClient` already provides the reusable gRPC/cache model layer for tvOS and future macOS code.
- tvOS app-owned files keep tvOS-specific SwiftUI views, focus behavior, and app entrypoint code.
- tvOS app and XCTest targets link `TVOSNetPlayerCore` to verify the shared AppCore product is usable from the app bundle.
- `TVOSNetPlayer/ContentView.swift` and `TVOSNetPlayer/TVOSNetPlayerApp.swift` should remain tvOS-specific UI/app-shell code.

## PR Plan

### PR 1: Extract Shared AppCore

- Move platform-neutral playback and cache-library view models into `TVOSNetPlayerCore`.
- Keep tvOS UI behavior unchanged.
- Keep tvOS-specific SwiftUI views, focus behavior, and app entrypoint in the tvOS app target.
- Add or move tests so AppCore behavior is covered through Swift package tests where practical.
- Run full local CI, `independent-codex-pr-review`, `offline-frozen-diff-review`, GitHub CI/review-gate, and resolve all PR conversations before merge.

### PR 2: Add macOS Frontend

- Add a macOS app target that reuses `TVOSNetPlayerCore` and `TVOSNetPlayerCacheClient`.
- Match tvOS functional scope: server URL, library browsing, playback source handling, and Bilibili task flow as it exists at that point.
- Use a desktop-oriented SwiftUI shell without expanding product scope beyond tvOS parity.
- Run full local CI, `independent-codex-pr-review`, `offline-frozen-diff-review`, GitHub CI/review-gate, and resolve all PR conversations before merge.

## Next Steps

- Complete the PR 1 gate: GitHub PR, GitHub CI, triple review, and all conversations resolved.
- Merge PR 1, update `master`, then branch PR 2 from the updated `master`.
- Resume broader TODOs only after the shared AppCore and macOS parity work lands.

## Evidence

- Current `master`: `a040e04 Add offline HLS cache finalization (#12)`.
- Current shared Swift targets: `Package.swift`.
- Current tvOS app sources: `TVOSNetPlayer/`.
- PR 1 local validation on 2026-06-15:
  - `just ci`
  - `just format`
  - `scripts/pre-commit.sh`
  - `git diff --check`
  - `plutil -lint TVOSNetPlayer.xcodeproj/project.pbxproj`
  - `project_journal.py validate --repo /Users/joey/Program/Codex-workspace/tvOS-net-player`
