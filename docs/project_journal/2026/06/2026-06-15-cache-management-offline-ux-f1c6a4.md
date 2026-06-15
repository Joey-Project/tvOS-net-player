---
id: 20260615-f1c6a4
title: Cache Management And Offline UX
status: completed
created: 2026-06-15
updated: 2026-06-15
branch: wip/discovery-cache-ux
pr: https://github.com/Joey-Project/tvOS-net-player/pull/19
supersedes: []
superseded_by:
---

# Cache Management And Offline UX

## Summary

- Implement the existing `CacheService.DeleteLibraryItem` control-plane API for local cache files and completed Bilibili HLS cache items.
- Add Swift cache root and delete bindings, plus shared AppCore state for cache root display and item deletion.
- Improve tvOS and macOS cache library rows with LAN/offline availability labels and user-facing delete actions.

## Current State

- `CacheService.ListCacheRoots` is surfaced through the Swift client and `CacheLibraryViewModel`.
- Local cache items can be deleted from the server-local cache root by their stable library item id when the server explicitly enables library deletion.
- Completed Bilibili HLS virtual items can be deleted with their HLS cache directory and persisted authorization task record when the server explicitly enables library deletion.
- Destructive library deletion defaults off for the cleartext unauthenticated control plane; the Swift clients only show delete actions when `ServerInfo` advertises the delete capability.
- tvOS and macOS show cache root capacity when available and label completed Bilibili HLS items as offline HLS cache entries.
- Bonjour discovery remains split out because it needs server-side mDNS advertisement, client browsing, and Local Network permission UX as a focused follow-up.

## Out Of Scope

- Bonjour/mDNS discovery.
- Automatic cache eviction policy.
- Bilibili task options/result schema changes.
- LAN-side transcoding policy and UI.

## Validation

- `cargo fmt --all`
- `scripts/format.sh`
- `python3 /Users/joey/.codex/personal-sync/overlays/private/releases/bb9b591d6375c3c11482cb4fa99394132419c816/personal_codex/skills/project-journal/scripts/project_journal.py validate --repo /Users/joey/Program/Codex-workspace/tvOS-net-player`
- `git diff --check`
- `cargo test --package tvos-net-player-cache-server --locked delete -- --nocapture`
- `swift test --filter CacheLibrary`
- `cargo test --package tvos-net-player-cache-server --locked supports_cache_roots_rescan_and_bilibili_task_lifecycle -- --nocapture`
- `just ci`

## Next Steps

- Follow up with Bonjour discovery as its own PR.
- Design automatic cache eviction policy after discovery and manual cache management settle.
