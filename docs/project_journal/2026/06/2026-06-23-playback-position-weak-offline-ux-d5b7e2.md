---
id: 20260623-d5b7e2
title: Playback Position Weak Offline UX
status: completed
created: 2026-06-23
updated: 2026-06-23
branch: wip/playback-position-weak-offline-ux
pr:
supersedes:
superseded_by:
---

# Playback Position Weak Offline UX

## Summary

- Added `CacheService.ReportPlaybackProgress` for advisory app-to-server HLS playback position reporting.
- Recorded active/recent HLS playback position in the Rust LAN cache server and surfaced it through `GetHlsCacheStatus.playback`.
- Extended shared Swift cache models and gRPC client support for playback progress reports and status snapshots.
- Wired tvOS/macOS shared `PlayerViewModel` playback start, seek, stop, and periodic progress reporting for cache/Bilibili playback contexts.
- Surfaced active/recent playback position in the cache status summary alongside quota, weak-network, and LAN transcoding state.

## Decisions

- Playback progress is best-effort control-plane metadata. AVPlayer playback never blocks on the report, and unsupported older servers are ignored by the client.
- Reports carry both playback URL and optional library item / variant identifiers. Completed offline HLS library item IDs take precedence; otherwise the server parses `/hls/{session_id}/master.m3u8`.
- The server keeps playback progress in memory with short active/recent TTLs. It also refreshes the existing HLS playback lease so automatic eviction treats reported playback as recent use.
- This PR establishes the stable position signal and UX surface. Segment-level fill/prefetch reordering remains a follow-up that can consume this signal without changing the app protocol again.

## Validation

- `cargo fmt --manifest-path CacheServer/RustCacheServer/Cargo.toml`
- `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml hls_playback_progress --lib`
- `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml hls_cache_status --lib`
- `swift test --filter PlayerViewModelTests/testTransientLoadCanReportPlaybackProgressWithContext`
- `swift test --filter CacheLibraryViewModelTests/testHLSCacheSummaryIncludesPlaybackPositionStatus`
- `swift test --filter CacheLibraryViewModelTests/testHLSCacheSummaryIncludesRecentlyStoppedPlaybackPositionStatus`
