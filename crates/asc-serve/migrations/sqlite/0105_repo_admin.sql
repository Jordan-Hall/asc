-- RepoAdmin (SPEC 8.7; PLAN U48c). A fork is created `ready = FALSE` and becomes readable when its
-- closure copy completes; `fork_from` is the source repository while the copy is pending, and
-- `fork_lease` (unix seconds) is held by the replica running the copy. `fork_error` says why a
-- copy stopped. Existing rows stay ready.
ALTER TABLE repos ADD COLUMN ready BOOLEAN NOT NULL DEFAULT TRUE;
ALTER TABLE repos ADD COLUMN fork_from BIGINT NULL;
ALTER TABLE repos ADD COLUMN fork_lease BIGINT NULL;
ALTER TABLE repos ADD COLUMN fork_error TEXT NULL;

-- A deleted repository's row and refs are dropped at once; its namespace (objects and the
-- namespace-keyed rows) is dropped after 30 days, when `drop_after` has passed.
CREATE TABLE repo_drops (
    ns         BLOB   NOT NULL PRIMARY KEY,
    repo_id    BIGINT NOT NULL,
    name       TEXT   NOT NULL,
    deleted_at BIGINT NOT NULL,
    drop_after BIGINT NOT NULL
);
CREATE INDEX repo_drops_due ON repo_drops (drop_after);
