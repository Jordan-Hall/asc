-- Stacked changes (SPEC 20.4): the change index, the versions of each change (GC roots, never in
-- ListRefs) and the review summary Forgejo writes.
CREATE TABLE changes (
    ns     BLOB     NOT NULL,
    cs     BLOB     NOT NULL,
    change BLOB     NOT NULL,
    meta   BLOB     NOT NULL,
    op     SMALLINT NOT NULL,
    preds  BLOB     NOT NULL,
    PRIMARY KEY (ns, cs)
);
CREATE INDEX changes_by_change ON changes (ns, change);

CREATE TABLE change_versions (
    ns            BLOB     NOT NULL,
    change        BLOB     NOT NULL,
    v             INT    NOT NULL,
    cs            BLOB     NOT NULL,
    parent_change BLOB     NULL,
    at            BIGINT NOT NULL,
    actor         TEXT   NOT NULL,
    op_id         BLOB     NOT NULL,
    PRIMARY KEY (ns, change, v)
);
CREATE INDEX change_versions_by_cs ON change_versions (ns, cs);

CREATE TABLE review_state (
    ns      BLOB     NOT NULL,
    change  BLOB     NOT NULL,
    state   BLOB     NOT NULL,
    updated BIGINT NOT NULL,
    PRIMARY KEY (ns, change)
);
