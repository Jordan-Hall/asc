-- The audit log (docs/BUILDOUT.md section 3). Append only: the server inserts events and updates the
-- head of a scope; nothing updates or deletes an event except retention, which first writes a segment
-- record that keeps the hash of the last dropped event, so the chain still verifies from the first
-- event that is left. Every event is written to each scope it belongs to (repository, owner, instance)
-- in the transaction of the action. Every number is BIGINT so the sqlx Any driver reads one type on
-- both databases.
CREATE TABLE audit_events (
    scope_kind     TEXT   NOT NULL,
    scope_name     TEXT   NOT NULL,
    seq            BIGINT NOT NULL,
    id             TEXT   NOT NULL,
    time_ms        BIGINT NOT NULL,
    actor_kind     TEXT   NOT NULL,
    actor_name     TEXT   NOT NULL,
    on_behalf_kind TEXT   NOT NULL DEFAULT '',
    on_behalf_name TEXT   NOT NULL DEFAULT '',
    action         TEXT   NOT NULL,
    repo           TEXT   NOT NULL DEFAULT '',
    target         TEXT   NOT NULL DEFAULT '',
    outcome        BIGINT NOT NULL,
    request_id     TEXT   NOT NULL DEFAULT '',
    remote_addr    TEXT   NOT NULL DEFAULT '',
    user_agent     TEXT   NOT NULL DEFAULT '',
    details_json   TEXT   NOT NULL DEFAULT '{}',
    prev_hash      BYTEA   NOT NULL,
    hash           BYTEA   NOT NULL,
    PRIMARY KEY (scope_kind, scope_name, seq)
);
CREATE INDEX audit_events_by_time ON audit_events (scope_kind, scope_name, time_ms);
CREATE INDEX audit_events_by_action ON audit_events (scope_kind, scope_name, action, seq);

-- The newest sequence number and hash of a scope: the lock row appends serialise on, and the anchor
-- that makes the removal of the newest events detectable.
CREATE TABLE audit_heads (
    scope_kind TEXT   NOT NULL,
    scope_name TEXT   NOT NULL,
    seq        BIGINT NOT NULL,
    hash       BYTEA   NOT NULL,
    PRIMARY KEY (scope_kind, scope_name)
);

-- Events dropped by retention: sequence numbers 1 to `to_seq` are gone and `head_hash` is the hash of
-- the event `to_seq`. One row per scope; a later drop replaces it.
CREATE TABLE audit_segments (
    scope_kind TEXT   NOT NULL,
    scope_name TEXT   NOT NULL,
    to_seq     BIGINT NOT NULL,
    head_hash  BYTEA   NOT NULL,
    events     BIGINT NOT NULL,
    dropped_ms BIGINT NOT NULL,
    PRIMARY KEY (scope_kind, scope_name)
);
