-- Where a person's worktrees are (SPEC 21.3.9): one row per worktree a device last reported for a
-- repository. `ws_hash` is an opaque hash the device derives from the worktree: no local path, file name or
-- content is stored. `head` is the native changeset id or empty, `dirty` 0 or 1, `ephemeral_until` unix
-- seconds (0 for a worktree that stays) and `reported_at` the time of the report. A report replaces every
-- row of its (ns, owner, device); at most 8 devices are kept per owner and repository, and a row older than
-- 7 days is dropped.
CREATE TABLE cloud_worktrees (
    ns              BYTEA   NOT NULL,
    owner           BIGINT NOT NULL,
    device          TEXT   NOT NULL,
    ws_hash         BYTEA   NOT NULL,
    head_ref        TEXT   NOT NULL,
    head            BYTEA   NOT NULL,
    mode            TEXT   NOT NULL,
    dirty           BIGINT NOT NULL,
    ahead           BIGINT NOT NULL,
    behind          BIGINT NOT NULL,
    ephemeral_until BIGINT NOT NULL,
    in_progress     TEXT   NOT NULL,
    reported_at     BIGINT NOT NULL,
    PRIMARY KEY (ns, owner, device, ws_hash)
);
CREATE INDEX cloud_worktrees_by_reported ON cloud_worktrees (ns, reported_at);
