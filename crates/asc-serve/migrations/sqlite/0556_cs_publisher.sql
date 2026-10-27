-- Who first put a changeset on the server through `asc submit`, as the principal id string
-- (`user:<id>`, a token, a host-claim person). `cs_ledger.ingester` names only a numeric user, so
-- a non-user principal could not be told from nobody; this table is what makes the first version
-- of a change belong to the principal that brought its changeset. A row is written only for a
-- changeset the server did not know before the submit, and never replaced.
CREATE TABLE cs_publisher (
    ns        BLOB NOT NULL,
    cs        BLOB NOT NULL,
    principal TEXT NOT NULL,
    PRIMARY KEY (ns, cs)
);
