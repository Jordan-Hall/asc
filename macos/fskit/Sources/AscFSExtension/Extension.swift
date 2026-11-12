// The FSKit extension's entry point (extension point `com.apple.fskit.fsmodule`).

import ExtensionFoundation
import FSKit

@main
struct AscFSExtension: UnaryFileSystemExtension {
    var fileSystem: FSUnaryFileSystem & FSUnaryFileSystemOperations {
        AscFileSystem.shared
    }
}
