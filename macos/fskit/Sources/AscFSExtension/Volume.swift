// The volume: maps FSKit's volume operations onto the daemon's wire calls.
//
// Item numbers: FSKit reserves identifier 1 (the root's parent) and 2 (the root); the daemon's
// root is 1 and its other items start at 2, so an item's FSKit identifier is its number plus one.
//
// Lookup counts: the daemon counts every LOOKUP, CREATE, MKDIR and SYMLINK it answers until the
// connection releases it with FORGET. Each `AscItem` remembers how many it holds and gives them
// all back when FSKit reclaims it; closing the connection releases the rest.

import AscWire
import Foundation
import FSKit

/// An item the kernel holds: the daemon's number, its last attributes, and the lookups and opens
/// this side owes the daemon.
final class AscItem: FSItem {
    let ino: UInt64
    var attr: Attr
    var lookups: UInt64 = 0
    var opens: Int = 0

    init(attr: Attr) {
        self.ino = attr.ino
        self.attr = attr
        super.init()
    }
}

func identifier(_ ino: UInt64) -> FSItem.Identifier {
    FSItem.Identifier(rawValue: ino &+ 1)
}

func itemType(_ k: ItemKind) -> FSItem.ItemType {
    switch k {
    case .file: return .file
    case .directory: return .directory
    case .symlink: return .symlink
    }
}

final class AscVolume: FSVolume, FSVolume.Operations, FSVolume.ReadWriteOperations,
    FSVolume.OpenCloseOperations
{
    let id: String
    let client: Client
    let root: AscItem
    let maxIO: UInt32
    private let lock = NSLock()
    private var items: [UInt64: AscItem] = [:]
    private var stats: StatFS?

    init(id: String, client: Client, hello: Hello) {
        self.id = id
        self.client = client
        self.root = AscItem(attr: hello.rootAttr)
        self.maxIO = max(4096, min(hello.maxIO, Wire.maxIO))
        super.init(
            volumeID: FSVolume.Identifier(uuid: Sockets.uuid(for: id)),
            volumeName: FSFileName(string: "asc"))
        items[root.ino] = root
        refreshStats()
    }

    func close() {
        client.shutdown()
    }

    private func refreshStats() {
        Task {
            if let s = try? await client.statfs() {
                lock.withLock { stats = s }
            }
        }
    }

    // MARK: items and attributes

    /// The item for `attr`, counting one daemon lookup when `counted`.
    private func item(_ attr: Attr, counted: Bool) -> AscItem {
        lock.lock()
        defer { lock.unlock() }
        let it: AscItem
        if let have = items[attr.ino] {
            have.attr = attr
            it = have
        } else {
            it = AscItem(attr: attr)
            items[attr.ino] = it
        }
        if counted && attr.ino != Wire.gitfileIno && attr.ino != Wire.rootIno {
            it.lookups += 1
        }
        return it
    }

    private func update(_ attr: Attr) {
        lock.lock()
        items[attr.ino]?.attr = attr
        lock.unlock()
    }

    private func attributes(_ a: Attr) -> FSItem.Attributes {
        let out = FSItem.Attributes()
        out.type = itemType(a.kind)
        let typeBits: UInt32
        switch a.kind {
        case .file: typeBits = UInt32(S_IFREG)
        case .directory: typeBits = UInt32(S_IFDIR)
        case .symlink: typeBits = UInt32(S_IFLNK)
        }
        out.mode = typeBits | (a.perm & 0o7777)
        out.linkCount = a.nlink
        out.uid = a.uid
        out.gid = a.gid
        out.size = a.size
        out.allocSize = a.alloc
        out.fileID = identifier(a.ino)
        out.parentID = a.ino == Wire.rootIno ? .parentOfRoot : identifier(a.parent)
        out.accessTime = a.atime.ts
        out.modifyTime = a.mtime.ts
        out.changeTime = a.ctime.ts
        out.birthTime = a.ctime.ts
        out.flags = 0
        return out
    }

    private func ascItem(_ item: FSItem) throws -> AscItem {
        guard let it = item as? AscItem else { throw posix(EINVAL) }
        return it
    }

    /// Runs `body` on a task and hands its result or errno to `reply`.
    private func run<T>(
        _ reply: @escaping (T?, (any Error)?) -> Void, _ body: @escaping () async throws -> T
    ) {
        Task {
            do {
                reply(try await body(), nil)
            } catch {
                reply(nil, posix(error))
            }
        }
    }

    private func runVoid(
        _ reply: @escaping ((any Error)?) -> Void, _ body: @escaping () async throws -> Void
    ) {
        Task {
            do {
                try await body()
                reply(nil)
            } catch {
                reply(posix(error))
            }
        }
    }

    // MARK: FSVolume.PathConfOperations

    var maximumLinkCount: Int { 1 }
    var maximumNameLength: Int {
        lock.lock()
        defer { lock.unlock() }
        return Int(stats?.nameMax ?? 255)
    }
    var restrictsOwnershipChanges: Bool { true }
    var truncatesLongNames: Bool { false }

    // MARK: FSVolume.Operations

    var supportedVolumeCapabilities: FSVolume.SupportedCapabilities {
        let c = FSVolume.SupportedCapabilities()
        c.supportsSymbolicLinks = true
        c.supportsHardLinks = false
        c.supportsPersistentObjectIDs = true
        c.supports64BitObjectIDs = true
        c.supportsSparseFiles = true
        c.supports2TBFiles = true
        c.supportsHiddenFiles = true
        c.caseFormat = .sensitive
        return c
    }

    var volumeStatistics: FSStatFSResult {
        lock.lock()
        let s = stats
        lock.unlock()
        refreshStats()
        let r = FSStatFSResult(fileSystemTypeName: "ascfs")
        let bsize = UInt64(max(s?.blockSize ?? 4096, 512))
        r.blockSize = Int(bsize)
        r.ioSize = Int(maxIO)
        r.totalBlocks = (s?.bytes ?? 0) / bsize
        r.freeBlocks = (s?.free ?? 0) / bsize
        r.availableBlocks = (s?.available ?? 0) / bsize
        r.usedBlocks = r.totalBlocks >= r.freeBlocks ? r.totalBlocks - r.freeBlocks : 0
        r.totalBytes = s?.bytes ?? 0
        r.freeBytes = s?.free ?? 0
        r.availableBytes = s?.available ?? 0
        r.usedBytes = r.totalBytes >= r.freeBytes ? r.totalBytes - r.freeBytes : 0
        r.totalFiles = s?.files ?? 0
        r.freeFiles = s?.freeFiles ?? 0
        return r
    }

    func mount(options: FSTaskOptions, replyHandler: @escaping ((any Error)?) -> Void) {
        replyHandler(nil)
    }

    func unmount(replyHandler: @escaping () -> Void) {
        replyHandler()
    }

    func synchronize(flags: FSSyncFlags, replyHandler: @escaping ((any Error)?) -> Void) {
        lock.lock()
        let open = items.values.filter { $0.opens > 0 }.map(\.ino)
        lock.unlock()
        runVoid(replyHandler) {
            for ino in open { try await self.client.sync(ino) }
        }
    }

    func activate(options: FSTaskOptions, replyHandler: @escaping (FSItem?, (any Error)?) -> Void) {
        replyHandler(root, nil)
    }

    func deactivate(
        options: FSDeactivateOptions = [], replyHandler: @escaping ((any Error)?) -> Void
    ) {
        replyHandler(nil)
    }

    func getAttributes(
        _ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem,
        replyHandler: @escaping (FSItem.Attributes?, (any Error)?) -> Void
    ) {
        run(replyHandler) {
            let it = try self.ascItem(item)
            let a = try await self.client.getattr(it.ino)
            self.update(a)
            return self.attributes(a)
        }
    }

    func setAttributes(
        _ newAttributes: FSItem.SetAttributesRequest, on item: FSItem,
        replyHandler: @escaping (FSItem.Attributes?, (any Error)?) -> Void
    ) {
        run(replyHandler) {
            let it = try self.ascItem(item)
            var mask = SetMask()
            var consumed: FSItem.Attribute = []
            if newAttributes.isValid(.mode) {
                mask.insert(.mode)
                consumed.insert(.mode)
            }
            if newAttributes.isValid(.uid) {
                mask.insert(.uid)
                consumed.insert(.uid)
            }
            if newAttributes.isValid(.gid) {
                mask.insert(.gid)
                consumed.insert(.gid)
            }
            if newAttributes.isValid(.size) {
                mask.insert(.size)
                consumed.insert(.size)
            }
            if newAttributes.isValid(.accessTime) {
                mask.insert(.atime)
                consumed.insert(.accessTime)
            }
            if newAttributes.isValid(.modifyTime) {
                mask.insert(.mtime)
                consumed.insert(.modifyTime)
            }
            let a = try await self.client.setattr(
                it.ino, mask: mask, mode: newAttributes.mode & 0o7777, uid: newAttributes.uid,
                gid: newAttributes.gid, size: newAttributes.size,
                atime: WireTime(newAttributes.accessTime),
                mtime: WireTime(newAttributes.modifyTime))
            newAttributes.consumedAttributes = consumed
            self.update(a)
            return self.attributes(a)
        }
    }

    func lookupItem(
        named name: FSFileName, inDirectory directory: FSItem,
        replyHandler: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void
    ) {
        Task {
            do {
                let dir = try self.ascItem(directory)
                let a = try await self.client.lookup(dir.ino, wireName(name))
                replyHandler(self.item(a, counted: true), name, nil)
            } catch {
                replyHandler(nil, nil, posix(error))
            }
        }
    }

    func reclaimItem(_ item: FSItem, replyHandler: @escaping ((any Error)?) -> Void) {
        guard let it = item as? AscItem, it.ino != Wire.rootIno else {
            replyHandler(nil)
            return
        }
        lock.lock()
        if items[it.ino] === it { items.removeValue(forKey: it.ino) }
        let n = it.lookups
        it.lookups = 0
        let wasOpen = it.opens > 0
        it.opens = 0
        lock.unlock()
        Task {
            if wasOpen { try? await self.client.close(it.ino) }
            if n > 0 { try? await self.client.forget(it.ino, n) }
            replyHandler(nil)
        }
    }

    func readSymbolicLink(
        _ item: FSItem, replyHandler: @escaping (FSFileName?, (any Error)?) -> Void
    ) {
        run(replyHandler) {
            let it = try self.ascItem(item)
            return FSFileName(data: try await self.client.readlink(it.ino))
        }
    }

    func createItem(
        named name: FSFileName, type: FSItem.ItemType, inDirectory directory: FSItem,
        attributes newAttributes: FSItem.SetAttributesRequest,
        replyHandler: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void
    ) {
        Task {
            do {
                let dir = try self.ascItem(directory)
                let given = newAttributes.isValid(.mode) ? newAttributes.mode & 0o7777 : nil
                let a: Attr
                switch type {
                case .file:
                    a = try await self.client.create(
                        dir.ino, wireName(name), mode: given ?? 0o644)
                case .directory:
                    a = try await self.client.mkdir(
                        dir.ino, wireName(name), mode: given ?? 0o755)
                default:
                    throw posix(ENOTSUP)
                }
                if given != nil { newAttributes.consumedAttributes.insert(.mode) }
                replyHandler(self.item(a, counted: true), name, nil)
            } catch {
                replyHandler(nil, nil, posix(error))
            }
        }
    }

    func createSymbolicLink(
        named name: FSFileName, inDirectory directory: FSItem,
        attributes newAttributes: FSItem.SetAttributesRequest, linkContents contents: FSFileName,
        replyHandler: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void
    ) {
        Task {
            do {
                let dir = try self.ascItem(directory)
                let a = try await self.client.symlink(
                    dir.ino, wireName(name), target: contents.data)
                replyHandler(self.item(a, counted: true), name, nil)
            } catch {
                replyHandler(nil, nil, posix(error))
            }
        }
    }

    func createLink(
        to item: FSItem, named name: FSFileName, inDirectory directory: FSItem,
        replyHandler: @escaping (FSFileName?, (any Error)?) -> Void
    ) {
        // A work tree holds no hard links (SPEC §10.3).
        replyHandler(nil, posix(ENOTSUP))
    }

    func removeItem(
        _ item: FSItem, named name: FSFileName, fromDirectory directory: FSItem,
        replyHandler: @escaping ((any Error)?) -> Void
    ) {
        runVoid(replyHandler) {
            let it = try self.ascItem(item)
            let dir = try self.ascItem(directory)
            try await self.client.remove(
                dir.ino, wireName(name), isDirectory: it.attr.kind == .directory)
        }
    }

    func renameItem(
        _ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName,
        to destinationName: FSFileName, inDirectory destinationDirectory: FSItem,
        overItem: FSItem?, replyHandler: @escaping (FSFileName?, (any Error)?) -> Void
    ) {
        run(replyHandler) {
            let from = try self.ascItem(sourceDirectory)
            let to = try self.ascItem(destinationDirectory)
            try await self.client.rename(
                from.ino, wireName(sourceName), to: to.ino,
                wireName(destinationName), noReplace: false)
            return destinationName
        }
    }

    func enumerateDirectory(
        _ directory: FSItem, startingAt cookie: FSDirectoryCookie,
        verifier: FSDirectoryVerifier, attributes: FSItem.GetAttributesRequest?,
        packer: FSDirectoryEntryPacker,
        replyHandler: @escaping (FSDirectoryVerifier, (any Error)?) -> Void
    ) {
        Task {
            do {
                let dir = try self.ascItem(directory)
                let v = try await self.list(
                    dir, from: cookie.rawValue, verifier: verifier.rawValue,
                    attributes: attributes, packer: packer)
                replyHandler(FSDirectoryVerifier(rawValue: v), nil)
            } catch WireError.errno(ESTALE) {
                replyHandler(verifier, FSError(.invalidDirectoryCookie))
            } catch {
                replyHandler(verifier, posix(error))
            }
        }
    }

    /// Lists `dir` into `packer` from the FSKit cookie `from`, answering the verifier.
    ///
    /// FSKit cookies: 0 starts; a plain listing (no attributes) gets "." with next cookie 1 and
    /// ".." with next cookie 2 first; a daemon cookie `c` (the position after an entry) is `c + 2`.
    private func list(
        _ dir: AscItem, from: UInt64, verifier: UInt64, attributes: FSItem.GetAttributesRequest?,
        packer: FSDirectoryEntryPacker
    ) async throws -> UInt64 {
        let dots = attributes == nil
        var verf = verifier
        if dots && from < 1 {
            guard
                packer.packEntry(
                    name: FSFileName(string: "."), itemType: .directory,
                    itemID: identifier(dir.ino), nextCookie: FSDirectoryCookie(rawValue: 1),
                    attributes: nil)
            else { return verf }
        }
        if dots && from < 2 {
            let parent = dir.ino == Wire.rootIno ? FSItem.Identifier.parentOfRoot
                : identifier(dir.attr.parent)
            guard
                packer.packEntry(
                    name: FSFileName(string: ".."), itemType: .directory, itemID: parent,
                    nextCookie: FSDirectoryCookie(rawValue: 2), attributes: nil)
            else { return verf }
        }
        var daemonCookie: UInt64 = from > 2 ? from - 2 : 0
        if daemonCookie == 0 { verf = 0 }
        while true {
            let page = try await client.readdir(
                dir.ino, cookie: daemonCookie, verifier: verf, max: 256,
                attrs: attributes != nil)
            verf = page.verifier
            for e in page.entries {
                let packed = packer.packEntry(
                    name: FSFileName(data: e.name), itemType: itemType(e.kind),
                    itemID: identifier(e.ino), nextCookie: FSDirectoryCookie(rawValue: e.cookie + 2),
                    attributes: e.attr.map(self.attributes))
                if !packed { return verf }
                daemonCookie = e.cookie
            }
            if page.eof || page.entries.isEmpty { return verf }
        }
    }

    // MARK: FSVolume.OpenCloseOperations

    func openItem(
        _ item: FSItem, modes: FSVolume.OpenModes, replyHandler: @escaping ((any Error)?) -> Void
    ) {
        if let it = item as? AscItem {
            lock.lock()
            it.opens += 1
            lock.unlock()
        }
        replyHandler(nil)
    }

    func closeItem(
        _ item: FSItem, modes: FSVolume.OpenModes, replyHandler: @escaping ((any Error)?) -> Void
    ) {
        guard let it = item as? AscItem else {
            replyHandler(nil)
            return
        }
        lock.lock()
        it.opens = max(0, it.opens - 1)
        let last = it.opens == 0
        lock.unlock()
        guard last, it.attr.kind == .file else {
            replyHandler(nil)
            return
        }
        runVoid(replyHandler) { try await self.client.close(it.ino) }
    }

    // MARK: FSVolume.ReadWriteOperations

    func read(
        from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer,
        replyHandler: @escaping (Int, (any Error)?) -> Void
    ) {
        Task {
            do {
                let it = try self.ascItem(item)
                guard offset >= 0 else { throw posix(EINVAL) }
                let want = min(length, buffer.length)
                var got = 0
                while got < want {
                    let n = UInt32(min(want - got, Int(self.maxIO)))
                    let (eof, data) = try await self.client.read(
                        it.ino, offset: UInt64(offset) + UInt64(got), count: n)
                    let take = min(data.count, want - got)
                    buffer.withUnsafeMutableBytes { dst in
                        data.withUnsafeBytes { src in
                            dst.baseAddress!.advanced(by: got)
                                .copyMemory(from: src.baseAddress!, byteCount: take)
                        }
                    }
                    got += take
                    if eof || data.isEmpty { break }
                }
                replyHandler(got, nil)
            } catch {
                replyHandler(0, posix(error))
            }
        }
    }

    func write(
        contents: Data, to item: FSItem, at offset: off_t,
        replyHandler: @escaping (Int, (any Error)?) -> Void
    ) {
        Task {
            do {
                let it = try self.ascItem(item)
                guard offset >= 0 else { throw posix(EINVAL) }
                var sent = 0
                while sent < contents.count {
                    let end = min(contents.count, sent + Int(self.maxIO))
                    let chunk = contents.subdata(
                        in: (contents.startIndex + sent)..<(contents.startIndex + end))
                    let (n, a) = try await self.client.write(
                        it.ino, offset: UInt64(offset) + UInt64(sent), data: chunk)
                    self.update(a)
                    if n == 0 { break }
                    sent += Int(n)
                }
                replyHandler(sent, nil)
            } catch {
                replyHandler(0, posix(error))
            }
        }
    }
}

/// The bytes of an FSKit name, as the daemon takes it.
func wireName(_ n: FSFileName) -> Data { n.data }
