---
id: 20260605-c4d8a2
title: tvOS Cache gRPC Client
status: completed
created: 2026-06-05
updated: 2026-06-05
branch: tvos-cache-grpc-client
pr:
supersedes: []
superseded_by:
---

# tvOS Cache gRPC Client

## Summary

- Added the first tvOS cache client slice: manual LAN cache server address, gRPC server/library calls, cached item list, playback source lookup, and handoff to the existing `AVPlayer` URL loader.
- Moved the shared cache control proto under `Sources/TVOSNetPlayerCacheClient/Protos/` so Swift package generation and the .NET cache server share one schema source.
- Raised the app deployment target to tvOS 18.0 to support gRPC Swift 2 generated client code.

## Current State

- `TVOSNetPlayerCacheClient` is a Swift package product generated from `cache_control.proto` using `GRPCProtobufGenerator`.
- The app uses `CacheLibraryViewModel` to persist a manual cache server address, refresh server/library state, and request HTTP playback sources.
- Media bytes still flow through ordinary HTTP URLs returned by the cache server; gRPC remains control-plane only.
- The client uses `GRPCNIOTransportHTTP2TransportServices` for Network.framework-backed plaintext h2c on the trusted LAN.
- `grpc-swift-nio-transport` is vendored under `Vendor/grpc-swift-nio-transport` with a manifest-only patch that adds direct `NIOHTTP1` and `NIOTLS` dependencies required by Xcode package product linking.

## Next Steps

- Add BBDown adapter task creation so Bilibili URLs/BV IDs can be submitted from the tvOS app and cached on the Mac mini.
- Add Bonjour discovery after the manual cache server address path remains stable.
- Replace the vendored gRPC transport with the upstream package URL when upstream manifest dependencies make Xcode package product linking pass without the local patch.

## Evidence

- `swift test`
- `just test-tvos`
- Architecture decision: `docs/architecture/cache-server.md`
