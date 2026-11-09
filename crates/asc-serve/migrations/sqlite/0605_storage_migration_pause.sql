-- A storage migration can be paused (design row 387): the worker stops after the chunk it is on and leaves the
-- migration where it is, mirror and cursor kept, until it is resumed. Cancelling clears the pause.
ALTER TABLE storage_migrations ADD COLUMN paused BIGINT NOT NULL DEFAULT 0;
