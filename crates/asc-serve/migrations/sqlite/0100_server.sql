-- Server schema, release 1 (SPEC 8.2). Later releases only add nullable columns and tables.
CREATE TABLE repos (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    name           TEXT    NOT NULL UNIQUE,
    ns             BLOB    NOT NULL,
    mode           TEXT    NOT NULL DEFAULT 'primary',
    default_branch TEXT    NOT NULL DEFAULT 'master',
    windows_safe   BOOLEAN NOT NULL DEFAULT TRUE,
    forgejo_reads  TEXT    NOT NULL DEFAULT 'git',
    bridge         TEXT    NOT NULL DEFAULT 'off',
    land_branches  TEXT    NOT NULL DEFAULT '[]',
    created        BIGINT  NOT NULL
);

CREATE TABLE cs_graph (
    ns      BLOB   NOT NULL,
    cs      BLOB   NOT NULL,
    gen     BIGINT NOT NULL,
    parents BLOB   NOT NULL,
    PRIMARY KEY (ns, cs)
);

CREATE TABLE git_map (
    ns   BLOB     NOT NULL,
    git  BLOB     NOT NULL,
    kind SMALLINT NOT NULL,
    asc   BLOB     NOT NULL,
    PRIMARY KEY (ns, git, kind)
);
CREATE INDEX git_map_asc ON git_map (ns, asc);

CREATE TABLE file_meta (
    ns   BLOB   NOT NULL,
    file BLOB   NOT NULL,
    size BIGINT NOT NULL,
    git  BLOB   NOT NULL,
    PRIMARY KEY (ns, file)
);

CREATE TABLE op_results (
    op_id   BLOB   NOT NULL PRIMARY KEY,
    repo_id BIGINT NOT NULL,
    result  BLOB   NOT NULL,
    at      BIGINT NOT NULL
);
CREATE INDEX op_results_by_at ON op_results (at);

CREATE TABLE outbox (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id    BIGINT  NOT NULL,
    op_id      BLOB    NOT NULL,
    effects    BLOB    NOT NULL,
    attempts   INTEGER NOT NULL DEFAULT 0,
    next_at    BIGINT  NOT NULL,
    done_at    BIGINT  NULL,
    last_error TEXT    NULL
);
CREATE INDEX outbox_pending ON outbox (next_at) WHERE done_at IS NULL;

CREATE TABLE mirror_state (
    repo_id     BIGINT NOT NULL PRIMARY KEY,
    last_import BIGINT NOT NULL,
    last_error  TEXT   NULL
);

CREATE TABLE gc_marks (
    run BIGINT NOT NULL,
    ns  BLOB   NOT NULL,
    key BLOB   NOT NULL
);
CREATE INDEX gc_marks_by_run ON gc_marks (run, ns);
