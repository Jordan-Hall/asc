-- Agent tokens and their budgets (docs/BUILDOUT.md section 11, docs/AGENTS.md). The secret is never stored:
-- `secret_sha256` is the hex SHA-256 of the `ascat_` string, found by a unique index and compared in constant
-- time. A token always expires. `lim_*` are the budget limits (0: none at this level; the policy and the
-- server defaults were merged in when the token was made) and `used_*` the counters, changed only by
-- conditional UPDATEs so concurrent callers can never pass a limit. `owner_id` is the id the agent's refs
-- live under (refs/users/<owner_id>/heads/agent/...); `run_id` is the run the token was made for, 0 for a
-- token a person made.
CREATE TABLE agent_tokens (
    id                  TEXT   NOT NULL PRIMARY KEY,
    repo                TEXT   NOT NULL,
    name                TEXT   NOT NULL,
    agent               TEXT   NOT NULL,
    secret_sha256       TEXT   NOT NULL,
    secret_hint         TEXT   NOT NULL,
    paths_json          TEXT   NOT NULL DEFAULT '[]',
    max_changes_per_run BIGINT NOT NULL DEFAULT 0,
    created_by          TEXT   NOT NULL,
    owner_id            BIGINT NOT NULL,
    created             BIGINT NOT NULL,
    expires_at          BIGINT NOT NULL,
    revoked_at          BIGINT NOT NULL DEFAULT 0,
    revoked_reason      TEXT   NOT NULL DEFAULT '',
    last_used           BIGINT NOT NULL DEFAULT 0,
    lim_changes         BIGINT NOT NULL DEFAULT 0,
    lim_tokens          BIGINT NOT NULL DEFAULT 0,
    lim_cost            BIGINT NOT NULL DEFAULT 0,
    lim_paths           BIGINT NOT NULL DEFAULT 0,
    lim_runtime         BIGINT NOT NULL DEFAULT 0,
    used_changes        BIGINT NOT NULL DEFAULT 0,
    used_tokens         BIGINT NOT NULL DEFAULT 0,
    used_cost           BIGINT NOT NULL DEFAULT 0,
    used_paths          BIGINT NOT NULL DEFAULT 0,
    used_requests       BIGINT NOT NULL DEFAULT 0,
    exhausted           BIGINT NOT NULL DEFAULT 0,
    run_id              BIGINT NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX agent_tokens_by_secret ON agent_tokens (secret_sha256);
CREATE INDEX agent_tokens_by_repo ON agent_tokens (repo, created);

-- The distinct paths a token wrote, for the paths budget.
CREATE TABLE agent_token_paths (
    token_id TEXT NOT NULL,
    path     TEXT NOT NULL,
    PRIMARY KEY (token_id, path)
);
