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

The cache server integrates the Rust `bbdown-core` crate from `https://github.com/Joey-Project/BBDown-rust` behind the server-local `BilibiliDownloadAdapter` trait. The dependency is pinned to the `v0.3.0` release tag in `CacheServer/RustCacheServer/Cargo.toml` so CI does not float with the upstream `master` branch.

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
- The adapter can load BBDown credentials from `Cache:BBDownCredentialPath` and pass restricted-area `playurl` / Bilibili API proxy lists through to `bbdown-core`. Runtime configuration stores only file paths and proxy base URLs; Bilibili cookies/access keys stay in the local credential JSON file.
- `BilibiliDownloadOptions.quality_preference` maps common labels such as `720p`, `1080p`, `1080p60`, `4k`, and raw Bilibili qn values into BBDown stream selection. Download tasks still reject non-empty `encoding_preference`; `prefer_tv_api` selects BBDown core's TV playurl mode for both download planning and progressive playback planning.
- BBDown core currently does not expose a chunk-level progress callback or cancellation hook for this adapter path. The worker reports coarse phases and marks late cancellation as cancelled after the core call returns; files may already exist on disk and can be discovered by library rescan.

Playback planning foundation:

- `bbdown-core` `v0.3.0` exposes playback planning as resolver output: entries, DASH/FLV variants, media URLs, backup URLs, request headers, mime/codec metadata, duration/size metadata, cache keys, ABR groups, and AVPlayer-oriented selection hints.
- The cache server maps those core playback structs into server-owned DTOs before any control-plane or media-pipeline exposure. This keeps BBDown API churn behind the adapter boundary.
- Variant selection starts with BBDown's `PlaybackCodecPreference::avplayer_default()` ranking, supports explicit H.264/HEVC/AV1 preferences for future progressive requests, and falls back to H.264/AAC when an explicit non-H.264 preference is not available.
- `v0.3.0` adds feed/history/watch-later style input parsing. Legacy playback maps those new collection/feed-style inputs to the latest item by default, and explicit task selection can expand resolved collection/feed candidates into per-result planning work.
- Playback planning currently rejects Bilibili short links because `bbdown-core` resolves them internally after the caller must already choose a default selection. Supporting short links without incorrect season/collection behavior requires a core API that exposes the resolved `Input` before planning.
- BBDown remains a resolver and metadata provider for progressive playback. The LAN cache server owns source fetch retry, HLS playlist/segment generation, durable cache layout, recovery, and optional LAN-side transcoding.
- The cache server exposes progressive playback through `TaskService.CreateBilibiliPlaybackTask`. The RPC creates a persisted `TASK_KIND_BILIBILI_PROGRESSIVE_PLAYBACK` task and returns it immediately in `PREPARING`; BBDown playback planning runs in the background, registers a runtime HLS session, then publishes persisted `BilibiliPlaybackSession` metadata and a HLS `PlaybackSource` through `GetTask` and `WatchTasks`.
- Progressive playback states extend the shared task model with `PLANNED`, `PREPARING`, `PLAYABLE`, and `COMPLETED`. `PREPARING` records in-flight background planning; `PLANNED` remains supported for older metadata-only snapshots; `PLAYABLE` means a HLS session manifest is persisted, a runtime HLS session is registered, and `PlaybackSource.uri` points at `/hls/{session_id}/master.m3u8`; `COMPLETED` means the selected HLS media resources have been cached under the cache root and can be restored for offline LAN playback.
- The HLS HTTP routes are `/hls/{session_id}/master.m3u8` and `/hls/{session_id}/segments/{segment_id}`. The master playlist points at per-track media playlists under `segments/*.m3u8`; media playlists expose the selected DASH video/audio requests as single VOD-style fMP4 byte-range segments with `EXT-X-MAP`. Media playlist generation uses cached MP4 initialization metadata when available and otherwise probes only a bounded MP4 initialization window. Segment requests serve cached media resources first and fall back to proxying BBDown media URLs with the required request headers and client `Range` header. Ranged upstream responses must be `206 Partial Content` with a `Content-Range` matching the requested byte range; invalid ranged responses and retryable CDN statuses fail over to backup URLs, while bounded upstream connect/read timeouts prevent stalled CDN attempts from holding playlist/segment handlers indefinitely.
- HLS cache manifests live under `Cache:RootPath/.tvos-net-player/hls/{session_id}`. `session.json` persists the server-owned HLS session and BBDown media request metadata; per-resource metadata records cached file size, initialization range length, content type, and BBDown cache keys as basic integrity and future eviction hooks. Startup loads these manifests into the runtime HLS registry, preserves restorable `PLAYABLE`/`COMPLETED` tasks, and fails only progressive tasks whose persisted playback state has no matching HLS manifest.
- Once all selected HLS media resources are cached, the background finalizer marks the progressive task `COMPLETED` and exposes a virtual `LIBRARY_SOURCE_BILIBILI` item with id `bilibili.hls.<session_id>`. `LibraryService.GetPlaybackSource` returns a fresh HLS URL for that item using the current media base URI. The first offline slice does not transcode or remux progressive media; LAN-side transcoding and richer weak-network scheduling remain later extensions.
- Completed HLS cache has a server-side quota policy. `Cache:HlsCacheMaxBytes` defaults to 50 GiB, `Cache:HlsCacheHighWatermarkPercent` defaults to 90, and `Cache:HlsCacheLowWatermarkPercent` defaults to 80. Setting the max bytes to `0` disables automatic eviction. Before HLS finalization starts, the server checks projected completed-HLS usage; when it would cross the high watermark, it evicts oldest eligible completed HLS sessions down toward the low watermark. Successful finalization, including startup restore shortcuts for already-complete cache resources, runs the same post-cache quota check. A periodic background monitor runs the same cleanup without projected bytes. Eviction is serialized, cancellation-aware for pre-finalization checks, and skips active/protected progressive playback work, incomplete sessions, recently issued/served completed playback sources, and the session currently being finalized. If protected or projected bytes make the low-watermark target unreachable, eviction still removes eligible unprotected entries and records `target_reached=false` once only the protected/projected portion remains. If task-state persistence is unavailable after a malformed snapshot, missing task records are not treated as orphan authorization for deletion. A missing HLS store directory under an existing cache root scans as empty cache, while missing cache roots, symlinked paths, and unreadable paths fail closed. Successful eviction removes the completed HLS cache directory and matching completed playback task record together. `CacheService.GetHlsCacheStatus` reports quota settings, completed-HLS usage, and the last eviction attempt summary to clients.

## Protocol Shape

The control plane starts from `Sources/TVOSNetPlayerCacheClient/Protos/tvos_net_player/v1/cache_control.proto`.

Initial services:

- `ServerService`: server info and health.
- `LibraryService`: list items, get item details, rescan cache roots, and request playback sources.
- `TaskService`: create Bilibili cache tasks, read task state, watch task events, and request cancellation.
- `TaskService.CreateBilibiliPlaybackTask`: create a progressive Bilibili playback task, return it in `PREPARING`, and publish a playable HLS source later through task reads or watches.
- `CacheService`: list cache roots, read HLS cache quota status, and delete cached items.

Playback sources intentionally return URLs instead of media bytes.

Bilibili task selection/result execution:

- `CreateBilibiliPlaybackTaskRequest.selection_id` remains the legacy single-candidate path.
- `CreateBilibiliPlaybackTaskRequest.selection` carries selection intent for default/current/single/multiple/range/all item requests. Default/current preserve legacy single-result planning; single/multiple/range/all resolve the input first and then plan each selected candidate.
- `Task.library_item_id` remains the primary single-result compatibility field.
- `Task.bilibili_selection` persists the normalized selection intent, including legacy `selection_id` mapped to a single-selection intent.
- `Task.result_items` persists per-result state, messages, library item IDs, playback sources, and playback session metadata.
- The first successful result remains the primary compatibility playback item. The first selected candidate uses the task id as its HLS session id; later candidates use stable `task-id-result-N` HLS session ids and can be served by the LAN HLS endpoint.
- Primary-result offline HLS cache fill continues to use the existing finalization path only when the primary session is the parent task id. Later-candidate result sessions are persisted and recoverable as playable HLS sessions; batch cache finalization policy remains deferred to the client UX/cache-management follow-up.

Cache deletion contract:

- `CacheService.DeleteLibraryItem` accepts the stable library item id visible in `LibraryService` responses and returns `deleted=false` when the item is already gone.
- `DeleteLibraryItem` is disabled unless `Cache:AllowLibraryItemDelete=true`; when disabled the server returns `permission_denied` and omits `SERVER_CAPABILITY_LIBRARY_ITEM_DELETE` from `ServerInfo`.
- Local cache items delete the validated media file under `Cache:RootPath`; internal `.tvos-net-player/hls` files are not addressable as local library items.
- Completed Bilibili HLS items with ids shaped as `bilibili.hls.<session_id>` delete the HLS cache session directory, remove the runtime HLS session, and remove the persisted completed progressive playback task that authorized the virtual library item.
- Active or playable-but-not-completed progressive HLS sessions are still controlled through task cancellation, not library item deletion.
- Automatic HLS cache eviction uses the same completed-item cleanup boundary as manual deletion, but only for completed HLS cache entries selected by the quota policy.

## Deployment Notes

- gRPC Swift 2 is the tvOS client library for the control plane.
- The app deployment target is tvOS 18.0 because generated gRPC Swift 2 client code is available on tvOS 18.0 or newer.
- The tvOS client uses the Network.framework-backed `GRPCNIOTransportHTTP2TransportServices` transport with plaintext h2c to the trusted LAN cache server.
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
7. Add HLS/progressive caching for weaker network conditions. Done for runtime passthrough, durable manifest restore, selected-resource offline finalization, user-visible offline labels, manual deletion of completed HLS cache items, and automatic completed-HLS quota eviction; transcoding and richer weak-network scheduling remain follow-up work.

## First Slice Notes

The first server slice intentionally implemented only local cache browsing and HTTP playback for complete files (`.mp4`, `.m4v`, and `.mov`). Bilibili task intake/cancellation now feeds a real BBDown Rust crate adapter, and progressive playback can expose runtime passthrough HLS sessions after BBDown planning, restore persisted HLS manifests after restart, and finalize selected media resources into offline Bilibili HLS library items. Manual cache item deletion, cache root display, tvOS task submission UI, completed-HLS offline UX, Bonjour discovery, and automatic completed-HLS quota eviction are implemented. Richer BBDown option mapping, transcoding, and weak-network fill/prewarm scheduling remain follow-up work.

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
- `Cache:BBDownCredentialPath`: optional BBDown credential JSON path. Supported fields are `cookie`, `access_key`, and `tv_access_key`.
- `Cache:BBDownRestrictedArea`: optional restricted-area hint: `cn`, `th`, `hk`, or `tw`.
- `Cache:BBDownRestrictedAreaProxy`: optional comma-separated restricted-area playurl proxy specs. Each spec is `[area=]URL`; the URL must use `http` or `https`.
- `Cache:BBDownRestrictedApiProxy`: optional comma-separated restricted-area Bilibili API proxy specs. Each spec is `[area=]URL`; the URL must use `http` or `https`.
