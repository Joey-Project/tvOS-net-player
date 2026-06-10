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
- `bbdown-core` is pinned to commit `55c764d660996f6547225957d680500b481c31bb`.
- The adapter writes downloads under `Cache:RootPath/Bilibili` by default and stores BBDown archive state beside the task snapshot as `bbdown-archive.json`.
- Startup validation rejects BBDown output paths outside `Cache:RootPath`, `..` parent components, and existing symlink components under that root before handing paths to BBDown.
- Runtime setup canonicalizes existing root/output prefixes before constructing the media library and BBDown adapter, keeping their path boundary checks aligned.
- The real BBDown adapter currently caps effective worker concurrency at `1` because archive updates are serialized around the core download call.
- The adapter lets `bbdown-core` handle planning/download/archive state, then runs server-owned `ffmpeg` muxing into an `.mp4`-suffixed temporary output before publishing `cache-server-playback.mp4` for library indexing. This avoids ffmpeg container inference failures on extensionless core mux temp paths.
- BV/av inputs default to current/first page, while ss/md inputs default to latest episode because the task result schema currently exposes one `library_item_id`.
- Progress is coarse-grained until BBDown core exposes chunk-level progress and cancellation hooks.

## Next Steps

- Add tvOS UI for submitting Bilibili URLs/BV IDs and watching task progress.
- Extend task options/result schema if we want explicit page/episode/all selection or multi-item task results.
- Add retention/cleanup policy for old persisted terminal tasks and downloaded output conflicts.
- Keep live BBDown validation as an opt-in local smoke path rather than default CI, because it depends on public Bilibili availability, network, and local `ffmpeg`.

## Evidence

- `cargo fmt --all -- --check`
- `cargo clippy --package tvos-net-player-cache-server --all-targets --locked -- -D warnings`
- `cargo test --package tvos-net-player-cache-server --locked`
- `just ci`
- Manual live smoke against `https://www.bilibili.com/video/BV15hdwBKEMG`: temporary ignored harness created a Bilibili task, completed download/mux, resolved a library playback source, and fetched HTTP range bytes successfully.
