-- Data export jobs (docs/BUILDOUT.md section 28). `state` 1 pending, 2 running, 3 ready, 4 failed, 5 expired. The archive is a
-- file `<export dir>/<id>.jsonl`; this row is its record. `login` is the person, lower case.
CREATE TABLE export_jobs (
    id        TEXT   NOT NULL PRIMARY KEY,
    login     TEXT   NOT NULL,
    state     BIGINT NOT NULL,
    requested BIGINT NOT NULL,
    finished  BIGINT NOT NULL DEFAULT 0,
    expires   BIGINT NOT NULL DEFAULT 0,
    size      BIGINT NOT NULL DEFAULT 0,
    rows      BIGINT NOT NULL DEFAULT 0,
    truncated BIGINT NOT NULL DEFAULT 0,
    error     TEXT   NOT NULL DEFAULT ''
);
CREATE INDEX export_jobs_by_login ON export_jobs (login, requested);
