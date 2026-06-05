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

The control plane starts from `proto/tvos_net_player/v1/cache_control.proto`.

Initial services:

- `ServerService`: server info and health.
- `LibraryService`: list items, get item details, rescan cache roots, and request playback sources.
- `TaskService`: create Bilibili cache tasks, read task state, watch task events, and request cancellation.
- `CacheService`: list cache roots and delete cached items.

Playback sources intentionally return URLs instead of media bytes.

## Deployment Notes

- gRPC Swift 2 is the preferred tvOS client library if the app can move to tvOS 18.0 or newer.
- The current app deployment target is tvOS 17.0, so adopting gRPC Swift 2 likely requires raising the deployment target.
- The server can be implemented in .NET because BBDown is already .NET and the Mac mini can run the downloader, ffmpeg, and filesystem watchers locally.

## First Implementation Slice

1. Implement a minimal LAN cache server with a local cache directory, gRPC control plane, and HTTP Range media endpoint. Done in the first implementation slice.
2. Add a tvOS gRPC client and simple library screen.
3. Add Bilibili task creation through a BBDown adapter.
4. Add Bonjour discovery once the manual server URL path works.
5. Add HLS/progressive caching for weaker network conditions.

## First Slice Notes

The first server slice intentionally implements only local cache browsing and HTTP playback for complete files (`.mp4`, `.m4v`, and `.mov`). Bilibili tasks, task cancellation, cache deletion, Bonjour discovery, and HLS playlist/segment management remain follow-up work.

Runtime shape:

- gRPC services are hosted by ASP.NET Core and generated from `proto/tvos_net_player/v1/cache_control.proto`.
- The server uses separate cleartext listeners by default: `http://0.0.0.0:50051` for gRPC/h2c and `http://0.0.0.0:8080` for HTTP media.
- `LibraryService.GetPlaybackSource` returns an HTTP URL under `/media/{itemId}/{variantId}`.
- `/media/{itemId}/{variantId}` serves files from the configured cache root with Range support enabled.
- Media file opens are fail-closed unless the host platform supports no-follow, root-anchored file opens. The first slice supports macOS and Linux; other platforms can list local files but do not serve media bytes until equivalent no-reparse open-by-handle semantics are implemented.
- `TaskService` returns `UNIMPLEMENTED` for Bilibili task creation and cancellation until the BBDown adapter lands.

Configuration:

- `Cache:RootPath`: local cache directory. Defaults to `cache` under the app base directory.
- `Cache:ServerName`: friendly server name returned by `ServerService.GetServerInfo`.
- `Cache:GrpcListenUrl`: gRPC listen URL. Defaults to `http://0.0.0.0:50051`.
- `Cache:MediaListenUrl`: HTTP media listen URL. Defaults to `http://0.0.0.0:8080`.
- `Cache:PublicMediaBaseUri`: optional public base URL for playback URLs when the server sits behind a proxy.
