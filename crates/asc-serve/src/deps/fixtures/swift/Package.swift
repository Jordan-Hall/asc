// swift-tools-version:5.5
import PackageDescription

let package = Package(
    name: "App",
    dependencies: [
        .package(url: "https://github.com/apple/swift-nio.git", from: "2.40.0"),
        .package(name: "Alamofire", url: "https://github.com/Alamofire/Alamofire.git", .upToNextMajor(from: "5.0.0")),
        .package(path: "../local"),
    ],
    targets: [.target(name: "App")]
)
