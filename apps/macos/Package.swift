// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "PolarisDesktop",
    platforms: [.macOS(.v13)],
    products: [
        .library(name: "PolarisSettings", targets: ["PolarisSettings"]),
        .executable(name: "PolarisDesktop", targets: ["PolarisDesktop"])
    ],
    targets: [
        .target(name: "PolarisSettings"),
        .executableTarget(name: "PolarisDesktop", dependencies: ["PolarisSettings"],
                          resources: [.process("Resources")]),
        .testTarget(name: "PolarisSettingsTests", dependencies: ["PolarisSettings", "PolarisDesktop"]),
        .testTarget(name: "PolarisDesktopTests", dependencies: ["PolarisSettings", "PolarisDesktop"])
    ]
)
