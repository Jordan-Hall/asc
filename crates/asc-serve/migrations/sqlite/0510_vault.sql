-- The vault (docs/BUILDOUT.md, "Secrets"): background re-sealing after a master key rotation.
-- The sealed values themselves live in the tables of the features that own them.
CREATE TABLE vault_rotations (
    id            TEXT   NOT NULL PRIMARY KEY,
    to_generation BIGINT NOT NULL,
    total         BIGINT NOT NULL,
    done          BIGINT NOT NULL,
    finished      BIGINT NOT NULL DEFAULT 0,
    retire_old    BIGINT NOT NULL DEFAULT 0,
    error         TEXT   NULL,
    started       BIGINT NOT NULL,
    updated       BIGINT NOT NULL
);
CREATE INDEX vault_rotations_open ON vault_rotations (finished, updated);
