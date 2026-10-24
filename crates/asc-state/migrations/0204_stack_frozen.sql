-- Locally frozen changes (SPEC 20.3): never rewritten by a local stack verb. `change` is the
-- 16-byte change id, so a freeze survives a restack that would mint a new changeset.
CREATE TABLE stack_frozen (
    repo_id BIGINT NOT NULL,
    change  BLOB   NOT NULL,
    PRIMARY KEY (repo_id, change)
);
