-- SSH public keys for the standalone token store (SPEC 15.6.2, 25.11.3; PLAN U84). Only public
-- material is stored: `public_key` is the canonical wire blob and `fingerprint` its SHA-256, the
-- key's identity. `kind` is `user` (`principal` names the user) or `deploy` (`repo_id` is the one
-- repository `owner/name`, `write` lets it push). The server rebuilds the principal from `kind`,
-- `principal` and `repo_id`; it never trusts the stored text as an identity.
CREATE TABLE ssh_keys (
    id          TEXT   PRIMARY KEY,
    kind        TEXT   NOT NULL,
    principal   TEXT   NOT NULL,
    repo_id     TEXT,
    "write"     BIGINT NOT NULL DEFAULT 0,
    algo        TEXT   NOT NULL,
    public_key  BLOB   NOT NULL,
    fingerprint BLOB   NOT NULL UNIQUE,
    title       TEXT   NOT NULL DEFAULT '',
    expires     BIGINT,
    revoked     BIGINT,
    created     BIGINT NOT NULL,
    last_used   BIGINT
);

CREATE INDEX ssh_keys_principal ON ssh_keys (principal);
