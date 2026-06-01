// swift-tools-version: 6.0

import PackageDescription

let package = Package(
    name: "TVOSNetPlayer",
    platforms: [
        .macOS(.v15)
    ],
    products: [
        .library(name: "TVOSNetPlayerCore", targets: ["TVOSNetPlayerCore"])
    ],
    targets: [
        .target(
            name: "TVOSNetPlayerCore"
        ),
        .testTarget(
            name: "TVOSNetPlayerCoreTests",
            dependencies: ["TVOSNetPlayerCore"],
            path: "Tests/TVOSNetPlayerCoreTests"
        ),
    ]
)
