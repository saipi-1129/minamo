// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "KickPlayerMacOS",
    platforms: [.macOS(.v13)],
    products: [
        .library(name: "KickPlayerCore", targets: ["KickPlayerCore"]),
        .library(name: "KickPlayerCBridge", type: .dynamic, targets: ["KickPlayerCBridge"]),
        .executable(name: "kick-player-demo", targets: ["KickPlayerDemo"]),
    ],
    targets: [
        .target(name: "KickPlayerCore"),
        .target(
            name: "KickPlayerCBridge",
            dependencies: ["KickPlayerCore"],
            linkerSettings: [.linkedFramework("Security")]
        ),
        .executableTarget(name: "KickPlayerDemo", dependencies: ["KickPlayerCore"]),
        .testTarget(name: "KickPlayerCoreTests", dependencies: ["KickPlayerCore"]),
        .testTarget(name: "KickPlayerCBridgeTests", dependencies: ["KickPlayerCBridge"]),
    ],
    swiftLanguageModes: [.v5]
)
