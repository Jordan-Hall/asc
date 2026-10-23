-- Stacked changes (SPEC 20.4): the change index, the versions of each change (GC roots, never in
-- ListRefs) and the review summary Forgejo writes.
CREATE TABLE changes (
    ns     BYTEA     NOT NULL,
    cs     BYTEA     NOT NULL,
    change BYTEA     NOT NULL,
    meta   BYTEA     NOT NULL,
    op     SMALLINT NOT NULL,
    preds  BYTEA     NOT NULL,
    PRIMARY KEY (ns, cs)
);
CREATE INDEX changes_by_change ON changes (ns, change);

CREATE TABLE change_versions (
    ns            BYTEA     NOT NULL,
    change        BYTEA     NOT NULL,
    v             INT    NOT NULL,
    cs            BYTEA     NOT NULL,
    parent_change BYTEA     NULL,
    at            BIGINT NOT NULL,
    actor         TEXT   NOT NULL,
    op_id         BYTEA     NOT NULL,
    PRIMARY KEY (ns, change, v)
);
CREATE INDEX change_versions_by_cs ON change_versions (ns, cs);

CREATE TABLE review_state (
    ns      BYTEA     NOT NULL,
    change  BYTEA     NOT NULL,
    state   BYTEA     NOT NULL,
    updated BIGINT NOT NULL,
    PRIMARY KEY (ns, change)
);
