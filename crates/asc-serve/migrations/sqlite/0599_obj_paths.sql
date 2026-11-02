-- objpaths.v1 (SPEC §15.5.1): every path a tree (kind 0) or file (kind 1) occurs at in a
-- repository's history, so `GetObjects` serves an object to a restricted caller only when the
-- caller can read one of its paths. `obj_paths_done` lists the changesets already walked; a
-- tree's row is written after everything below it, so a row means its subtree is indexed.
CREATE TABLE obj_paths (
    ns BLOB NOT NULL,
    kind SMALLINT NOT NULL,
    obj BLOB NOT NULL,
    path BLOB NOT NULL,
    PRIMARY KEY (ns, kind, obj, path)
);
CREATE TABLE obj_paths_done (
    ns BLOB NOT NULL,
    cs BLOB NOT NULL,
    PRIMARY KEY (ns, cs)
);
