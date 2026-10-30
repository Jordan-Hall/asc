-- Sharing of AI keys and the warning/stop rules of their caps (docs/BUILDOUT.md section 29). `grantee_kind` and `grantee_name` are
-- the OwnerRef of the wire (user 1, org 2, repo 3). A share is deleted with its key. `ai_key_caps` is the side table of a key's warning
-- percentage and stop rule; `warned_month` (YYYY-MM) says the warning for that month has gone out.
CREATE TABLE ai_key_shares (
    id           TEXT   NOT NULL PRIMARY KEY,
    key_id       TEXT   NOT NULL,
    grantee_kind BIGINT NOT NULL,
    grantee_name TEXT   NOT NULL,
    allow_use    BIGINT NOT NULL DEFAULT 1,
    see_spend    BIGINT NOT NULL DEFAULT 0,
    created      BIGINT NOT NULL,
    created_by   TEXT   NOT NULL
);
CREATE UNIQUE INDEX ai_key_shares_by_grantee ON ai_key_shares (key_id, grantee_kind, grantee_name);
CREATE INDEX ai_key_shares_for ON ai_key_shares (grantee_kind, grantee_name);

CREATE TABLE ai_key_caps (
    key_id       TEXT   NOT NULL PRIMARY KEY,
    warn_percent BIGINT NOT NULL DEFAULT 0,
    stop_at_cap  BIGINT NOT NULL DEFAULT 0,
    warned_month TEXT   NOT NULL DEFAULT ''
);
