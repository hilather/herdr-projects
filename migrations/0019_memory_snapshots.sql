CREATE TABLE memory_snapshots (
    id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT NOT NULL,
    task_revision INTEGER NOT NULL CHECK (task_revision>0),
    profile_name TEXT NOT NULL,
    profile_digest TEXT NOT NULL CHECK (length(profile_digest)=64),
    config_digest TEXT,
    selection_policy_version INTEGER NOT NULL CHECK (selection_policy_version>0),
    estimator TEXT NOT NULL,
    sequence INTEGER NOT NULL CHECK (sequence>0),
    required_bytes INTEGER NOT NULL CHECK (required_bytes>=0),
    optional_bytes INTEGER NOT NULL CHECK (optional_bytes>=0),
    budget_bytes INTEGER NOT NULL CHECK (budget_bytes>=0),
    omitted_optional_count INTEGER NOT NULL CHECK (omitted_optional_count>=0),
    manifest_hash TEXT NOT NULL CHECK (length(manifest_hash)=64),
    scope_digest TEXT NOT NULL CHECK (length(scope_digest)=64)
) STRICT;
CREATE TABLE snapshot_entries (
    snapshot_id TEXT NOT NULL REFERENCES memory_snapshots(id),
    ordinal INTEGER NOT NULL CHECK (ordinal>0),
    record_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision>0),
    role TEXT NOT NULL CHECK (role IN ('mandatory','optional')),
    reason TEXT NOT NULL,
    PRIMARY KEY (snapshot_id, ordinal),
    FOREIGN KEY (record_id, revision) REFERENCES memory_revisions(record_id, revision)
) STRICT;
CREATE TABLE memory_subscriptions (
    id TEXT PRIMARY KEY NOT NULL,
    subscriber TEXT NOT NULL,
    snapshot_id TEXT NOT NULL REFERENCES memory_snapshots(id),
    since_seq INTEGER NOT NULL CHECK (since_seq>=0),
    UNIQUE (subscriber, snapshot_id)
) STRICT;
CREATE INDEX snapshot_entry_usage ON snapshot_entries(record_id, revision, snapshot_id);
CREATE TRIGGER memory_snapshots_no_update BEFORE UPDATE ON memory_snapshots BEGIN SELECT RAISE(ABORT,'memory snapshot is immutable'); END;
CREATE TRIGGER memory_snapshots_no_delete BEFORE DELETE ON memory_snapshots BEGIN SELECT RAISE(ABORT,'memory snapshot is immutable'); END;
CREATE TRIGGER snapshot_entries_no_update BEFORE UPDATE ON snapshot_entries BEGIN SELECT RAISE(ABORT,'snapshot entry is immutable'); END;
CREATE TRIGGER snapshot_entries_no_delete BEFORE DELETE ON snapshot_entries BEGIN SELECT RAISE(ABORT,'snapshot entry is immutable'); END;
UPDATE store_meta SET schema_version=19;
PRAGMA user_version=19;
