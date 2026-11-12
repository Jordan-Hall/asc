-- Learned prefetch profiles (SPEC 10.2 item 5b): per build target or plugin entry, the paths it
-- read, warmed again on switch and clone. `hits` counts the learns that named the path.
CREATE TABLE prefetch_profiles (
    repo_id   BIGINT  NOT NULL,
    name      TEXT    NOT NULL,
    path      BLOB    NOT NULL,
    hits      BIGINT  NOT NULL DEFAULT 1,
    last_used BIGINT  NOT NULL,
    PRIMARY KEY (repo_id, name, path)
);
CREATE INDEX prefetch_profiles_by_used ON prefetch_profiles (repo_id, last_used);
