# Backup and restore

`asc serve backup` and `asc serve restore` read `asc-serve.toml` like the server does (`--config`). Backup runs against a live server.
Restore is for an empty install: stop the server first.

```sh
# A backup key: 32 random bytes, mode 0600. Keep it somewhere other than the backup.
head -c 32 /dev/urandom > /etc/asc/backup.key && chmod 600 /etc/asc/backup.key

asc serve backup --config /etc/asc/asc-serve.toml --out /srv/backups/2026-10-27 \
    --key-file /etc/asc/backup.key --include-vault-key

# On a new machine, with the same asc-serve.toml pointing at an empty database and bucket:
asc serve restore --config /etc/asc/asc-serve.toml --from /srv/backups/2026-10-27 --key-file /etc/asc/backup.key
asc serve migrate        # only when this release is newer than the backup
```

## What a backup holds

The directory is created with mode 0700 and every file in it is 0600.

| File | Content |
|---|---|
| `db.sqlite` | A transactionally consistent copy made with `VACUUM INTO` while the server runs (`db = "sqlite:..."`). |
| `db.pgdump` | `pg_dump --format=custom` of a Postgres `db`. Needs the PostgreSQL client tools on the path; the password goes in `PGPASSWORD`, never on a command line. |
| `objects/...`, `objects.idx` | Every object of the instance bucket (`store`), with its size and SHA-256. `--no-objects` leaves them out, for a bucket that is versioned and backed up by its own means. |
| `vault.keys.sealed` | The vault master key file, sealed (XChaCha20-Poly1305) under `--key-file`. Present only with `--include-vault-key`, which needs `--key-file`: the key is never written in the clear. |
| `manifest.json` | Format, time, release, database kind, and the size and SHA-256 of every other file. |

Not included: bring-your-own storage buckets (they belong to their owners), the mirror and git view caches (rebuilt from the bucket),
and the SSH host keys and `[auth]` key files (copy them as part of configuration management).

Without the vault master key a restored database cannot open any stored secret (storage credentials, AI keys, workflow secrets).
Either include it, sealed, or keep your own copy of `[vault] key_file`.

## Restore

1. The manifest is read and every file and every object is checked against it. A changed or missing file stops the restore before
   anything is written.
2. Restore refuses to write over a database that has rows, a bucket that has objects, or an existing vault key file, and says which.
   `--force` writes over them. A freshly migrated database with no rows counts as empty.
3. SQLite: the copy is written next to the target, passes `PRAGMA integrity_check`, and is renamed into place (a stale `-wal` and `-shm`
   are removed). Postgres: `pg_restore` (with `--clean --if-exists` under `--force`).
4. Objects are written to the bucket and counted back. The vault key is written with mode 0600.

Then run the drill in [restore.md](restore.md) (`asc serve admin verify --all`).
