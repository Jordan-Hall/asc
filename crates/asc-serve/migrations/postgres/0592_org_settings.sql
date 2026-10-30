-- Organisation settings (docs/BUILDOUT.md section 26). `org` is the lower-case organisation name. `merge_json` holds the
-- organisation's default merge requirements (empty: none set). An organisation without a row has the defaults: agents allowed,
-- a person approves an agent's changes, no ceiling, no merge defaults.
CREATE TABLE org_settings (
    org                          TEXT   NOT NULL PRIMARY KEY,
    allow_agents                 BIGINT NOT NULL DEFAULT 1,
    agent_changesets_need_person BIGINT NOT NULL DEFAULT 1,
    agent_spend_ceiling          BIGINT NOT NULL DEFAULT 0,
    merge_json                   TEXT   NOT NULL DEFAULT '',
    updated                      BIGINT NOT NULL DEFAULT 0,
    updated_by                   TEXT   NOT NULL DEFAULT ''
);

-- The organisation's agent identities. `state` 0 active, 1 paused.
CREATE TABLE org_agents (
    org         TEXT   NOT NULL,
    name        TEXT   NOT NULL,
    display     TEXT   NOT NULL DEFAULT '',
    run_by      TEXT   NOT NULL DEFAULT '',
    paths_json  TEXT   NOT NULL DEFAULT '[]',
    repos_json  TEXT   NOT NULL DEFAULT '[]',
    state       BIGINT NOT NULL DEFAULT 0,
    created     BIGINT NOT NULL,
    created_by  TEXT   NOT NULL,
    updated     BIGINT NOT NULL,
    PRIMARY KEY (org, name)
);

-- The instance's agent switch and default budget: always the row with id 1.
CREATE TABLE instance_settings (
    id                   BIGINT NOT NULL PRIMARY KEY,
    allow_agents         BIGINT NOT NULL DEFAULT 1,
    default_agent_budget BIGINT NOT NULL DEFAULT 0,
    updated              BIGINT NOT NULL DEFAULT 0,
    updated_by           TEXT   NOT NULL DEFAULT ''
);
