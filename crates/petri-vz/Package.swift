// swift-tools-version: 5.9

import PackageDescription

let package = Package(
    name: "petri-vz",
    platforms: [
        .macOS(.v13)
    ],
    products: [
        .executable(name: "petri-vz", targets: ["petri-vz"])
    ],
    targets: [
        .executableTarget(name: "petri-vz")
    ]
)
