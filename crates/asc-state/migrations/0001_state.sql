-- state.db (SPEC 9.2). Integers are INTEGER/BIGINT and booleans INTEGER 0/1 so that
-- sqlx::Any decodes every column as i64.
CREATE TABLE meta (
    schema_version INTEGER NOT NULL,
    machine_id     BLOB    NOT NULL,
    asc_version     TEXT    NOT NULL DEFAULT ''
);
INSERT INTO meta (schema_version, machine_id) VALUES (1, randomblob(16));

CREATE TABLE repos (
    id             TEXT    NOT NULL PRIMARY KEY,
    name           TEXT    NOT NULL,
    ns             BLOB    NOT NULL,
    remote_url     TEXT    NULL,
    remote_kind    TEXT    NOT NULL,
    default_branch TEXT    NOT NULL,
    windows_safe   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX repos_by_name ON repos (name);
CREATE INDEX repos_by_remote ON repos (remote_kind, remote_url);

-- Refs and ref log (SPEC 3.4); identical to asc-store's migrations/sqlite/0001_refs.sql.
-- `repo_id` is the rowid of `repos`.
CREATE TABLE refs (
    repo_id    BIGINT NOT NULL,
    name       TEXT   NOT NULL,
    target     BLOB   NOT NULL,
    version    BIGINT NOT NULL,
    expires_at BIGINT NULL,
    PRIMARY KEY (repo_id, name)
);

CREATE TABLE ref_log (
    seq     INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id BIGINT NOT NULL,
    name    TEXT   NOT NULL,
    old     BLOB   NULL,
    new     BLOB   NULL,
    actor   TEXT   NOT NULL,
    via     TEXT   NOT NULL,
    reason  TEXT   NOT NULL,
    op_id   BLOB   NOT NULL,
    at      BIGINT NOT NULL
);

CREATE INDEX ref_log_by_ref ON ref_log (repo_id, name, seq);

CREATE TABLE cs_gen (
    repo_id BIGINT NOT NULL,
    cs      BLOB   NOT NULL,
    gen     BIGINT NOT NULL,
    parents BLOB   NOT NULL,
    PRIMARY KEY (repo_id, cs)
);

CREATE TABLE history_boundary (
    repo_id BIGINT NOT NULL,
    cs      BLOB   NOT NULL,
    PRIMARY KEY (repo_id, cs)
);

CREATE TABLE git_map (
    repo_id BIGINT  NOT NULL,
    git     BLOB    NOT NULL,
    kind    INTEGER NOT NULL,
    asc      BLOB    NOT NULL,
    PRIMARY KEY (repo_id, git, kind)
);
CREATE INDEX git_map_by_asc ON git_map (repo_id, asc);

CREATE TABLE workspaces (
    id              TEXT    NOT NULL PRIMARY KEY,
    repo_id         BIGINT  NOT NULL,
    path            TEXT    NOT NULL UNIQUE,
    mount           TEXT    NOT NULL,
    mode            TEXT    NOT NULL,
    head            TEXT    NOT NULL,
    base_cs         BLOB    NULL,
    epoch           BIGINT  NOT NULL,
    epoch_time      BIGINT  NOT NULL,
    sparse          TEXT    NULL,
    ephemeral_until BIGINT  NULL,
    locked          TEXT    NULL,
    created         BIGINT  NOT NULL
);
CREATE INDEX workspaces_by_repo ON workspaces (repo_id);
CREATE INDEX workspaces_by_mount ON workspaces (mount);

CREATE TABLE stage (
    ws_id         TEXT    NOT NULL,
    set_id        BIGINT  NOT NULL DEFAULT 0,
    path          BLOB    NOT NULL,
    stage         INTEGER NOT NULL,
    kind          INTEGER NOT NULL,
    file          BLOB    NULL,
    size          BIGINT  NOT NULL,
    git           BLOB    NULL,
    intent_to_add INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (ws_id, set_id, path, stage)
);

CREATE TABLE stage_sets (
    ws_id      TEXT   NOT NULL,
    set_id     BIGINT NOT NULL,
    index_path TEXT   NOT NULL UNIQUE,
    created    BIGINT NOT NULL,
    PRIMARY KEY (ws_id, set_id)
);

CREATE TABLE entry_times (
    ws_id TEXT   NOT NULL,
    path  BLOB   NOT NULL,
    time  BIGINT NOT NULL,
    PRIMARY KEY (ws_id, path)
);

CREATE TABLE journal (
    ws_id TEXT    NOT NULL,
    seq   BIGINT  NOT NULL,
    path  BLOB    NOT NULL,
    op    INTEGER NOT NULL,
    PRIMARY KEY (ws_id, seq)
);

CREATE TABLE switch_intent (
    ws_id     TEXT   NOT NULL PRIMARY KEY,
    from_root BLOB   NOT NULL,
    to_root   BLOB   NOT NULL,
    to_head   TEXT   NOT NULL,
    pending   BLOB   NOT NULL,
    started   BIGINT NOT NULL
);

CREATE TABLE ws_ops (
    ws_id           TEXT NOT NULL PRIMARY KEY,
    pending_renames BLOB NOT NULL
);

CREATE TABLE inodes (
    ws_id TEXT   NOT NULL,
    ino   BIGINT NOT NULL,
    path  BLOB   NOT NULL,
    PRIMARY KEY (ws_id, ino),
    UNIQUE (ws_id, path)
);
