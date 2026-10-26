-- Bring-your-own storage (docs/BUILDOUT.md section 9, docs/BYOS.md). Targets are scoped to one
-- owner; the credentials column holds a vault-sealed blob and is never selected for a response.
CREATE TABLE storage_targets (
    id                 TEXT   NOT NULL PRIMARY KEY,
    owner_kind         TEXT   NOT NULL,
    owner_name         TEXT   NOT NULL,
    name               TEXT   NOT NULL,
    endpoint           TEXT   NOT NULL,
    region             TEXT   NOT NULL,
    bucket             TEXT   NOT NULL,
    prefix             TEXT   NOT NULL,
    addressing         TEXT   NOT NULL,
    access_key_hint    TEXT   NOT NULL,
    sealed_credentials BYTEA  NOT NULL,
    key_gen            BIGINT NOT NULL,
    state              TEXT   NOT NULL,
    was_enabled        BIGINT NOT NULL DEFAULT 0,
    last_error         TEXT   NOT NULL DEFAULT '',
    verified_at        BIGINT NOT NULL DEFAULT 0,
    version            BIGINT NOT NULL DEFAULT 1,
    verify_window      BIGINT NOT NULL DEFAULT 0,
    verify_count       BIGINT NOT NULL DEFAULT 0,
    created            BIGINT NOT NULL,
    updated            BIGINT NOT NULL
);
CREATE UNIQUE INDEX storage_targets_by_name ON storage_targets (owner_kind, owner_name, name);
CREATE INDEX storage_targets_by_state ON storage_targets (state);

-- Which target a repository keeps its objects in. `target_id` '' is the instance default;
-- `fallback` is where objects stored before an assignment still are (NULL: nowhere else, '': the
-- instance default); `mirror_to` is set only while a migration copies, and makes every write go
-- to both places.
CREATE TABLE repo_storage (
    repo_id   BIGINT NOT NULL PRIMARY KEY,
    target_id TEXT   NOT NULL,
    fallback  TEXT   NULL,
    mirror_to TEXT   NULL,
    version   BIGINT NOT NULL DEFAULT 1,
    updated   BIGINT NOT NULL
);
CREATE INDEX repo_storage_by_target ON repo_storage (target_id);

CREATE TABLE storage_migrations (
    id             TEXT   NOT NULL PRIMARY KEY,
    repo_id        BIGINT NOT NULL,
    repo_name      TEXT   NOT NULL,
    from_target    TEXT   NOT NULL,
    to_target      TEXT   NOT NULL,
    state          TEXT   NOT NULL,
    delete_source  BIGINT NOT NULL,
    objects_total  BIGINT NOT NULL DEFAULT 0,
    objects_copied BIGINT NOT NULL DEFAULT 0,
    bytes_copied   BIGINT NOT NULL DEFAULT 0,
    cursor         TEXT   NOT NULL DEFAULT '',
    cancel         BIGINT NOT NULL DEFAULT 0,
    switched       BIGINT NOT NULL DEFAULT 0,
    mirror_since   BIGINT NOT NULL DEFAULT 0,
    error          TEXT   NOT NULL DEFAULT '',
    lease_until    BIGINT NOT NULL DEFAULT 0,
    created_ms     BIGINT NOT NULL,
    updated        BIGINT NOT NULL
);
CREATE INDEX storage_migrations_by_repo ON storage_migrations (repo_id, created_ms);
CREATE UNIQUE INDEX storage_migrations_one_active ON storage_migrations (repo_id)
    WHERE state IN ('queued', 'copying', 'verifying', 'switching');
