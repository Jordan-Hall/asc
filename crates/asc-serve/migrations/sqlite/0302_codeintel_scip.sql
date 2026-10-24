-- SCIP upload tier (SPEC 23.6 CI-D3, PLAN I31). Compiler-grade occurrences, keyed by FileId.
CREATE TABLE ci_scip (
    ns      BLOB     NOT NULL,
    file    BLOB     NOT NULL,
    name    TEXT     NOT NULL,
    symbol  TEXT     NOT NULL,
    is_def  SMALLINT NOT NULL,
    line    BIGINT   NOT NULL,
    col     BIGINT   NOT NULL,
    end_line BIGINT  NOT NULL,
    end_col BIGINT   NOT NULL
);
CREATE INDEX ci_scip_by_name ON ci_scip (ns, name);
CREATE INDEX ci_scip_by_symbol ON ci_scip (ns, symbol);
CREATE INDEX ci_scip_by_file ON ci_scip (ns, file);
