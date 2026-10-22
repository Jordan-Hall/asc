-- Branchless drafts and the commit-cloud queue (SPEC 21.2.2). Maintained in both modes.
CREATE TABLE draft_heads (
    repo_id   BIGINT  NOT NULL,
    cs        BLOB    NOT NULL,
    change    BLOB    NOT NULL,
    added     BIGINT  NOT NULL,
    hidden_at BIGINT  NULL,
    PRIMARY KEY (repo_id, cs)
);
CREATE INDEX draft_heads_by_hidden ON draft_heads (repo_id, hidden_at);

-- state: 0 pending, 1 failed, 2 too_large.
CREATE TABLE cloud_queue (
    repo_id    BIGINT  NOT NULL,
    cs         BLOB    NOT NULL,
    enq        BIGINT  NOT NULL,
    attempts   INTEGER NOT NULL DEFAULT 0,
    next_at    BIGINT  NOT NULL DEFAULT 0,
    state      INTEGER NOT NULL DEFAULT 0,
    last_error TEXT    NULL,
    PRIMARY KEY (repo_id, cs)
);

CREATE TABLE cloud_state (
    repo_id   BIGINT PRIMARY KEY,
    ws_name   TEXT   NOT NULL,
    version   BIGINT NOT NULL,
    last_sync BIGINT NOT NULL
);

-- Head set at the last successful sync (3-way base).
CREATE TABLE cloud_base (
    repo_id BIGINT NOT NULL,
    cs      BLOB   NOT NULL,
    PRIMARY KEY (repo_id, cs)
);
