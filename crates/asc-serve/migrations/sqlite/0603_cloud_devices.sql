-- Where a draft is (SPEC 21.3.9). One row per visible draft a device last reported for a workspace:
-- `cs` the native changeset id, `change` its change id, `added` when the draft was made, `state` its
-- upload state (1 uploaded, 2 waiting, 3 failed, 4 too large, 5 queue full) and `reported_at` the time
-- of the report. A report replaces every row of its device; at most 8 devices are kept per workspace
-- and a report older than 30 days is dropped. Metadata only: no content is stored.
CREATE TABLE cloud_device_drafts (
    ns          BLOB   NOT NULL,
    owner       BIGINT NOT NULL,
    name        TEXT   NOT NULL,
    device      TEXT   NOT NULL,
    cs          BLOB   NOT NULL,
    change      BLOB   NOT NULL,
    added       BIGINT NOT NULL,
    state       BIGINT NOT NULL,
    reported_at BIGINT NOT NULL,
    PRIMARY KEY (ns, owner, name, device, cs)
);
