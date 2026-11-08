# Worktrees without duplicate files

A second, third or tenth checkout of one repository should not cost the disk space of a full
copy each time. asc has three ways to keep unchanged files from being stored once per worktree.
This page tells you what each one does and what it costs.

| Mode | How unchanged files are kept | Where it works | How to choose it |
|---|---|---|---|
| Virtual | Not on disk at all. A file is read from the shared local store the first time a program opens it. | Windows with ProjFS and the daemon running | the default for `asc worktree add` where it works (`asc.worktree.mode auto`) |
| Plain, copy | A copy-on-write clone (reflink) of the stored file where the file system can clone, else a plain copy. | everywhere | the default elsewhere; `--mode plain` or `asc.worktree.mode plain` |
| Plain, link | A symbolic link to one read-only copy per machine, in `<asc home>/shared-blobs/`. | everywhere symbolic links can be made | `asc config asc.worktree.link symlink` |

Reflinks cost no space on ReFS, APFS, Btrfs and XFS. On NTFS and ext4 a plain copy is a real copy,
so link mode is what saves the space there.

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

## How link mode works

* Each unchanged file in the worktree is a symbolic link to `<asc home>/shared-blobs/ab/cdef…`,
  named after the file's git blob id (`….x` for an executable file). Two worktrees that hold the
  same file point at the same copy.
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
* A committed symbolic link whose target is inside a shared area is never made as a link; it is
  written as a text file that holds the target. A repository cannot use one to read another
  repository's files.
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
