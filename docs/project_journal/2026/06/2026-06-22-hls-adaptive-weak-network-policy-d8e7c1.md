---
id: 20260622-d8e7c1
title: HLS Adaptive Weak-Network Policy
status: completed
created: 2026-06-22
updated: 2026-06-22
branch: wip/hls-adaptive-weak-network-policy
pr:
supersedes: []
superseded_by:
---

# HLS Adaptive Weak-Network Policy

## Summary
- PR 10 adds a LAN-server-owned weak-network policy for progressive HLS playback.
- Physical Apple TV validation remains deferred; macOS app/shared AppCore status is the validation surface for this slice.

## Current State
- The Rust cache server tracks short-lived HLS upstream retry, slow-response, failed-upstream, and cache-only states per playback session and variant.
- HLS master playlists use the policy to hide unhealthy variants temporarily. If every advertised variant is unhealthy, the server keeps the lowest-bandwidth variant so the master playlist is never empty.
- Existing media playlist and segment routes still resolve all runtime variants, so stale AVPlayer requests can continue while refreshed master playlists prefer healthier lower variants.
- `GetHlsCacheStatus` now includes redacted weak-network policy status for clients.
- Shared AppCore cache summary appends active weak-network policy messages for macOS/tvOS clients.

## Next Steps
- PR 11 should add the LAN transcoding foundation as the remaining planned HLS infrastructure slice.
- Later weak-network work can add playback-position-aware segment scheduling and richer per-item UI once the media pipeline reports playback position.

## Evidence
- `cargo fmt --manifest-path CacheServer/RustCacheServer/Cargo.toml`
- `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server hls_network_policy::tests --lib`
- `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server hls::tests::master_playlist_filter --lib`
- `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server get_hls_cache_status_reports --lib`
- `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server hls_master_playlist_demotes_unhealthy_variant_from_network_policy --lib`
- `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server --lib`
- `swift test --filter CacheLibraryViewModelTests/testHLSCacheSummaryIncludesWeakNetworkPolicyStatus`
- `swift test`
- `git diff --check`
- `just ci`
