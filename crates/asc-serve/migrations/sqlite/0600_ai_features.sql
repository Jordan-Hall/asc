-- AI feature routing (aiproviders.proto ListFeatures and SetFeature): per owner and feature, the key the
-- feature uses ('' = the first usable key of the owner chain) and its monthly cap in micro-USD (0 = none).
-- The most specific owner of the chain with a row decides. The feature's spending is counted in ai_month
-- under the scope 'feature:' || owner label || ':' || feature. `created_by` on ai_keys is the person who
-- added the key ('' for keys added before it was kept).
CREATE TABLE ai_features (
    owner_kind  BIGINT NOT NULL,
    owner_name  TEXT   NOT NULL,
    feature     TEXT   NOT NULL,
    key_id      TEXT   NOT NULL DEFAULT '',
    monthly_cap BIGINT NOT NULL DEFAULT 0,
    updated     BIGINT NOT NULL,
    updated_by  TEXT   NOT NULL DEFAULT '',
    PRIMARY KEY (owner_kind, owner_name, feature)
);
CREATE INDEX ai_features_by_key ON ai_features (key_id);
ALTER TABLE ai_keys ADD COLUMN created_by TEXT NOT NULL DEFAULT '';
