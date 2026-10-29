-- Releases (docs/BUILDOUT.md section 26): the record of a tagged release with its notes, its provenance and the
-- checksums of its assets. The asset bytes live where `url` says (the host, object storage); asc keeps the metadata.
CREATE TABLE releases (
    ns             BLOB   NOT NULL,
    id             BIGINT NOT NULL,
    tag            TEXT   NOT NULL,
    title          TEXT   NOT NULL,
    notes          TEXT   NOT NULL,
    channel        BIGINT NOT NULL,
    target         BLOB   NOT NULL,
    previous_tag   TEXT   NOT NULL DEFAULT '',
    changes        BIGINT NOT NULL DEFAULT 0,
    created        BIGINT NOT NULL,
    published      BIGINT NOT NULL DEFAULT 0,
    created_by     TEXT   NOT NULL,
    created_by_id  BIGINT NOT NULL DEFAULT 0,
    build_source   TEXT   NOT NULL DEFAULT '',
    build_run      TEXT   NOT NULL DEFAULT '',
    build_url      TEXT   NOT NULL DEFAULT '',
    built_from     TEXT   NOT NULL DEFAULT '',
    checks_passed  BIGINT NOT NULL DEFAULT 0,
    checks_total   BIGINT NOT NULL DEFAULT 0,
    signed_by      TEXT   NOT NULL DEFAULT '',
    signing_key    TEXT   NOT NULL DEFAULT '',
    signature_ok   BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (ns, id)
);
CREATE UNIQUE INDEX releases_by_tag ON releases (ns, tag);

CREATE TABLE release_assets (
    ns           BLOB   NOT NULL,
    release_id   BIGINT NOT NULL,
    name         TEXT   NOT NULL,
    size         BIGINT NOT NULL,
    sha256       TEXT   NOT NULL,
    url          TEXT   NOT NULL DEFAULT '',
    content_type TEXT   NOT NULL DEFAULT '',
    PRIMARY KEY (ns, release_id, name)
);
