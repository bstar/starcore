// swift-tools-version: 6.1
import PackageDescription
let package = Package(
    name: "StarCoreApple",
    platforms: [.macOS(.v13), .tvOS(.v17), .iOS(.v16)],
    products: [.library(name: "StarCore", targets: ["StarCore"]), .executable(name: "starcore-probe", targets: ["StarCoreProbe"])],
    dependencies: [
        .package(url: "https://github.com/apple/swift-nio-ssh.git", exact: "0.15.0"),
        .package(url: "https://github.com/apple/swift-nio-transport-services.git", exact: "1.28.0"),
        .package(url: "https://github.com/apple/swift-nio.git", from: "2.83.0"),
        .package(url: "https://github.com/apple/swift-crypto.git", from: "3.0.0")
    ],
    targets: [
        .target(name: "StarCore", dependencies: [.product(name:"NIOSSH", package:"swift-nio-ssh"), .product(name:"NIOTransportServices", package:"swift-nio-transport-services"), .product(name:"NIOCore", package:"swift-nio"), .product(name:"Crypto", package:"swift-crypto")]),
        .executableTarget(name:"StarCoreProbe", dependencies:["StarCore"]),
        .testTarget(name:"StarCoreTests", dependencies:["StarCore"])
    ], swiftLanguageModes: [.v5]
)
