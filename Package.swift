// swift-tools-version: 6.1

import PackageDescription

let package = Package(
    name: "TVOSNetPlayer",
    platforms: [
        .macOS(.v15),
        .tvOS(.v18),
    ],
    products: [
        .library(name: "TVOSNetPlayerCore", targets: ["TVOSNetPlayerCore"]),
        .library(name: "TVOSNetPlayerCacheClient", targets: ["TVOSNetPlayerCacheClient"]),
        .executable(name: "task-watch-ui-fixture", targets: ["TaskWatchUIFixture"]),
    ],
    dependencies: [
        .package(url: "https://github.com/grpc/grpc-swift-2.git", from: "2.4.1"),
        .package(path: "Vendor/grpc-swift-nio-transport"),
        .package(url: "https://github.com/grpc/grpc-swift-protobuf.git", from: "2.4.0"),
        .package(url: "https://github.com/apple/swift-nio-transport-services.git", from: "1.28.0"),
        .package(url: "https://github.com/apple/swift-protobuf.git", from: "1.38.0"),
    ],
    targets: [
        .target(
            name: "TVOSNetPlayerCore",
            dependencies: ["TVOSNetPlayerCacheClient"]
        ),
        .target(
            name: "TVOSNetPlayerCacheClient",
            dependencies: [
                .product(name: "GRPCCore", package: "grpc-swift-2"),
                .product(name: "GRPCNIOTransportHTTP2TransportServices", package: "grpc-swift-nio-transport"),
                .product(name: "GRPCProtobuf", package: "grpc-swift-protobuf"),
                .product(name: "NIOTransportServices", package: "swift-nio-transport-services"),
                .product(name: "SwiftProtobuf", package: "swift-protobuf"),
            ],
            plugins: [
                .plugin(name: "GRPCProtobufGenerator", package: "grpc-swift-protobuf")
            ]
        ),
        .testTarget(
            name: "TVOSNetPlayerCoreTests",
            dependencies: [
                "TVOSNetPlayerCore",
                .product(name: "GRPCCore", package: "grpc-swift-2"),
                .product(name: "GRPCProtobuf", package: "grpc-swift-protobuf"),
                .product(name: "GRPCNIOTransportHTTP2Posix", package: "grpc-swift-nio-transport"),
                .product(name: "SwiftProtobuf", package: "swift-protobuf"),
            ],
            path: "Tests/TVOSNetPlayerCoreTests"
        ),
        .testTarget(
            name: "TVOSNetPlayerCacheClientTests",
            dependencies: [
                "TVOSNetPlayerCacheClient",
                .product(name: "GRPCCore", package: "grpc-swift-2"),
            ],
            path: "Tests/TVOSNetPlayerCacheClientTests"
        ),
        .executableTarget(
            name: "TaskWatchUIFixture",
            dependencies: [
                "TVOSNetPlayerCacheClient",
                .product(name: "GRPCCore", package: "grpc-swift-2"),
                .product(name: "GRPCProtobuf", package: "grpc-swift-protobuf"),
                .product(name: "GRPCNIOTransportHTTP2Posix", package: "grpc-swift-nio-transport"),
                .product(name: "SwiftProtobuf", package: "swift-protobuf"),
            ],
            path: "Sources/TaskWatchUIFixture"
        ),
        .testTarget(
            name: "TaskWatchUIFixtureTests",
            dependencies: [
                "TaskWatchUIFixture",
                "TVOSNetPlayerCore",
                "TVOSNetPlayerCacheClient",
                .product(name: "GRPCCore", package: "grpc-swift-2"),
                .product(name: "GRPCProtobuf", package: "grpc-swift-protobuf"),
                .product(name: "GRPCNIOTransportHTTP2Posix", package: "grpc-swift-nio-transport"),
                .product(name: "SwiftProtobuf", package: "swift-protobuf"),
            ],
            path: "Tests/TaskWatchUIFixtureTests"
        ),
    ]
)
