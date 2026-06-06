---
id: 20260606-f2a4c7
title: Rust Cache Server Runtime
status: completed
created: 2026-06-06
updated: 2026-06-06
branch: wip/rust-cache-server
pr:
supersedes: []
superseded_by:
---

# Rust Cache Server Runtime

## Summary

- Migrated the LAN cache server runtime from .NET/ASP.NET Core to Rust.
- Kept the existing gRPC proto contract and dual-listener shape: gRPC h2c control plane plus HTTP media endpoint.
- Added Rust formatter/linter gates with `cargo fmt --check` and `cargo clippy -D warnings`.

## Current State

- Rust crate lives at `CacheServer/RustCacheServer`.
- `tonic` generates and hosts the gRPC services from the shared proto.
- `axum` hosts `/media/{itemId}/{variantId}` and serves Range/HEAD responses through the same item-id validation path as gRPC playback source lookup.
- macOS media serving keeps the fail-closed strategy with root-anchored no-follow file opens.
- Bilibili tasks remain an in-memory pre-adapter queue; the real BBDown adapter worker remains a follow-up.

## Evidence

- `cargo check --package tvos-net-player-cache-server`
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo test --package tvos-net-player-cache-server --locked`

