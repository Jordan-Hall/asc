-- The standalone token store (SPEC 15.1, PLAN M30). The token itself is never stored: `digest`
-- is HMAC-SHA256 under the server key. `id` is the public half of the token.
CREATE TABLE auth_tokens (
    id        TEXT   PRIMARY KEY,
    kind      TEXT   NOT NULL,
    principal TEXT   NOT NULL,
    digest    BLOB  NOT NULL,
    label     TEXT   NOT NULL DEFAULT '',
    created   BIGINT NOT NULL,
    expires   BIGINT,
    revoked   BIGINT
);

-- Per-repository grants: `repo` is owner/name, `owner/*` or `*`; `access` is read, write or admin.
CREATE TABLE auth_token_grants (
    token_id TEXT NOT NULL,
    repo     TEXT NOT NULL,
    access   TEXT NOT NULL,
    PRIMARY KEY (token_id, repo)
);

-- An OIDC subject linked to a principal (M33).
CREATE TABLE auth_identity (
    issuer    TEXT NOT NULL,
    subject   TEXT NOT NULL,
    principal TEXT NOT NULL,
    PRIMARY KEY (issuer, subject)
);
