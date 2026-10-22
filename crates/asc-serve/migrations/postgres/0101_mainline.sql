-- Mainline and path index of the land branches (SPEC 5.1).
CREATE TABLE mainline (
    ns  BYTEA  NOT NULL,
    ref TEXT   NOT NULL,
    pos BIGINT NOT NULL,
    cs  BYTEA  NOT NULL,
    PRIMARY KEY (ns, ref, pos)
);
CREATE UNIQUE INDEX mainline_by_cs ON mainline (ns, ref, cs);

CREATE TABLE path_changes (
    ns       BYTEA  NOT NULL,
    ref      TEXT   NOT NULL,
    path_key BYTEA  NOT NULL,
    pos      BIGINT NOT NULL,
    PRIMARY KEY (ns, ref, path_key, pos)
);
