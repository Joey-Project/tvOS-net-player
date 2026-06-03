---
id: 20260531-a1b2c3
title: Initial tvOS Repository Setup
status: completed
created: 2026-05-31
updated: 2026-06-03
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
- v0 播放工作流选择手动 URL 输入；app 会保存上次 URL，并提供停止、清空和输入校验状态。
- CI workflow `CI` 在 `macos-26` 上运行 pre-commit checks、tvOS simulator build、tvOS XCTest target 和 Swift package core tests。
- `just lint` 运行 shell lint 和 `swift-format lint --strict`；`just format` 运行 `swift-format format --in-place`。
- `just ci` 对齐 GitHub CI，依次覆盖 lint、tvOS simulator build、XCTest bundle compile、tvOS XCTest target 和 Swift package core tests。
- `just build-for-testing` 编译 Xcode XCTest bundle 但不启动 simulator；`just test-tvos` 运行 app target 的 tvOS XCTest；`just test` 运行不依赖 tvOS simulator runtime 的 Swift package tests。
- `just install-hooks` 安装 tracked pre-commit hook，hook 默认调用 `scripts/pre-commit.sh` 的快速 lint 检查。
- 本机部署入口是 `just deploy` / `scripts/deploy-lan.sh`，通过 `DEVELOPMENT_TEAM`、`TVOS_DEVICE_ID` 和可选 `PRODUCT_BUNDLE_IDENTIFIER` 控制签名和设备。

## Next Steps

- 在 Apple TV 实机配对后运行一次 `just deploy`，确认 automatic signing profile、安装和启动都正常。
- 后续选择是否加入固定家庭媒体源、Bonjour 发现，或配套局域网服务端。

## Evidence

- Xcode project: `TVOSNetPlayer.xcodeproj`
- CI workflow: `.github/workflows/ci.yml`
- Deploy script: `scripts/deploy-lan.sh`
- Hook installer: `scripts/install-hooks.sh`
