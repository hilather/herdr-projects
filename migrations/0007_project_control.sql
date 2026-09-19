CREATE TABLE project_control (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK(singleton=1),
    revision INTEGER NOT NULL CHECK(revision>0),
    epoch INTEGER NOT NULL CHECK(epoch>0),
    state TEXT NOT NULL CHECK(state IN ('paused','active','archived')),
    reconciliation_required INTEGER NOT NULL CHECK(reconciliation_required IN (0,1)),
    config_digest TEXT CHECK(config_digest IS NULL OR length(config_digest)=64)
) STRICT;
INSERT INTO project_control VALUES(1,1,1,'paused',1,NULL);
UPDATE store_meta SET schema_version=7;
PRAGMA user_version=7;
