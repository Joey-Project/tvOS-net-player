# LAN Cache Server Architecture

## Decision

tvOS Net Player uses gRPC for the control plane only. Media bytes stay on generic playback protocols that `AVPlayer` can consume directly:

- HLS playlists and segments over HTTP.
- MP4 or other compatible assets over HTTP with Range support.

The tvOS app asks the LAN cache server for library state, task state, and playback sources. The playback source response contains a normal HTTP URL, and the app passes that URL to `AVPlayer`.

## Responsibilities

### tvOS App

- Discover or configure the LAN cache server.
- Browse library items and task progress through gRPC.
- Submit Bilibili URLs or IDs as cache tasks.
- Request a playback source for a library item.
- Play the returned HTTP/HLS URL with `AVPlayer`.

The tvOS app should not parse Bilibili APIs, store Bilibili credentials, run BBDown, mount SMB shares, or stream media bytes over gRPC.

### LAN Cache Server

- Run on the Mac mini or another LAN host.
- Expose gRPC services for library, task, cache, and playback control.
- Expose HTTP endpoints for media playback.
- Manage cache roots on local disk or mounted shares.
- Call a Bilibili resolver/downloader adapter, initially BBDown-compatible.
- Queue downloads and transcode or remux outputs into tvOS-friendly assets.

### Media Storage

The first cache backend should be a server-local directory. SMB/NAS storage can be added by mounting it on the Mac mini and registering the mounted path as another cache root.

tvOS should not talk to SMB directly in the first design. Keeping SMB behind the LAN cache server gives the app one consistent playback model and avoids shipping SMB client complexity into the Apple TV app.

## BBDown Adapter

The local BBDown checkout already contains a `serve` mode with a JSON task API. It can add tasks, report running and finished tasks, and return saved output paths.

For this project, BBDown should be treated as an adapter behind the LAN cache server rather than the API the tvOS app talks to directly. The cache server can either:

- Call the BBDown CLI.
- Proxy to BBDown `serve`.
- Later replace BBDown internals with a custom resolver without changing the tvOS app.

Important limitations to hide behind our server boundary:

- BBDown does not currently provide robust per-task cancellation after a task has started.
- BBDown's server API does not enforce a download queue or concurrency limit.
- BBDown output paths need to be normalized into stable library item IDs and HTTP playback URLs.

## Protocol Shape

The control plane starts from `Sources/TVOSNetPlayerCacheClient/Protos/tvos_net_player/v1/cache_control.proto`.

Initial services:

- `ServerService`: server info and health.
- `LibraryService`: list items, get item details, rescan cache roots, and request playback sources.
- `TaskService`: create Bilibili cache tasks, read task state, watch task events, and request cancellation.
- `CacheService`: list cache roots and delete cached items.

Playback sources intentionally return URLs instead of media bytes.

## Deployment Notes

- gRPC Swift 2 is the tvOS client library for the control plane.
- The app deployment target is tvOS 18.0 because generated gRPC Swift 2 client code is available on tvOS 18.0 or newer.
- The tvOS client uses the Network.framework-backed `GRPCNIOTransportHTTP2TransportServices` transport with plaintext h2c to the trusted LAN cache server.
- `grpc-swift-nio-transport` is vendored under `Vendor/grpc-swift-nio-transport` at the 2.7.0 source level with a manifest-only Xcode linkage patch for `GRPCNIOTransportCore` direct dependencies. Remove the vendor and return to the upstream package URL once upstream declares the same direct dependencies and Xcode package product linking succeeds without the local patch.
- The server can be implemented in .NET because BBDown is already .NET and the Mac mini can run the downloader, ffmpeg, and filesystem watchers locally.

## First Implementation Slice

1. Implement a minimal LAN cache server with a local cache directory, gRPC control plane, and HTTP Range media endpoint. Done in the first implementation slice.
2. Add a tvOS gRPC client and simple library screen. Done in the tvOS client slice.
3. Add Bilibili task intake, lookup, watching, and pre-adapter cancellation. Done in the task-intake slice.
4. Add the real BBDown adapter worker that consumes queued Bilibili tasks and materializes finished downloads into the library.
5. Add Bonjour discovery once the manual server URL path works.
6. Add HLS/progressive caching for weaker network conditions.

## First Slice Notes

The first server slice intentionally implements only local cache browsing and HTTP playback for complete files (`.mp4`, `.m4v`, and `.mov`). Bilibili tasks, task cancellation, cache deletion, Bonjour discovery, and HLS playlist/segment management remain follow-up work.

Runtime shape:

- The canonical proto lives under `Sources/TVOSNetPlayerCacheClient/Protos/tvos_net_player/v1/cache_control.proto` so the Swift package plugin and .NET server share one schema source.
- gRPC services are hosted by ASP.NET Core and generated from `Sources/TVOSNetPlayerCacheClient/Protos/tvos_net_player/v1/cache_control.proto`.
- The server uses separate cleartext listeners by default: `http://localhost:50051` for gRPC/h2c and `http://localhost:8080` for HTTP media.
- LAN exposure is explicit: bind `Cache:GrpcListenUrl` and `Cache:MediaListenUrl` to `0.0.0.0` or a specific LAN address only on a trusted network. The first slice does not implement authentication.
- `Cache:GrpcListenUrl` and `Cache:MediaListenUrl` must use `http://` in this slice. TLS should be added explicitly later rather than accepting an `https://` URL that Kestrel does not actually serve.
- `LibraryService.GetPlaybackSource` returns an HTTP URL under `/media/{itemId}/{variantId}`.
- `/media/{itemId}/{variantId}` serves files from the configured cache root with Range support enabled.
- The media route is also constrained to the configured media listener port, so the gRPC listener does not serve media bytes even though both listeners share one ASP.NET Core app in this slice.
- Media file opens are fail-closed unless the host platform supports no-follow, root-anchored file opens. The first slice supports HTTP Range playback on macOS, matching the Mac mini deployment target. Linux and other platforms can list basic local item identities, but they do not advertise HTTP Range playback, return playable variants, expose file size/mtime metadata, or serve media bytes until equivalent no-reparse open-by-handle semantics are implemented and covered per architecture.
- `TaskService` accepts Bilibili URL/BV task intake into an in-memory queue, returns active duplicate submissions as the same task, streams task snapshots and updates through `WatchTasks`, and supports idempotent cancellation before a download adapter starts.
- Submitted Bilibili tasks intentionally remain queued in this slice. The next server slice should add the BBDown adapter worker behind the same task boundary rather than exposing BBDown's JSON API directly to tvOS.
- The tvOS client currently loads a bounded first-page library preview, capped at the server page-size limit of 200 items. The cache client API exposes page tokens and search text, so full library pagination/search should be added in the next library UI iteration instead of making refresh collect every page.

Configuration:

- `Cache:RootPath`: local cache directory. Defaults to `cache` under the app base directory.
- `Cache:ServerName`: friendly server name returned by `ServerService.GetServerInfo`.
- `Cache:GrpcListenUrl`: gRPC listen URL. Defaults to `http://localhost:50051`.
- `Cache:MediaListenUrl`: HTTP media listen URL. Defaults to `http://localhost:8080`.
- `Cache:PublicMediaBaseUri`: optional public base URL for playback URLs when the server sits behind a proxy.
