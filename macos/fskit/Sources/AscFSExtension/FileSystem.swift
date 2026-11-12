// The file system: probes and loads `ascfs://<work tree id>` resources by connecting to the
// work tree's socket in the app group container, where the asc daemon serves it.

import AscWire
import Foundation
import FSKit

/// Where the daemon puts the sockets: `<group container>/s/<id>.sock`.
enum Sockets {
    /// The app group, written into the extension's Info.plist (`AscAppGroup`) when it is built.
    static var group: String? {
        Bundle.main.object(forInfoDictionaryKey: "AscAppGroup") as? String
    }

    /// The work tree id of a volume URL, as the daemon prints it: 16 lowercase hex digits.
    static func id(of url: URL) -> String? {
        guard url.scheme == "ascfs", let host = url.host, host.count == 16,
            host.allSatisfy({ $0.isHexDigit && !$0.isUppercase })
        else { return nil }
        return host
    }

    static func path(for id: String) -> String? {
        guard let group,
            let dir = FileManager.default.containerURL(
                forSecurityApplicationGroupIdentifier: group)
        else { return nil }
        return dir.appendingPathComponent("s").appendingPathComponent("\(id).sock").path
    }

    /// A stable UUID for a work tree id, so a volume keeps its identity across mounts.
    static func uuid(for id: String) -> UUID {
        let v = UInt64(id, radix: 16) ?? 0
        var b = [UInt8](repeating: 0, count: 16)
        for i in 0..<8 { b[i] = UInt8(truncatingIfNeeded: v >> (56 - 8 * UInt64(i))) }
        // "asc fskit" in the low half; the version and variant bits make it a valid UUID.
        let tag: [UInt8] = [0x61, 0x73, 0x63, 0x20, 0x66, 0x73, 0x6b, 0x74]
        for i in 0..<8 { b[8 + i] = tag[i] }
        b[6] = (b[6] & 0x0f) | 0x80
        b[8] = (b[8] & 0x3f) | 0x80
        return UUID(uuid: (b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
                           b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]))
    }
}

func posix(_ e: Error) -> Error {
    if let w = e as? WireError {
        return POSIXError(POSIXErrorCode(rawValue: w.posix) ?? .EIO)
    }
    return e
}

func posix(_ code: Int32) -> Error {
    POSIXError(POSIXErrorCode(rawValue: code) ?? .EIO)
}

final class AscFileSystem: FSUnaryFileSystem, FSUnaryFileSystemOperations {
    static let shared = AscFileSystem()

    private let lock = NSLock()
    private var volumes: [String: AscVolume] = [:]

    private func workTree(_ resource: FSResource) -> String? {
        guard let r = resource as? FSGenericURLResource else { return nil }
        return Sockets.id(of: r.url)
    }

    func probeResource(
        resource: FSResource, replyHandler: @escaping (FSProbeResult?, (any Error)?) -> Void
    ) {
        guard let id = workTree(resource) else {
            replyHandler(.notRecognized, nil)
            return
        }
        guard let path = Sockets.path(for: id), FileManager.default.fileExists(atPath: path) else {
            replyHandler(.notRecognized, nil)
            return
        }
        replyHandler(
            .usable(name: "asc", containerID: FSContainerIdentifier(uuid: Sockets.uuid(for: id))),
            nil)
    }

    func loadResource(
        resource: FSResource, options: FSTaskOptions,
        replyHandler: @escaping (FSVolume?, (any Error)?) -> Void
    ) {
        guard let id = workTree(resource) else {
            replyHandler(nil, posix(EINVAL))
            return
        }
        guard let path = Sockets.path(for: id) else {
            replyHandler(nil, posix(ENOENT))
            return
        }
        Task {
            do {
                let client = try Client.connect(path: path)
                let hello: Hello
                do {
                    hello = try await client.hello()
                } catch {
                    client.shutdown()
                    throw error
                }
                let volume = AscVolume(id: id, client: client, hello: hello)
                self.lock.withLock { self.volumes[id] = volume }
                self.containerStatus = .ready
                replyHandler(volume, nil)
            } catch {
                replyHandler(nil, posix(error))
            }
        }
    }

    func unloadResource(
        resource: FSResource, options: FSTaskOptions,
        replyHandler: @escaping ((any Error)?) -> Void
    ) {
        if let id = workTree(resource) {
            lock.lock()
            let v = volumes.removeValue(forKey: id)
            lock.unlock()
            v?.close()
        }
        replyHandler(nil)
    }
}
