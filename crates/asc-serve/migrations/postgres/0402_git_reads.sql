-- Host-neutral name for the per-repo read switch (`git` or `asc`). Expand only: `forgejo_reads` stays so
-- an older server keeps reading and writing it during a rolling deploy; a later release drops it.
-- The new column is NULL until a writer sets it; readers use COALESCE(git_reads, forgejo_reads), and new
-- writers set both columns.
ALTER TABLE repos ADD COLUMN git_reads TEXT;
