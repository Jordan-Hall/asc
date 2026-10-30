-- Notification events for the hub (docs/BUILDOUT.md section 21): what asc serve tells the hub's people about,
-- with the retry state of the delivery. A row is delivered at least once.
CREATE TABLE notify_events (
    ns           BLOB   NOT NULL,
    id           BIGINT NOT NULL,
    event        TEXT   NOT NULL,
    users_json   TEXT   NOT NULL,
    title        TEXT   NOT NULL,
    detail       TEXT   NOT NULL DEFAULT '',
    path         TEXT   NOT NULL DEFAULT '',
    dedup        TEXT   NOT NULL DEFAULT '',
    created_at   BIGINT NOT NULL,
    state        BIGINT NOT NULL DEFAULT 0,
    attempts     BIGINT NOT NULL DEFAULT 0,
    next_at      BIGINT NOT NULL,
    last_error   TEXT   NOT NULL DEFAULT '',
    done_at      BIGINT NOT NULL DEFAULT 0,
    notified     BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (ns, id)
);
CREATE INDEX notify_events_due ON notify_events (state, next_at);
CREATE UNIQUE INDEX notify_events_dedup ON notify_events (ns, dedup) WHERE dedup <> '';
