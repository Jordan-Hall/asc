-- Agent runs and their event stream (docs/BUILDOUT.md section 12, docs/AGENTS.md). `state` is the wire
-- AgentRunState (1 queued, 2 running, 3 waiting input, 4 succeeded, 5 failed, 6 cancelled, 7 budget exceeded,
-- 8 expired). The usage of a run is the usage of its token (`token_id`), so there is one set of counters.
-- Event text has been through the secret rules before it is stored. `heartbeat` is written every few seconds
-- by the process that runs the run; a run whose heartbeat is stale belongs to a process that died.
CREATE TABLE agent_runs (
    id                BIGINT NOT NULL PRIMARY KEY,
    repo              TEXT   NOT NULL,
    agent             TEXT   NOT NULL,
    title             TEXT   NOT NULL DEFAULT '',
    state             BIGINT NOT NULL,
    target            TEXT   NOT NULL,
    base              BLOB   NOT NULL,
    task              TEXT   NOT NULL,
    paths_json        TEXT   NOT NULL DEFAULT '[]',
    ai_key_id         TEXT   NOT NULL DEFAULT '',
    model             TEXT   NOT NULL DEFAULT '',
    limits_json       TEXT   NOT NULL DEFAULT '{}',
    changes_json      TEXT   NOT NULL DEFAULT '[]',
    ref_name          TEXT   NOT NULL DEFAULT '',
    created_by        TEXT   NOT NULL,
    starter           TEXT   NOT NULL DEFAULT '',
    owner_id          BIGINT NOT NULL DEFAULT 0,
    created           BIGINT NOT NULL,
    started           BIGINT NOT NULL DEFAULT 0,
    finished          BIGINT NOT NULL DEFAULT 0,
    failure_code      TEXT   NOT NULL DEFAULT '',
    failure_message   TEXT   NOT NULL DEFAULT '',
    token_id          TEXT   NOT NULL DEFAULT '',
    workflow_run_id   BIGINT NOT NULL DEFAULT 0,
    workflow_step     TEXT   NOT NULL DEFAULT '',
    cancel_requested  BIGINT NOT NULL DEFAULT 0,
    question          TEXT   NOT NULL DEFAULT '',
    heartbeat         BIGINT NOT NULL DEFAULT 0
);
CREATE INDEX agent_runs_by_repo ON agent_runs (repo, id);
CREATE INDEX agent_runs_by_state ON agent_runs (state, id);
CREATE INDEX agent_runs_by_workflow ON agent_runs (workflow_run_id, workflow_step);

CREATE TABLE agent_run_events (
    run_id     BIGINT NOT NULL,
    seq        BIGINT NOT NULL,
    kind       BIGINT NOT NULL,
    time_ms    BIGINT NOT NULL,
    state      BIGINT NOT NULL DEFAULT 0,
    text       TEXT   NOT NULL DEFAULT '',
    tool       TEXT   NOT NULL DEFAULT '',
    change     BLOB   NOT NULL,
    usage_json TEXT   NOT NULL DEFAULT '',
    PRIMARY KEY (run_id, seq)
);

-- Messages a person sent to a running or waiting run; the runtime takes them between steps.
CREATE TABLE agent_run_inputs (
    run_id   BIGINT NOT NULL,
    seq      BIGINT NOT NULL,
    text     TEXT   NOT NULL,
    at       BIGINT NOT NULL,
    consumed BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (run_id, seq)
);
