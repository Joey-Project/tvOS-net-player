---
id: 20260604-b7c9e1
title: LAN Cache Server Control Plane
status: active
created: 2026-06-04
updated: 2026-06-06
branch: architecture-cache-control-plane
pr:
supersedes: []
superseded_by:
---

# LAN Cache Server Control Plane

## Summary

- 将下一阶段拆成 LAN cache server 和 Bilibili remote playback 两部分。
- 明确 tvOS 只通过 gRPC 调控制面；媒体面继续使用 `AVPlayer` 可直接播放的 HTTP/HLS/Range URL。
- BBDown 作为 Mac mini cache server 背后的 Bilibili adapter，不直接暴露给 tvOS app。

## Current State

- 架构文档在 `docs/architecture/cache-server.md`。
- 控制面 proto 草案在 `Sources/TVOSNetPlayerCacheClient/Protos/tvos_net_player/v1/cache_control.proto`。
- 第一片 LAN cache server MVP 已实现：本地缓存目录扫描、gRPC library/server/cache services、面向 macOS/Mac mini 的 HTTP Range playback endpoint、integration test harness。
- server runtime 已从 .NET/ASP.NET Core 迁移到 Rust/tonic/axum；迁移记录见 `docs/project_journal/2026/06/2026-06-06-rust-cache-server-runtime-f2a4c7.md`。
- tvOS app 已提升到 tvOS 18.0 并接入 gRPC Swift 2 cache client slice；详情见 `docs/project_journal/2026/06/2026-06-05-tvos-cache-grpc-client-c4d8a2.md`。

## Next Steps

- 后续再接 BBDown adapter：提交 Bilibili URL/BV，下载完成后入库。
- Bonjour discovery 和边下边播/HLS progressive cache 放到基础链路稳定之后。

## Evidence

- Local BBDown server API: `/Users/joey/Program/Codex-workspace/BBDown/json-api-doc.md`
- Local BBDown implementation: `/Users/joey/Program/Codex-workspace/BBDown/BBDown/BBDownApiServer.cs`
