-- The size of what a storage migration moves and how many copies the check redid (design rows 387, UserSettingsStorage):
-- `bytes_total` is summed when the objects are counted; `mismatched` counts the copies the verify pass found missing or
-- different and copied again, for the pass that ran last.
ALTER TABLE storage_migrations ADD COLUMN bytes_total BIGINT NOT NULL DEFAULT 0;
ALTER TABLE storage_migrations ADD COLUMN mismatched BIGINT NOT NULL DEFAULT 0;
