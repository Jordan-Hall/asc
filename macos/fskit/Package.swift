// swift-tools-version:6.0
// The asc FSKit module (SPEC §10.3b, docs/design/FSKIT.md): a user-space file system extension
// that mounts a virtual work tree served by the asc daemon over a Unix-domain socket.
//
// * AscWire: the wire protocol and the socket client. Plain Foundation; `swift test` covers it.
// * AscFSExtension: the FSKit extension (`com.apple.fskit.fsmodule`), packed as
//   AscFS.app/Contents/Extensions/AscFSExtension.appex by scripts/build-app.sh.
// * AscFS: the host app that carries the extension and opens its System Settings pane.
import PackageDescription

let package = Package(
    name: "AscFS",
    platforms: [.macOS("26.0")],
    products: [
        .library(name: "AscWire", targets: ["AscWire"]),
        .executable(name: "AscFSExtension", targets: ["AscFSExtension"]),
        .executable(name: "AscFS", targets: ["AscFS"]),
    ],
    targets: [
        .target(name: "AscWire"),
        .executableTarget(
            name: "AscFSExtension",
            dependencies: ["AscWire"],
            // FSKit's reply handlers and items are not Sendable; the extension hands them to tasks.
            swiftSettings: [.swiftLanguageMode(.v5)],
            linkerSettings: [.linkedFramework("FSKit")]
        ),
        .executableTarget(name: "AscFS"),
        .testTarget(name: "AscWireTests", dependencies: ["AscWire"]),
    ]
)
