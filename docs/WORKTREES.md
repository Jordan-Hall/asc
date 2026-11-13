# Worktrees without duplicate files

A second, third or tenth checkout of one repository should not cost the disk space of a full
copy each time. asc has three ways to keep unchanged files from being stored once per worktree.
This page tells you what each one does and what it costs.

| Mode | How unchanged files are kept | Where it works | How to choose it |
|---|---|---|---|
| Virtual | Not on disk at all. A file is read from the shared local store the first time a program opens it. | The daemon running, and: Windows 10 1709 or later (ProjFS when it is installed, otherwise the built-in Cloud Files API); Linux with FUSE (`/dev/fuse`, fuse3) or the NFS client; macOS with the asc FSKit module (`AscFS.app`, macOS 26 and later) or else the system NFS client (a loopback NFSv3 server, no kernel extension) | the default for `asc worktree add` where it works (`asc.worktree.mode auto`) |
| Plain, copy | A copy-on-write clone (reflink) of the stored file where the file system can clone, else a plain copy. | everywhere | the default elsewhere; `--mode plain` or `asc.worktree.mode plain` |
| Plain, link | A symbolic link to one read-only copy per machine, in `<asc home>/shared-blobs/<repo-id>/`. | everywhere symbolic links can be made | `asc config asc.worktree.link symlink`; the last resort, see "Which mode to use" |

Reflinks cost no space on ReFS, APFS, Btrfs and XFS. On NTFS and ext4 a plain copy is a real copy,
so link mode is what saves the space there.

## Which mode to use

Use the first mode on this list that fits your machine.

1. Virtual, when the machine can serve it. Nothing unchanged is on disk, and tools see ordinary files.
2. Plain copy on a volume that clones (Dev Drive or ReFS, Btrfs, XFS, APFS). Files are ordinary files and cost no extra space.
3. Link mode only as a last resort, on a volume that cannot clone (NTFS, ext4) where virtual mode is not available. Tools that resolve real paths (Node, bundlers, test runners) look beside the shared copy, real git reports a type change for each link, and Windows needs the right to make symbolic links. See "Limits of link mode" below.

## Choosing the mode

```sh
asc worktree add ../review-1 topic                 # virtual if this machine can, else plain
asc worktree add --mode plain ../review-2 topic    # plain whatever the setting
asc config --global asc.worktree.mode plain        # never make virtual worktrees by default
asc config asc.worktree.link symlink               # plain checkouts link unchanged files
```

`--mode` beats `asc.worktree.mode`. `asc.worktree.mode` is `auto` (the default), `virtual` or
`plain`; `virtual` on a machine that cannot serve a virtual workspace is an error that tells you
why. `asc.worktree.link` is `copy` (the default) or `symlink`, and applies to every plain checkout
of the repository: `worktree add`, `switch`, `checkout`, `restore`, `reset` and the rest. It has
no effect on a virtual worktree, which keeps no copies to share.

`asc worktree convert --plain` turns a virtual worktree into a plain one. It stops serving the
worktree and takes the virtual marks off the files on disk, keeping your edits, new files and
deletions. It asks the daemon for the deletions before the worktree stops being served, because
Windows can hide a deleted file from a folder listing and from a lookup by its name. The daemon
remembers each file and folder you delete or move away while it serves the worktree, and also
looks for deleted files on disk. Then it writes the other tracked files from the object store. If it stops part way
(a crash, a full disk), run it again: it goes on from where it stopped.

`asc worktree move` moves a virtual worktree too. It stops serving the worktree, moves the folder
and serves it again at the new path, so edits, new files, deletions and files that were never
read move with it. On Windows a Cloud Files folder cannot be renamed while it is registered, so
asc unregisters it, moves it and registers it again, putting back each file's virtual mark. A
virtual worktree cannot be deleted with `rm -rf` while it is served: use `asc worktree remove`.

## Installed dependencies

A new plain worktree has no `node_modules`, so the package manager would install again and the
build caches inside it (Rspack's and webpack's `node_modules/.cache`, Vite's `node_modules/.vite`)
would start cold. `asc worktree add` clones them from the worktree you ran it in instead, when
both pin the same dependencies: every lockfile at the root (`bun.lock`, `bun.lockb`,
`package-lock.json`, `npm-shrinkwrap.json`, `pnpm-lock.yaml`, `yarn.lock`) and the `package.json`
beside each `node_modules` (up to 6 levels down) are byte for byte the same. turbo's
`.turbo/cache` is cloned too, because its entries are named by content hash.

The clone is a reflink: both worktrees share the blocks until one side writes, so it costs no
space. On a volume that cannot reflink (NTFS, ext4) nothing is cloned and asc says so; a Dev
Drive or ReFS volume on Windows, Btrfs or XFS on Linux and APFS on macOS can. asc never falls
back to a byte copy or to hard links. When a lockfile differs, asc says which one and leaves the
install to you.

```sh
asc config asc.worktree.cloneDeps false   # never clone dependencies into new worktrees
```

## Putting worktrees on a volume that clones

Plain worktrees, cloned `node_modules` and build cache restores cost no space only on a volume that
can reflink. Keep the repository, its worktrees and the asc home on the same such volume: a reflink
cannot cross volumes.

| System | Volume | How to get one |
|---|---|---|
| Windows 11 | Dev Drive (ReFS) | Settings, System, Storage, Disks and volumes, Create Dev Drive; or `Format-Volume -DriveLetter E -DevDrive` on a new partition or VHDX |
| Linux | XFS with `reflink=1` (the default of `mkfs.xfs` since 5.1), or Btrfs | `mkfs.xfs /dev/sdX` or `mkfs.btrfs /dev/sdX` |
| macOS | APFS | every APFS volume |

Then point asc at it:

```sh
export ASC_HOME=/vol/asc                       # the object store, shared blobs and the build cache
asc config --global asc.buildcache.dir /vol/asc/buildcache
asc clone <url> /vol/src/app && cd /vol/src/app
asc worktree add /vol/src/app-review topic     # same volume: files and node_modules are clones
```

`asc cache status` tells you what the build cache found: `blobs plain, restored by reflink` on a
volume that clones, `blobs compressed where it saves space` on one that does not. On a volume that
cannot reflink, link mode (below) is what keeps worktree files from being stored twice, and the
build cache keeps its blobs compressed (`asc.buildcache.compress`).

## How link mode works

* Each unchanged file in the worktree is a symbolic link to
  `<asc home>/shared-blobs/<repo-id>/ab/cdef…`, named after the file's git blob id (`….x` for an
  executable file). Two worktrees of one repository that hold the same file point at the same
  copy. Each repository has its own area, so two repositories never share a copy.
* Shared copies are read-only and carry a fixed modification time (2000-01-01). asc reads the
  link as the file it points to: `status`, `diff`, `add`, `commit`, `grep`, `blame` and the rest
  see a regular file. `status` reads the blob id from the link name, so a clean link costs no
  read of the file.
* Saving a file is an edit, as usual. Most editors save by writing a new file and renaming it
  over the old one. That replaces the link with a regular file, and the shared copy is not
  touched.
* A program that writes through the link instead (vim's `:w!`, or any tool run as root or
  administrator, which ignores the read-only bit) changes the shared copy. Its modification time
  then no longer matches, so `status` hashes the file and shows the edit, and no later checkout
  links to that copy again: it writes a plain file instead. Other worktrees that link to the
  changed copy show the same edit; restore them with `asc restore <path>`.
* asc reads a link as its file only when the link points into the shared area of an asc home it
  opened and the area file is a regular file, not another link. Any other link, such as one into
  a directory that only looks like an area, is an ordinary symbolic link.
* A committed symbolic link whose target looks like a shared area file is never made as a link; it
  is written as a text file that holds the target. A repository cannot use one to read another
  repository's files.
* `asc mcp` reads through a link only into the area of the repository it serves. A link that an
  agent makes into another repository's area is refused like any other link. The code index never
  reads through links.
* asc never uses hard links: an edit through a hard link would change the shared copy and
  nothing could tell.

`asc gc` removes the shared copies that no link in a registered plain worktree names. A copy made
in the last hour is kept, because a checkout that is still running may not have linked it yet;
`asc.gc.sharedGrace` changes that time (`0s` removes at once). If a plain worktree's directory
cannot be read (a removed disk, for example), `asc gc` keeps every copy and says which worktree
it could not read. A link outside every registered worktree, such as a tree you copied with
`cp -a`, is not counted: run `asc gc` only after you replace such links with files.

```sh
asc gc --dry-run      # shared: would remove 3 unlinked files (12.0 KiB); 840 linked; nothing was deleted
asc gc                # shared: removed 3 unlinked files (12.0 KiB); 840 linked
```

## Limits of link mode

* **Tools that resolve real paths.** Node's module resolution, most bundlers and some test
  runners follow a link to its target and then look for `node_modules`, `package.json` or a
  config file next to the target, in the shared area, where there are none. A worktree that
  such a tool reads should stay in copy mode, or the tool should be told to keep link paths
  (`node --preserve-symlinks`, `resolve.symlinks: false` in webpack and Vite).
* **Windows needs the right to make symbolic links.** Turn on Developer Mode, or run asc with the
  `SeCreateSymbolicLinkPrivilege` right. Without it, link mode falls back to copies, file by
  file, and says nothing; the checkout is correct either way.
* **Real git sees a type change.** git run directly in the worktree, not through asc, sees each
  link where its index holds a regular file and reports `T` (type change) for it. Use asc's
  commands in a link-mode worktree, and keep a worktree that git or a git-based tool must read
  in copy mode.
* **Archives and copies.** `tar`, `zip` and `cp` without `-L` store the links, not the files.
  Use `asc archive` to make an archive of a revision.

## macOS: the FSKit backend

On macOS 26 and later, virtual worktrees can be served by the asc FSKit module instead of the
loopback NFS mount. The module is a file system extension carried by `AscFS.app`. It needs no
network port, does not cache file attributes for a second after a switch, and does not leave
`.nfsXXXX` files behind.

1. Build and sign `AscFS.app` with `macos/fskit/scripts/build-app.sh --team TEAMID --sign ...`
   (see [design/FSKIT.md](design/FSKIT.md)), or install a signed copy.
2. Copy it to `/Applications` and open it once.
3. Enable "asc" under System Settings > General > Login Items & Extensions > File System
   Extensions.
4. Run `asc doctor`. The `fskit` check says `ok` when the module can be used.

`asc.vfs.backend auto` then picks FSKit for new virtual worktrees, and NFS when the module is
missing. `asc config --global asc.vfs.backend nfs` keeps NFS. A worktree that is already mounted
keeps its backend until the daemon mounts it again.

If a mount fails with "could not mount the FSKit work tree", check that the extension is enabled,
and that `~/Library/Group Containers` holds exactly one directory ending in `.dev.asc.fskit`. Set
`ASC_FSKIT_GROUP_DIR` to the group directory when the module was signed with another group.
