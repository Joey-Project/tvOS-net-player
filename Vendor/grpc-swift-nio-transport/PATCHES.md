# Local Patches

Vendored source: `grpc/grpc-swift-nio-transport` 2.7.0.

## Package Manifest

`Package.swift` adds direct dependencies from `GRPCNIOTransportCore` to:

- `NIOHTTP1` from `swift-nio`
- `NIOTLS` from `swift-nio`

The source imports these modules directly. SwiftPM CLI builds can succeed without the declarations, but Xcode package product linking for the tvOS app fails with undefined symbols unless the dependencies are explicit.

Remove this vendor directory and return the root package dependency to the upstream URL once upstream declares these direct dependencies and Xcode package product linking passes without the local patch.
