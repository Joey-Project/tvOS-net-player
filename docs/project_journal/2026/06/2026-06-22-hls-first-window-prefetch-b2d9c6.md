---
id: 20260622-b2d9c6
title: HLS First Window Prefetch
status: completed
created: 2026-06-22
updated: 2026-06-22
branch: wip/hls-segment-fill-prefetch
pr:
supersedes: []
superseded_by:
---

# HLS First Window Prefetch

## Summary
- PR 9 replaces the fixed 1 MiB HLS prewarm range with a bandwidth-aware first-playback-window prefetch target.
- The cache server still treats completed offline HLS as whole selected resources; partial prewarm remains a cache/read optimization and does not make a session complete.

## Current State
- HLS prewarm now requests `1 MiB + 30 seconds` of the resource bitrate, capped at 8 MiB and clamped to known resource size when BBDown provides one.
- Prewarmed resource metadata records the requested target prefix and target window seconds while preserving backwards compatibility with older `.prewarm.json` sidecars.
- Existing prewarm sidecars are reused only when their prefix already covers the current first-window target; shorter legacy prefixes are upgraded while preserving the old prefix if the replacement fetch fails.
- The HTTP media path continues to serve byte ranges from prewarmed partial resources when the requested range is fully covered, then falls back to upstream proxying otherwise.
- The background finalizer task status now reports first playback window prefetch separately from later full offline cache fill.

## Next Steps
- PR 10 should build adaptive weak-network policy on top of this partial-cache foundation.
- True fMP4 segment-index playlist splitting remains future work; this PR avoids fabricating HLS segment boundaries without `sidx`/`moof` range metadata.

## Evidence
- `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server hls_cache::tests:: --lib`
- `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server --lib`
