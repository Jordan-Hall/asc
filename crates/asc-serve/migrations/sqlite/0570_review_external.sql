-- A comment the host (the hub) wrote on its own side and mirrors here keeps the host's id, so
-- mirroring it twice adds it once (`Review.AddComment.external_id`).
ALTER TABLE review_comments ADD COLUMN external_id TEXT NOT NULL DEFAULT '';
CREATE INDEX review_comments_by_external ON review_comments (ns, external_id);
