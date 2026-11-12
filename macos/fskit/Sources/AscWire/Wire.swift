// The FSKit wire protocol (docs/design/FSKIT.md). crates/asc-vfs/src/fskit/wire.rs is the other
// side and must agree with this file.
//
// Every message is one frame: a big-endian UInt32 length, then that many bytes. A request is
// `xid u32 ‖ op u32 ‖ arguments`; its reply is `xid u32 ‖ err u32 ‖ results`, with results only
// when `err` is 0. Fields are XDR: big-endian words, byte strings as a length and the bytes
// padded to four.

import Foundation

public enum Wire {
    /// The protocol version HELLO agrees on.
    public static let version: UInt32 = 1
    /// The largest READ or WRITE payload.
    public static let maxIO: UInt32 = 1 << 20
    /// The largest frame either side sends or accepts.
    public static let maxFrame = Int(maxIO) + 64 * 1024
    /// The longest name the daemon reads.
    public static let maxName = 1024
    /// The root directory's item number.
    public static let rootIno: UInt64 = 1
    /// The read-only `.git` gitfile's item number; the daemon does not count its lookups.
    public static let gitfileIno: UInt64 = (1 << 62) - 1
}

/// Operation numbers.
public enum Op: UInt32, Sendable {
    case hello = 0
    case getattr = 1
    case lookup = 2
    case forget = 3
    case setattr = 4
    case readlink = 5
    case read = 6
    case write = 7
    case create = 8
    case mkdir = 9
    case symlink = 10
    case remove = 11
    case rename = 12
    case readdir = 13
    case statfs = 14
    case sync = 15
    case close = 16
}

/// SETATTR mask bits.
public struct SetMask: OptionSet, Sendable {
    public let rawValue: UInt32
    public init(rawValue: UInt32) { self.rawValue = rawValue }
    public static let mode = SetMask(rawValue: 1)
    public static let uid = SetMask(rawValue: 2)
    public static let gid = SetMask(rawValue: 4)
    public static let size = SetMask(rawValue: 8)
    public static let atime = SetMask(rawValue: 0x10)
    public static let mtime = SetMask(rawValue: 0x20)
}

/// RENAME flag: refuse to replace an existing target.
public let renameNoReplace: UInt32 = 1

/// Item kinds.
public enum ItemKind: UInt32, Sendable {
    case file = 1
    case directory = 2
    case symlink = 3
}

/// A failed call: a Darwin errno from the daemon, or a broken connection or reply.
public enum WireError: Error, Equatable, Sendable {
    /// The daemon answered with this errno.
    case errno(Int32)
    /// The reply did not decode.
    case garbage
    /// The connection is closed.
    case disconnected
    /// The daemon speaks another protocol version.
    case version(UInt32)

    /// The errno FSKit should see.
    public var posix: Int32 {
        switch self {
        case .errno(let e): return e
        case .garbage: return EIO
        case .disconnected: return ENOTCONN
        case .version: return EPROTONOSUPPORT
        }
    }
}

/// A time on the wire: seconds since the epoch as a two's-complement UInt64, then nanoseconds.
public struct WireTime: Equatable, Sendable {
    public var seconds: Int64
    public var nanoseconds: UInt32
    public init(seconds: Int64, nanoseconds: UInt32) {
        self.seconds = seconds
        self.nanoseconds = nanoseconds
    }
    public static let zero = WireTime(seconds: 0, nanoseconds: 0)
    /// The time as FSKit's attributes hold it.
    public var ts: timespec {
        timespec(tv_sec: Int(seconds), tv_nsec: Int(nanoseconds))
    }
    public init(_ ts: timespec) {
        self.init(seconds: Int64(ts.tv_sec), nanoseconds: UInt32(clamping: ts.tv_nsec))
    }
}

/// An item's attributes as the daemon sends them.
public struct Attr: Equatable, Sendable {
    public var ino: UInt64
    public var parent: UInt64
    public var kind: ItemKind
    public var perm: UInt32
    public var nlink: UInt32
    public var uid: UInt32
    public var gid: UInt32
    public var size: UInt64
    public var alloc: UInt64
    public var atime: WireTime
    public var mtime: WireTime
    public var ctime: WireTime
}

/// One directory entry of a READDIR reply.
public struct DirEntry: Equatable, Sendable {
    public var ino: UInt64
    public var name: Data
    /// The cookie that resumes the listing after this entry.
    public var cookie: UInt64
    public var kind: ItemKind
    public var attr: Attr?
}

/// A page of a directory listing.
public struct DirPage: Equatable, Sendable {
    public var verifier: UInt64
    public var entries: [DirEntry]
    public var eof: Bool
}

/// The volume's space and limits.
public struct StatFS: Equatable, Sendable {
    public var bytes: UInt64
    public var free: UInt64
    public var available: UInt64
    public var files: UInt64
    public var freeFiles: UInt64
    public var blockSize: UInt32
    public var nameMax: UInt32
}

/// What HELLO answers.
public struct Hello: Equatable, Sendable {
    public var version: UInt32
    public var root: UInt64
    public var rootAttr: Attr
    public var maxIO: UInt32
}

/// Builds XDR. A class, so calls chain (`w.u64(ino).u32(mode)`): each returns the same writer.
/// One writer belongs to one call; it is not shared between threads.
public final class XDRWriter: @unchecked Sendable {
    public private(set) var bytes = Data()
    public init() {}

    @discardableResult
    public func u32(_ v: UInt32) -> XDRWriter {
        var be = v.bigEndian
        withUnsafeBytes(of: &be) { bytes.append(contentsOf: $0) }
        return self
    }

    @discardableResult
    public func u64(_ v: UInt64) -> XDRWriter {
        _ = u32(UInt32(truncatingIfNeeded: v >> 32))
        return u32(UInt32(truncatingIfNeeded: v))
    }

    @discardableResult
    public func bool(_ v: Bool) -> XDRWriter { u32(v ? 1 : 0) }

    @discardableResult
    public func opaque(_ d: Data) -> XDRWriter {
        _ = u32(UInt32(d.count))
        bytes.append(d)
        let pad = (4 - d.count % 4) % 4
        if pad > 0 { bytes.append(contentsOf: [UInt8](repeating: 0, count: pad)) }
        return self
    }

    @discardableResult
    public func time(_ t: WireTime) -> XDRWriter {
        _ = u64(UInt64(bitPattern: t.seconds))
        return u32(t.nanoseconds)
    }
}

/// Reads XDR; every read throws `WireError.garbage` past the end.
public struct XDRReader {
    private let data: Data
    private var at: Int

    public init(_ data: Data) {
        self.data = data
        self.at = data.startIndex
    }

    public var remaining: Int { data.endIndex - at }

    private mutating func take(_ n: Int) throws -> Data {
        guard n >= 0, remaining >= n else { throw WireError.garbage }
        let s = data[at..<(at + n)]
        at += n
        return s
    }

    public mutating func u32() throws -> UInt32 {
        let b = try take(4)
        return b.reduce(UInt32(0)) { $0 << 8 | UInt32($1) }
    }

    public mutating func u64() throws -> UInt64 {
        let hi = UInt64(try u32())
        let lo = UInt64(try u32())
        return hi << 32 | lo
    }

    public mutating func bool() throws -> Bool {
        switch try u32() {
        case 0: return false
        case 1: return true
        default: throw WireError.garbage
        }
    }

    public mutating func opaque(max: Int) throws -> Data {
        let n = Int(try u32())
        guard n <= max else { throw WireError.garbage }
        let d = try take(n)
        _ = try take((4 - n % 4) % 4)
        return Data(d)
    }

    public mutating func time() throws -> WireTime {
        let s = Int64(bitPattern: try u64())
        let n = min(try u32(), 999_999_999)
        return WireTime(seconds: s, nanoseconds: n)
    }

    public mutating func kind() throws -> ItemKind {
        guard let k = ItemKind(rawValue: try u32()) else { throw WireError.garbage }
        return k
    }

    public mutating func attr() throws -> Attr {
        Attr(
            ino: try u64(),
            parent: try u64(),
            kind: try kind(),
            perm: try u32(),
            nlink: try u32(),
            uid: try u32(),
            gid: try u32(),
            size: try u64(),
            alloc: try u64(),
            atime: try time(),
            mtime: try time(),
            ctime: try time()
        )
    }

    public mutating func dirPage() throws -> DirPage {
        let verifier = try u64()
        var entries: [DirEntry] = []
        while try bool() {
            let ino = try u64()
            let name = try opaque(max: Wire.maxName)
            let cookie = try u64()
            let kind = try kind()
            let attr = try bool() ? try self.attr() : nil
            entries.append(DirEntry(ino: ino, name: name, cookie: cookie, kind: kind, attr: attr))
        }
        return DirPage(verifier: verifier, entries: entries, eof: try bool())
    }

    public mutating func statfs() throws -> StatFS {
        StatFS(
            bytes: try u64(),
            free: try u64(),
            available: try u64(),
            files: try u64(),
            freeFiles: try u64(),
            blockSize: try u32(),
            nameMax: try u32()
        )
    }

    public mutating func hello() throws -> Hello {
        Hello(version: try u32(), root: try u64(), rootAttr: try attr(), maxIO: try u32())
    }
}

/// Frames: a big-endian UInt32 length, then the body.
public enum Frame {
    /// The bytes that put `body` on the wire.
    public static func encode(_ body: Data) throws -> Data {
        guard body.count <= Wire.maxFrame else { throw WireError.garbage }
        var out = Data(capacity: 4 + body.count)
        var be = UInt32(body.count).bigEndian
        withUnsafeBytes(of: &be) { out.append(contentsOf: $0) }
        out.append(body)
        return out
    }

    /// The length a frame header announces; throws when it is over the limit.
    public static func length(_ header: Data) throws -> Int {
        guard header.count == 4 else { throw WireError.garbage }
        let n = Int(header.reduce(UInt32(0)) { $0 << 8 | UInt32($1) })
        guard n <= Wire.maxFrame else { throw WireError.garbage }
        return n
    }
}
