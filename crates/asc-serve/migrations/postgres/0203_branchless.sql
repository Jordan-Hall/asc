-- Branchless repositories and the commit cloud (SPEC 21.5.1, PLAN B08a). Existing repositories
-- stay in branch mode (`branchless = FALSE`) until `asc serve admin branchless enable` (B18) or a
-- RepoAdmin setting turns the policy on. `shared_refs` is a JSON array of ref globs; NULL means
-- the default set (default branch, release/*, green/*, tags). `legacy_shared` is a JSON array of
-- branch names kept shared during a migration. `git_import_grade` is the SPEC 25.6 window.
ALTER TABLE repos ADD COLUMN branchless BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE repos ADD COLUMN cloud BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE repos ADD COLUMN shared_refs TEXT NULL;
ALTER TABLE repos ADD COLUMN legacy_shared TEXT NULL;
ALTER TABLE repos ADD COLUMN git_import_grade BOOLEAN NOT NULL DEFAULT FALSE;
