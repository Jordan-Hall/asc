-- The branch HEAD named before and after an op (SPEC 20.3), so `asc undo` puts a switch back.
-- NULL when HEAD was detached, and for rows written before this column existed.
ALTER TABLE op_log ADD COLUMN head_ref_before TEXT NULL;
ALTER TABLE op_log ADD COLUMN head_ref_after TEXT NULL;
