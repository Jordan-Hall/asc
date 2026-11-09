-- A user's signing defaults (design row 104, UserSettingsKeys): `sign_by_default` = 1 asks the user's clients to sign every
-- new change without a flag. One row per user handle; a user with no row has never set it, which reads as off.
-- Every number is BIGINT so the sqlx Any driver reads one type on both databases.
CREATE TABLE signing_defaults (
    owner           TEXT   NOT NULL PRIMARY KEY,
    sign_by_default BIGINT NOT NULL DEFAULT 0,
    updated         BIGINT NOT NULL
);
