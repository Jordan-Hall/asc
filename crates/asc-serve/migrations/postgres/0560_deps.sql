-- Dependency scanning (docs/DEPENDENCIES.md, docs/BUILDOUT.md section 15). `deps_cache` keeps what the
-- scanner learned from outside for a time: `kind` is `osv` (the advisory ids of one package version),
-- `vuln` (one OSV record) or `latest` (the versions a registry lists for one package); `fetched` is the
-- unix time it was read, and a row older than the configured TTL is read again, never trusted.
-- `deps_findings` holds one row per (package version in a file, advisory) of a repository, per
-- repository ids allocated as max + 1 in the writing transaction (a lost race is a primary-key conflict
-- and the writer retries). `state`: 1 open, 2 dismissed, 3 fixed. `severity`: 0 not stated, 1 low,
-- 2 moderate, 3 high, 4 critical. `detail` is the advisory as JSON (aliases, summary, score, fixed
-- versions, link). `deps_scans` is the last scan of trunk: `lookup` 1 complete, 2 partial, 3 failed.
-- Every number is BIGINT so the sqlx Any driver reads one type on both databases.
CREATE TABLE deps_cache (
    kind    TEXT   NOT NULL,
    ckey    TEXT   NOT NULL,
    value   TEXT   NOT NULL,
    fetched BIGINT NOT NULL,
    PRIMARY KEY (kind, ckey)
);
CREATE INDEX deps_cache_by_time ON deps_cache (fetched);

CREATE TABLE deps_findings (
    repo_id        BIGINT NOT NULL,
    id             BIGINT NOT NULL,
    ecosystem      TEXT   NOT NULL,
    name           TEXT   NOT NULL,
    version        TEXT   NOT NULL,
    path           TEXT   NOT NULL,
    advisory       TEXT   NOT NULL,
    detail         TEXT   NOT NULL,
    severity       BIGINT NOT NULL,
    direct         BIGINT NOT NULL,
    state          BIGINT NOT NULL,
    fixed_in       TEXT   NOT NULL DEFAULT '',
    first_seen     BIGINT NOT NULL,
    last_seen      BIGINT NOT NULL,
    dismissed_by   TEXT   NOT NULL DEFAULT '',
    dismissed_at   BIGINT NOT NULL DEFAULT 0,
    dismiss_reason BIGINT NOT NULL DEFAULT 0,
    note           TEXT   NOT NULL DEFAULT '',
    PRIMARY KEY (repo_id, id)
);
CREATE UNIQUE INDEX deps_findings_by_package ON deps_findings (repo_id, ecosystem, name, version, path, advisory);
CREATE INDEX deps_findings_by_state ON deps_findings (repo_id, state, id);

CREATE TABLE deps_scans (
    repo_id  BIGINT NOT NULL PRIMARY KEY,
    at       BIGINT NOT NULL,
    rev      BYTEA  NOT NULL,
    lookup   BIGINT NOT NULL,
    packages BIGINT NOT NULL,
    unknown  BIGINT NOT NULL
);
