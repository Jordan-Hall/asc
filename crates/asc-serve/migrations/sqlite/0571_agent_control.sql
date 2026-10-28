-- Control of a live agent run and the data of the run boards (docs/BUILDOUT.md section 18): permission and plan requests with
-- their decisions, the approved plan, pause and progress, checkpoints, and spend per day for monthly budgets.
CREATE TABLE agent_run_requests (
    id           BIGINT NOT NULL PRIMARY KEY,
    run_id       BIGINT NOT NULL,
    repo         TEXT   NOT NULL,
    agent        TEXT   NOT NULL DEFAULT '',
    kind         BIGINT NOT NULL,
    summary      TEXT   NOT NULL,
    detail       TEXT   NOT NULL DEFAULT '',
    state        BIGINT NOT NULL,
    requested_at BIGINT NOT NULL,
    decided_by   TEXT   NOT NULL DEFAULT '',
    decided_at   BIGINT NOT NULL DEFAULT 0,
    reason       TEXT   NOT NULL DEFAULT ''
);
CREATE INDEX agent_run_requests_by_run ON agent_run_requests (run_id, id);
CREATE INDEX agent_run_requests_by_repo ON agent_run_requests (repo, id);

CREATE TABLE agent_run_plan (
    run_id BIGINT NOT NULL,
    idx    BIGINT NOT NULL,
    text   TEXT   NOT NULL,
    state  BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (run_id, idx)
);

CREATE TABLE agent_run_ctl (
    run_id      BIGINT NOT NULL PRIMARY KEY,
    paused      BIGINT NOT NULL DEFAULT 0,
    paused_by   TEXT   NOT NULL DEFAULT '',
    plan_first  BIGINT NOT NULL DEFAULT 0,
    plan_ok     BIGINT NOT NULL DEFAULT 0,
    steps_done  BIGINT NOT NULL DEFAULT 0,
    for_change  BLOB   NOT NULL,
    continues   BIGINT NOT NULL DEFAULT 0,
    rollback_to BIGINT NOT NULL DEFAULT 0
);

CREATE TABLE agent_checkpoints (
    id              BIGINT NOT NULL PRIMARY KEY,
    run_id          BIGINT NOT NULL,
    seq             BIGINT NOT NULL,
    at              BIGINT NOT NULL,
    label           TEXT   NOT NULL,
    changes_json    TEXT   NOT NULL,
    tip             BLOB   NOT NULL,
    rolled_back_at  BIGINT NOT NULL DEFAULT 0,
    rolled_back_by  TEXT   NOT NULL DEFAULT '',
    rollback_reason TEXT   NOT NULL DEFAULT ''
);
CREATE INDEX agent_checkpoints_by_run ON agent_checkpoints (run_id, seq);

CREATE TABLE agent_spend (
    repo     TEXT   NOT NULL,
    day      TEXT   NOT NULL,
    agent    TEXT   NOT NULL,
    tokens   BIGINT NOT NULL DEFAULT 0,
    cost     BIGINT NOT NULL DEFAULT 0,
    requests BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (repo, day, agent)
);

CREATE TABLE agent_monthly_budget (
    repo   TEXT   NOT NULL,
    agent  TEXT   NOT NULL,
    cap    BIGINT NOT NULL,
    set_by TEXT   NOT NULL,
    set_at BIGINT NOT NULL,
    PRIMARY KEY (repo, agent)
);
