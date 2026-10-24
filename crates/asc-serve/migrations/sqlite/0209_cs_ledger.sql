-- Byte ledger of ingested changesets (SPEC 21.3.7; PLAN B08b). `cs_ledger` has one row per
-- changeset an ingest or import added: its logical cost, the verified owner id of the principal
-- that ingested it (0 for none), and `shared` once a shared ref made it reachable (shared history
-- costs a user nothing). `cs_claim` records that a user already paid for a changeset, so a user
-- ref is charged exactly once for what it newly makes reachable, whether the ingest was its own
-- or an earlier dangling one. A changeset with no ledger row predates this table (or was written
-- by land or mirror) and is free.
CREATE TABLE cs_ledger (
    ns       BLOB     NOT NULL,
    cs       BLOB     NOT NULL,
    bytes    BIGINT NOT NULL,
    ingester BIGINT NOT NULL,
    shared   SMALLINT NOT NULL DEFAULT 0,
    PRIMARY KEY (ns, cs)
);

CREATE TABLE cs_claim (
    ns    BLOB     NOT NULL,
    owner BIGINT NOT NULL,
    cs    BLOB     NOT NULL,
    PRIMARY KEY (ns, owner, cs)
);
