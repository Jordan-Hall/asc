-- Purged objects (SPEC 3.6, 21.3.7; PLAN B10). `Cloud.Purge` lists here the objects of a purged
-- draft; the next GC sweep deletes each one that no strict root reaches, without the 14-day age
-- rule. `key` is the object's layout path (`f/ab/abcd...`). The expiry worker prunes rows after
-- their retention. `ns` keyed, so the drop of a deleted repository clears it.
CREATE TABLE gc_purge (
    ns        BYTEA  NOT NULL,
    key       TEXT   NOT NULL,
    purged_at BIGINT NOT NULL,
    PRIMARY KEY (ns, key)
);
CREATE INDEX gc_purge_at ON gc_purge (ns, purged_at);
