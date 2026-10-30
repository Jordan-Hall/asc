-- Packed objects (docs/design/PACKED-OBJECTS.md). `packs` is every pack file of a repository namespace;
-- `pack_objects` is the index: one row per packed key, naming the pack and the byte range of the stored
-- value. `state` is 1 live, 2 retired (replaced by another pack; its file is deleted after the retire age).
-- `off` is the offset of the stored bytes in the pack, after the entry header. A row is added only after
-- its pack file is durable. `store_features` records that a namespace holds data an older server cannot read.
CREATE TABLE packs (
    ns      BYTEA   NOT NULL,
    pack    BYTEA   NOT NULL,
    objects BIGINT NOT NULL,
    bytes   BIGINT NOT NULL,
    created BIGINT NOT NULL,
    state   BIGINT NOT NULL,
    retired BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (ns, pack)
);

CREATE TABLE pack_objects (
    ns    BYTEA   NOT NULL,
    class BIGINT NOT NULL,
    id    BYTEA   NOT NULL,
    pack  BYTEA   NOT NULL,
    off   BIGINT NOT NULL,
    len   BIGINT NOT NULL,
    PRIMARY KEY (ns, class, id)
);
CREATE INDEX pack_objects_by_pack ON pack_objects (ns, pack);

CREATE TABLE store_features (
    ns      BYTEA   NOT NULL,
    feature TEXT   NOT NULL,
    since   BIGINT NOT NULL,
    PRIMARY KEY (ns, feature)
);
