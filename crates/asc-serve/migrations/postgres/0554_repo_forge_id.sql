-- The forge's stable id of a repository (K07 `repo_id`). The first authorization answer that
-- carries one binds it to the registry entry; a later answer with another id for the same name
-- (a deleted and recreated repository, a renamed one taking the name) is refused. 0: unbound.
ALTER TABLE repos ADD COLUMN forge_id BIGINT NOT NULL DEFAULT 0;
