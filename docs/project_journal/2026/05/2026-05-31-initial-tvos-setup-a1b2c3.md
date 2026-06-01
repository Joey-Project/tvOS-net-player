---
id: 20260531-a1b2c3
title: Initial tvOS Repository Setup
status: completed
created: 2026-05-31
updated: 2026-06-01
branch:
pr:
supersedes: []
superseded_by:
---

# Initial tvOS Repository Setup

## Summary

- 建立最小 SwiftUI tvOS app、Xcode project、Swift package core tests、tvOS XCTest target、CI workflow、本机 Apple TV LAN deploy 脚本、formatter/linter 和 pre-commit hook。

## Current State

- app target `TVOSNetPlayer` 使用 SwiftUI + `AVPlayer` 播放 HTTP/HTTPS URL。
- CI workflow `CI` 在 `macos-26` 上运行 pre-commit checks、tvOS simulator build、Xcode XCTest bundle compile 和 Swift package core tests。
- `make lint` 运行 shell lint 和 `swift-format lint --strict`；`make format` 运行 `swift-format format --in-place`。
- `make build-for-testing` 编译 Xcode XCTest bundle 但不启动 simulator；`make test` 运行不依赖 tvOS simulator runtime 的 Swift package tests；`make test-tvos` 保留给本机或 runner 有 tvOS simulator runtime 时使用。
- `make install-hooks` 安装 tracked pre-commit hook，hook 默认调用 `scripts/pre-commit.sh` 的快速 lint 检查。
- 本机部署入口是 `make deploy` / `scripts/deploy-lan.sh`，通过 `DEVELOPMENT_TEAM`、`TVOS_DEVICE_ID` 和可选 `PRODUCT_BUNDLE_IDENTIFIER` 控制签名和设备。

## Next Steps

- 在 Apple TV 实机配对后运行一次 `make deploy`，确认 automatic signing profile、安装和启动都正常。
- 选择首个真实网络播放工作流，再扩展 UI、错误处理和媒体源发现。

## Evidence

- Xcode project: `TVOSNetPlayer.xcodeproj`
- CI workflow: `.github/workflows/ci.yml`
- Deploy script: `scripts/deploy-lan.sh`
- Hook installer: `scripts/install-hooks.sh`
