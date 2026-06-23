---
id: 20260623-c1e4d8
title: HLS Segment Index Splitting
status: completed
created: 2026-06-23
updated: 2026-06-23
branch: wip/segment-index-hls-splitting
pr:
supersedes:
superseded_by:
---

# HLS Segment Index Splitting

## Summary

- Added conservative fMP4 fragment range and timing parsing for completed cached resources.
- Persisted optional segment byte ranges and durations in HLS cached-resource metadata with legacy fallback.
- Updated HLS media playlist generation to emit multiple `EXTINF`/`EXT-X-BYTERANGE` entries when verified fragment ranges are available.

## Decisions

- Segment splitting is currently completed-cache-only. Runtime upstream playback and first-window prewarm continue to use the existing single-range playlist shape.
- The parser only trusts top-level fMP4 `moof` plus following `mdat` byte ranges with parseable per-track fragment timing and full payload coverage through EOF. Single-fragment files, malformed fragments, missing timing, uncovered trailer boxes, or unsupported layouts fall back to the single-range playlist instead of fabricating boundaries.
- Playlist requests consume persisted segment metadata and do not rescan large cached media files.
- Segment durations come from fMP4 timing metadata; multi-track fragments use the longest parsed track duration, and split playlist output is disabled when timing metadata is unavailable.
- Review hardening added bounded `trun` sample-count parsing so corrupt metadata cannot force unbounded cache-finalization work.

## Validation

- `cargo test -p tvos-net-player-cache-server media_playlist_ -- --nocapture`
- `cargo test -p tvos-net-player-cache-server segment -- --nocapture`
