-- Secrets the server makes for itself and every replica must share. `page_key` seals list page
-- tokens (see `pagetoken.rs`): a token made by one replica opens on any other.
CREATE TABLE server_secrets (
    name  TEXT NOT NULL PRIMARY KEY,
    value BYTEA NOT NULL
);
