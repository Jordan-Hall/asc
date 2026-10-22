-- Local operation log (SPEC 20.3): one row per `op_id`; ref deltas are the `ref_log` rows.
-- `argv` is stored redacted. Heads and the snapshot are 32-byte changeset ids.
CREATE TABLE op_log (
    repo_id     BIGINT  NOT NULL,
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    op_id       BLOB    NOT NULL UNIQUE,
    ws_id       TEXT    NOT NULL,
    at          BIGINT  NOT NULL,
    argv        TEXT    NOT NULL,
    kind        TEXT    NOT NULL,
    head_before BLOB    NULL,
    head_after  BLOB    NULL,
    wc_snapshot BLOB    NULL,
    undoes_seq  INTEGER NULL
);
CREATE INDEX op_log_by_repo ON op_log (repo_id, seq);
CREATE INDEX op_log_by_time ON op_log (repo_id, at);
