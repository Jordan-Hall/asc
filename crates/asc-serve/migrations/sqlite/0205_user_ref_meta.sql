-- User-ref landed marker (SPEC 21.2, 21.2.4a; PLAN B08c). One row per user ref whose stack has
-- landed: the land transaction sets `landed_at`, and the expiry worker deletes the ref
-- `landed_ref_days` later. The index serves the worker; it never scans `refs`.
CREATE TABLE user_ref_meta (
    ns        BLOB   NOT NULL,
    owner     BIGINT NOT NULL,
    name      TEXT   NOT NULL,
    landed_at BIGINT NULL,
    PRIMARY KEY (ns, owner, name)
);
CREATE INDEX user_ref_meta_landed ON user_ref_meta (ns, landed_at);
