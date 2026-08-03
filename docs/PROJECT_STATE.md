# Project State

## Current State

- 仓库现在包含初始 SwiftUI tvOS app、Rust LAN cache server、tvOS/macOS gRPC cache client with LAN plaintext and remote HTTPS endpoint support、macOS validation/operator diagnostics surface、Bilibili task intake/progressive playback control plane、runtime passthrough HLS media pipeline、runtime multi-variant HLS master playlist、durable HLS offline cache manifests/recovery、completed cached-resource fMP4 segment-index HLS playlist splitting、playback-position-aware HLS progress reporting/status foundation、progressive HLS ABR metadata manifests、completed-HLS quota/watermark eviction、LAN transcoding execution MVP、可落盘恢复的 server-side task worker、基于 `BBDown-rust` `v0.5.0` 的真实 BBDown Rust crate adapter、BBDown native download progress/cancellation 映射、Bilibili download options schema 扩展、server-owned BBDown credential status/profile/login-session foundation control plane、Bilibili fetch UX notices/re-resolve/clear-selection actions、repo-local Bilibili live e2e skill with authenticated page-fetch and collection/list fixtures、Xcode project、Swift package core tests、Xcode XCTest compile gate、CI workflow、`Justfile` 本机 task runner、本机 build/test/deploy 脚本、Swift/Rust formatter/linter 和 pre-commit hook installer，以及 Codex review gate。
- 下一阶段 productization roadmap 记录在 `docs/project_journal/2026/06/2026-06-24-next-phase-productization-roadmap-a8d2c5.md`；PR1 macOS validation/operator UX、PR2 playback-position segment scheduling、PR3 weak/offline UX completion 和 PR4 credential profile/login foundation 已完成，后续执行顺序是 authenticated/restricted live validation、transcoding/ABR policy controls，然后在 Bilibili task options/result schema v2 前暂停讨论。
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
- Bilibili task options/result schema roadmap：`docs/project_journal/2026/06/2026-06-19-bilibili-task-schema-roadmap-b7e3f1.md`
- BBDown 0.5 and progressive HLS roadmap：`docs/project_journal/2026/06/2026-06-21-bbdown-050-hls-roadmap-c9f0a2.md`
- BBDown native progress/cancellation workstream：`docs/project_journal/2026/06/2026-06-21-bbdown-native-progress-cancellation-d1a7b4.md`
- Bilibili download options schema workstream：`docs/project_journal/2026/06/2026-06-21-bilibili-download-options-schema-e4c8d9.md`
- Bilibili credential control-plane workstream：`docs/project_journal/2026/06/2026-06-21-bilibili-credential-control-plane-a5d7c3.md`
- Authenticated Bilibili page-fetch live fixtures workstream：`docs/project_journal/2026/06/2026-06-21-authenticated-page-fetch-fixtures-f6a8b0.md`
- Bilibili collection/list fetch coverage workstream：`docs/project_journal/2026/06/2026-06-21-collection-list-fetch-coverage-d3b6e1.md`
- Bilibili fetch UX polish workstream：`docs/project_journal/2026/06/2026-06-21-bilibili-fetch-ux-polish-b6f2a9.md`
- HLS ABR metadata foundation workstream：`docs/project_journal/2026/06/2026-06-22-hls-abr-metadata-foundation-a3c9f4.md`
- HLS multi-variant master workstream：`docs/project_journal/2026/06/2026-06-22-hls-multi-variant-master-e6b4a8.md`
- Playback controls / remote endpoint / HLS execution roadmap：`docs/project_journal/2026/06/2026-06-22-playback-remote-hls-roadmap-f4a7b2.md`
- HLS segment-index splitting workstream：`docs/project_journal/2026/06/2026-06-23-hls-segment-index-splitting-c1e4d8.md`
- Playback-position weak/offline UX workstream：`docs/project_journal/2026/06/2026-06-23-playback-position-weak-offline-ux-d5b7e2.md`
- Next phase productization roadmap：`docs/project_journal/2026/06/2026-06-24-next-phase-productization-roadmap-a8d2c5.md`
- macOS validation/operator UX workstream：`docs/project_journal/2026/06/2026-06-25-macos-validation-operator-ux-c3e9a1.md`
- 本地 journal index 可用 project-journal helper 生成到 `docs/project_journal/INDEX.md`，该文件不提交。

## Global Blockers

- 当前路线图没有全局 blocker。物理 Apple TV 部署验证暂时剔除，后续验证优先使用 macOS app。

## Notes

- 架构决策：gRPC 只做控制面；媒体面继续使用 `AVPlayer` 可直接播放的 HTTP/HLS/Range URL。
- CI 不保存 Apple Developer secrets；设备刷新只在本机通过 `scripts/deploy-lan.sh` 执行。
- 本地 hook 用 `just install-hooks` 安装；CI 运行同一个 `scripts/pre-commit.sh` 快速检查入口。
- 真实 Bilibili smoke suite 由 `.agents/skills/bilibili-live-e2e/` 维护；默认跑稳定的非区域限制、非登录、非 collection/list case。Rust LAN cache server 已支持 BBDown credential file 和 restricted-area proxy runtime 配置，番剧区域限制 case 已用本机私有 credential 和 web-mode restricted API proxy 验证通过；authenticated history、watch-later、following 和 space dynamic cases 需要本机 web cookie 后 opt-in 运行；favorite、space videos、collection、series 和 recommendations collection/list cases 需要显式 opt-in，其中认证型 list/feed 仍需要 web cookie，标记为 `requires_live_sample_override` 的 favorite/series 样例需要 env override 后才进入未过滤 smoke；客户端只能读取 server-owned credential readiness/status，不读取 credential 路径或 secret 值。
