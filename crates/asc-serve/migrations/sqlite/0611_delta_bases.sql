-- Delta storage at rest (docs/design/DELTA-STORAGE.md). A row says the loose copy of `file` may be
-- a delta against `base`: GC keeps every `base` named here as a strict root, and the sweep
-- removes the row of a file it deletes. `delta_cursor` is how far the delta pass has read each
-- land branch's mainline. Both are `ns` keyed, so the drop of a deleted repository clears them.
CREATE TABLE delta_bases (
    ns   BLOB   NOT NULL,
    file BLOB   NOT NULL,
    base BLOB   NOT NULL,
    PRIMARY KEY (ns, file)
);
CREATE INDEX delta_bases_by_base ON delta_bases (ns, base);

CREATE TABLE delta_cursor (
    ns  BLOB     NOT NULL,
    ref TEXT   NOT NULL,
    pos BIGINT NOT NULL,
    PRIMARY KEY (ns, ref)
);
