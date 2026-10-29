-- Scheduled backups (docs/BUILDOUT.md section 24). One row per run of a backup; `state` 1 running, 2 succeeded, 3 failed;
-- `trig` 1 manual, 2 schedule; `verify` 0 not verified, 1 running, 2 verified, 3 failed. `location` is where the backup
-- is on its target (a directory name under the target, never a credential). `purged` is set when retention removed the files.
CREATE TABLE backup_runs (
    id          TEXT   NOT NULL PRIMARY KEY,
    state       BIGINT NOT NULL,
    trig        BIGINT NOT NULL,
    target      TEXT   NOT NULL,
    started     BIGINT NOT NULL,
    finished    BIGINT NOT NULL DEFAULT 0,
    size_bytes  BIGINT NOT NULL DEFAULT 0,
    objects     BIGINT NOT NULL DEFAULT 0,
    encrypted   BIGINT NOT NULL DEFAULT 0,
    verify      BIGINT NOT NULL DEFAULT 0,
    verified_at BIGINT NOT NULL DEFAULT 0,
    note        TEXT   NOT NULL DEFAULT '',
    done_bytes  BIGINT NOT NULL DEFAULT 0,
    total_bytes BIGINT NOT NULL DEFAULT 0,
    actor       TEXT   NOT NULL DEFAULT '',
    location    TEXT   NOT NULL DEFAULT '',
    purged      BIGINT NOT NULL DEFAULT 0,
    seal        TEXT   NOT NULL DEFAULT ''
);
CREATE INDEX backup_runs_by_time ON backup_runs (started);

-- The schedule: always the row with id 1.
CREATE TABLE backup_schedule (
    id                BIGINT NOT NULL PRIMARY KEY,
    enabled           BIGINT NOT NULL DEFAULT 0,
    interval_hours    BIGINT NOT NULL DEFAULT 24,
    minute_of_day     BIGINT NOT NULL DEFAULT 120,
    target            TEXT   NOT NULL DEFAULT '',
    keep_days         BIGINT NOT NULL DEFAULT 30,
    keep_weeks        BIGINT NOT NULL DEFAULT 52,
    include_vault_key BIGINT NOT NULL DEFAULT 0,
    include_objects   BIGINT NOT NULL DEFAULT 1,
    updated           BIGINT NOT NULL DEFAULT 0
);

-- A staged restore, applied by the server when it next starts. At most one is pending.
CREATE TABLE backup_restores (
    id           TEXT   NOT NULL PRIMARY KEY,
    backup_id    TEXT   NOT NULL,
    state        BIGINT NOT NULL,
    requested    BIGINT NOT NULL,
    actor        TEXT   NOT NULL,
    note         TEXT   NOT NULL DEFAULT ''
);
