-- Local change index (SPEC 20.3). `repo_id` is the rowid of `repos`.
CREATE TABLE changes (
    repo_id BIGINT  NOT NULL,
    cs      BLOB    NOT NULL,
    change  BLOB    NOT NULL,
    meta    BLOB    NOT NULL,
    op      INTEGER NOT NULL,
    PRIMARY KEY (repo_id, cs)
);
CREATE INDEX changes_by_change ON changes (repo_id, change);

CREATE TABLE change_preds (
    repo_id BIGINT NOT NULL,
    cs      BLOB   NOT NULL,
    pred    BLOB   NOT NULL,
    PRIMARY KEY (repo_id, cs, pred)
);
CREATE INDEX change_preds_by_pred ON change_preds (repo_id, pred);
