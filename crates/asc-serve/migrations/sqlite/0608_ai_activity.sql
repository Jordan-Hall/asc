-- Who used an AI key and for what: one row per key and use (an agent run, one person's review summaries of one
-- change, the tests of one day), summed over its calls. `kind`: 1 agent run, 2 review summary, 3 key test, 4 other.
-- `subject` is the run id, the change id or the day; `actor` the handle of the person who started or asked.
-- Rows are dropped 90 days after their last call.
CREATE TABLE ai_activity (
    key_id         TEXT   NOT NULL,
    owner_kind     BIGINT NOT NULL,
    owner_name     TEXT   NOT NULL,
    kind           BIGINT NOT NULL,
    subject        TEXT   NOT NULL,
    actor          TEXT   NOT NULL,
    repo           TEXT   NOT NULL,
    agent          TEXT   NOT NULL,
    model          TEXT   NOT NULL,
    first_at       BIGINT NOT NULL,
    last_at        BIGINT NOT NULL,
    requests       BIGINT NOT NULL DEFAULT 0,
    input_tokens   BIGINT NOT NULL DEFAULT 0,
    output_tokens  BIGINT NOT NULL DEFAULT 0,
    cost_micro_usd BIGINT NOT NULL DEFAULT 0,
    unpriced       BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (key_id, kind, subject, actor)
);
CREATE INDEX ai_activity_by_key ON ai_activity (key_id, last_at);
CREATE INDEX ai_activity_by_actor ON ai_activity (actor);
