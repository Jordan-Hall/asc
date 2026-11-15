-- Security status notices (docs/DEPENDENCIES.md "Notifications"). `status` is the last status of the repository
-- that was notified or taken as the baseline (0 none yet, 1 unknown, 2 clean, 3 findings); `unknown_runs` counts
-- the consecutive scans that came out unknown (an unknown is told only after two). `notify_events.watchers` is 1
-- for an event the hub sends to the watchers of the repository (its users list is then empty).
ALTER TABLE deps_scans ADD COLUMN status BIGINT NOT NULL DEFAULT 0;
ALTER TABLE deps_scans ADD COLUMN unknown_runs BIGINT NOT NULL DEFAULT 0;
ALTER TABLE notify_events ADD COLUMN watchers BIGINT NOT NULL DEFAULT 0;
