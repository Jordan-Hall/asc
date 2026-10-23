-- Landing map keyed by ref (security fix on U66): a rewrite recorded on one ref (a user ref, say)
-- must not poison or block the mapping of the same changeset on another ref. The key moves from
-- (ns, old_cs) to (ns, ref, old_cs); every existing row carries over with its stored `ref`.
CREATE TABLE land_map_v2 (
    ns          BLOB   NOT NULL,
    old_cs      BLOB   NOT NULL,
    new_cs      BLOB   NOT NULL,
    ref         TEXT   NOT NULL,
    ref_log_seq BIGINT NOT NULL,
    op_id       BLOB   NOT NULL,
    PRIMARY KEY (ns, ref, old_cs)
);
INSERT INTO land_map_v2 (ns, old_cs, new_cs, ref, ref_log_seq, op_id)
    SELECT ns, old_cs, new_cs, ref, ref_log_seq, op_id FROM land_map;
DROP TABLE land_map;
ALTER TABLE land_map_v2 RENAME TO land_map;
CREATE INDEX land_map_new ON land_map (ns, new_cs);
