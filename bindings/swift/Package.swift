// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "OsaurusSync",
    platforms: [.macOS(.v14), .iOS(.v17)],
    products: [
        .library(name: "OsaurusSync", targets: ["OsaurusSync"]),
    ],
    targets: [
        // Built by scripts/build-xcframework.sh.
        .binaryTarget(name: "OsaurusSyncFFI", path: "OsaurusSyncFFI.xcframework"),
        .target(
            name: "OsaurusSync",
            dependencies: ["OsaurusSyncFFI"],
            path: "Sources/OsaurusSync",
            linkerSettings: [
                .linkedFramework("CoreFoundation"),
                .linkedFramework("Foundation"),
                .linkedFramework("Network"),
                .linkedFramework("Security"),
                .linkedFramework("SystemConfiguration"),
                .linkedLibrary("iconv"),
            ]
        ),
        .testTarget(
            name: "OsaurusSyncTests",
            dependencies: ["OsaurusSync"],
            path: "Tests/OsaurusSyncTests"
        ),
    ]
)
