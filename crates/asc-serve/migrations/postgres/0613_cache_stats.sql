-- Cache use counted at the cache endpoints: one row per repository and day, and one per repository and
-- change version (`cs`, only when the client names a version of this repository). `day` is the unix time of
-- 00:00 UTC. Counters only add up; they hold no token, principal or key. Daily rows are dropped after 180
-- days, change rows 30 days after their last update and beyond the newest 2000 per repository.
CREATE TABLE cache_daily (
    ns           BYTEA NOT NULL,
    day          BIGINT NOT NULL,
    hits         BIGINT NOT NULL DEFAULT 0,
    misses       BIGINT NOT NULL DEFAULT 0,
    puts         BIGINT NOT NULL DEFAULT 0,
    bytes_served BIGINT NOT NULL DEFAULT 0,
    saved_ms     BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (ns, day)
);
CREATE INDEX cache_daily_by_day ON cache_daily (day);

CREATE TABLE cache_by_rev (
    ns           BYTEA NOT NULL,
    cs           BYTEA NOT NULL,
    hits         BIGINT NOT NULL DEFAULT 0,
    misses       BIGINT NOT NULL DEFAULT 0,
    puts         BIGINT NOT NULL DEFAULT 0,
    bytes_served BIGINT NOT NULL DEFAULT 0,
    saved_ms     BIGINT NOT NULL DEFAULT 0,
    updated      BIGINT NOT NULL,
    PRIMARY KEY (ns, cs)
);
CREATE INDEX cache_by_rev_by_updated ON cache_by_rev (updated);
CREATE INDEX cache_by_rev_by_ns_updated ON cache_by_rev (ns, updated);
