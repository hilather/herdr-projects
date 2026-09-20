CREATE TABLE budget_policies (
    revision INTEGER PRIMARY KEY NOT NULL CHECK(revision>0),
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    payload_hash TEXT NOT NULL CHECK(length(payload_hash)=64)
) STRICT;
CREATE TRIGGER budget_policies_no_update BEFORE UPDATE ON budget_policies BEGIN SELECT RAISE(ABORT,'budget policy is immutable'); END;
CREATE TRIGGER budget_policies_no_delete BEFORE DELETE ON budget_policies BEGIN SELECT RAISE(ABORT,'budget policy is immutable'); END;
UPDATE store_meta SET schema_version=14;
PRAGMA user_version=14;
