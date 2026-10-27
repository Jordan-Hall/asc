# Restore drill (SPEC §16.3, quarterly)

Restores Postgres to a time T on staging, points an `asc serve` at the bucket, and proves the
repositories are whole. The drill passes when `verify --all` exits 0.

```sh
export CFG=/etc/asc/asc-serve-staging.toml     # staging db and the production bucket (read-only)
ascs() { asc serve --config "$CFG" "$@"; }
T="2026-01-01T00:00:00Z"                     # the restore point

# 1. Restore the Postgres backup to T into the staging database named by `db` in $CFG.
#    (Use the platform's PITR: restore to a new instance, then update `db`.)
# 2. Apply any newer migrations, without starting the server.
ascs migrate
# 3. Every ref's closure exists and hashes; the git map round-trips. Exit 0 = pass.
ascs admin verify --all || { echo "restore drill FAILED"; exit 1; }
# 4. Rebuild the bridge from asc and compare with git (mirror copies are rebuilt from asc).
asc export --git /srv/asc/bridge-check.git --refs 'refs/*'
ascs admin verify-import --repo asc/dds --repo asc/soma
```

Notes:

* `asc serve backup` and `asc serve restore` take and restore a whole-install backup: [backup.md](backup.md).

* The bucket is the source of objects; Postgres holds refs, the graph and the git map. A restore
  to T therefore loses ref moves after T but never objects; objects newer than T are unreachable
  until GC (`asc.gc.pitrDays`) and can be re-referenced from the ref log.
* Step 3 failures name the first missing or corrupt objects. A "git map" failure means a commit
  row is missing or points elsewhere; `ascs admin import --repo R` rewrites it from the mirror.
* Step 4 needs `cache.disk_dir` set so the mirror copy exists. Record the drill date and result
  in the ops log.
