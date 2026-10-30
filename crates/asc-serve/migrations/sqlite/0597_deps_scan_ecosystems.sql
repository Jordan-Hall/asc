-- `by_ecosystem` is the per-ecosystem package count of the last scan of trunk as a JSON object (OSV
-- ecosystem name to the number of distinct packages checked); empty for a scan recorded before this column.
ALTER TABLE deps_scans ADD COLUMN by_ecosystem TEXT NOT NULL DEFAULT '';
