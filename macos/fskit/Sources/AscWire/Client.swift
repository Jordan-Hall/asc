// The socket client the extension calls the daemon with. One connection carries every call; a
// reader thread matches replies to calls by xid, so calls from many FSKit tasks overlap.

import Foundation

#if canImport(Darwin)
import Darwin
#elseif canImport(Glibc)
import Glibc
#endif

/// A connection to the daemon's work tree socket.
public final class Client: @unchecked Sendable {
    private let fd: Int32
    private let lock = NSLock()
    private let sendLock = NSLock()
    private var nextXid: UInt32 = 1
    private var pending: [UInt32: CheckedContinuation<Data, Error>] = [:]
    private var closed = false
    /// Set, under `sendLock`, once the reader closed the descriptor.
    private var fdClosed = false
    private var reader: Thread?

    /// Takes over a connected stream socket.
    public init(fd: Int32) {
        self.fd = fd
        #if canImport(Darwin)
        var one: Int32 = 1
        _ = setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
        #endif
        let t = Thread { [weak self] in self?.readLoop() }
        t.name = "asc-fskit-reader"
        reader = t
        t.start()
    }

    /// Connects to the socket at `path`.
    public static func connect(path: String) throws -> Client {
        #if canImport(Darwin)
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        #else
        let fd = socket(AF_UNIX, Int32(SOCK_STREAM.rawValue), 0)
        #endif
        guard fd >= 0 else { throw WireError.errno(errno) }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8)
        let cap = MemoryLayout.size(ofValue: addr.sun_path)
        guard bytes.count < cap else {
            sysClose(fd)
            throw WireError.errno(ENAMETOOLONG)
        }
        withUnsafeMutableBytes(of: &addr.sun_path) { raw in
            raw.copyBytes(from: bytes)
            raw[bytes.count] = 0
        }
        let rc = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                sysConnect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard rc == 0 else {
            let e = errno
            sysClose(fd)
            throw WireError.errno(e)
        }
        return Client(fd: fd)
    }

    deinit { shutdown() }

    /// Closes the connection; calls in flight fail with `.disconnected`. The daemon then releases
    /// every lookup this connection held.
    public func shutdown() {
        lock.lock()
        let was = closed
        closed = true
        let waiting = pending
        pending.removeAll()
        lock.unlock()
        if !was {
            _ = sysShutdown(fd)
        }
        for (_, c) in waiting { c.resume(throwing: WireError.disconnected) }
    }

    public var isClosed: Bool {
        lock.lock()
        defer { lock.unlock() }
        return closed
    }

    // MARK: transport

    private func readExactly(_ n: Int) -> Data? {
        var out = Data(count: n)
        var got = 0
        while got < n {
            let r = out.withUnsafeMutableBytes { raw -> Int in
                sysRead(fd, raw.baseAddress! + got, n - got)
            }
            if r < 0 && errno == EINTR { continue }
            if r <= 0 { return nil }
            got += r
        }
        return out
    }

    private func readLoop() {
        while true {
            guard let header = readExactly(4),
                let n = try? Frame.length(header),
                let body = n == 0 ? Data() : readExactly(n)
            else { break }
            guard body.count >= 4 else { continue }
            var r = XDRReader(body)
            guard let xid = try? r.u32() else { continue }
            lock.lock()
            let c = pending.removeValue(forKey: xid)
            lock.unlock()
            c?.resume(returning: body.dropFirst(4))
        }
        shutdown()
        sendLock.lock()
        fdClosed = true
        sysClose(fd)
        sendLock.unlock()
    }

    private func writeAll(_ d: Data) -> Bool {
        sendLock.lock()
        defer { sendLock.unlock() }
        if fdClosed { return false }
        var sent = 0
        while sent < d.count {
            let r = d.withUnsafeBytes { raw -> Int in
                sysWrite(fd, raw.baseAddress! + sent, d.count - sent)
            }
            if r < 0 && errno == EINTR { continue }
            if r <= 0 { return false }
            sent += r
        }
        return true
    }

    /// Sends `op` with `args` and answers the result bytes, throwing the daemon's errno.
    public func call(_ op: Op, _ args: (inout XDRWriter) -> Void = { _ in }) async throws -> XDRReader {
        let w = XDRWriter()
        let xid: UInt32 = try lock.withLock {
            guard !closed else { throw WireError.disconnected }
            let x = nextXid
            nextXid = nextXid == UInt32.max ? 1 : nextXid + 1
            return x
        }
        w.u32(xid).u32(op.rawValue)
        var arg = w
        args(&arg)
        let frame = try Frame.encode(w.bytes)
        let reply: Data = try await withCheckedThrowingContinuation { c in
            let added: Bool = lock.withLock {
                if closed { return false }
                pending[xid] = c
                return true
            }
            guard added else {
                c.resume(throwing: WireError.disconnected)
                return
            }
            if !writeAll(frame) {
                let mine = lock.withLock { pending.removeValue(forKey: xid) }
                mine?.resume(throwing: WireError.disconnected)
            }
        }
        var r = XDRReader(reply)
        let err = try r.u32()
        if err != 0 { throw WireError.errno(Int32(bitPattern: err)) }
        return r
    }

    // MARK: operations

    public func hello() async throws -> Hello {
        do {
            var r = try await call(.hello) { $0.u32(Wire.version) }
            let h = try r.hello()
            guard h.version == Wire.version else { throw WireError.version(h.version) }
            return h
        } catch WireError.errno(75) {
            throw WireError.version(Wire.version)
        }
    }

    public func getattr(_ ino: UInt64) async throws -> Attr {
        var r = try await call(.getattr) { $0.u64(ino) }
        return try r.attr()
    }

    /// Looks `name` up in `dir`; the daemon counts the lookup until `forget`.
    public func lookup(_ dir: UInt64, _ name: Data) async throws -> Attr {
        var r = try await call(.lookup) { $0.u64(dir).opaque(name) }
        return try r.attr()
    }

    /// Releases `n` counted lookups of `ino`.
    public func forget(_ ino: UInt64, _ n: UInt64) async throws {
        _ = try await call(.forget) { $0.u64(ino).u64(n) }
    }

    public func setattr(
        _ ino: UInt64, mask: SetMask, mode: UInt32 = 0, uid: UInt32 = 0, gid: UInt32 = 0,
        size: UInt64 = 0, atime: WireTime = .zero, mtime: WireTime = .zero
    ) async throws -> Attr {
        var r = try await call(.setattr) {
            $0.u64(ino).u32(mask.rawValue).u32(mode).u32(uid).u32(gid).u64(size)
                .time(atime).time(mtime)
        }
        return try r.attr()
    }

    public func readlink(_ ino: UInt64) async throws -> Data {
        var r = try await call(.readlink) { $0.u64(ino) }
        return try r.opaque(max: 4096)
    }

    public func read(_ ino: UInt64, offset: UInt64, count: UInt32) async throws -> (eof: Bool, data: Data) {
        var r = try await call(.read) { $0.u64(ino).u64(offset).u32(min(count, Wire.maxIO)) }
        let eof = try r.bool()
        return (eof, try r.opaque(max: Int(Wire.maxIO)))
    }

    public func write(_ ino: UInt64, offset: UInt64, data: Data) async throws -> (count: UInt32, attr: Attr) {
        var r = try await call(.write) { $0.u64(ino).u64(offset).opaque(data) }
        return (try r.u32(), try r.attr())
    }

    public func create(_ dir: UInt64, _ name: Data, mode: UInt32) async throws -> Attr {
        var r = try await call(.create) { $0.u64(dir).opaque(name).u32(mode) }
        return try r.attr()
    }

    public func mkdir(_ dir: UInt64, _ name: Data, mode: UInt32) async throws -> Attr {
        var r = try await call(.mkdir) { $0.u64(dir).opaque(name).u32(mode) }
        return try r.attr()
    }

    public func symlink(_ dir: UInt64, _ name: Data, target: Data) async throws -> Attr {
        var r = try await call(.symlink) { $0.u64(dir).opaque(name).opaque(target) }
        return try r.attr()
    }

    public func remove(_ dir: UInt64, _ name: Data, isDirectory: Bool) async throws {
        _ = try await call(.remove) { $0.u64(dir).opaque(name).bool(isDirectory) }
    }

    public func rename(
        _ from: UInt64, _ fromName: Data, to: UInt64, _ toName: Data, noReplace: Bool
    ) async throws {
        _ = try await call(.rename) {
            $0.u64(from).opaque(fromName).u64(to).opaque(toName).u32(noReplace ? renameNoReplace : 0)
        }
    }

    public func readdir(
        _ dir: UInt64, cookie: UInt64, verifier: UInt64, max: UInt32, attrs: Bool
    ) async throws -> DirPage {
        var r = try await call(.readdir) {
            $0.u64(dir).u64(cookie).u64(verifier).u32(max).bool(attrs)
        }
        return try r.dirPage()
    }

    public func statfs() async throws -> StatFS {
        var r = try await call(.statfs)
        return try r.statfs()
    }

    public func sync(_ ino: UInt64) async throws {
        _ = try await call(.sync) { $0.u64(ino) }
    }

    public func close(_ ino: UInt64) async throws {
        _ = try await call(.close) { $0.u64(ino) }
    }
}

// The libc calls, named apart from the methods above.
#if canImport(Darwin)
private func sysConnect(_ fd: Int32, _ a: UnsafePointer<sockaddr>, _ l: socklen_t) -> Int32 {
    Darwin.connect(fd, a, l)
}
private func sysShutdown(_ fd: Int32) -> Int32 { Darwin.shutdown(fd, SHUT_RDWR) }
private func sysRead(_ fd: Int32, _ p: UnsafeMutableRawPointer, _ n: Int) -> Int { Darwin.read(fd, p, n) }
private func sysWrite(_ fd: Int32, _ p: UnsafeRawPointer, _ n: Int) -> Int { Darwin.write(fd, p, n) }
private func sysClose(_ fd: Int32) { _ = Darwin.close(fd) }
#else
private func sysConnect(_ fd: Int32, _ a: UnsafePointer<sockaddr>, _ l: socklen_t) -> Int32 {
    Glibc.connect(fd, a, l)
}
private func sysShutdown(_ fd: Int32) -> Int32 { Glibc.shutdown(fd, Int32(SHUT_RDWR)) }
private func sysRead(_ fd: Int32, _ p: UnsafeMutableRawPointer, _ n: Int) -> Int { Glibc.read(fd, p, n) }
private func sysWrite(_ fd: Int32, _ p: UnsafeRawPointer, _ n: Int) -> Int { Glibc.write(fd, p, n) }
private func sysClose(_ fd: Int32) { _ = Glibc.close(fd) }
#endif
