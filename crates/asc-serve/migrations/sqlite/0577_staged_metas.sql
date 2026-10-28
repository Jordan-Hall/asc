-- Change metas (`m/` objects) a client uploaded and the server has not yet indexed. A meta names
-- no changeset the server could find it by, so `PutObjects` records here, for the uploading
-- principal, which changeset each uploaded meta claims. The ingest of that principal's push
-- consumes the rows of the changesets it adds (an `m/` of the push closure, SPEC section 20.2);
-- a row of another principal, or of a changeset that is not part of the push, is never used.
-- Rows older than a day are removed whenever a row is staged.
CREATE TABLE staged_metas (
    ns        BLOB NOT NULL,
    principal TEXT NOT NULL,
    cs        BLOB NOT NULL,
    meta      BLOB NOT NULL,
    staged_at BIGINT NOT NULL,
    PRIMARY KEY (ns, principal, cs, meta)
);
CREATE INDEX staged_metas_at ON staged_metas (staged_at);
