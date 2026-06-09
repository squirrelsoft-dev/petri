// swift-tools-version: 5.9

import PackageDescription

let package = Package(
    name: "petri-vz",
    platforms: [
        // macOS 14 is required by VZNetworkBlockDeviceStorageDeviceAttachment.
        .macOS(.v14)
    ],
    products: [
        .executable(name: "petri-vz", targets: ["petri-vz"])
    ],
    targets: [
        .executableTarget(name: "petri-vz")
    ]
)
