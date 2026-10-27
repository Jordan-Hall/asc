-- Signed changes (docs/BUILDOUT.md section 5). `signing_keys` holds public keys only: `public_key` is one
-- `<algorithm> <base64>` line without options or comment, `fingerprint` is `SHA256:...` as ssh-keygen
-- prints it and is unique across all owners, so a key maps to one user. A revoked key keeps its row so
-- old signatures stay explainable. `change_signatures` keeps the armored SSHSIG next to the changeset
-- (an id never changes), one per (repository, changeset); `key_id` is a signing key id or `server` for a
-- signature made with the server's own key. `status` is the verdict at submission; the server derives
-- the current verdict from the stored signature, the key row and `signed_at` whenever it is asked.
-- Every number is BIGINT so the sqlx Any driver reads one type on both databases.
CREATE TABLE signing_keys (
    id             TEXT   NOT NULL PRIMARY KEY,
    owner          TEXT   NOT NULL,
    title          TEXT   NOT NULL,
    kind           BIGINT NOT NULL,
    fingerprint    TEXT   NOT NULL UNIQUE,
    public_key     TEXT   NOT NULL,
    created        BIGINT NOT NULL,
    expires_at     BIGINT NOT NULL DEFAULT 0,
    revoked_at     BIGINT NOT NULL DEFAULT 0,
    revoked_reason TEXT   NOT NULL DEFAULT '',
    last_used      BIGINT NOT NULL DEFAULT 0
);
CREATE INDEX signing_keys_by_owner ON signing_keys (owner, created);

CREATE TABLE change_signatures (
    repo_id   BIGINT NOT NULL,
    changeset BYTEA  NOT NULL,
    signature TEXT   NOT NULL,
    key_id    TEXT   NOT NULL,
    status    BIGINT NOT NULL,
    signed_at BIGINT NOT NULL,
    PRIMARY KEY (repo_id, changeset)
);
