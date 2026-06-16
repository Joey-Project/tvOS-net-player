---
id: 20260616-c2d8e4
title: HLS Cache Quota Watermarks
status: completed
created: 2026-06-16
updated: 2026-06-16
branch: wip/hls-cache-quota-watermarks
pr: 21
supersedes: []
superseded_by:
---

# HLS Cache Quota Watermarks

## Summary

- Implemented PR B from the discovery/cache/weak-network roadmap.
- Added completed-HLS cache quota settings with defaults of 50 GiB max, 90% high watermark, and 80% low watermark.
- Added automatic oldest-first cleanup for eligible completed HLS sessions before HLS finalization and through a periodic server monitor.
- Added `CacheService.GetHlsCacheStatus` so Swift clients can read quota settings, completed-HLS usage, and the last eviction attempt summary.

## Decisions

- Automatic eviction remains HLS-only and completed-session-only in this slice.
- `Cache:HlsCacheMaxBytes=0` disables automatic eviction, while watermark values still validate.
- Cleanup uses projected bytes before finalization when BBDown metadata includes complete resource sizes, then rechecks after finalization using actual cached size so unknown-size sessions are still enforced. Startup restore shortcuts for already-complete HLS resources run the same post-finalization quota check.
- Eviction skips protected/current progressive playback work, the session being finalized, recently issued/served completed playback sources, and incomplete sessions. Eligible completed sessions delete the HLS session directory and matching completed playback task record together.
- If cancellation wins before or during pre-finalization quota enforcement, the server stops before deleting unrelated completed HLS items for that cancelled task.
- If protected/projected bytes make the low-watermark target unreachable, the server records an eviction attempt but avoids wiping unrelated completed cache entries.
- If task-state persistence is unavailable after a malformed snapshot, missing tasks are not treated as orphan authorization for deletion. Missing HLS cache roots scan as empty cache.
- The Swift cache client exposes status as a read-only model without adding UI yet. Weak-network and cache-management UX belongs to the next PR.

## Validation

- `cargo fmt --all`
- `swift format lint --recursive Sources TVOSNetPlayer MacOSNetPlayer Tests TVOSNetPlayerTests MacOSNetPlayerTests`
- `cargo test --package tvos-net-player-cache-server hls_cache`
- `cargo test --package tvos-net-player-cache-server hls_cache_quota`
- `cargo test --package tvos-net-player-cache-server app_state_restore_shortcut_enforces_quota_after_completed_hls_cache_restart`
- `cargo test --package tvos-net-player-cache-server missing_hls_cache_root_scans_as_empty_cache`
- `cargo test --package tvos-net-player-cache-server get_hls_cache_status`
- `cargo test --package tvos-net-player-cache-server hls_eviction_policy`
- `swift test --filter CacheLibraryPaginationTests`

## Next Steps

- Branch PR C for progressive weak-network scheduler/prewarm UX.
