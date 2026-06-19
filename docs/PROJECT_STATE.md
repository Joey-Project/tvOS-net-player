# Project State

## Current State

- 仓库现在包含初始 SwiftUI tvOS app、Rust LAN cache server、tvOS gRPC cache client、Bilibili task intake/progressive playback control plane、runtime passthrough HLS media pipeline、durable HLS offline cache manifests/recovery、completed-HLS quota/watermark eviction、可落盘恢复的 server-side task worker、真实 BBDown Rust crate adapter、repo-local Bilibili live e2e skill、Xcode project、Swift package core tests、Xcode XCTest compile gate、CI workflow、`Justfile` 本机 task runner、本机 build/test/deploy 脚本、Swift/Rust formatter/linter 和 pre-commit hook installer，以及 Codex review gate。
- 普通 workstream 状态放在 `docs/project_journal/`，顶层文件只保留 repo-wide 入口。

## Recovery Pointers

- 初始 setup workstream：`docs/project_journal/2026/05/2026-05-31-initial-tvos-setup-a1b2c3.md`
- LAN cache server 控制面 workstream：`docs/project_journal/2026/06/2026-06-04-cache-server-control-plane-b7c9e1.md`
- tvOS cache gRPC client workstream：`docs/project_journal/2026/06/2026-06-05-tvos-cache-grpc-client-c4d8a2.md`
- Bilibili task intake workstream：`docs/project_journal/2026/06/2026-06-06-bilibili-task-intake-e5f1a9.md`
- Rust cache server runtime workstream：`docs/project_journal/2026/06/2026-06-06-rust-cache-server-runtime-f2a4c7.md`
- Bilibili task worker foundation workstream：`docs/project_journal/2026/06/2026-06-07-bilibili-task-worker-foundation-9a3d2f.md`
- BBDown Rust adapter workstream：`docs/project_journal/2026/06/2026-06-09-bbdown-rust-adapter-b4e2c8.md`
- HLS progressive cache workstream：`docs/project_journal/2026/06/2026-06-14-hls-progressive-cache-f3a9d1.md`
- BBDown 0.3.0 and product UX roadmap：`docs/project_journal/2026/06/2026-06-15-bbdown-030-product-roadmap-c8f4d2.md`
- Bilibili task UI workstream：`docs/project_journal/2026/06/2026-06-15-bilibili-task-ui-a6b9e4.md`
- Cache library pagination/search workstream：`docs/project_journal/2026/06/2026-06-15-cache-library-pagination-search-d2f8a1.md`
- Task retention cleanup workstream：`docs/project_journal/2026/06/2026-06-15-task-retention-cleanup-e7c2b5.md`
- Discovery/cache/weak-network/Bilibili schema roadmap：`docs/project_journal/2026/06/2026-06-16-discovery-cache-bilibili-roadmap-a9d4c1.md`
- Bilibili live e2e skill：`docs/project_journal/2026/06/2026-06-19-bilibili-live-e2e-skill-6e4b2a.md`
- 本地 journal index 可用 project-journal helper 生成到 `docs/project_journal/INDEX.md`，该文件不提交。

## Global Blockers

- 物理 Apple TV 部署需要本机 Xcode 能看到并配对设备，还需要 `DEVELOPMENT_TEAM` 对应的开发签名权限。

## Notes

- 架构决策：gRPC 只做控制面；媒体面继续使用 `AVPlayer` 可直接播放的 HTTP/HLS/Range URL。
- CI 不保存 Apple Developer secrets；设备刷新只在本机通过 `scripts/deploy-lan.sh` 执行。
- 本地 hook 用 `just install-hooks` 安装；CI 运行同一个 `scripts/pre-commit.sh` 快速检查入口。
- 真实 Bilibili smoke suite 由 `.agents/skills/bilibili-live-e2e/` 维护；默认跑非区域限制 case。Rust LAN cache server 已支持 BBDown credential file 和 restricted-area proxy runtime 配置，番剧区域限制 case 仍需要本机真实 proxy/credential 后再验证。
