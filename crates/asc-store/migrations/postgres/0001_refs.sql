-- Refs and ref log (SPEC 3.4). `target` is the 33-byte RefTarget encoding.
CREATE TABLE refs (
    repo_id    BIGINT NOT NULL,
    name       TEXT   NOT NULL,
    target     BYTEA  NOT NULL,
    version    BIGINT NOT NULL,
    expires_at BIGINT NULL,
    PRIMARY KEY (repo_id, name)
);

CREATE TABLE ref_log (
    seq     BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repo_id BIGINT NOT NULL,
    name    TEXT   NOT NULL,
    old     BYTEA  NULL,
    new     BYTEA  NULL,
    actor   TEXT   NOT NULL,
    via     TEXT   NOT NULL,
    reason  TEXT   NOT NULL,
    op_id   BYTEA  NOT NULL,
    at      BIGINT NOT NULL
);

CREATE INDEX ref_log_by_ref ON ref_log (repo_id, name, seq);
