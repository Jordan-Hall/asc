-- Delta storage covers directory trees and large-file chunks as well as files
-- (docs/design/DELTA-STORAGE.md). `kind` says what `file` and `base` name: 0 a file, 1 a tree,
-- 2 a chunk. A delta and its base are always of the same kind, and ids of different kinds never
-- collide (each kind hashes under its own key), so the primary key stays (ns, file).
ALTER TABLE delta_bases ADD COLUMN kind SMALLINT NOT NULL DEFAULT 0;
