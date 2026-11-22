// swift-tools-version: 6.0

import PackageDescription

let package = Package(
    name: "TachyonCore",
    platforms: [
        .iOS(.v17),
        .macOS(.v14),
    ],
    products: [
        .library(
            name: "TachyonCore",
            targets: ["TachyonCore"]
        ),
    ],
    targets: [
        .target(
            name: "TachyonCore"
        ),
        .testTarget(
            name: "TachyonCoreTests",
            dependencies: ["TachyonCore"]
        ),
    ]
)
