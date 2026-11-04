-- The tool calls an agent run made, counted as its tool-call events are appended, so the count outlives the events
-- (they are pruned after the retention window). For a run from before the column the events still kept are counted.
ALTER TABLE agent_runs ADD COLUMN tool_calls BIGINT NOT NULL DEFAULT 0;
