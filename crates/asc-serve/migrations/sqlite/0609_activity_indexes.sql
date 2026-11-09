-- The site administrator's activity figures (Usage.GetActivity): changes by the time their first version was
-- published, agent runs by the time they were created.
CREATE INDEX change_versions_first_by_at ON change_versions (at) WHERE v = 1;
CREATE INDEX agent_runs_by_created ON agent_runs (created);
