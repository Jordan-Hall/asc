-- The merge queue (docs/BUILDOUT.md section 7): entries, the pause per target, and the event log that Watch follows.
CREATE TABLE queue_entries (
    ns             BLOB   NOT NULL,
    id             BIGINT NOT NULL,
    target         TEXT   NOT NULL,
    change         BLOB   NOT NULL,
    stack_json     TEXT   NOT NULL,
    state          BIGINT NOT NULL,
    style          BIGINT NOT NULL DEFAULT 0,
    enqueued_by    TEXT   NOT NULL,
    enqueued_at    BIGINT NOT NULL,
    reval_json     TEXT   NOT NULL DEFAULT '',
    reval_at       BIGINT NOT NULL DEFAULT 0,
    started_at     BIGINT NOT NULL DEFAULT 0,
    landed_as      BLOB   NOT NULL,
    landed_at      BIGINT NOT NULL DEFAULT 0,
    removed_reason TEXT   NOT NULL DEFAULT '',
    stage          TEXT   NOT NULL DEFAULT '',
    progress       BIGINT NOT NULL DEFAULT 0,
    detail         TEXT   NOT NULL DEFAULT '',
    PRIMARY KEY (ns, id)
);
CREATE INDEX queue_entries_live ON queue_entries (ns, target, state, id);

CREATE TABLE queue_pause (
    ns     BLOB   NOT NULL,
    target TEXT   NOT NULL,
    paused BIGINT NOT NULL,
    reason TEXT   NOT NULL DEFAULT '',
    by     TEXT   NOT NULL DEFAULT '',
    at     BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (ns, target)
);

CREATE TABLE queue_events (
    ns       BLOB   NOT NULL,
    seq      BIGINT NOT NULL,
    entry_id BIGINT NOT NULL,
    at       BIGINT NOT NULL,
    PRIMARY KEY (ns, seq)
);
