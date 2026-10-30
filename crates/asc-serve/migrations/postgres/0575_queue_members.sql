-- A change sits in at most one live queue entry of a target: the unique key is what makes two concurrent
-- enqueues of the same change lose one of them, whatever the isolation level of the database.
CREATE TABLE queue_members (
    ns       BLOB   NOT NULL,
    target   TEXT   NOT NULL,
    change   BLOB   NOT NULL,
    entry_id BIGINT NOT NULL,
    PRIMARY KEY (ns, target, change)
);
CREATE INDEX queue_members_by_entry ON queue_members (ns, entry_id);
