-- The server processes that execute agent runs, for the site administrator's runners page. Each process upserts its
-- row every few seconds (`heartbeat`, unix seconds); a row whose heartbeat is older than 90 seconds is offline, and
-- one offline for 7 days is removed. `slots` is agents.max_concurrent_runs, `vcpu` the CPUs the process may use.
-- `agent_runs.runner` names the process that took the run.
CREATE TABLE agent_runners (
    name      TEXT   NOT NULL PRIMARY KEY,
    slots     BIGINT NOT NULL,
    vcpu      BIGINT NOT NULL,
    version   TEXT   NOT NULL,
    started   BIGINT NOT NULL,
    heartbeat BIGINT NOT NULL
);
ALTER TABLE agent_runs ADD COLUMN runner TEXT NOT NULL DEFAULT '';
CREATE INDEX agent_runs_by_runner ON agent_runs (runner, state);
