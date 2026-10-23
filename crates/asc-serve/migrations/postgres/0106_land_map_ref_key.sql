-- Landing map keyed by ref (security fix on U66): a rewrite recorded on one ref (a user ref, say)
-- must not poison or block the mapping of the same changeset on another ref. The key is
-- (ns, ref, old_cs) instead of (ns, old_cs). Expand-only (SPEC 8.2): this adds land_map_ref and
-- copies nothing (an INSERT ... SELECT is not expand-only). The server writes land_map_ref alone
-- and reads land_map (rows of N-1 servers, filtered by ref) before it. The old land_map stays until
-- a later contract migration backfills it into land_map_ref and drops it (docs/PLAN.md, U66).
CREATE TABLE land_map_ref (
    ns          BYTEA  NOT NULL,
    ref         TEXT   NOT NULL,
    old_cs      BYTEA  NOT NULL,
    new_cs      BYTEA  NOT NULL,
    ref_log_seq BIGINT NOT NULL,
    op_id       BYTEA  NOT NULL,
    PRIMARY KEY (ns, ref, old_cs)
);
CREATE INDEX land_map_ref_new ON land_map_ref (ns, new_cs);
