// AscFS.app: carries the asc FSKit extension. Opening it creates the app group container the
// daemon puts its sockets in, and opens the System Settings pane where the extension is enabled.

import AppKit
import Foundation

let group = Bundle.main.object(forInfoDictionaryKey: "AscAppGroup") as? String
var lines: [String] = []
if let group,
    let dir = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: group)
{
    lines.append("asc FSKit module: app group \(group) at \(dir.path)")
} else {
    lines.append("asc FSKit module: this copy of AscFS.app has no app group; rebuild it with build-app.sh --team")
}
lines.append(
    "Enable \"asc\" under System Settings > General > Login Items & Extensions > File System Extensions,"
)
lines.append("then mount virtual work trees as usual: asc picks the fskit backend on its own.")
print(lines.joined(separator: "\n"))

if let url = URL(string: "x-apple.systempreferences:com.apple.LoginItems-Settings.extension") {
    NSWorkspace.shared.open(url)
}
