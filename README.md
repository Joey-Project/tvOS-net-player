# TVOS Net Player

这是一个准备给家用 Apple TV 自签名使用的 tvOS 网络播放器仓库。当前版本提供手动播放和 LAN cache control-plane 第一片：

- SwiftUI tvOS app target：输入 HTTP/HTTPS 地址后用 `AVPlayer` 播放。
- 手动 URL v0 工作流：保存上次播放地址，支持停止、清空和输入校验。
- LAN cache client v0：输入 Mac mini cache server 地址，刷新 gRPC library 首屏预览，选择可播放 variant，并把 HTTP/HLS playback source 交给 `AVPlayer`。
- LAN cache server MVP：用 gRPC 提供控制面，用 HTTP Range endpoint 提供 `AVPlayer` 可播放媒体 URL。
- Swift package tests：覆盖 core URL 规范化和 cache client model/pagination 行为，不依赖本机 tvOS simulator runtime。
- GitHub Actions CI：pre-commit checks、tvOS simulator build、LAN cache server build、tvOS XCTest target、core tests、cache server integration tests。
- LAN 刷新脚本：从这台 Mac build/sign，然后通过 `devicectl` 安装到同一局域网内已配对的 Apple TV。
- Codex review gate：保留模板仓库已有的 `codex/review-gate` workflow。

## 本地环境

- Xcode 26 或更新版本。
- `just` task runner。
- Apple TV 开启开发者模式，并在 Xcode / Devices and Simulators 中和这台 Mac 配对。
- Xcode 里登录 Apple ID；物理设备安装时需要能生成 tvOS development provisioning profile。

## 常用命令

```bash
just ci
just lint
just format
just build
just build-cache-server
just build-for-testing
just test-tvos
just test
just test-cache-server
```

`just lint` 会运行 shell 脚本语法检查、`shellcheck`（如果已安装）、`swift-format lint --strict` 和 `dotnet format --verify-no-changes`。`just format` 会用仓库根目录的 `.swift-format` 原地格式化 Swift 源码，并用 `dotnet format` 格式化 cache server C# 源码。

`just build` 默认使用 `generic/platform=tvOS Simulator`，不会要求本机配置签名。`just build-for-testing` 会编译 Xcode XCTest bundle，但不会启动 simulator。`just test` 运行 Swift package core/cache client tests，不需要本机安装 tvOS simulator runtime。

如果要跑 Xcode/tvOS simulator XCTest target：

```bash
TVOS_TEST_DESTINATION='platform=tvOS Simulator,name=Apple TV' just test-tvos
```

## LAN Cache Server

cache server 默认只监听本机 loopback，适合本机开发：

```bash
dotnet run --project CacheServer/TVOSNetPlayer.CacheServer -- --Cache:RootPath /path/to/cache
```

如果要给同一局域网内的 Apple TV 访问，需要在可信 LAN 上显式绑定：

```bash
dotnet run --project CacheServer/TVOSNetPlayer.CacheServer -- \
  --Cache:RootPath /path/to/cache \
  --Cache:GrpcListenUrl http://0.0.0.0:50051 \
  --Cache:MediaListenUrl http://0.0.0.0:8080
```

当前第一片只支持 cleartext `http://` listener，HTTP Range 媒体服务先面向 Mac mini/macOS；认证、TLS、Bonjour discovery、BBDown task adapter 和其他服务端平台的安全媒体打开都是后续工作。

tvOS app 目前只在刷新时请求首屏 library preview（最多 200 条），避免在服务端每页重新扫描本地 cache root 的第一片实现上触发多次全量目录枚举。完整分页浏览和搜索会随后续 library UI 一起补齐。

## Pre-commit Hook

安装本仓库自带的 Git hook：

```bash
just install-hooks
```

默认 pre-commit 只运行快速检查：

```bash
scripts/pre-commit.sh
```

如果希望本地提交前也跑 build 或 tests：

```bash
PRE_COMMIT_RUN_BUILD=1 scripts/pre-commit.sh
PRE_COMMIT_RUN_TESTS=1 scripts/pre-commit.sh
```

## 刷新到 Apple TV

先列出 Xcode/CoreDevice 能看到的设备：

```bash
xcrun devicectl list devices
```

然后用你的 Team ID 和 Apple TV identifier 安装并启动：

```bash
DEVELOPMENT_TEAM=ABCDE12345 \
TVOS_DEVICE_ID=00000000-0000-0000-0000-000000000000 \
just deploy
```

默认 bundle id 是 `dev.joey.tvos-net-player`。如果你需要避开已有 provisioning profile 或换成自己的命名空间：

```bash
DEVELOPMENT_TEAM=ABCDE12345 \
PRODUCT_BUNDLE_IDENTIFIER=com.example.tvos-net-player \
TVOS_DEVICE_ID=00000000-0000-0000-0000-000000000000 \
just deploy
```

`scripts/deploy-lan.sh` 会执行 `xcodebuild -allowProvisioningUpdates`，然后用：

```bash
xcrun devicectl device install app --device "$TVOS_DEVICE_ID" build/DerivedData/Build/Products/Debug-appletvos/TVOSNetPlayer.app
```

自签名或个人开发签名的有效期取决于你的 Apple Developer 账户和生成出来的 provisioning profile；过期后重新运行 `just deploy` 就会刷新本机 build 和设备安装。

## CI

`.github/workflows/ci.yml` 使用 `macos-26` runner 跑：

```bash
scripts/pre-commit.sh
scripts/build.sh
scripts/build-cache-server.sh
scripts/build-for-testing.sh
scripts/test-tvos-simulator.sh
scripts/test.sh
scripts/test-cache-server.sh
```

其中 pre-commit 检查包括 Swift formatter/linter 和 shell script lint，`scripts/build-cache-server.sh` 编译 .NET LAN cache server，`scripts/build-for-testing.sh` 编译 Xcode XCTest bundle，`scripts/test-tvos-simulator.sh` 执行 app target 的 tvOS XCTest，`scripts/test.sh` 跑不依赖 simulator runtime 的 core tests，`scripts/test-cache-server.sh` 启动真实 Kestrel server 并覆盖 gRPC 控制面和 HTTP Range 媒体面。CI 不做设备签名，也不需要 Apple Developer secrets。物理 Apple TV 的安装只保留在本机脚本里执行。

后续设置 required checks 时，建议至少 gate：

- `CI / tvOS build and tests`
- `codex/review-gate`

## Codex Review Gate

模板自带的 `.github/workflows/codex-review-gate.yml` 仍然保留。它写入 `codex/review-gate` status check，并把
`JoeyTeng/codex-review-gate-action` pin 到 v1.2.1 commit SHA，避免 privileged `pull_request_target` 运行依赖可移动 tag。

启用 required status check 时，可以使用 `JoeyTeng/codex-review-gate` 的 bootstrap helper：

```bash
node scripts/bootstrap-codex-review-gate.mjs --repo OWNER/REPO
node scripts/bootstrap-codex-review-gate.mjs --repo OWNER/REPO --apply
```

helper 默认 dry-run，并且会在 workflow 已经存在于默认分支前拒绝要求 `codex/review-gate`。

## 可选仓库变量

- `CODEX_REVIEW_GATE_RUNNER_LABELS`: JSON runner label array. Defaults to
  `["ubuntu-slim"]`; use `["ubuntu-latest"]` when `ubuntu-slim` is unavailable.
- `CODEX_REVIEW_GATE_AUTO_RETRY=false`: disables scheduled retry jobs before a
  runner is allocated.
- `CODEX_REVIEW_GATE_EVENT_MODE`: `standard`, `comment-only`, or `full`.
- `CODEX_REVIEW_GATE_BOT_LOGINS`: comma-separated additional Codex bot logins.
- `CODEX_REVIEW_GATE_COMPLETION_SIGNAL_BUFFER_SECONDS`: clean completion buffer.
- `CODEX_REVIEW_GATE_FAILED_FINDINGS_RECOVERY`: set to `false` to disable
  same-head recovery after resolved Codex findings.
- `CODEX_REVIEW_GATE_FAILED_FINDINGS_RECOVERY_MODE`: `head` or `fresh`.
