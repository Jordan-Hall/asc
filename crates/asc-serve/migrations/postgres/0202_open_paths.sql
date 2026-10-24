-- Overlaps and mergeability of open changes (SPEC 20.4).
-- open_paths: the path keys (16 bytes of blake3 of the casefolded path, and of each ancestor
-- directory) an open change's head touches. One row per (path, change).
CREATE TABLE open_paths (
    ns       BYTEA   NOT NULL,
    path_key BYTEA   NOT NULL,
    change   BYTEA   NOT NULL,
    PRIMARY KEY (ns, path_key, change)
);
CREATE INDEX open_paths_by_change ON open_paths (ns, change);

-- mergeable: one overwritten row per head changeset; it cannot grow with trunk history.
CREATE TABLE mergeable (
    ns          BYTEA     NOT NULL,
    head        BYTEA     NOT NULL,
    trunk_tip   BYTEA     NOT NULL,
    state       SMALLINT NOT NULL,
    conflicts   BYTEA     NOT NULL,
    computed_at BIGINT NOT NULL,
    PRIMARY KEY (ns, head)
);
