-- Landing map (SPEC 8.9): every changeset a land or a Merge rewrote, old -> new.
CREATE TABLE land_map (
    ns          BYTEA  NOT NULL,
    old_cs      BYTEA  NOT NULL,
    new_cs      BYTEA  NOT NULL,
    ref         TEXT   NOT NULL,
    ref_log_seq BIGINT NOT NULL,
    op_id       BYTEA  NOT NULL,
    PRIMARY KEY (ns, old_cs)
);
CREATE INDEX land_map_new ON land_map (ns, new_cs);
