# LAN Cache Server Architecture

## Decision

tvOS Net Player uses gRPC for the control plane only. Media bytes stay on generic playback protocols that `AVPlayer` can consume directly:

- HLS playlists and segments over HTTP.
- MP4 or other compatible assets over HTTP with Range support.

The tvOS app asks the LAN cache server for library state, task state, and playback sources. The playback source response contains a normal HTTP URL, and the app passes that URL to `AVPlayer`.

## Responsibilities

### tvOS App

- Discover or configure the LAN cache server.
- Browse Bonjour `_tvos-net-player._tcp` advertisements and fall back to manual host entry.
- Browse library items and task progress through gRPC.
- Submit Bilibili URLs or IDs as cache tasks.
- Request a playback source for a library item.
- Play the returned HTTP/HLS URL with `AVPlayer`.

The tvOS app should not parse Bilibili APIs, store Bilibili credentials, run BBDown, mount SMB shares, or stream media bytes over gRPC.

### LAN Cache Server

- Run on the Mac mini or another LAN host.
- Publish a Bonjour `_tvos-net-player._tcp` advertisement for the gRPC control-plane listener when discovery is enabled.
- Expose gRPC services for library, task, cache, and playback control.
- Expose HTTP endpoints for media playback.
- Manage cache roots on local disk or mounted shares.
- Call a Bilibili resolver/downloader adapter, initially BBDown-compatible.
- Queue downloads and transcode or remux outputs into tvOS-friendly assets.

### Media Storage

The first cache backend should be a server-local directory. SMB/NAS storage can be added by mounting it on the Mac mini and registering the mounted path as another cache root.

tvOS should not talk to SMB directly in the first design. Keeping SMB behind the LAN cache server gives the app one consistent playback model and avoids shipping SMB client complexity into the Apple TV app.

## BBDown Adapter

The cache server integrates the Rust `bbdown-core` crate from `https://github.com/Joey-Project/BBDown-rust` behind the server-local `BilibiliDownloadAdapter` trait. The dependency is pinned to the `v0.5.0` release commit in `CacheServer/RustCacheServer/Cargo.toml` so CI does not float with the upstream `master` branch.

For this project, BBDown remains an adapter behind the LAN cache server rather than an API the tvOS app talks to directly. The Rust crate runs inside the Mac mini cache server process. CLI execution should remain a fallback or diagnostic path, not the primary app integration model.

The cache server can either:

- Call the Rust `bbdown-core` crate through a server-local adapter trait.
- Call the BBDown CLI as a fallback adapter.
- Proxy to BBDown `serve` for compatibility experiments.
- Later replace BBDown internals with another resolver without changing the tvOS app.

Important limitations to hide behind our server boundary:

- Adapter implementations must report typed task progress instead of forcing the cache server to parse CLI logs.
- Cancellation after a task starts must be observable by the adapter, even if a specific adapter can only finish best-effort cleanup.
- Adapter output paths need to be normalized into stable library item IDs and HTTP playback URLs.

Current Rust crate adapter behavior:

- Worker startup is controlled by `Cache:BilibiliWorkerEnabled`; it defaults to enabled in the normal server runtime.
- Downloads go to `Cache:BBDownOutputDir`, which defaults to `Cache:RootPath/Bilibili`; this path is validated when the worker is enabled or when the output path is explicitly configured, and must be inside `Cache:RootPath` with no `..` parent components and no existing symlink components under the root.
- At runtime, existing root/output path prefixes are canonicalized before constructing the media library and BBDown adapter so common symlink ancestors such as `/tmp` do not make BBDown download to a path the library later rejects.
- Download archive state goes to `Cache:BBDownArchivePath`, defaulting to `bbdown-archive.json` beside `Cache:TaskStatePath`.
- The adapter requires `ffmpeg`; BBDown core downloads the selected media streams, and the server runs its own `ffmpeg` mux step to publish a title-preserving `.mp4` output that the current local media library can index.
- Legacy playback creation still defaults BV/av inputs to current/first page and ss/md inputs to latest episode. Explicit Bilibili task selection can now request single, multiple, range, and all resolved candidates through the Rust server planner while keeping LAN HLS URLs as the only client-facing media URLs.
- The adapter can load BBDown credentials from `Cache:BBDownCredentialPath`, optionally select a named `Cache:BBDownCredentialProfile`, and pass restricted-area `playurl` / Bilibili API proxy lists through to `bbdown-core`. Runtime configuration stores only file paths, profile IDs, and proxy base URLs; Bilibili cookies/access keys stay in the local credential JSON file.
- `ServerService.GetBilibiliCredentialStatus` reports server-owned credential readiness behind `SERVER_CAPABILITY_BILIBILI_CREDENTIAL_STATUS`. It exposes only coarse state, whether a credential path/file/material is present, active/default profile IDs, redacted profile summaries, restricted-area label, proxy counts, and check time; it never returns the credential path, cookie/access key values, proxy URLs, or parse error details. `ServerService.ListBilibiliCredentialProfiles` exposes the same redacted profile list behind `SERVER_CAPABILITY_BILIBILI_CREDENTIAL_PROFILES`. When any BBDown credential path is configured, background playback/cache log sites emit a fixed omitted-detail marker instead of formatting upstream errors that may contain signed URLs or access keys.
- `ServerService.StartBilibiliLoginSession` / `GetBilibiliLoginSession` reserve the control-plane shape for future QR/web login sessions. This slice returns an explicit `unsupported` session state, keeps only the 64 most recent descriptors in memory, and does not write credential files or expose refresh secrets.
- `BilibiliDownloadOptions.quality_preference` maps common labels such as `720p`, `1080p`, `1080p60`, `4k`, and raw Bilibili qn values into BBDown stream selection. `audio_language` maps to BBDown stream selection for complete-download tasks and is carried by progressive playback control-plane requests for later ABR/audio selection work. Download tasks still reject non-empty `encoding_preference`; `prefer_tv_api` selects BBDown core's TV playurl mode for both download planning and progressive playback planning.
- Complete-download Bilibili options expose BBDown `v0.5.0` sidecar controls for cover, subtitles, danmaku, danmaku format selection, and AI subtitle filtering. Non-default subtitle AI policy requires `download_subtitles`, and explicit danmaku formats require `download_danmaku`, so unsupported combinations fail in the control-plane adapter instead of being silently ignored.
- Complete-download Bilibili tasks use `BBDown-rust` `v0.5.0` native download progress and cancellation APIs. The adapter maps coalesced `DownloadProgressEvent` file-level byte updates into the existing task `progress`, `downloaded_bytes`, `total_bytes`, and `message` fields, and bridges running task cancellation into `DownloadCancellationToken` so BBDown can unwind partial downloads through its own cancellation path. Planning cancellation still uses the cache-server polling helper because the planning API has no standalone cancellation token in this release.

Playback planning foundation:

- `bbdown-core` `v0.5.0` exposes playback planning as resolver output: entries, DASH/FLV variants, media URLs, backup URLs, request headers, mime/codec metadata, duration/size metadata, cache keys, ABR groups, and AVPlayer-oriented selection hints.
- The cache server maps those core playback structs into server-owned DTOs before any control-plane or media-pipeline exposure. This keeps BBDown API churn behind the adapter boundary.
- Variant selection starts with BBDown's `PlaybackCodecPreference::avplayer_default()` ranking, supports explicit H.264/HEVC/AV1 preferences for future progressive requests, and falls back to H.264/AAC when an explicit non-H.264 preference is not available.
- `v0.5.0` retains feed/history/watch-later style input parsing and the newer page/list fetch foundation. Legacy playback maps collection/feed-style inputs to the latest item by default, and explicit task selection can expand resolved collection/feed candidates into per-result planning work.
- Playback planning currently rejects Bilibili short links because `bbdown-core` resolves them internally after the caller must already choose a default selection. Supporting short links without incorrect season/collection behavior requires a core API that exposes the resolved `Input` before planning.
- BBDown remains a resolver and metadata provider for progressive playback. The LAN cache server owns source fetch retry, HLS playlist/segment generation, durable cache layout, recovery, and optional LAN-side transcoding.
- The cache server exposes progressive playback through `TaskService.CreateBilibiliPlaybackTask`. The RPC creates a persisted `TASK_KIND_BILIBILI_PROGRESSIVE_PLAYBACK` task and returns it immediately in `PREPARING`; BBDown playback planning runs in the background, registers a runtime HLS session, then publishes persisted `BilibiliPlaybackSession` metadata and a HLS `PlaybackSource` through `GetTask` and `WatchTasks`.
- Public BiliRoaming reverse proxies are treated as web-mode restricted API proxies, not TV API endpoints. TV login remains useful for direct TV playurl checks, but restricted-area public proxy fallback should use the web/app planning path and `Cache:BBDownRestrictedApiProxy`.
- Progressive playback states extend the shared task model with `PLANNED`, `PREPARING`, `PLAYABLE`, and `COMPLETED`. `PREPARING` records in-flight background planning; `PLANNED` remains supported for older metadata-only snapshots; `PLAYABLE` means a HLS session manifest is persisted, a runtime HLS session is registered, and `PlaybackSource.uri` points at `/hls/{session_id}/master.m3u8`; `COMPLETED` means the selected HLS media resources have been cached under the cache root and can be restored for offline LAN playback.
- Apps can report advisory HLS playback position through `CacheService.ReportPlaybackProgress`. Reports carry the playback URL plus optional library item and variant identifiers; the server resolves that into an HLS session id, records active/recent position in memory, refreshes the session's recent-playback lease, and exposes the snapshot through `CacheService.GetHlsCacheStatus.playback`. This is intentionally best-effort control-plane state: playback never blocks on the report, and media bytes still flow only through HTTP/HLS.
- The HLS HTTP routes are `/hls/{session_id}/master.m3u8` and `/hls/{session_id}/segments/{segment_id}`. The master playlist points at per-track media playlists under `segments/*.m3u8`; media playlists expose the selected DASH video/audio requests as fMP4 byte-range playlists with `EXT-X-MAP`. Runtime upstream and first-window-prewarmed resources still use a single VOD-style media range, while fully cached resources can persist verified top-level fMP4 `moof` plus following `mdat` ranges with parsed per-track fragment durations and expose them as multiple HLS byte-range segments only when the ranges continuously cover the media payload through EOF. Multi-track fragments use `tkhd`/`mdhd` track timing, sum repeated `traf` durations for each track, and use the longest track duration for each HLS segment. Timing metadata reads are bounded; oversized `moov`/`moof` payloads fall back to the single-range playlist rather than allocating untrusted metadata into memory. Media playlist generation uses cached MP4 initialization and segment metadata when available and otherwise probes only a bounded MP4 initialization window. Segment requests serve cached media resources first and fall back to proxying BBDown media URLs with the required request headers and client `Range` header. Ranged upstream responses must be `206 Partial Content` with a `Content-Range` matching the requested byte range; invalid ranged responses and retryable CDN statuses fail over to backup URLs, while bounded upstream connect/read timeouts prevent stalled CDN attempts from holding playlist/segment handlers indefinitely.
- HLS cache manifests live under `Cache:RootPath/.tvos-net-player/hls/{session_id}`. `session.json` persists the server-owned HLS session and BBDown media request metadata; per-resource metadata records cached file size, initialization range length, optional validated fMP4 segment byte ranges and durations, content type, and BBDown cache keys as basic integrity and future eviction hooks. Startup loads these manifests into the runtime HLS registry, preserves restorable `PLAYABLE`/`COMPLETED` tasks, and fails only progressive tasks whose persisted playback state has no matching HLS manifest. Older resource metadata without segment ranges remains valid and falls back to the single-range playlist shape.
- When a progressive session becomes complete, newly fetched master playlists stop advertising alternate variants. The in-memory session retains hidden alternate upstream request metadata for one fixed 60-second absolute grace period so clients traversing a previously fetched master can finish stale video/audio playlist and segment requests; after that deadline, the registry atomically replaces the matching runtime session with a fully sanitized copy. Each registry insert receives a monotonic generation token, and the timer can replace only the generation it registered, so an old timer cannot overwrite even a byte-for-byte identical newer session with the same id. The completed manifest is sanitized before persistence at every point; restored hidden resources have no upstream URLs or headers and return `404` unless their bytes are present in the completed cache.
- Once all media resources for an HLS playback session are cached, the background finalizer exposes a virtual `LIBRARY_SOURCE_BILIBILI` item with id `bilibili.hls.<session_id>`. Primary playback-session completion marks the progressive task `COMPLETED` and fills `Task.library_item_id`; non-primary selected results can finalize independently and retain their completed cache item on the result record without replacing the primary compatibility fields. `LibraryService.GetPlaybackSource` returns a fresh HLS URL for each completed item using the current media base URI. If LAN transcoding is enabled and a selected HLS session has a `ready` transcoding plan, the finalizer runs the configured ffmpeg command after the source resources are cached, writes a generated H.264/AAC fMP4 resource, rewrites the completed session manifest to that generated resource, and only then exposes the completed library item. The MVP pins the generated output to H.264 High@4.2/AAC, caps video at 1080p60 with a 10 Mbps video VBV envelope plus 128 kbps audio, and writes matching codec/resolution/frame-rate/bandwidth metadata. Generated fMP4 resources use the same completed-resource segment-index path when the file contains multiple verified fragments with trustworthy timing and full payload coverage, and otherwise fall back to the existing whole-resource byte-range playlist.
- Completed HLS cache has a server-side quota policy. `Cache:HlsCacheMaxBytes` defaults to 50 GiB, `Cache:HlsCacheHighWatermarkPercent` defaults to 90, and `Cache:HlsCacheLowWatermarkPercent` defaults to 80. Setting the max bytes to `0` disables automatic eviction. Before HLS finalization starts, the server checks projected completed-HLS usage; when it would cross the high watermark, it evicts oldest eligible completed HLS sessions down toward the low watermark. Successful finalization, including startup restore shortcuts for already-complete cache resources, runs the same post-cache quota check. A periodic background monitor runs the same cleanup without projected bytes. Eviction is serialized, cancellation-aware for pre-finalization checks, and skips active/protected progressive playback work, incomplete sessions, recently issued/served completed playback sources, playback-position-reported sessions with an active/recent lease, and the session currently being finalized. If protected or projected bytes make the low-watermark target unreachable, eviction still removes eligible unprotected entries and records `target_reached=false` once only the protected/projected portion remains. If task-state persistence is unavailable after a malformed snapshot, missing task records are not treated as orphan authorization for deletion. A missing HLS store directory under an existing cache root scans as empty cache, while missing cache roots, symlinked paths, and unreadable paths fail closed. Successful eviction removes the completed HLS cache directory and matching completed playback task record together. `CacheService.GetHlsCacheStatus` reports quota settings, completed-HLS usage, the last eviction attempt summary, weak-network status, LAN transcoding status, and active/recent playback progress to clients.
- LAN transcoding is exposed through the control plane, task metadata, durable HLS manifests, and the finalizer execution path. `Cache:LanTranscodingEnabled` defaults to `false`; when enabled, `ServerService.GetServerInfo` includes `SERVER_CAPABILITY_LAN_TRANSCODING`, and `CacheService.GetHlsCacheStatus` reports a `LanTranscodingStatus` with the conservative `avplayer-h264-aac-hls-v1` H.264/AAC HLS/fMP4 target profile plus active job count. Each progressive `BilibiliPlaybackSession` and HLS session manifest can persist a `LanTranscodingPlan` with `disabled`, `not_required`, `ready`, or `unsupported` state. A `ready` session is not considered a completed offline item until ffmpeg succeeds and the completed manifest is rewritten to the generated output. Cancellation and preemption kill the ffmpeg child and keep the task out of completed cache exposure.

## Protocol Shape

The control plane starts from `Sources/TVOSNetPlayerCacheClient/Protos/tvos_net_player/v1/cache_control.proto`.

Initial services:

- `ServerService`: server info, health, coarse BBDown credential readiness, redacted credential profiles, and login-session control-plane foundation.
- `LibraryService`: list items, get item details, rescan cache roots, and request playback sources.
- `TaskService`: create Bilibili cache tasks, read task state, watch task events, and request cancellation.
- `TaskService.CreateBilibiliPlaybackTask`: create a progressive Bilibili playback task, return it in `PREPARING`, and publish a playable HLS source later through task reads or watches.
- `CacheService`: list cache roots, read HLS cache quota / weak-network / LAN-transcoding / playback-position status, report advisory playback progress, and delete cached items.

Playback sources intentionally return URLs instead of media bytes.

Bilibili task selection/result execution:

- `CreateBilibiliPlaybackTaskRequest.selection_id` remains the legacy single-candidate path.
- `CreateBilibiliPlaybackTaskRequest.selection` carries selection intent for default/current/single/multiple/range/all item requests. Default/current preserve legacy single-result planning; single/multiple/range/all resolve the input first and then plan each selected candidate.
- `SERVER_CAPABILITY_BILIBILI_RESOLVE` gates resolver calls and the legacy `selection_id` single-candidate playback path. Clients must require `SERVER_CAPABILITY_BILIBILI_TASK_SELECTION` before sending the newer structured `selection` field.
- `SERVER_CAPABILITY_BILIBILI_CREDENTIAL_STATUS` gates `ServerService.GetBilibiliCredentialStatus`. Clients should treat the response as diagnostics/control-plane readiness only; the credential file remains server-local and secret values are never serialized.
- `SERVER_CAPABILITY_BILIBILI_CREDENTIAL_PROFILES` gates `ServerService.ListBilibiliCredentialProfiles`. The profile list contains profile IDs, active/default markers, and credential-material presence booleans only.
- `Task.library_item_id` remains the primary single-result compatibility field.
- `Task.bilibili_selection` persists the normalized selection intent, including legacy `selection_id` mapped to a single-selection intent.
- `Task.result_items` persists per-result state, messages, library item IDs, playback sources, playback session metadata, and any LAN transcoding plan attached to that session.
- The first successful result remains the primary compatibility playback item. The first selected candidate uses the task id as its HLS session id; later candidates use stable `task-id-result-N` HLS session ids and can be served by the LAN HLS endpoint.
- Resolve responses expose whether the candidate window is truncated. Collection/feed-style inputs resolve a bounded candidate window for explicit selection instead of the legacy latest-only item. Explicit range selection fails unless every requested 1-based index is present in the resolved candidate window; `all` selection fails when the resolver reports a truncated candidate window instead of silently planning only the first window.
- Collection/feed `item:` selection ids embed a canonical token for the parsed collection source plus the resolved BVID/AID/CID identity. A selection bound to another favorite/list/feed endpoint is rejected. If the same refreshed recommendation or dynamic feed reorders or omits the item before background planning starts, the server recovers the candidate from the source-bound identity and the BBDown adapter plans the video directly instead of reusing the stale feed index. The completed plan must still match the embedded CID and video identity before it becomes playable.
- Opt-in live e2e cases use one isolated server/cache root per case. The harness records the created task before polling playback, requests task cancellation, explicitly cancels queued/current HLS fills for that task, and requires both a terminal task state and stable server background-idle evidence before stopping listeners or dropping the cache root. Scheduler cancellation is distinct from playback preemption: it removes queued jobs and marks the current job `Cancel`; current-job completion and optional demoted requeue are decided atomically under the scheduler lock, so cancellation cannot land in between and be lost. After stable idle evidence, the harness closes and awaits the per-state HLS fill worker before dropping the cache root or credential-bearing clients. Background-idle evidence includes planning activity registered before `tokio::spawn`, planning/finalization/transcoding permits, active transcoding jobs, and queued/current HLS fill work; timeout diagnostics contain only these counts/booleans, and a timeout stops the suite before another case can be contaminated.
- Primary-result offline HLS cache fill follows the actual primary HLS session id, even when the first successful result is a later `task-id-result-N` candidate. Non-primary result sessions are persisted, recoverable, and queued for demoted background cache finalization after the primary foreground fill. Completed secondary results authorize their own `bilibili.hls.<session_id>` library items and remain grouped with the parent task for deletion and quota cleanup.

Cache deletion contract:

- `CacheService.DeleteLibraryItem` accepts the stable library item id visible in `LibraryService` responses and returns `deleted=false` when the item is already gone.
- `DeleteLibraryItem` is disabled unless `Cache:AllowLibraryItemDelete=true`; when disabled the server returns `permission_denied` and omits `SERVER_CAPABILITY_LIBRARY_ITEM_DELETE` from `ServerInfo`.
- Local cache items delete the validated media file under `Cache:RootPath`; internal `.tvos-net-player/hls` files are not addressable as local library items.
- Completed Bilibili HLS items with ids shaped as `bilibili.hls.<session_id>` delete the HLS cache session directory and remove the runtime HLS session. When the item is the primary compatibility library item, deletion also removes the persisted completed progressive playback task that authorized it. When the item belongs to a secondary batch result, deletion clears that result's completed library metadata and leaves the parent task plus other completed results intact.
- Active or playable-but-not-completed progressive HLS sessions are still controlled through task cancellation, not library item deletion.
- Automatic HLS cache eviction uses the same completed-item cleanup boundary as manual deletion, but only for completed HLS cache entries selected by the quota policy.

## Deployment Notes

- gRPC Swift 2 is the tvOS client library for the control plane.
- The app deployment target is tvOS 18.0 because generated gRPC Swift 2 client code is available on tvOS 18.0 or newer.
- The tvOS and macOS clients use the Network.framework-backed `GRPCNIOTransportHTTP2TransportServices` transport. Plaintext `host[:port]`/`http://host[:port]` endpoints target trusted LAN h2c; `https://host[:port]` endpoints target remote HTTP/2 TLS control-plane access such as Cloudflare Tunnel style hostnames.
- Manual cache server URLs are origin URLs only. Path-prefixed gRPC URLs such as `https://cache.example.com/grpc` are not supported; reverse proxies should route the cache control plane at the configured host root. HTTPS endpoints must use DNS hostnames, not IP literals, so TLS SNI and hostname verification remain well-defined.
- The tvOS and macOS apps declare `NSBonjourServices` for `_tvos-net-player._tcp`; discovery fills the same manual `host:port` control-plane address model that the gRPC client already uses.
- `grpc-swift-nio-transport` is vendored under `Vendor/grpc-swift-nio-transport` at the 2.7.0 source level with a manifest-only Xcode linkage patch for `GRPCNIOTransportCore` direct dependencies. Remove the vendor and return to the upstream package URL once upstream declares the same direct dependencies and Xcode package product linking succeeds without the local patch.
- The server runtime is Rust. `tonic` hosts the gRPC h2c control plane, and `axum` hosts the HTTP media listener. BBDown remains behind an adapter boundary and is not part of the server runtime contract.
- When `Cache:BonjourEnabled=true`, the gRPC listener includes a non-loopback address, and playback media is also LAN-reachable through either a non-loopback media listener or a non-localhost `Cache:PublicMediaBaseUri`, the Rust server publishes `_tvos-net-player._tcp.local.` through mDNS with TXT metadata for `server_id`, `server_name`, and server `version`. Bonjour publication is best-effort: if mDNS registration fails, gRPC and media listeners still start and the apps can use manual host entry. The default `localhost` listeners are not advertised because LAN clients cannot reach them.

## First Implementation Slice

1. Implement a minimal LAN cache server with a local cache directory, gRPC control plane, and HTTP Range media endpoint. Done in the first implementation slice.
2. Add a tvOS gRPC client and simple library screen. Done in the tvOS client slice.
3. Add Bilibili task intake, lookup, watching, and pre-adapter cancellation. Done in the task-intake slice.
4. Add the server-side task worker foundation, adapter boundary, and persisted task state. Done in the worker-foundation slice.
5. Add the real BBDown crate adapter worker that consumes queued Bilibili tasks and materializes finished downloads into the library. Done in the BBDown Rust adapter slice.
6. Add Bonjour discovery once the manual server URL path works. Done in the Bonjour discovery slice.
7. Add HLS/progressive caching for weaker network conditions. Done for runtime passthrough, durable manifest restore, selected-resource offline finalization including multi-result batch finalization, user-visible offline labels, manual deletion of completed HLS cache items, automatic completed-HLS quota eviction, adaptive weak-network policy, LAN transcoding control-plane/persistence, the conservative LAN transcoding execution MVP, and completed-resource segment-index playlist splitting.

## First Slice Notes

The first server slice intentionally implemented only local cache browsing and HTTP playback for complete files (`.mp4`, `.m4v`, and `.mov`). Bilibili task intake/cancellation now feeds a real BBDown Rust crate adapter, and progressive playback can expose runtime passthrough HLS sessions after BBDown planning, restore persisted HLS manifests after restart, finalize selected media resources into offline Bilibili HLS library items for primary and secondary batch results, optionally finalize transcoding-ready sessions into generated H.264/AAC fMP4 HLS output, and split fully cached fMP4 resources into conservative segment-index byte-range playlists. Manual cache item deletion, cache root display, tvOS task submission UI, completed-HLS offline UX, Bonjour discovery, automatic completed-HLS quota eviction, adaptive weak-network policy, and the LAN transcoding execution MVP are implemented. Server-side ABR/transcoding policy remains follow-up work.

Runtime shape:

- The canonical proto lives under `Sources/TVOSNetPlayerCacheClient/Protos/tvos_net_player/v1/cache_control.proto` so the Swift package plugin and Rust server build share one schema source.
- gRPC services are hosted by `tonic` and generated from `Sources/TVOSNetPlayerCacheClient/Protos/tvos_net_player/v1/cache_control.proto`.
- The server uses separate cleartext listeners by default: `http://localhost:50051` for gRPC/h2c and `http://localhost:8080` for HTTP media.
- LAN exposure is explicit: bind `Cache:GrpcListenUrl` and `Cache:MediaListenUrl` to `0.0.0.0` or a specific LAN address only on a trusted network. Wildcard hosts (`0.0.0.0`, `[::]`, `*`, and `+`) try to open both IPv4 and IPv6 listeners, and continue when one address family is unavailable but the other is bound; use a concrete LAN IP to restrict address family or interface. Bonjour advertises all host LAN addresses for wildcard gRPC listeners, but a concrete LAN IP listener advertises only that IP. Bonjour stays disabled unless playback media is also reachable through the LAN media listener or a configured public media base URI. The first slice does not implement authentication.
- Destructive library deletion is opt-in with `Cache:AllowLibraryItemDelete=true` and otherwise returns `permission_denied`.
- `Cache:GrpcListenUrl` and `Cache:MediaListenUrl` must use `http://` in this slice. TLS should be added explicitly later rather than accepting an `https://` URL that the server does not actually serve.
- `LibraryService.GetPlaybackSource` returns an HTTP URL under `/media/{itemId}/{variantId}`.
- `/media/{itemId}/{variantId}` serves files from the configured cache root with Range support enabled.
- The media route is hosted only on the configured media listener. The gRPC listener does not serve media bytes.
- Media file opens are fail-closed unless the host platform supports no-follow, root-anchored file opens. The first slice supports HTTP Range playback on macOS, matching the Mac mini deployment target. Linux and other platforms can list basic local item identities, but they do not advertise HTTP Range playback, return playable variants, expose file size/mtime metadata, or serve media bytes until equivalent no-reparse open-by-handle semantics are implemented and covered per architecture.
- `Cache:RootPath` is treated as a security boundary and must not contain symlink path components; use the real canonical directory path when a shell alias such as `/tmp` or `/var` would resolve through a system symlink.
- `TaskService` accepts Bilibili URL/BV task intake into a persisted queue, returns active duplicate download submissions as the same task, streams task snapshots and updates through `WatchTasks`, and supports idempotent queued cancellation plus running/preparing cancellation requests. Progressive playback task creation is request-scoped, so repeated playback creates return fresh `preparing` tasks rather than sharing a HLS URI derived from an earlier gRPC request; BBDown playback planning runs in the background, then publishes `playable` `BilibiliPlaybackSession` metadata plus a HLS `PlaybackSource` through `GetTask`/`WatchTasks` once the runtime HLS session is registered.
- Durable task lifecycle state is written as a JSON snapshot to `Cache:TaskStatePath` after queue, claim, cancellation, and completion mutations. High-frequency progress updates remain in memory and on `WatchTasks` instead of forcing a disk sync per update. Lifecycle mutations generate immutable snapshots under the registry lock, then serialize snapshot write/fsync outside that lock through a generation-ordered persistence coordinator so stale snapshots cannot overwrite newer state. On restart, terminal tasks remain terminal, queued tasks remain queued, `running` tasks are conservatively restored as `queued`, and `cancel_requested` tasks are restored as `cancelled` because the interrupted worker is gone and the user's cancellation intent should not be retried as new work. If the snapshot cannot be loaded because it is malformed or from an unsupported schema, the server starts with an empty in-memory registry and disables task-state writeback so the original file is preserved for repair. Persisted task snapshots are pruned on lifecycle writeback using count and age retention limits for ordinary terminal task history. Active tasks are never pruned, and completed progressive HLS playback tasks remain retained until cache eviction can delete the matching HLS cache session and virtual library item atomically.
- The server has a worker-facing task state machine and `BilibiliDownloadAdapter` trait. The default app state starts the real BBDown Rust crate adapter unless `Cache:BilibiliWorkerEnabled=false`. Tests use mock adapters and disabled real-worker integration servers to cover `queued -> running -> succeeded/failed/cancelled` transitions, progress updates, bounded worker concurrency, adapter-visible cancellation, restart recovery, and control-plane behavior without relying on live Bilibili network access.
- The tvOS client currently loads a bounded first-page library preview, capped at the server page-size limit of 200 items. The cache client API exposes page tokens and search text, so full library pagination/search should be added in the next library UI iteration instead of making refresh collect every page.

Configuration:

- `Cache:RootPath`: local cache directory. Defaults to `cache` under the app base directory.
- `Cache:ServerName`: friendly server name returned by `ServerService.GetServerInfo`.
- `Cache:GrpcListenUrl`: gRPC listen URL. Defaults to `http://localhost:50051`.
- `Cache:MediaListenUrl`: HTTP media listen URL. Defaults to `http://localhost:8080`.
- `Cache:PublicMediaBaseUri`: optional public base URL for playback URLs when the server sits behind a proxy.
- `Cache:BonjourEnabled`: enables Bonjour/mDNS publication when both the control-plane gRPC endpoint and media playback endpoint are LAN-reachable. Defaults to `true`, but the default `localhost` listeners are intentionally not advertised; bind both `Cache:GrpcListenUrl` and `Cache:MediaListenUrl` to `0.0.0.0`, `[::]`, or trusted LAN IPs, or configure a non-localhost `Cache:PublicMediaBaseUri` for playback. Wildcard listeners publish automatically detected LAN addresses; concrete LAN IP listeners publish only that configured IP.
- `Cache:TaskStatePath`: JSON task snapshot path. Defaults to `cache-server-state/tasks.json` next to the server executable.
- `Cache:TaskRetentionMaxTerminalTasks`: maximum ordinary terminal task records to retain in the persisted task snapshot. Defaults to `200`; set `0` to disable this limit.
- `Cache:TaskRetentionTerminalAgeDays`: maximum age in days for ordinary terminal task records in the persisted task snapshot. Defaults to `30`; set `0` to disable this limit.
- `Cache:AllowLibraryItemDelete`: enables destructive cache library deletion RPCs and the matching `SERVER_CAPABILITY_LIBRARY_ITEM_DELETE` capability. Defaults to `false` while the control plane is cleartext and unauthenticated.
- `Cache:HlsCacheMaxBytes`: maximum completed HLS cache budget before automatic eviction. Defaults to 50 GiB; set `0` to disable automatic HLS eviction.
- `Cache:HlsCacheHighWatermarkPercent`: high watermark that triggers completed-HLS cleanup when current or projected usage crosses it. Defaults to `90`.
- `Cache:HlsCacheLowWatermarkPercent`: target watermark for cleanup after the high watermark triggers. Defaults to `80` and must be lower than the high watermark.
- `Cache:BilibiliWorkerEnabled`: starts the real BBDown worker when true. Defaults to `true`.
- `Cache:BilibiliWorkerMaxConcurrentTasks`: maximum task worker concurrency. Defaults to `1`; the real BBDown adapter currently caps effective concurrency at `1` to avoid concurrent writes to the same archive.
- `Cache:BBDownOutputDir`: BBDown download output directory. It defaults to `Cache:RootPath/Bilibili`; when the worker is enabled or this path is explicitly configured, it must be inside `Cache:RootPath`, with no `..` parent components and no existing symlink components under the root.
- Existing root/output prefixes are canonicalized at runtime before the media library and BBDown adapter are built.
- `Cache:BBDownArchivePath`: BBDown archive JSON path. Defaults to `bbdown-archive.json` beside `Cache:TaskStatePath`.
- `Cache:BBDownFfmpegPath`: `ffmpeg` executable path. Defaults to `ffmpeg` from `PATH`.
- `Cache:LanTranscodingEnabled`: enables LAN transcoding planning/execution and advertises `SERVER_CAPABILITY_LAN_TRANSCODING`. Defaults to `false`.
- `Cache:LanTranscodingFfmpegPath`: `ffmpeg` executable path for LAN transcoding execution. Defaults to `ffmpeg` from `PATH`.
- `Cache:LanTranscodingMaxConcurrentJobs`: LAN transcoding job concurrency. The execution MVP currently supports exactly `1` because the HLS cache fill worker is single-threaded; values above `1` are rejected until parallel fill workers are implemented.
- `Cache:BBDownCredentialPath`: optional BBDown credential JSON path. Supported fields are `cookie`, `access_key`, and `tv_access_key`.
- `Cache:BBDownCredentialProfile`: optional named credential profile to load from the BBDown credential store. When unset, the store default profile is used.
- `Cache:BBDownRestrictedArea`: optional restricted-area hint: `cn`, `th`, `hk`, or `tw`.
- `Cache:BBDownRestrictedAreaProxy`: optional comma-separated restricted-area playurl proxy specs. Each spec is `[area=]URL`; the URL must use `http` or `https`.
- `Cache:BBDownRestrictedApiProxy`: optional comma-separated restricted-area Bilibili API proxy specs. Each spec is `[area=]URL`; the URL must use `http` or `https`.
