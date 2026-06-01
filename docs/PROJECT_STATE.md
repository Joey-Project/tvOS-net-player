# Project State

## Current State

- 仓库现在包含初始 SwiftUI tvOS app、Xcode project、Swift package core tests、Xcode XCTest compile gate、CI workflow、本机 build/test/deploy 脚本、Swift formatter/linter 和 pre-commit hook installer，以及 Codex review gate。
- 普通 workstream 状态放在 `docs/project_journal/`，顶层文件只保留 repo-wide 入口。

## Recovery Pointers

- 初始 setup workstream：`docs/project_journal/2026/05/2026-05-31-initial-tvos-setup-a1b2c3.md`
- 本地 journal index 可用 project-journal helper 生成到 `docs/project_journal/INDEX.md`，该文件不提交。

## Global Blockers

- 物理 Apple TV 部署需要本机 Xcode 能看到并配对设备，还需要 `DEVELOPMENT_TEAM` 对应的开发签名权限。

## Notes

- CI 不保存 Apple Developer secrets；设备刷新只在本机通过 `scripts/deploy-lan.sh` 执行。
- 本地 hook 用 `make install-hooks` 安装；CI 运行同一个 `scripts/pre-commit.sh` 快速检查入口。
