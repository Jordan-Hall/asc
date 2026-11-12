# Runbook: virtual CI checkout

`asc ci checkout --mode virtual` (SPEC §12.3) mounts the job's work tree from an asc daemon on
the runner host, so a job downloads only the files it reads. Plain mode stays the default; use
virtual mode when checkout time or disk on the runner matters more than a FUSE dependency.

## Runner requirements

- Linux with FUSE 3 (`/dev/fuse` usable by the runner user, `fuse3` installed).
- A runner that is not root needs `user_allow_other` in `/etc/fuse.conf`. The checkout refuses
  to start without it.
- Jobs in containers see the mount only through a shared bind mount. Make the workspace parent
  shared once per boot, and bind it into containers with `rshared` propagation:

      mount --bind /srv/runner/work /srv/runner/work
      mount --make-rshared /srv/runner/work
      docker run --mount type=bind,source=/srv/runner/work,target=/srv/runner/work,bind-propagation=rshared …

  The checkout warns when the parent of `--dir` is not on a shared mount: containers started
  before the checkout step (service containers) then do not see the work tree.

## Who can read the mount

`--mount-access` (action input `mount-access`) is persisted as `asc.vfs.mountAccess`:

| Value | Mount option | Use for |
| --- | --- | --- |
| `root` (default) | `allow_root` | Containers that run as root. |
| `all` | `allow_other` | Containers that run as another user. Every local user of the runner can then read the tracked files; use it only on single-tenant runners. |

## Workflow

    - uses: asc/actions/checkout@v1
      with:
        mode: virtual
        mount-access: root

## Lifecycle

The daemon starts with the job's `ASC_HOME` (`$RUNNER_TEMP/asc-home`). It checks every 5 s that
the home still exists and stops, unmounting the work tree, once the runner removes the job's
temporary directory. To stop it at once, run `asc daemon stop` with the same `ASC_HOME`.

## First diagnostic command

    grep fuse /proc/self/mountinfo

A missing line means the daemon did not mount (check the checkout step's log); a line without
`shared:` on the parent mount explains a container that sees an empty directory.
