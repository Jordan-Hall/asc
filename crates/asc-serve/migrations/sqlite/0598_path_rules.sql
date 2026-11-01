-- Per-directory access rules of a repository (SPEC §15.5.1). `ord` keeps the order they were set;
-- `path_rule_sets` holds the revision a writer must name to replace them.
CREATE TABLE path_rule_sets (
    repo TEXT PRIMARY KEY,
    revision BIGINT NOT NULL,
    updated BIGINT NOT NULL,
    updated_by TEXT NOT NULL
);
CREATE TABLE path_rules (
    repo TEXT NOT NULL,
    ord INTEGER NOT NULL,
    dir TEXT NOT NULL,
    subject TEXT NOT NULL,
    level TEXT NOT NULL,
    PRIMARY KEY (repo, ord)
);
-- Team members per organisation, sent by the hub; `team:<name>` subjects resolve here.
CREATE TABLE path_team_members (
    org TEXT NOT NULL,
    team TEXT NOT NULL,
    member TEXT NOT NULL,
    PRIMARY KEY (org, team, member)
);
CREATE INDEX path_team_members_by_member ON path_team_members (org, member);
