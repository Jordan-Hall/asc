-- Storage usage of a repository (docs/BYOS.md, usage). `storage_usage` is the last scan of a namespace, one
-- row per kind of stored bytes (1 history, 2 files, 3 large files, 4 indexes, 5 CI, 6 other), all with the
-- scan's time. `storage_reads` is the bytes read from a namespace per UTC month (YYYY-MM).
CREATE TABLE storage_usage (
    ns          BLOB NOT NULL,
    kind        BIGINT NOT NULL,
    bytes       BIGINT NOT NULL,
    objects     BIGINT NOT NULL,
    computed_at BIGINT NOT NULL,
    PRIMARY KEY (ns, kind)
);

CREATE TABLE storage_reads (
    ns    BLOB NOT NULL,
    month TEXT NOT NULL,
    bytes BIGINT NOT NULL,
    PRIMARY KEY (ns, month)
);
