---
id: 20260609-b4e2c8
title: BBDown Rust Adapter
status: completed
created: 2026-06-09
updated: 2026-06-10
branch: wip/bbdown-rust-adapter
pr:
supersedes: []
superseded_by:
---

# BBDown Rust Adapter

## Summary

- Connected the LAN cache server's Bilibili task worker to the Rust `bbdown-core` crate from `Joey-Project/BBDown-rust`.
- Kept tvOS on the existing gRPC control-plane and HTTP media-plane contract; Swift still does not link Rust or BBDown directly.
- Added runtime configuration for enabling the worker, selecting worker concurrency, BBDown output/archive paths, and the `ffmpeg` executable path.
- Materialized downloaded Bilibili media as local cache library item ids so completed tasks can be played through the existing HTTP Range endpoint.

## Current State

- `Cache:BilibiliWorkerEnabled` defaults to `true` in normal server runtime; integration tests disable it to keep control-plane tests offline and deterministic.
- `bbdown-core` is pinned to `BBDown-rust` `v0.3.0` tag commit `4905a2f06b8038a979cf4d6078c7dd5f40f6a2d8`; Cargo resolves the `bbdown-core` crate source to `433591b6c14b97a4adfe189e6d8826fa699d8470`.
- The adapter writes downloads under `Cache:RootPath/Bilibili` by default and stores BBDown archive state beside the task snapshot as `bbdown-archive.json`.
- Startup validation rejects enabled or explicitly configured BBDown output paths outside `Cache:RootPath`, `..` parent components, and existing symlink components under that root before handing paths to BBDown.
- Runtime setup canonicalizes existing root/output prefixes before constructing the media library and BBDown adapter, keeping their path boundary checks aligned.
- The real BBDown adapter currently caps effective worker concurrency at `1` because archive updates are serialized around the core download call.
- The adapter lets `bbdown-core` handle planning/download/archive state, then runs server-owned `ffmpeg` muxing into a hidden non-indexed temporary output before publishing a title-preserving `.mp4` for library indexing. This avoids ffmpeg container inference failures without exposing partial mux outputs to library scans.
- The mux phase checks task cancellation before starting and while `ffmpeg` is running; cancellation kills and reaps the child process before returning a cancelled task result.
- BV/av inputs default to current/first page, while ss/md inputs default to latest episode because the task result schema currently exposes one `library_item_id`.
- `BBDown-rust` `v0.3.0` feed/history/watch-later inputs are parsed by the core, and the cache server maps those collection/feed-style inputs to the latest item by default until a richer task result schema exists.
- Progress is coarse-grained until BBDown core exposes chunk-level download progress and native download cancellation hooks.

## Next Steps

- Add tvOS UI for submitting Bilibili URLs/BV IDs and watching task progress.
- Discuss task options/result schema separately if we want explicit page/episode/all selection, feed/history/watch-later workflows, or multi-item task results.
- Add retention/cleanup policy for old persisted terminal tasks and downloaded output conflicts.
- Keep live BBDown validation as an opt-in local smoke path rather than default CI, because it depends on public Bilibili availability, network, and local `ffmpeg`.

## Evidence

- `cargo fmt --all -- --check`
- `cargo clippy --package tvos-net-player-cache-server --all-targets --locked -- -D warnings`
- `cargo test --package tvos-net-player-cache-server --locked`
- `just ci`
- Manual live smoke against `https://www.bilibili.com/video/BV15hdwBKEMG`: temporary ignored harness created a Bilibili task, completed download/mux, resolved a library playback source, and fetched HTTP range bytes successfully.
