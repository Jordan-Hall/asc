-- The vault's master key generation, shared by every replica (docs/BUILDOUT.md, "Secrets"). A rotation
-- moves `generation` from the value it read to the next one with a compare-and-swap before it writes the
-- key file, so two replicas that rotate at once cannot each write a different key for the same generation.
CREATE TABLE vault_state (
    id         BIGINT NOT NULL PRIMARY KEY,
    generation BIGINT NOT NULL,
    holder     TEXT   NOT NULL DEFAULT '',
    updated    BIGINT NOT NULL DEFAULT 0
);
