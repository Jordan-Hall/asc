-- Bring your own AI keys (docs/BUILDOUT.md section 10). `sealed_key` is the provider key sealed by the vault
-- (kind "ai-key", owner = the owner label, row = the key id); the plaintext is never stored. `owner_kind` and
-- `owner_name` are the OwnerRef of the wire (user 1, org 2, repo 3, instance 4; the instance name is empty).
-- `key_version` counts changes of the secret or of the endpoint, `rev` every change of the row (the compare-and-swap of updates). `state`: 1 untested, 2 active, 3 failed; `disabled` is the owner's switch. Every number is BIGINT so the
-- sqlx Any driver reads the same type on both databases.
CREATE TABLE ai_keys (
    id            TEXT   NOT NULL PRIMARY KEY,
    owner_kind    BIGINT NOT NULL,
    owner_name    TEXT   NOT NULL,
    label         TEXT   NOT NULL,
    provider      BIGINT NOT NULL,
    base_url      TEXT   NOT NULL DEFAULT '',
    default_model TEXT   NOT NULL DEFAULT '',
    sealed_key    BYTEA   NOT NULL,
    key_gen       BIGINT NOT NULL,
    key_hint      TEXT   NOT NULL DEFAULT '',
    key_version   BIGINT NOT NULL DEFAULT 1,
    rev           BIGINT NOT NULL DEFAULT 0,
    state         BIGINT NOT NULL,
    disabled      BIGINT NOT NULL DEFAULT 0,
    monthly_cap   BIGINT NOT NULL DEFAULT 0,
    created       BIGINT NOT NULL,
    updated       BIGINT NOT NULL,
    tested_at     BIGINT NOT NULL DEFAULT 0,
    last_error    TEXT   NOT NULL DEFAULT ''
);
CREATE UNIQUE INDEX ai_keys_by_label ON ai_keys (owner_kind, owner_name, label);
CREATE INDEX ai_keys_by_gen ON ai_keys (key_gen);

-- Usage per key, day (UTC, YYYY-MM-DD), repository, agent and model. `unpriced` counts requests whose
-- model had no entry in the price table (their cost is recorded as 0).
CREATE TABLE ai_usage (
    key_id         TEXT   NOT NULL,
    owner_kind     BIGINT NOT NULL,
    owner_name     TEXT   NOT NULL,
    day            TEXT   NOT NULL,
    repo           TEXT   NOT NULL,
    agent          TEXT   NOT NULL,
    model          TEXT   NOT NULL,
    requests       BIGINT NOT NULL DEFAULT 0,
    input_tokens   BIGINT NOT NULL DEFAULT 0,
    output_tokens  BIGINT NOT NULL DEFAULT 0,
    cost_micro_usd BIGINT NOT NULL DEFAULT 0,
    unpriced       BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (key_id, day, repo, agent, model)
);
-- The owner is kept on the row so the history survives the deletion of the key.
CREATE INDEX ai_usage_by_owner ON ai_usage (owner_kind, owner_name, day);

-- The monthly cap: `spent` is the recorded cost of the calendar month (UTC, YYYY-MM) and `reserved` the
-- worst-case cost of calls in flight, so concurrent calls cannot jointly pass the cap. `scope` is a key id,
-- or '*' for the instance cap. A reservation row exists until its call settles; a sweep returns the ones a
-- crashed process left behind.
CREATE TABLE ai_month (
    scope    TEXT   NOT NULL,
    month    TEXT   NOT NULL,
    spent    BIGINT NOT NULL DEFAULT 0,
    reserved BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (scope, month)
);
CREATE TABLE ai_reservations (
    id     TEXT   NOT NULL PRIMARY KEY,
    scope  TEXT   NOT NULL,
    month  TEXT   NOT NULL,
    amount BIGINT NOT NULL,
    at     BIGINT NOT NULL
);
CREATE INDEX ai_reservations_by_time ON ai_reservations (at);

-- Idempotency of mutating calls that name no repository (x-asc-op-id): AI keys, agent tokens and runs,
-- workflows. state 0 pending, 1 done (result kept), 2 done (result withheld: it held a secret).
CREATE TABLE service_ops (
    op_id     TEXT   NOT NULL PRIMARY KEY,
    principal TEXT   NOT NULL,
    method    TEXT   NOT NULL,
    state     BIGINT NOT NULL DEFAULT 0,
    result    BYTEA   NOT NULL,
    at        BIGINT NOT NULL
);
CREATE INDEX service_ops_by_time ON service_ops (at);
