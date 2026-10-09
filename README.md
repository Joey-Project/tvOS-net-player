# TVOS Net Player

这是一个准备给家用 Apple TV 自签名使用的 tvOS 网络播放器仓库。当前版本提供手动播放和 LAN cache control-plane 第一片：

- SwiftUI tvOS app target：输入 HTTP/HTTPS 地址后用 `AVPlayer` 播放。
- SwiftUI macOS app target：复用同一套 AppCore/cache client，用于桌面调试和轻量使用。
- 手动 URL v0 工作流：保存上次播放地址，支持停止、清空和输入校验。
- LAN cache client v0：输入 Mac mini cache server 地址，刷新 gRPC library 首屏预览，选择可播放 variant，并把 HTTP/HLS playback source 交给 `AVPlayer`。
- LAN cache server MVP：用 gRPC 提供控制面，用 HTTP Range endpoint 提供 `AVPlayer` 可播放媒体 URL，并包含可落盘恢复的 Bilibili task worker 和 BBDown Rust adapter。
- Swift package tests：覆盖 core URL 规范化和 cache client model/pagination 行为，不依赖本机 tvOS simulator runtime。
- GitHub Actions CI：pre-commit checks、tvOS simulator build、macOS app build、LAN cache server build、tvOS/macOS XCTest targets、core tests、cache server integration tests。
- LAN 刷新脚本：从这台 Mac build/sign，然后通过 `devicectl` 安装到同一局域网内已配对的 Apple TV。
- Codex review gate：保留模板仓库已有的 `codex/review-gate` workflow。

## 本地环境

- Xcode 26 或更新版本。
- Rust 1.95.0 toolchain with `rustfmt` and `clippy`; `rust-toolchain.toml` pins this for Cargo/rustup.
- `ffmpeg` 可执行文件；真实 Bilibili task worker 会调用 BBDown Rust core 下载媒体，需要合并音视频流时再用 `ffmpeg` remux。audio-only 输出保留可播放的 `.m4a`。
- `just` task runner。
- Apple TV 开启开发者模式，并在 Xcode / Devices and Simulators 中和这台 Mac 配对。
- Xcode 里登录 Apple ID；物理设备安装时需要能生成 tvOS development provisioning profile。

## 常用命令

```bash
just ci
just lint
just format
just build
just build-macos
just build-cache-server
just build-for-testing
just test-macos
just test-tvos
just test
just test-cache-server
just test-bilibili-live
just test-lan-playback
```

`just lint` 会运行 shell 脚本语法检查、`shellcheck`（如果已安装）、`swift-format lint --strict`、`cargo fmt --check` 和 `cargo clippy -D warnings`。`just format` 会用仓库根目录的 `.swift-format` 原地格式化 Swift 源码，并用 `cargo fmt` 格式化 Rust cache server 源码。

`just build` 默认使用 `generic/platform=tvOS Simulator`，不会要求本机配置签名。`just build-macos` 默认使用 `generic/platform=macOS` 编译 macOS app。`just build-for-testing` 会编译 tvOS Xcode XCTest bundle，但不会启动 simulator。`just test-macos` 运行 macOS app-shell XCTest target。`just test` 运行 Swift package core/cache client tests，不需要本机安装 tvOS simulator runtime。

如果要跑 Xcode/tvOS simulator XCTest target：

```bash
TVOS_TEST_DESTINATION='platform=tvOS Simulator,name=Apple TV' just test-tvos
```

真实 Bilibili URL smoke suite 由 repo-local skill `.agents/skills/bilibili-live-e2e/` 维护，覆盖公开视频、多 P 视频、favorite/space/collection/series/recommendation list fetch、restricted Bangumi 页面，以及需要 web cookie 的账号页面 fetch case。它会访问公网 Bilibili 和 BBDown core，所以不属于默认 CI；需要显式运行：

```bash
just test-bilibili-live
BILIBILI_LIVE_E2E_CASES=ordinary-video-playlist just test-bilibili-live
BILIBILI_LIVE_E2E_CASES=space-collection just test-bilibili-live
BILIBILI_LIVE_E2E_CASES=bangumi-media-series just test-bilibili-live
```

collection/list cases 默认跳过，需要通过 `BILIBILI_LIVE_E2E_CASES` 或 `BILIBILI_LIVE_E2E_INCLUDE_COLLECTION_LIST=1` 显式运行，因为这些 Bilibili list/feed API 可能需要 cookie、为空、被限流或随上游状态波动。稳定公开 collection smoke 优先使用 `BILIBILI_LIVE_E2E_CASES=space-collection just test-bilibili-live`；`BILIBILI_LIVE_E2E_INCLUDE_COLLECTION_LIST=1` 会把符合条件的非认证 collection/list case 加入更宽的未过滤本地 sweep，但该 sweep 仍可能受上游可用性影响。`space-videos` 和 `homepage-recommendations` 还需要 `BILIBILI_LIVE_E2E_INCLUDE_AUTHENTICATED=1` 以及 web-cookie credential，`favorite-list` 和 `space-series` 需要用 `BILIBILI_LIVE_E2E_FAVORITE_URL` / `BILIBILI_LIVE_E2E_SERIES_URL` 指向当前可用样例后才加入未过滤 sweep。测试会通过 v2 RPC 分页读取候选项和任务结果，并确认选择使用 server 生成的不透明 token，HLS master/media playlist 不会逃逸到 Bilibili 源站 URL。

默认 live suite 会跳过标记为 `requires_restricted_area_path` 的番剧 case、`requires_collection_list_validation` 的 collection/list case、`requires_live_sample_override` 且未设置 URL override 的 case，以及 `requires_authentication` 的账号 case；显式指定这些 case 时会真正访问它们。番剧 restricted-area 验证可以把 BBDown runtime 覆盖传给测试启动的本地 cache server：

```bash
BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PATH=/path/to/credentials.json \
BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PROFILE=family-room \
BILIBILI_LIVE_E2E_RESTRICTED_AREA=hk \
BILIBILI_LIVE_E2E_RESTRICTED_API_PROXY='hk=https://proxy.example/api' \
BILIBILI_LIVE_E2E_CASES=bangumi-media-series,bangumi-episode \
just test-bilibili-live
```

公共 restricted-area 反代只走 Web API，不能配合 TV API mode 使用；配置的 `access_key` 可能随请求送到该反代。账号页面 fetch 验证需要 BBDown credential 文件里包含 web cookie，单独的 `access_key` 不能替代它。可以指定单个账号 case，或用 `BILIBILI_LIVE_E2E_INCLUDE_AUTHENTICATED=1` 批量包含账号 case：

```bash
BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PATH=/path/to/credentials.json \
BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PROFILE=family-room \
BILIBILI_LIVE_E2E_CASES=authenticated-history \
just test-bilibili-live
```

当 credential 文件是多 profile store 且目标凭证不是 default profile 时，使用 `BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PROFILE` 选择 server 侧 profile；该值只是 profile ID，测试不会把 cookie/access key 传给客户端或写入日志。

The optional full-fill mode uses the first candidate of the canonical ordinary-video case, a bounded completion wait, same-root server restarts, and a 300-second completed-cache replay. It is separate from the online readability smoke and remains outside default CI:

```bash
BILIBILI_LIVE_E2E_CASES=ordinary-video-playlist \
BILIBILI_LIVE_E2E_FULL_FILL=1 \
BILIBILI_LIVE_E2E_OFFLINE_SECONDS=300 \
just test-bilibili-live
```

`just test-lan-playback` compiles a native macOS `AVPlayer` probe with strict Swift 6 concurrency checks and runs credential-free argument self-tests by default. Supplying a clean LAN playback URL enables actual video-frame decoding, pause/resume, forward/backward seeks, and 1.25x playback. The URL must have no userinfo, query, or fragment. Hostname validation is an input restriction, not DNS pinning. The native deadline is cooperative; use an outer process timeout for a hard live-run bound.

```bash
LAN_PLAYBACK_URL='http://127.0.0.1:8080/hls/SESSION_ID/master.m3u8' \
LAN_PLAYBACK_DURATION_SECONDS=60 \
LAN_PLAYBACK_DEADLINE_SECONDS=120 \
just test-lan-playback
```

## LAN Cache Server

cache server 默认只监听本机 loopback，适合本机开发：

```bash
cargo run --package tvos-net-player-cache-server -- --Cache:RootPath /path/to/cache
```

如果要给同一局域网内的 Apple TV 访问，需要在可信 LAN 上显式绑定：

```bash
cargo run --package tvos-net-player-cache-server -- \
  --Cache:RootPath /path/to/cache \
  --Cache:GrpcListenUrl http://0.0.0.0:50051 \
  --Cache:MediaListenUrl http://0.0.0.0:8080
```

`0.0.0.0`、`[::]`、`*` 和 `+` 都会尝试展开为 IPv4/IPv6 双栈 wildcard listener；如果系统不支持某个地址族，只要另一个地址族可用就会继续启动。如果只想暴露某个地址族或某个网卡，请改用具体 LAN IP。非 loopback gRPC listener 加上 LAN 可达的 media listener（或非 localhost/loopback 的 `Cache:PublicMediaBaseUri`）默认会发布 Bonjour `_tvos-net-player._tcp`，客户端可自动发现；如需关闭 discovery，设置 `--Cache:BonjourEnabled false`。

当前 server 只支持 cleartext `http://` listener；认证和 TLS 是后续工作。Rust server 默认启动真实 Bilibili worker：worker 消费已提交的 task，调用 pin 到指定 commit 的 `bbdown-core`，把选中的多项结果下载到 cache root 下的 `Bilibili/`，按需 mux，并把可播放输出映射成独立的 library item ID。高频 progress 更新会通过内存状态和 watch 事件暴露，不逐次强制写盘；BBDown core 当前没有逐 chunk callback，所以真实下载中的 progress 是 coarse-grained 阶段状态。tvOS/macOS 客户端可以展示 cache root 容量，并把 completed Bilibili HLS 项标记为 offline HLS；可见 local cache/离线 HLS 库项删除默认关闭，需要显式设置 `--Cache:AllowLibraryItemDelete true` 后 server 才声明能力，客户端才显示删除入口。只在 loopback 或可信受控 LAN 上开启删除能力，例如：

```bash
cargo run --package tvos-net-player-cache-server -- \
  --Cache:RootPath /path/to/cache \
  --Cache:GrpcListenUrl http://127.0.0.1:50051 \
  --Cache:MediaListenUrl http://127.0.0.1:8080 \
  --Cache:AllowLibraryItemDelete true
```

默认 task state 路径是 server 可执行文件旁的 `cache-server-state/tasks.json`。本机部署建议显式指定，便于备份和排查：

```bash
cargo run --package tvos-net-player-cache-server -- \
  --Cache:RootPath /path/to/cache \
  --Cache:TaskStatePath /path/to/server-state/tasks.json \
  --Cache:BBDownFfmpegPath /opt/homebrew/bin/ffmpeg
```

如果 task state snapshot 无法加载，server 会保留原文件并禁用后续 task state 写回，避免把可修复状态覆盖掉。

BBDown adapter 相关配置：

- `Cache:BilibiliWorkerEnabled`: 是否启动真实 worker。默认 `true`；测试或只想保留排队 control-plane 时可设为 `false`。
- `Cache:BilibiliWorkerMaxConcurrentTasks`: worker 最大并发 task 数。默认 `1`。v2 task 使用 task-local archive 管理同一任务内的重复输出，legacy task 仍使用共享 archive。
- `Cache:TaskRetentionMaxTerminalTasks`: 持久化 task snapshot 里最多保留的普通 terminal task 数。默认 `200`；设为 `0` 可关闭数量限制。
- `Cache:TaskRetentionTerminalAgeDays`: 持久化 task snapshot 里普通 terminal task 的最长保留天数。默认 `30`；设为 `0` 可关闭时间限制。
- `Cache:AllowLibraryItemDelete`: 是否允许 gRPC control-plane 删除可见 local cache 和 completed Bilibili HLS library item。默认 `false`，因为当前 control-plane 是 cleartext 且未鉴权；只在可信 LAN 或 loopback-only 部署中显式打开。
- `Cache:HlsCacheMaxBytes`: completed HLS cache 的自动淘汰预算。默认 50 GiB；设为 `0` 可关闭自动淘汰。
- `Cache:HlsCacheHighWatermarkPercent`: current/projected completed-HLS usage 触发清理的高水位。默认 `90`。
- `Cache:HlsCacheLowWatermarkPercent`: 触发清理后的目标低水位。默认 `80`，必须低于 high watermark。
  自动淘汰只会处理 completed HLS sessions，并会跳过 active/finalizing/recently-served sessions；如果 task-state snapshot 不可读，server 会保留可能可恢复的 HLS cache，而不是把它当 orphan 删除。
- `Cache:BBDownOutputDir`: BBDown 输出目录，默认是 `Cache:RootPath/Bilibili`；当 worker 启用或显式配置该路径时，它必须位于 `Cache:RootPath` 内，不能包含 `..` parent components，且 root 内已经存在的输出路径组件不能是 symlink。
- `Cache:BBDownArchivePath`: BBDown 下载 archive JSON。默认和 `Cache:TaskStatePath` 同目录，文件名为 `bbdown-archive.json`。
- `Cache:BBDownFfmpegPath`: `ffmpeg` 可执行文件路径。默认从 `PATH` 查找 `ffmpeg`。
- `Cache:BBDownCredentialPath`: BBDown credential JSON 文件路径，字段兼容 `bbdown-core` 的 `cookie`、`access_key` 和 `tv_access_key`。不要把这个文件提交到仓库。
- `Cache:BBDownCredentialProfile`: 可选 BBDown credential profile 名称；设置后 server 会从 `Cache:BBDownCredentialPath` 指向的 profile store 读取该 profile。未设置时继续使用 credential store 的默认 profile。
- `Cache:AllowBilibiliLoginSessions`: 是否允许 LAN 客户端发起服务器管理的 Web QR 和通用 key 登录。默认 `false`；仅在可信 LAN 上显式设为 `true`，并同时配置 `Cache:BBDownCredentialPath` 指向不纳入版本控制的私有 JSON 路径。Mac mini 负责创建票据、轮询或验证回调、校验账号并保存凭据；tvOS/macOS 只发起登录、显示短时二维码或链接并查询脱敏状态。健康凭据直接复用；server 每 15 分钟检查所选 profile，并在有 refresh material 且提供方要求刷新时续期 Web cookie。新请求读取最新凭据，不需要重启服务器。
- Web QR 重新登录只能更新已验证的同一账号，保留其他凭据、profile 和默认 profile。server 在 credential 文件旁保存私有的 `.bilibili-bindings.json` 账号绑定；备份时应一并保留。过期 cookie 若没有可信的既有绑定，会拒绝自动替换，不把重新扫码当作账号切换入口。
- BiliPlus/BALH 通用 `access_key` 登录通过 server 的 `/login/bilibili/<session>` 页面和一次性回调完成，必须先有已验证的同账号 Web credential。请把 `Cache:PublicMediaBaseUri` 配置成浏览器和客户端均可访问的 LAN HTTP 地址或反代 HTTPS 地址；支持路径前缀，反代必须保留配置的 `Host` 并正确移除路由前缀。未配置具体可用 origin 的 wildcard listener 不提供该登录能力。登录页只接受签发的提供方窗口、origin、短时一次性 capability，并由 server 校验账号后保存；客户端 app 没有提交 cookie、key、refresh token 或切换账号的 RPC。
- 凭据存在不等于已验证可用。新的 credential readiness 状态区分检查中、可用、需要登录和暂时不可验证；网络故障不会被静默当作凭据失效。当前 gRPC 和 LAN HTTP 仍未鉴权、未加密，二维码及登录链接包含短时授权票据；不要直接暴露到公网，反代部署需要另行提供访问控制。
- `Cache:BBDownRestrictedArea`: restricted-area 优先区域，可选 `cn`、`th`、`hk` 或 `tw`。
- `Cache:BBDownRestrictedAreaProxy`: restricted-area playurl proxy 列表，格式为逗号分隔的 `[area=]URL`，例如 `hk=https://proxy.example/playurl,https://fallback.example/playurl`。
- `Cache:BBDownRestrictedApiProxy`: restricted-area Bilibili API proxy 列表，格式同上。

启动时 server 会把 `Cache:RootPath` 和启用中的 BBDown output 的已存在路径前缀 canonicalize，再交给 media library 和 BBDown adapter 使用，避免 symlink ancestor 造成下载路径和索引边界不一致。

新客户端使用 v2 Bilibili resolution session 读取分页候选项，以 server 生成的不透明 token 选择单项、多项、范围或全部，并用 `CreateBilibiliTaskV2` 提交播放或下载。`ListTaskResults` 分页返回每项的状态、播放或 library 引用及可用 artifact；媒体和 sidecar 字节仍由 LAN server 的 HTTP/HLS/Range endpoint 提供。一次任务最多接受 100 个候选项，较大列表需要分批选择；旧 RPC 和单结果字段仅保留给旧客户端兼容。没有 v2 能力的 server 需要升级，客户端不会回退到旧提交路径。

tvOS app 目前只在刷新时请求首屏 library preview（最多 200 条），避免在服务端每页重新扫描本地 cache root 的第一片实现上触发多次全量目录枚举。cache client contract 已暴露 page token 和 search text；完整分页浏览和搜索会随后续 library UI 一起补齐。

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
scripts/build-macos.sh
scripts/build-cache-server.sh
scripts/build-for-testing.sh
scripts/test-tvos-simulator.sh
scripts/test.sh
scripts/test-macos.sh
bash scripts/test-lan-playback.sh
scripts/test-cache-server.sh
```

其中 pre-commit 检查包括 Swift formatter/linter、Rust formatter/linter 和 shell script lint，`scripts/build-cache-server.sh` 编译 Rust LAN cache server，`scripts/build-for-testing.sh` 编译 tvOS Xcode XCTest bundle，`scripts/test-tvos-simulator.sh` 执行 tvOS app target 的 XCTest，`scripts/test.sh` 跑不依赖 simulator runtime 的 core tests，`scripts/test-macos.sh` 执行 macOS app-shell XCTest，`scripts/test-cache-server.sh` 启动真实 Rust server 并覆盖 gRPC 控制面和 HTTP Range 媒体面。CI 不做设备签名，也不需要 Apple Developer secrets。物理 Apple TV 的安装只保留在本机脚本里执行。

后续设置 required checks 时，建议至少 gate：

- `CI / tvOS build and tests`
- Default-branch v2 cutover target: `codex/github-review-gate` (activate only after the independent canary and coordinator readback).

## Codex Review Gate

目标分支上的 `.github/workflows/codex-review-gate.yml` 现在使用 floating `v2` verifier，产生 `codex/github-review-gate`；旧 v1 status producer 写入的 `codex/review-gate` 是不同的 required context，不能由 v2 verifier 代发。

当前 v2 contract 只支持目标为 repository default branch 的普通同仓库 PR。Verifier workflow 对所有 `pull_request` base 都会被 GitHub 触发；对 `release/*` 等非默认 base，v2 action 会 fail closed 并返回 unsupported，而不是提供有效 review-gate 覆盖。本迁移不会为 release 分支声称 v2 支持或更改 workflow trigger。依照已批准的 release 迁移边界，既有 `release/*` required-check 需求保持单独暂停，不由本 PR 恢复或扩大；只有确认 v2 支持相应 base、完成独立验证并由协调者核验后，才恢复原有的精确 release 保护要求。Fork-head PR 也不受支持：fork 上下文中的 `workflow_run.pull_requests` 可能为空，controller 因而无法安全关联 PR 并自动请求 review；这不是本安装的支持范围。

本次 v2 安装不更改 production ruleset。迁移 PR 合并后，普通 PR 在 ruleset 仍要求旧 `codex/review-gate` context 时会暂时被阻塞；这是有意限定的维护窗口。窗口期间冻结其他合并，先由独立 canary PR 证明 v2 原生 gate 为绿色，再由协调者将 required context 切换为 `codex/github-review-gate` 并回读确认，随后才退役旧 v1 required context。保留所有无关的非 status 保护，不添加 legacy status writer，也不在此 PR 修改现网规则。

v2 controller 不订阅 `pull_request_review` 或 `pull_request_review_comment`。因此 Codex 通过 PR review 或 inline findings 完成后不会自动触发 reconcile；这是未授予这些 review events 写工作流权限的边界，不通过新增可写 review-event job 绕过。需要恢复 gate 状态时，可从默认分支手动运行 controller 的 `reconcile` 操作：

```bash
gh workflow run codex-review-gate-controller.yml \
  --repo Joey-Project/tvOS-net-player \
  --ref master \
  --field operation=reconcile \
  --field pr_number=PR_NUMBER \
  --field expected_head_sha=CURRENT_40_CHARACTER_HEAD_SHA \
  --field request_review=false
```

`expected_head_sha` 必须是该 PR 的当前完整 head SHA。`reconcile` 不会发起新的 Codex review 请求。

Legacy `JoeyTeng/codex-review-gate` bootstrap helper 只适用于 v1 的 `codex/review-gate` status；不要用它代替上面的 v2 ruleset cutover。此安装不自动修改 required contexts。

## 可选仓库变量

- `CODEX_REVIEW_GATE_USE_UBUNTU_LATEST`: set to `true` to run both v2 verifier
  and controller on `ubuntu-latest` when `ubuntu-slim` is unavailable. Otherwise
  both use `ubuntu-slim`.
- `CODEX_REVIEW_GATE_AUTO_REQUEST`: set to `true` to let the controller request
  a Codex review for an eligible failed same-repository, default-base PR run.
- `CODEX_REVIEW_GATE_LIMITS_PROFILE`: set to `expanded` to select the v2
  expanded limits profile; unset or other values use `default`.

Legacy v1 control variables are not read by these v2 workflows; setting them
has no effect.
