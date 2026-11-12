# macOS FSKit backend

asc serves virtual work trees on macOS through the system NFS client (SPEC §10.3b). On macOS 26
and later, a user-space file system written with FSKit can take the place of that mount when the
asc FSKit module is installed. This document describes the module, the protocol between the
module and the daemon, and how to build and enable it.

## Parts

| Part | Where | What it does |
|---|---|---|
| Server | `crates/asc-vfs/src/fskit/server.rs` | Answers the protocol over the merged file system (`unixfs::MergedFs`), the same one the FUSE and NFS front ends serve |
| Wire | `crates/asc-vfs/src/fskit/wire.rs`, `macos/fskit/Sources/AscWire/Wire.swift` | The protocol, written once in Rust and once in Swift; a golden test on each side checks the same bytes |
| Provider | `crates/asc-vfs/src/fskit/provider.rs` | Starts the server on the work tree's socket and runs `mount -F -t ascfs ascfs://<id> <dir>` |
| Module | `macos/fskit/Sources/AscFSExtension` | The FSKit extension (`FSUnaryFileSystem`): connects to the socket and turns each FSKit call into one request |
| Host app | `macos/fskit/Sources/AscFS` | `AscFS.app`, which carries the extension and opens the System Settings pane where it is enabled |

## Why not only NFS

The NFS mount works with no install, and it stays the fallback. The FSKit module removes what NFS
adds: a TCP port, the client's attribute cache (`actimeo`), silly renames and `soft` time-outs.
The module sees every open and close, so the server can count opens like the FUSE front end.

## Choosing the backend

`asc.vfs.backend` is `auto`, `fskit` or `nfs` on macOS. `auto` picks FSKit when all of these are
true, and NFS otherwise:

* the build has the `vfs-fskit` feature (the default),
* the system is macOS 26 or later,
* `AscFS.app` is in `/Applications` or `~/Applications`,
* the app group directory exists: `ASC_FSKIT_GROUP_DIR`, or else the one directory in
  `~/Library/Group Containers` whose name ends in `.dev.asc.fskit`.

`asc doctor` reports the result as the `fskit` check. When the app is installed but the group
directory is missing, the check warns and says to open the app and enable the extension under
System Settings > General > Login Items & Extensions > File System Extensions.

## Socket and security

* The socket is `<group dir>/s/<work tree id as 16 hex digits>.sock`. The module is sandboxed and
  can reach only its app group container, so the socket must live there.
* The `s` directory is mode `0700` and must belong to the daemon's user; the socket is mode
  `0600`. A connection from another user is closed (Linux also checks the peer's credentials).
* The socket path must fit in `sun_path` (103 bytes). A longer path is refused with the reason.
* At most a fixed number of connections are open at once; more are closed.

## Protocol

Every message is a 4-byte big-endian length, then the body. Bodies use XDR (RFC 4506): big-endian
integers, opaque data padded to 4 bytes.

* Request: `xid u32, op u32, arguments`.
* Reply: `xid u32, error u32, results`. `error` is a Darwin errno number, 0 for success. A reply
  with an error has no results.
* Replies can come in any order; the client matches them by `xid`.

| Op | Number | Arguments | Results |
|---|---|---|---|
| HELLO | 0 | version | version, root ino, root attr, max I/O size |
| GETATTR | 1 | ino | attr |
| LOOKUP | 2 | dir ino, name | attr |
| FORGET | 3 | ino, count | none |
| SETATTR | 4 | ino, mask, mode, uid, gid, size, atime, mtime | attr |
| READLINK | 5 | ino | target |
| READ | 6 | ino, offset, count | eof, data |
| WRITE | 7 | ino, offset, data | count, attr |
| CREATE | 8 | dir ino, name, mode | attr |
| MKDIR | 9 | dir ino, name, mode | attr |
| SYMLINK | 10 | dir ino, name, target | attr |
| REMOVE | 11 | dir ino, name, is a directory | none |
| RENAME | 12 | from dir, from name, to dir, to name, flags (1: fail when the target exists) | none |
| READDIR | 13 | dir ino, cookie, verifier, max bytes, with attributes | verifier, entries, eof |
| STATFS | 14 | none | blocks, free, available, files, free files, block size, name max |
| SYNC | 15 | ino | none |
| CLOSE | 16 | ino | none |

The authoritative layout of each message is the comment on its op number in
`crates/asc-vfs/src/fskit/wire.rs`.

* **Attribute (88 bytes):** ino u64, parent u64, kind, mode, links, uid, gid (u32 each), size
  u64, bytes used u64, then access, modify and change times, each `seconds i64, nanoseconds u32`.
* **Inodes:** the root is 1. The work tree's `.git` file is `2^62 - 1`, read-only, and is not
  counted. LOOKUP, CREATE, MKDIR and SYMLINK each count one lookup of the inode they answer;
  FORGET(ino, n) gives back n. When a connection closes, the server gives back everything that
  connection counted, so a crashed module leaks nothing.
* **Listings:** READDIR leaves out `.` and `..`. Each entry carries its cookie: the index after
  it. The verifier changes when the directory changes; a cookie with an old verifier gets
  `ESTALE` (70).
* **Version:** HELLO with any other version gets `ERPCMISMATCH` (75), and the module refuses to
  mount.

## How the module maps FSKit

* **Item identifiers:** FSKit's identifier is the server inode plus 1, so the root (inode 1) is
  `.rootDirectory` (2) and the root's parent is `.parentOfRoot`.
* **Items:** each `FSItem` holds the inode, the last attribute, its lookup count and its open
  count. When FSKit reclaims an item, the module sends CLOSE if the item is still open, then
  FORGET with the lookup count.
* **Directory cookies:** the module adds `.` and `..` itself. Cookie 0 starts a listing; `.`
  ends at cookie 1 and `..` at cookie 2; a server cookie `c` is sent to FSKit as `c + 2`.
  `ESTALE` becomes `FSError(.invalidDirectoryCookie)`, so FSKit restarts the listing.
* **Reads and writes** larger than the server's maximum I/O size are split into several requests.
* **Resources:** the module accepts only `FSGenericURLResource` URLs of the form
  `ascfs://<16 lowercase hex digits>`, and only when that work tree's socket exists. The volume
  UUID is derived from the work tree id, so it stays the same across mounts.

## Building and signing

The module needs Swift 6 and the macOS 26 SDK.

```sh
swift test --package-path macos/fskit              # the wire client against a fake daemon
macos/fskit/scripts/build-app.sh --adhoc           # a build check; macOS will not load it
macos/fskit/scripts/build-app.sh --team TEAMID \
  --sign "Developer ID Application: ..." --profile ext.provisionprofile
```

`build-app.sh` writes both `Info.plist` files, the entitlements (app sandbox, the app group
`<TEAMID>.dev.asc.fskit`, and `com.apple.developer.fskit.fsmodule` for the extension), signs the
extension and then the app, and verifies the result. Then:

1. Copy `AscFS.app` to `/Applications`.
2. Open it once. It creates the app group container and opens the System Settings pane.
3. Enable "asc" under File System Extensions.
4. Restart the daemon (`asc daemon stop`, then any command). New virtual work trees use FSKit.
   A work tree that is already mounted keeps its backend until it is mounted again.

## What is tested where

* The server, its lookup counts, listings, hydration, writes, socket checks and connection
  limits are tested on Linux with an in-process client (`crates/asc-vfs/src/fskit/tests.rs`).
* The Swift wire client is tested on macOS 26 in CI (`fskit-swift` job): golden encodings,
  framing, replies out of order, errors, version mismatch and a closed connection.
* CI builds the app with an ad-hoc signature. A real mount needs a Developer ID signature, a
  provisioning profile with the FSKit entitlement, and the user enabling the extension, so it is
  not part of CI.

## Limits

* No extended attributes, as with NFS: projection state comes from the overlay.
* The hydration deny policy (`asc.vfs.denyHydration`) cannot see the calling process; FSKit does
  not pass it to the module.
* The tree is case-sensitive, like the repository.
* A stopped daemon makes calls fail with `ENOTCONN`. `asc daemon start` mounts again.
