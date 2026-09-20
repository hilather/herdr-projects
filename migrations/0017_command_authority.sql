CREATE TABLE authority_denials (
    id TEXT PRIMARY KEY NOT NULL,
    unix_ms INTEGER NOT NULL,
    class TEXT NOT NULL CHECK (class IN ('approval','budget','routine-store','memory')),
    command TEXT NOT NULL,
    actor_channel TEXT NOT NULL CHECK (actor_channel IN ('cli-owner','unknown-rejected')),
    reason_code TEXT NOT NULL,
    policy_digest TEXT NOT NULL CHECK (length(policy_digest) = 64),
    expected_head INTEGER,
    actual_head INTEGER
) STRICT;
CREATE TABLE memory_policies (
    revision INTEGER PRIMARY KEY NOT NULL CHECK (revision > 0),
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    payload_hash TEXT NOT NULL CHECK (length(payload_hash) = 64)
) STRICT;
CREATE TRIGGER memory_policies_no_update BEFORE UPDATE ON memory_policies BEGIN SELECT RAISE(ABORT,'memory policy is immutable'); END;
CREATE TRIGGER memory_policies_no_delete BEFORE DELETE ON memory_policies BEGIN SELECT RAISE(ABORT,'memory policy is immutable'); END;
CREATE TRIGGER authority_denials_no_update BEFORE UPDATE ON authority_denials BEGIN SELECT RAISE(ABORT,'authority denial is immutable'); END;
CREATE TRIGGER authority_denials_no_delete BEFORE DELETE ON authority_denials BEGIN SELECT RAISE(ABORT,'authority denial is immutable'); END;
UPDATE store_meta SET schema_version=17;
PRAGMA user_version=17;
