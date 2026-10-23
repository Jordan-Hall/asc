-- The commit cloud (SPEC 21.2.3, PLAN B09). A workspace is one user's set of draft heads in one
-- repository; `version` is the compare-and-swap counter of `Cloud.UpdateWorkspace`. `heads` counts
-- the rows of `cloud_heads` so a listing and the heads cap never count rows (additive to SPEC
-- 21.2.3). `change` is derived by the server from the `m/` object of `cs` and is never client
-- supplied; `labels` is a validated JSON array, display text only. `hidden_at` is NULL while a head
-- is visible. `cloud_usage` is the quota ledger of SPEC 21.3.7: `bytes` charged by ingest and
-- `workspaces` created, both moved only by conditional upserts so a cap holds under concurrency.
CREATE TABLE cloud_ws (
    ns      BYTEA  NOT NULL,
    owner   BIGINT NOT NULL,
    name    TEXT   NOT NULL,
    version BIGINT NOT NULL,
    updated BIGINT NOT NULL,
    heads   BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (ns, owner, name)
);

CREATE TABLE cloud_heads (
    ns        BYTEA  NOT NULL,
    owner     BIGINT NOT NULL,
    name      TEXT   NOT NULL,
    cs        BYTEA  NOT NULL,
    change    BYTEA  NOT NULL,
    labels    TEXT   NOT NULL,
    added     BIGINT NOT NULL,
    hidden_at BIGINT NULL,
    PRIMARY KEY (ns, owner, name, cs)
);
CREATE INDEX cloud_heads_hidden ON cloud_heads (ns, hidden_at);
CREATE INDEX cloud_heads_added ON cloud_heads (ns, added);

CREATE TABLE cloud_usage (
    ns         BYTEA  NOT NULL,
    owner      BIGINT NOT NULL,
    bytes      BIGINT NOT NULL DEFAULT 0,
    workspaces INT    NOT NULL DEFAULT 0,
    PRIMARY KEY (ns, owner)
);
