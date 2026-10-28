-- The immutable hub user id of the person behind a comment, a verdict and a queue entry (0 when the caller had none, as a
-- token-only caller has not): the name beside it can be renamed or recycled, the id cannot.
ALTER TABLE review_comments ADD COLUMN author_id BIGINT NOT NULL DEFAULT 0;
ALTER TABLE review_approvals ADD COLUMN reviewer_id BIGINT NOT NULL DEFAULT 0;
ALTER TABLE queue_entries ADD COLUMN queued_by_id BIGINT NOT NULL DEFAULT 0;
