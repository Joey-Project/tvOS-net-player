---
id: 20260607-9a3d2f
title: Bilibili Task Worker Foundation
status: completed
created: 2026-06-07
updated: 2026-06-07
branch: wip/task-worker-foundation
pr:
supersedes: []
superseded_by:
---

# Bilibili Task Worker Foundation

## Summary

- Added the server-side worker foundation for Bilibili cache tasks without wiring a real downloader yet.
- Added persisted task state so submitted, terminal, and restart-interrupted tasks survive cache server restarts.
- Kept the tvOS contract unchanged: gRPC remains the control plane, media remains HTTP/HLS/Range, and Swift does not link Rust or BBDown directly.
- Prepared the Rust server for a crate-first BBDown adapter running inside the Mac mini cache server process.

## Current State

- `BilibiliTaskRegistry` now keeps an internal FIFO queue for newly submitted Bilibili tasks.
- Worker-facing APIs can claim queued tasks, mark them running, report progress, complete them as succeeded/failed/cancelled, and expose cancellation tokens to running adapters.
- `bilibili_worker` defines `BilibiliDownloadAdapter`, `BilibiliDownloadRequest`, `BilibiliDownloadContext`, and a bounded worker loop.
- `TaskStateStore` writes lifecycle snapshots to `Cache:TaskStatePath`; restart recovery keeps queued tasks queued, requeues `running` tasks, and restores `cancel_requested` tasks as `cancelled`.
- Lifecycle snapshot write/fsync is serialized outside the task registry mutex with generation ordering, so stale saves cannot overwrite newer task state.
- `AppState` exposes `spawn_bilibili_task_worker` for future runtime wiring.
- The default server runtime does not start a worker yet, so submitted tasks remain queued until a real adapter is configured.

## Next Steps

- Implement the real BBDown Rust crate adapter behind `BilibiliDownloadAdapter`.
- Normalize adapter outputs into cache-root library items and HTTP/HLS playback variants.
- Add retention/cleanup policy for old persisted terminal tasks.
- Add tvOS TaskService client and UI for submitting Bilibili URLs/BV IDs and watching task progress.

## Evidence

- `cargo test --package tvos-net-player-cache-server --locked`
- `just ci`
