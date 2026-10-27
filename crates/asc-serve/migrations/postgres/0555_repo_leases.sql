-- Short leases on a repository's long operations (a restack), so two instances on one database
-- cannot run the same operation on the same repository at once. A lease is a row; it is held while
-- `expires` is in the future and the holder renews it; a holder that dies is replaced once it passes.
CREATE TABLE repo_leases (
    ns      BYTEA   NOT NULL,
    name    TEXT   NOT NULL,
    holder  TEXT   NOT NULL,
    expires BIGINT NOT NULL,
    PRIMARY KEY (ns, name)
);
