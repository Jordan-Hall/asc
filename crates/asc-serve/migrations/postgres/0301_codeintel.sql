-- Code intelligence tables (SPEC 23.2, 23.5). Per namespace, derived and rebuildable.
CREATE TABLE ci_docs (
    ns       BYTEA   NOT NULL,
    file     BYTEA   NOT NULL,
    ord      BIGINT NOT NULL,
    size     BIGINT NOT NULL,
    flags    INTEGER NOT NULL DEFAULT 0,
    versions BYTEA   NOT NULL,
    PRIMARY KEY (ns, file)
);
CREATE UNIQUE INDEX ci_docs_by_ord ON ci_docs (ns, ord);

CREATE TABLE ci_doc_langs (
    ns   BYTEA   NOT NULL,
    ord  BIGINT NOT NULL,
    lang TEXT   NOT NULL,
    PRIMARY KEY (ns, ord, lang)
);

CREATE TABLE ci_chunks (
    ns         BYTEA   NOT NULL,
    chunk_ord  BIGINT NOT NULL,
    doc_ord    BIGINT NOT NULL,
    lang       TEXT   NOT NULL,
    start_line BIGINT NOT NULL,
    end_line   BIGINT NOT NULL,
    text_id    BYTEA   NOT NULL,
    PRIMARY KEY (ns, chunk_ord)
);
CREATE INDEX ci_chunks_by_doc ON ci_chunks (ns, doc_ord);

CREATE TABLE ci_manifest (
    ns          BYTEA   NOT NULL,
    idx         TEXT   NOT NULL,
    generation  BIGINT NOT NULL DEFAULT 0,
    segments    BYTEA   NOT NULL,
    model_gen   BIGINT NOT NULL DEFAULT 0,
    indexed_ref BYTEA   NULL,
    leader      TEXT   NULL,
    lease_until BIGINT NULL,
    PRIMARY KEY (ns, idx)
);

-- The work queue. priority: 0 interactive, 1 land tip, 2 PR or draft head, 3 backfill.
-- state: 'queued', 'leased' or 'dead'. A finished job is deleted.
CREATE TABLE ci_jobs (
    ns          BYTEA   NOT NULL,
    kind        TEXT   NOT NULL,
    key         BYTEA   NOT NULL,
    version     BIGINT NOT NULL,
    priority    SMALLINT NOT NULL,
    state       TEXT   NOT NULL DEFAULT 'queued',
    attempts    INTEGER NOT NULL DEFAULT 0,
    available_at BIGINT NOT NULL,
    lease_until BIGINT NULL,
    created_at  BIGINT NOT NULL,
    last_error  TEXT   NULL,
    PRIMARY KEY (ns, kind, key, version)
);
CREATE INDEX ci_jobs_ready ON ci_jobs (priority, created_at);
