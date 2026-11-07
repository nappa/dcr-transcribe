// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "DCRMonitor",
    platforms: [.macOS(.v14)],
    products: [.executable(name: "DCRMonitor", targets: ["DCRMonitor"])],
    targets: [
        .executableTarget(name: "DCRMonitor"),
        .testTarget(name: "DCRMonitorTests", dependencies: ["DCRMonitor"])
    ]
)
