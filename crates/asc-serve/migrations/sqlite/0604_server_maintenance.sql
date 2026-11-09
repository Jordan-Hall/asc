-- Read-only maintenance of the server (SPEC §14.5): one row, id 1. Every node reads it every 5 seconds and refuses writes
-- while `enabled` is 1. `until_at` is shown, never acted on. Switching off keeps the message for the next time.
CREATE TABLE server_maintenance (
    id         BIGINT NOT NULL PRIMARY KEY,
    enabled    BIGINT NOT NULL,
    message    TEXT NOT NULL,
    until_at   BIGINT NOT NULL,
    version    TEXT NOT NULL,
    updated    BIGINT NOT NULL,
    updated_by TEXT NOT NULL
);
