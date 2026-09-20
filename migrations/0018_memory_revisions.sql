CREATE TABLE objects (
    hash TEXT PRIMARY KEY NOT NULL CHECK (length(hash)=64),
    size INTEGER NOT NULL CHECK (size>=0),
    availability TEXT NOT NULL CHECK (availability IN ('available','missing','purged')),
    collection TEXT NOT NULL CHECK (collection IN ('unclaimed','gc_pending','gc_deleting')),
    pin_count INTEGER NOT NULL CHECK (pin_count>=0),
    fencing_token INTEGER NOT NULL CHECK (fencing_token>=0)
) STRICT;
CREATE TABLE memory_records (
    id TEXT PRIMARY KEY NOT NULL,
    record_key TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('constraint','hard_memory','contract','observation','assumption','task_local')),
    is_hard INTEGER NOT NULL CHECK (is_hard IN (0,1)),
    UNIQUE (scope_id, record_key)
) STRICT;
CREATE TABLE memory_revisions (
    record_id TEXT NOT NULL REFERENCES memory_records(id),
    revision INTEGER NOT NULL CHECK (revision>0),
    body_hash TEXT NOT NULL REFERENCES objects(hash),
    provenance_hash TEXT NOT NULL REFERENCES objects(hash),
    promoted_seq INTEGER NOT NULL REFERENCES events(sequence),
    applicability TEXT NOT NULL CHECK (json_valid(applicability) AND json_type(applicability,'$.domains')='array' AND json_type(applicability,'$.paths')='array'),
    PRIMARY KEY (record_id, revision)
) STRICT;
CREATE TABLE memory_heads (
    record_id TEXT PRIMARY KEY NOT NULL REFERENCES memory_records(id),
    revision INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active','revoked')),
    row_revision INTEGER NOT NULL CHECK (row_revision>0),
    FOREIGN KEY (record_id, revision) REFERENCES memory_revisions(record_id, revision)
) STRICT;
CREATE TABLE memory_dependencies (
    derived_record TEXT NOT NULL,
    derived_revision INTEGER NOT NULL,
    source_record TEXT NOT NULL,
    source_revision INTEGER NOT NULL,
    kind TEXT NOT NULL,
    PRIMARY KEY (derived_record, derived_revision, source_record, source_revision, kind),
    FOREIGN KEY (derived_record, derived_revision) REFERENCES memory_revisions(record_id, revision),
    FOREIGN KEY (source_record, source_revision) REFERENCES memory_revisions(record_id, revision),
    CHECK (derived_record<>source_record OR derived_revision<>source_revision)
) STRICT;
CREATE TABLE memory_validity (
    record_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('valid','stale','blocked')),
    reason TEXT NOT NULL,
    expiry_unix_ms INTEGER,
    evaluated_seq INTEGER NOT NULL,
    PRIMARY KEY (record_id, revision),
    FOREIGN KEY (record_id, revision) REFERENCES memory_revisions(record_id, revision)
) STRICT;
CREATE INDEX memory_revision_dependents ON memory_dependencies(source_record, source_revision);
CREATE TRIGGER memory_revisions_no_update BEFORE UPDATE ON memory_revisions BEGIN SELECT RAISE(ABORT,'memory revision is immutable'); END;
CREATE TRIGGER memory_revisions_no_delete BEFORE DELETE ON memory_revisions BEGIN SELECT RAISE(ABORT,'memory revision is immutable'); END;
CREATE TRIGGER memory_dependencies_no_update BEFORE UPDATE ON memory_dependencies BEGIN SELECT RAISE(ABORT,'memory dependency is immutable'); END;
CREATE TRIGGER memory_dependencies_no_delete BEFORE DELETE ON memory_dependencies BEGIN SELECT RAISE(ABORT,'memory dependency is immutable'); END;
UPDATE store_meta SET schema_version=18;
PRAGMA user_version=18;
