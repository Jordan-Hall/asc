-- Usage counters and quotas (docs/BUILDOUT.md section 25). `usage_counters` holds, per repository and UTC day, the storage
-- gauge (the last report of the day) and the bandwidth the server served (added). `byo_storage` marks a repository on storage its
-- owner brought, which does not count against a quota. Rows older than `[usage] retention_days` are deleted by a sweep.
-- AI spend and agent minutes are not counted here: they are read from `ai_usage`, `agent_spend` and `agent_runs`.
CREATE TABLE usage_counters (
    repo            TEXT   NOT NULL,
    day             TEXT   NOT NULL,
    storage_bytes   BIGINT NOT NULL DEFAULT 0,
    bandwidth_bytes BIGINT NOT NULL DEFAULT 0,
    byo_storage     BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (repo, day)
);
CREATE INDEX usage_counters_by_day ON usage_counters (day);

-- The limits of an owner; `scope` is `user:<login>`, `org:<name>` or `instance` (the default). 0 = inherits (unlimited at the instance).
CREATE TABLE usage_quotas (
    scope              TEXT   NOT NULL PRIMARY KEY,
    storage_bytes      BIGINT NOT NULL DEFAULT 0,
    agent_minutes      BIGINT NOT NULL DEFAULT 0,
    ai_spend_micro_usd BIGINT NOT NULL DEFAULT 0,
    bandwidth_bytes    BIGINT NOT NULL DEFAULT 0,
    updated            BIGINT NOT NULL DEFAULT 0,
    updated_by         TEXT   NOT NULL DEFAULT ''
);
