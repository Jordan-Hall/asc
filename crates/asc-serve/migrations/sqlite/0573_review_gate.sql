-- Gate history and maintainer overrides (docs/BUILDOUT.md section 19).
CREATE TABLE gate_decisions (
    ns           BLOB   NOT NULL,
    change       BLOB   NOT NULL,
    version      BIGINT NOT NULL,
    first_at     BIGINT NOT NULL,
    at           BIGINT NOT NULL,
    first_passed BIGINT NOT NULL,
    passed       BIGINT NOT NULL,
    cleared_at   BIGINT NOT NULL DEFAULT 0,
    rules_json   TEXT   NOT NULL DEFAULT '[]',
    PRIMARY KEY (ns, change, version)
);
CREATE INDEX gate_decisions_by_time ON gate_decisions (ns, first_at);

CREATE TABLE gate_overrides (
    ns         BLOB   NOT NULL,
    id         BIGINT NOT NULL,
    change     BLOB   NOT NULL,
    rule       TEXT   NOT NULL,
    reason     TEXT   NOT NULL,
    granted_by TEXT   NOT NULL,
    granted_at BIGINT NOT NULL,
    revoked_by TEXT   NOT NULL DEFAULT '',
    revoked_at BIGINT NOT NULL DEFAULT 0,
    prev       BLOB   NOT NULL,
    proof      BLOB   NOT NULL,
    PRIMARY KEY (ns, id)
);
CREATE INDEX gate_overrides_by_change ON gate_overrides (ns, change, id);
