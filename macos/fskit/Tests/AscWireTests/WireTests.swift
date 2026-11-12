import Foundation
import XCTest

@testable import AscWire

#if canImport(Darwin)
import Darwin
#elseif canImport(Glibc)
import Glibc
#endif

private func hex(_ s: String) -> Data {
    var out = Data()
    var it = s.makeIterator()
    while let a = it.next(), let b = it.next() {
        out.append(UInt8(String([a, b]), radix: 16)!)
    }
    return out
}

/// The bytes crates/asc-vfs/src/fskit/wire.rs encodes for the same attributes
/// (`the_attribute_encoding_matches_the_swift_client`).
private let goldenAttr =
    "1122334455667788000000000000000100000001000001a4"
    + "00000001000001f5000000140000000000000005000000000000100000000000000000011dcd6500"
    + "ffffffffffffffff2cb41780000000000000000000000000"

private let sampleAttr = Attr(
    ino: 0x1122_3344_5566_7788, parent: 1, kind: .file, perm: 0o644, nlink: 1, uid: 501,
    gid: 20, size: 5, alloc: 4096,
    atime: WireTime(seconds: 1, nanoseconds: 500_000_000),
    mtime: WireTime(seconds: -1, nanoseconds: 750_000_000),
    ctime: .zero
)

private func encode(_ a: Attr, into w: inout XDRWriter) {
    w.u64(a.ino).u64(a.parent).u32(a.kind.rawValue).u32(a.perm).u32(a.nlink).u32(a.uid)
        .u32(a.gid).u64(a.size).u64(a.alloc).time(a.atime).time(a.mtime).time(a.ctime)
}

final class WireTests: XCTestCase {
    func testTheAttributeEncodingMatchesTheDaemon() throws {
        var r = XDRReader(hex(goldenAttr))
        XCTAssertEqual(try r.attr(), sampleAttr)
        XCTAssertEqual(r.remaining, 0)
        var w = XDRWriter()
        encode(sampleAttr, into: &w)
        XCTAssertEqual(w.bytes, hex(goldenAttr))
    }

    func testOpaquesArePaddedAndBounded() throws {
        var w = XDRWriter()
        w.opaque(Data("abcde".utf8)).u32(7)
        XCTAssertEqual(w.bytes.count, 4 + 8 + 4)
        var r = XDRReader(w.bytes)
        XCTAssertEqual(try r.opaque(max: 5), Data("abcde".utf8))
        XCTAssertEqual(try r.u32(), 7)
        var short = XDRReader(w.bytes)
        XCTAssertThrowsError(try short.opaque(max: 4))
        var cut = XDRReader(w.bytes.prefix(6))
        XCTAssertThrowsError(try cut.opaque(max: 10))
        var badBool = XDRReader(XDRWriter().u32(2).bytes)
        XCTAssertThrowsError(try badBool.bool())
    }

    func testFramesRefuseOversizedLengths() throws {
        let f = try Frame.encode(Data("hi".utf8))
        XCTAssertEqual(f, Data([0, 0, 0, 2]) + Data("hi".utf8))
        XCTAssertEqual(try Frame.length(f.prefix(4)), 2)
        var big = UInt32(Wire.maxFrame + 1).bigEndian
        let header = withUnsafeBytes(of: &big) { Data($0) }
        XCTAssertThrowsError(try Frame.length(header))
        XCTAssertThrowsError(try Frame.encode(Data(count: Wire.maxFrame + 1)))
    }

    func testADirectoryPageDecodesEntriesAndEnd() throws {
        var w = XDRWriter()
        w.u64(9)
        w.bool(true).u64(5).opaque(Data("a.txt".utf8)).u64(3).u32(1).bool(false)
        w.bool(true).u64(6).opaque(Data("src".utf8)).u64(4).u32(2).bool(true)
        encode(sampleAttr, into: &w)
        w.bool(false).bool(true)
        var r = XDRReader(w.bytes)
        let page = try r.dirPage()
        XCTAssertEqual(page.verifier, 9)
        XCTAssertEqual(page.entries.map(\.ino), [5, 6])
        XCTAssertEqual(page.entries[1].kind, .directory)
        XCTAssertEqual(page.entries[1].attr, sampleAttr)
        XCTAssertNil(page.entries[0].attr)
        XCTAssertTrue(page.eof)
    }

    func testErrorsMapToPosix() {
        XCTAssertEqual(WireError.errno(ENOENT).posix, ENOENT)
        XCTAssertEqual(WireError.garbage.posix, EIO)
        XCTAssertEqual(WireError.disconnected.posix, ENOTCONN)
    }
}

/// A daemon stand-in on the other end of a socket pair: reads requests and answers them through
/// `answer`, holding the first reply back until the second is sent to check xid matching.
private final class FakeDaemon: @unchecked Sendable {
    let fd: Int32
    let answer: (UInt32, inout XDRReader, inout XDRWriter) -> UInt32
    var seen: [UInt32] = []
    let lock = NSLock()

    init(fd: Int32, answer: @escaping (UInt32, inout XDRReader, inout XDRWriter) -> UInt32) {
        self.fd = fd
        self.answer = answer
        Thread { self.run() }.start()
    }

    private func readExactly(_ n: Int) -> Data? {
        var out = Data(count: n)
        var got = 0
        while got < n {
            let r = out.withUnsafeMutableBytes { read(fd, $0.baseAddress! + got, n - got) }
            if r <= 0 { return nil }
            got += r
        }
        return out
    }

    private func send(_ body: Data) {
        let f = try! Frame.encode(body)
        _ = f.withUnsafeBytes { write(fd, $0.baseAddress!, f.count) }
    }

    private func run() {
        var held: Data?
        while let h = readExactly(4), let n = try? Frame.length(h), let body = readExactly(n) {
            var r = XDRReader(body)
            let xid = try! r.u32()
            let op = try! r.u32()
            lock.lock()
            seen.append(op)
            lock.unlock()
            var res = XDRWriter()
            let err = answer(op, &r, &res)
            var out = XDRWriter()
            out.u32(xid).u32(err)
            var reply = out.bytes
            if err == 0 { reply.append(res.bytes) }
            if op == Op.getattr.rawValue && held == nil {
                held = reply
                continue
            }
            send(reply)
            if let first = held {
                send(first)
                held = nil
            }
        }
        close(fd)
    }
}

final class ClientTests: XCTestCase {
    private func pair() -> (Int32, Int32) {
        var fds: [Int32] = [0, 0]
        #if canImport(Darwin)
        XCTAssertEqual(socketpair(AF_UNIX, SOCK_STREAM, 0, &fds), 0)
        #else
        XCTAssertEqual(socketpair(AF_UNIX, Int32(SOCK_STREAM.rawValue), 0, &fds), 0)
        #endif
        return (fds[0], fds[1])
    }

    func testCallsAreMatchedToRepliesByXidAndErrorsSurface() async throws {
        let (a, b) = pair()
        let daemon = FakeDaemon(fd: b) { op, r, w in
            switch Op(rawValue: op) {
            case .hello:
                XCTAssertEqual(try? r.u32(), Wire.version)
                w.u32(Wire.version).u64(Wire.rootIno)
                encode(sampleAttr, into: &w)
                w.u32(Wire.maxIO)
                return 0
            case .getattr:
                _ = try? r.u64()
                encode(sampleAttr, into: &w)
                return 0
            case .lookup:
                _ = try? r.u64()
                let name = (try? r.opaque(max: Wire.maxName)) ?? Data()
                if name == Data("missing".utf8) { return UInt32(ENOENT) }
                var found = sampleAttr
                found.ino = 42
                encode(found, into: &w)
                return 0
            default:
                return UInt32(ENOSYS)
            }
        }
        let c = Client(fd: a)
        let hello = try await c.hello()
        XCTAssertEqual(hello.root, Wire.rootIno)
        XCTAssertEqual(hello.maxIO, Wire.maxIO)

        // The getattr reply is held back until the lookup's is sent.
        async let first = c.getattr(7)
        try await Task.sleep(nanoseconds: 50_000_000)
        async let second = c.lookup(1, Data("x".utf8))
        let (g, l) = try await (first, second)
        XCTAssertEqual(g.ino, sampleAttr.ino)
        XCTAssertEqual(l.ino, 42)

        do {
            _ = try await c.lookup(1, Data("missing".utf8))
            XCTFail("a missing name must fail")
        } catch let e as WireError {
            XCTAssertEqual(e, .errno(ENOENT))
        }
        do {
            try await c.sync(1)
            XCTFail("an unknown op must fail")
        } catch let e as WireError {
            XCTAssertEqual(e.posix, ENOSYS)
        }
        let firstOp = daemon.lock.withLock { daemon.seen.first }
        XCTAssertEqual(firstOp, Op.hello.rawValue)

        c.shutdown()
        do {
            _ = try await c.getattr(1)
            XCTFail("a closed client must fail")
        } catch let e as WireError {
            XCTAssertEqual(e, .disconnected)
        }
    }

    func testADaemonOfAnotherVersionIsRefused() async throws {
        let (a, b) = pair()
        _ = FakeDaemon(fd: b) { _, _, _ in 75 }
        let c = Client(fd: a)
        do {
            _ = try await c.hello()
            XCTFail("a version mismatch must fail")
        } catch let e as WireError {
            XCTAssertEqual(e, .version(Wire.version))
        }
        c.shutdown()
    }

    func testCallsInFlightFailWhenTheDaemonGoesAway() async throws {
        let (a, b) = pair()
        let c = Client(fd: a)
        async let pending = c.getattr(1)
        try await Task.sleep(nanoseconds: 50_000_000)
        close(b)
        do {
            _ = try await pending
            XCTFail("the call must fail when the socket closes")
        } catch let e as WireError {
            XCTAssertEqual(e, .disconnected)
        }
        XCTAssertTrue(c.isClosed)
    }
}
