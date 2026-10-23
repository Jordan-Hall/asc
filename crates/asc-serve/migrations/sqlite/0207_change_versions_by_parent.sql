-- Children of a change (SPEC 20.5 Stack): the changes whose version names it as parent.
CREATE INDEX change_versions_by_parent ON change_versions (ns, parent_change);
