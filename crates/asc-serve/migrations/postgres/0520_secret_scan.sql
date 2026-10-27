-- Secret scanning (docs/BUILDOUT.md section 4). A finding never holds the secret: `preview` keeps at
-- most two characters and `fingerprint` is HMAC-SHA256(repository key, rule id || value), so it is
-- stable per (repository, rule, value) and cannot be reversed. `secret_scan_keys` holds the per
-- repository key the fingerprints are keyed with. Ids are per repository, allocated as max + 1 inside
-- the writing transaction; a lost race is a primary-key conflict and the writer retries. Every number
-- is BIGINT so the sqlx Any driver reads one type on both databases.
CREATE TABLE secret_scan_keys (
    repo_id BIGINT NOT NULL PRIMARY KEY,
    hmac_key BYTEA NOT NULL,
    created BIGINT NOT NULL
);

CREATE TABLE secret_findings (
    repo_id     BIGINT NOT NULL,
    id          BIGINT NOT NULL,
    rule_id     TEXT   NOT NULL,
    severity    BIGINT NOT NULL,
    state       BIGINT NOT NULL,
    path        BYTEA  NOT NULL,
    line        BIGINT NOT NULL,
    col         BIGINT NOT NULL,
    changeset   BYTEA  NOT NULL,
    change      BYTEA  NOT NULL,
    fingerprint TEXT   NOT NULL,
    preview     TEXT   NOT NULL,
    first_seen  BIGINT NOT NULL,
    last_seen   BIGINT NOT NULL,
    resolved_by TEXT   NOT NULL DEFAULT '',
    resolved_at BIGINT NOT NULL DEFAULT 0,
    note        TEXT   NOT NULL DEFAULT '',
    PRIMARY KEY (repo_id, id)
);
CREATE UNIQUE INDEX secret_findings_by_fingerprint ON secret_findings (repo_id, fingerprint, path);
CREATE INDEX secret_findings_by_state ON secret_findings (repo_id, state, id);
