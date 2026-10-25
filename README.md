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
- Python 3；UI e2e runner 只使用标准库，并需要 `ffmpeg` 与 `ffprobe`。
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
just test-ci-helpers
just test-bilibili-live
just test-lan-playback
just test-ui-e2e-macos
just test-ui-e2e-tvos
```

`just lint` 会运行 shell 脚本语法检查、`shellcheck`（如果已安装）、`swift-format lint --strict`、`cargo fmt --check` 和 `cargo clippy -D warnings`。`just format` 会用仓库根目录的 `.swift-format` 原地格式化 Swift 源码，并用 `cargo fmt` 格式化 Rust cache server 源码。

`just build` 默认使用 `generic/platform=tvOS Simulator`，不会要求本机配置签名。`just build-macos` 默认使用 `generic/platform=macOS` 编译 macOS app。`just build-for-testing` 会编译 tvOS Xcode XCTest bundle，但不会启动 simulator。`just test-macos` 运行 macOS app-shell XCTest target。`just test` 运行 Swift package core/cache client tests，不需要本机安装 tvOS simulator runtime。

如果要跑 Xcode/tvOS simulator XCTest target：

```bash
TVOS_TEST_DESTINATION='platform=tvOS Simulator,name=Apple TV' just test-tvos
```

### CI Diagnostics

`just test-ci-helpers` 会运行普通 CI 阶段 runner 与原生 UI helper 的 Python 回归；`just ci` 也包含此检查。普通 GHA CI 将共享 Core 测试与其余 Apple 构建、lint、XCTest 和 Rust 检查分为两个并行作业，保留全部原有命令。统一的 `tvOS build and tests` 门禁只有在两个作业都成功时才通过，失败、取消或跳过均不算成功。

各阶段记录真实退出码、耗时、超时、输出截断及清理结果，并上传有界日志和小型 JSON 摘要。`ci-stages-<run_id>` 与 `ci-core-stages-<run_id>` 分别保存各自负责的阶段；核对完整 CI 时需一并检查两个产物。本版 JSON 的 `test_summary` 明确为 `null`，测试计数需从保留的阶段日志或原生 UI 结果读取，不把缺失计数当作零；阶段退出成功不替代原生 UI 的指定用例和 metrics 验证。

执行构建和测试的普通 CI 与 UI job 的上限均为 60 分钟，主要工作期限在各自 checkout 前记录，截止于各自首步后的 45 分钟；余量用于独立的诊断、清理、上传和缓存保存。普通 CI 的汇总门禁另有两分钟上限，不执行测试。取消或 runner 被强制终止时，不能保证仍能上传产物。普通 CI 每阶段只保留有界的首尾输出并继续排空其余日志，不因正常输出量大而终止测试。

编译缓存区分 Rust、Swift、普通 CI 与两端 UI job，并绑定实际工具链、平台、依赖及对应源码。PR 可读取适用缓存，只有成功的默认分支 push 才保存；不缓存凭据、用户目录、运行状态、媒体或诊断。缓存命中不会跳过任何原有命令，未命中仍须完成冷构建；配置缓存本身不证明实际命中或提速。

Rust 测试 waiter 的超时诊断仅包含有限的任务/结果状态、观察次数和耗时，不包含 source、标题、原始 message、URL、路径或完整 task。原有等待期限和成功条件保持不变；单次超时及后续通过仍不足以证明根因。下一阶段的完整顺序见 [home LAN readiness roadmap](docs/project_journal/2026/10/2026-10-23-home-lan-readiness-roadmap-b46388.md)。

### Native UI E2E

`just test-ui-e2e-macos` 和 `just test-ui-e2e-tvos` 是独立的 XCUITest 界面测试入口，不是现有 AppCore/hosted-view XCTest 的别名。它们启动隔离的真实 Rust LAN cache server，生成不含音轨的短视频和跨越 50 条分页边界的本地目录，再通过实际客户端界面连接、选片和操作播放。tvOS 入口需要已安装且可用的 simulator runtime；仓库没有 iOS app target，安装 iOS runtime 不会增加 iOS 客户端覆盖。

macOS 用例还检查暂停/恢复、以暂停后的稳定时间点为基准的前后跳转、实际 2x 时间推进、92 秒视频自然结束，以及先在终点点击前进、再通过普通 Resume 控件回到开头继续播放并保留倍速。另一路径会在片尾前暂停，再点击前进到终点，确认显示播放结束且 Resume 能回到开头继续播放。tvOS 用例通过真实遥控器导航到惰性加载的第 55 条，再检查播放状态和时间推进。macOS 点击必须落在应用窗口及屏幕可用区域内，不通过后台调用替代界面操作。

每个平台还执行自动恢复、手动重连、静默校准和退出后续接四项任务用例。独立的测试专用 Swift gRPC 进程只监听 loopback，并注入明确的控制流故障；实际客户端仍通过界面输入、提交任务和点击 `Reconnect Updates`。手动重连用例会等待生产代码的限次退避耗尽，在更新停止时播放已有结果，再恢复原任务的更新并确认播放继续。静默用例保持 watch 连接打开，跨越状态未变和发现完成的校准读取，检查没有错误重连且已有播放持续推进。续接用例暂停在非零位置，真正终止并重开应用，确认不会自动播放，再明确继续原结果并检查位置与时间推进。runner 独立核对每个场景只创建一个任务，续接不会重新解析或提交；媒体仍来自真实 Rust server，不访问 Bilibili，也不给生产服务器加入故障注入开关。

自然播放的 90 秒等待窗口从实际重播点击开始计算，滚动和查找按钮的准备时间单独记录；按钮已经可见时立即停止滚动检查。播放结束后用原生播放器控件往回拖动，再点自定义 Resume，会从所选位置续播并保留倍速，而不是无条件回到开头；这项共享 AppCore 行为另有专项回归测试。

这两个 Just 命令会先构建 Rust server；设置 `CACHE_SERVER_BINARY` 可复用已构建的可执行文件。`DERIVED_DATA_PATH` 可复用已有 Xcode 编译缓存，该目录不会作为夹具状态被删除。使用 `UI_E2E_ARTIFACT_DIR` 可指定一个新的空目录保存诊断；默认目录位于系统临时目录下的 `tvos-net-player-ui-e2e-artifacts/`。保留的诊断仅限 allowlist 日志、测试结果 JSON 和大小允许的 xcresult，不包含生成媒体、server 状态或整个编译目录；总计最多 32 MiB，xcresult 最多 20 MiB。

本地 helper 的工作预算上限为 35 分钟，fixture 编译最多 15 分钟。GHA 在 checkout 前记录整个 job 的单调时钟工作期限，最晚在首步后的 45 分钟结束主要构建/测试工作，包含安装工具和 Rust 构建已花费的时间；60 分钟 job 的余量留给有界诊断、清理和产物上传。诊断与清理使用独立期限，不复用已耗尽的工作预算。

Xcode 失败退出后也会尝试提取有界的失败摘要和活动元数据，并从首个失败用例的 teardown 附件中单独导出最多一份 accessibility tree（512 KiB）和一张截图（2 MiB），不导出录屏。完整 xcresult 超限时仍可保留这些小附件；导出结果见 `xcresult-attachment-export.json`，归档缺失、超限或失败见 `artifact-staging.log`。附件导出失败不会覆盖原始测试失败，只有元数据不等于图片留存。

测试会先设置并确认系统静音，结束后保持静音。调试测试实例使用隔离偏好并禁用 Bonjour 自动连接；夹具 server 关闭 Bilibili worker、登录会话和 Bonjour，不读取已有账号凭据，也不修改日常 cache server。普通 CI 不依赖 Bilibili、账号 Cookie 或公共反代。真实 Bilibili 和长播放验证仍使用下方显式 opt-in live suite。

macOS UI 测试使用本地 ad-hoc 签名，不需要开发者证书或 team；tvOS simulator 构建不启用签名。这不替代实机 provisioning、分发签名或公证。本机 UI 执行仍需可用的桌面会话及 Xcode 所需权限，成功构建和签名校验不代表 UI 测试通过。

GHA 的 UI 测试与现有构建/单元测试分开记录。每个平台预期的五项用例都必须实际执行并通过，且失败、跳过数量均为零；只执行其中一项、成功编译或列出 simulator runtime 都不能代替完整 UI e2e。设计、验证证据与限制见 [UI e2e journal](docs/project_journal/2026/10/2026-10-22-ui-e2e-ci-b46385.md)、[任务观察恢复 UI journal](docs/project_journal/2026/10/2026-10-23-task-watch-ui-e2e-b46387.md)、[静默校准 journal](docs/project_journal/2026/10/2026-10-23-silent-task-reconciliation-b46390.md) 和 [客户端续接 journal](docs/project_journal/2026/10/2026-10-23-client-task-playback-resumption-b46391.md)。

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

可选 full-fill 模式支持普通视频、番剧，以及显式启用的 following/dynamic 认证 feed。认证 feed 只允许选取首个候选项，仍需有效 Web cookie；仅设置 full-fill 不会自动启用认证用例。该模式会等待完整缓存、在同一目录重启 server，并进行 300 秒 cache-only 回放，与在线可读性 smoke 分开，仍不属于默认 CI：

```bash
BILIBILI_LIVE_E2E_CASES=ordinary-video-playlist \
BILIBILI_LIVE_E2E_FULL_FILL=1 \
BILIBILI_LIVE_E2E_OFFLINE_SECONDS=300 \
just test-bilibili-live
```

认证 feed 的长时间验证可以显式选择 following，或使用默认公开 uploader 动态样例 `https://space.bilibili.com/441167301/dynamic`，也可通过 `BILIBILI_LIVE_E2E_SPACE_DYNAMIC_URL` 覆盖。仓库 pin 的 `bbdown-core v0.10.0` commit `03baba367d846379f5fd4bddabc4784d1b0de8e7` 包含个人空间动态的 WBI 签名、缺失设备 Cookie 的内存初始化，以及有界元数据分页接口。凭据仍由 server 读取和管理，不需要让客户端接触 Cookie。空 feed、上游拒绝和默认样例不可用都保留为失败，不能降低候选数要求来算作通过；API 返回成功也不等于视频已可播放：

```bash
BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PATH=/path/to/credentials.json \
BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PROFILE=test-profile \
BILIBILI_LIVE_E2E_CASES=authenticated-following-feed \
BILIBILI_LIVE_E2E_FULL_FILL=1 \
BILIBILI_LIVE_E2E_SUSTAINED_SECONDS=600 \
BILIBILI_LIVE_E2E_OFFLINE_SECONDS=300 \
just test-bilibili-live
```

运行长测试时需要外层进程期限和输出上限；完整缓存、恢复证据和 native probe 的交接约束见 [full-fill validation](.agents/skills/bilibili-live-e2e/references/full-fill-validation.md)。

following 和个人空间动态的单次候选解析最多使用 128 次上游 API 请求、扫描 512 条源记录、调用 8 次元数据分页，元数据收集阶段限制在 90 秒内。每次分页最多使用 32 次请求、扫描 64 条源记录、返回 64 个条目，同时受剩余总预算和请求候选数量约束。触及请求、扫描或调用数量预算时，非空候选窗口通过 V2 会话的 `candidates_truncated` 标记为截断，即使其数量尚未达到请求上限；预算耗尽却没有候选、超时和取消均不会伪装成成功的空 feed。上游 cursor 只提供尽力续页，不保证 feed 不变；server 先收集这个有限窗口，再创建自己的不可变候选快照。macOS 和 tvOS 列表滚动到末尾时通过 V2 的 opaque token 自动加载快照的下一页，保留手动重试；窗口本身被截断时会提示，即使所有快照页都已加载。自动分页不会突破上游解析预算或扩展现有快照。

客户端启动解析使用独立的 120 秒超时，给服务器的有界元数据收集留出时间；服务器信息查询、候选快照翻页等普通操作仍使用原来的短超时。新增会话字段向后兼容旧的协议数据；旧客户端需要更新后才能显示截断提示和自动分页。

`just test-lan-playback` 会使用严格 Swift 6 并发检查编译 macOS 原生 `AVPlayer` probe，默认只运行不需要凭据的参数 self-test。提供干净的 LAN 播放 URL 后，才会验证真实视频帧解码、暂停/恢复、前后跳转和 1.25x 播放。URL 不能包含 userinfo、query 或 fragment；hostname 校验是输入限制，不是 DNS pinning。native deadline 是协作式的，硬性期限需要外层进程 timeout。

原生回放验证需要允许 AVFoundation 系统服务访问；受限执行沙盒可能在媒体本身可正常播放时返回 `AVFoundationErrorDomain -11821`。遇到此错误时，先用同一缓存和探针在允许系统媒体服务访问的环境中对照验证，不要仅凭此错误修改媒体格式。

```bash
LAN_PLAYBACK_URL='http://127.0.0.1:8080/hls/SESSION_ID/master.m3u8' \
LAN_PLAYBACK_DURATION_SECONDS=60 \
LAN_PLAYBACK_DEADLINE_SECONDS=120 \
just test-lan-playback
```

## LAN Cache Server

HLS 前台读取与后台整片填充分开选择传输粒度：建立原有 512 KiB 启动身份后，前台按最多约 64 KiB 的小 Range 获取当前需要的字节，后台继续使用较大的逻辑块补齐缺口。已验证的连续缓存使用独立、有界的较大交付窗口，避免每个小请求都重新哈希整个大块；缓存未命中时不等待大窗口下载。两者共享经过验证的缓存，保留原有 ETag、跨 CDN 抽样、取消、配额和抢占规则；截断响应不会作为有效缓存交付。首次启动和跨 CDN 身份验证仍可能需要较大读取，64 KiB 不是端到端响应时间保证。小块合并保持有界，不修改 gRPC 或持久化 schema。设计与验证边界见 [foreground Range cache journal](docs/project_journal/2026/10/2026-10-24-foreground-range-cache-foundation-b46393.md)；这项基础改造不单独证明原生 ABR 降档和恢复已通过。

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
- Web QR 重新登录只能更新已验证的同一账号，保留其他凭据、profile 和默认 profile。server 在 credential 文件旁保存私有的 `.bilibili-bindings.json` 账号绑定；备份时应一并保留。过期 cookie 若没有可信的既有绑定，会拒绝自动替换，不把重新扫码当作账号切换入口。新 profile 先完成 Web 登录，再获取通用 key；Web 登录成功后，server 会立即在后台复检保留的旧 key，不必等待下一个 15 分钟检查周期。登录启动默认至少等待 90 秒，不影响普通状态查询和轮询的超时。
- BiliPlus/BALH 通用 `access_key` 登录通过 server 的 `/login/bilibili/<session>` 页面和一次性回调完成，必须先有已验证的同账号 Web credential。请把 `Cache:PublicMediaBaseUri` 配置成浏览器和客户端均可访问的 LAN HTTP 地址或反代 HTTPS 地址；支持路径前缀，反代必须保留配置的 `Host` 并正确移除路由前缀。未配置具体可用 origin 的 wildcard listener 不提供该登录能力。登录页只接受签发的提供方窗口、origin、短时一次性 capability，并由 server 校验账号后保存；客户端 app 没有提交 cookie、key、refresh token 或切换账号的 RPC。
- 凭据存在不等于已验证可用。新的 credential readiness 状态区分检查中、可用、需要登录和暂时不可验证；网络故障不会被静默当作凭据失效。当前 gRPC 和 LAN HTTP 仍未鉴权、未加密，二维码及登录链接包含短时授权票据；不要直接暴露到公网，反代部署需要另行提供访问控制。
- `Cache:BBDownRestrictedArea`: restricted-area 优先区域，可选 `cn`、`th`、`hk` 或 `tw`。
- `Cache:BBDownRestrictedAreaProxy`: restricted-area playurl proxy 列表，格式为逗号分隔的 `[area=]URL`，例如 `hk=https://proxy.example/playurl,https://fallback.example/playurl`。
- `Cache:BBDownRestrictedApiProxy`: restricted-area Bilibili API proxy 列表，格式同上。

启动时 server 会把 `Cache:RootPath` 和启用中的 BBDown output 的已存在路径前缀 canonicalize，再交给 media library 和 BBDown adapter 使用，避免 symlink ancestor 造成下载路径和索引边界不一致。

新客户端使用 v2 Bilibili resolution session 读取分页候选项，以 server 生成的不透明 token 选择单项、多项、范围或全部，并用 `CreateBilibiliTaskV2` 提交播放或下载。`ListTaskResults` 分页返回每项的状态、播放或 library 引用及可用 artifact；媒体和 sidecar 字节仍由 LAN server 的 HTTP/HLS/Range endpoint 提供。一次任务最多接受 100 个候选项，较大列表需要分批选择；旧 RPC 和单结果字段仅保留给旧客户端兼容。没有 v2 能力的 server 需要升级，客户端不会回退到旧提交路径。

tvOS/macOS 使用共享 AppCore 观察已提交的任务。更新流暂时断开或提前结束时，客户端限次退避、读取原任务的最新状态，再恢复订阅；不会重新解析或提交任务，也不会因此停止已开始的播放。重连耗尽或遇到权限、任务不存在等明确错误时，可用独立的 `Reconnect Updates` 恢复同一任务的更新；原有 `Retry` 仍表示重试失败的任务，两者不是同一个操作。取消请求失败时，客户端只提示取消尚未确认，最终以服务器任务状态为准。

watch 保持打开但长期没有消息时，客户端默认按 30 秒静默观察窗口，用有超时上限的 `GetTask` 校准原任务。有效更新会推迟读取；状态未变的正常响应不会显示重连错误、消耗或补充恢复次数，也不会重新订阅或打断播放。真正的读取错误才进入原有恢复或不可用路径。取消请求进行中暂停新的校准读取，先前读取的迟到响应不能覆盖取消或新快照；取消未确认时仍保留原任务的权威状态观察。清除、任务替换和完成会结束旧校准。

tvOS/macOS 各自在本机保留最近一次已接受的 Bilibili 任务和所选结果的播放位置。重开应用会核对原控制端点、服务器配置名称、任务和具体结果，恢复原任务观察；播放必须明确选择继续，不会启动时自动播放、重新解析或另建下载任务。已自然播放结束的内容从头重播，普通片尾附近的暂停不会被误当成看完。

续接记录不保存凭据、原视频链接或临时媒体 URL；播放前重新向原服务器确认所选输出，缓存被删、任务过期、服务器不匹配或暂时不可达时会明确提示，允许重试或忘记记录，而不是悄悄下载另一份。位置随正常进度周期及暂停、停止、结束和场景切换保存；强制终止或系统崩溃只能恢复最近已保存的位置，不保证精确到最后一帧。服务器配置名称并非全局唯一或经过认证的实例身份；此功能仍面向可信 LAN。忘记客户端记录不等于取消服务器任务，服务器端下载和落盘恢复机制不变。这是同设备单条续接记录，不是跨设备观看历史；手动输入流地址的既有偏好另行保留。

两端 cache library 已支持每页 50 条的分页浏览和搜索，后续页面通过 page token 加载。

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
