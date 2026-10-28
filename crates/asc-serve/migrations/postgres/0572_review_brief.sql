-- The review brief, approvals per version and suggested changes (docs/BUILDOUT.md section 19).
CREATE TABLE review_briefs (
    ns            BLOB   NOT NULL,
    change        BLOB   NOT NULL,
    what          TEXT   NOT NULL DEFAULT '',
    why           TEXT   NOT NULL DEFAULT '',
    how           TEXT   NOT NULL DEFAULT '',
    risk_note     TEXT   NOT NULL DEFAULT '',
    pointers_json TEXT   NOT NULL DEFAULT '[]',
    updated_by    TEXT   NOT NULL DEFAULT '',
    updated       BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (ns, change)
);

CREATE TABLE review_approvals (
    ns       BLOB   NOT NULL,
    change   BLOB   NOT NULL,
    reviewer TEXT   NOT NULL,
    version  BIGINT NOT NULL,
    state    BIGINT NOT NULL,
    at       BIGINT NOT NULL,
    PRIMARY KEY (ns, change, reviewer, version)
);

ALTER TABLE review_comments ADD COLUMN suggestion TEXT NOT NULL DEFAULT '';
ALTER TABLE review_comments ADD COLUMN applied_version BIGINT NOT NULL DEFAULT 0;
ALTER TABLE review_comments ADD COLUMN applied_by TEXT NOT NULL DEFAULT '';
