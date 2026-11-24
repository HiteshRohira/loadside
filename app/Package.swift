// swift-tools-version:5.10
import PackageDescription

let package = Package(
    name: "Loadside",
    platforms: [.macOS(.v14)],
    targets: [
        .executableTarget(
            name: "Loadside",
            path: "Sources/Loadside"
        )
    ]
)
