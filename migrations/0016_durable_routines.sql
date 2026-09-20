CREATE TABLE routine_revisions (
    name TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK(revision>0),
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    payload_hash TEXT NOT NULL CHECK(length(payload_hash)=64),
    PRIMARY KEY(name,revision)
) STRICT;
CREATE TABLE routine_cursors (
    name TEXT NOT NULL,
    revision INTEGER NOT NULL,
    after_unix_ms INTEGER NOT NULL CHECK(after_unix_ms>=-1),
    PRIMARY KEY(name,revision),
    FOREIGN KEY(name,revision) REFERENCES routine_revisions(name,revision)
) STRICT;
CREATE TABLE routine_occurrences (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    revision INTEGER NOT NULL,
    scheduled_unix_ms INTEGER NOT NULL CHECK(scheduled_unix_ms>=0),
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    payload_hash TEXT NOT NULL CHECK(length(payload_hash)=64),
    operation_id TEXT UNIQUE REFERENCES operations(id),
    FOREIGN KEY(name,revision) REFERENCES routine_revisions(name,revision),
    UNIQUE(name,revision,scheduled_unix_ms)
) STRICT;
CREATE TRIGGER routine_revisions_no_update BEFORE UPDATE ON routine_revisions BEGIN SELECT RAISE(ABORT,'routine revision is immutable'); END;
CREATE TRIGGER routine_revisions_no_delete BEFORE DELETE ON routine_revisions BEGIN SELECT RAISE(ABORT,'routine revision is immutable'); END;
CREATE TRIGGER routine_occurrences_no_update BEFORE UPDATE ON routine_occurrences BEGIN SELECT RAISE(ABORT,'routine occurrence is immutable'); END;
CREATE TRIGGER routine_occurrences_no_delete BEFORE DELETE ON routine_occurrences BEGIN SELECT RAISE(ABORT,'routine occurrence is immutable'); END;
CREATE TRIGGER routine_cursor_monotonic BEFORE UPDATE ON routine_cursors
WHEN NEW.name<>OLD.name OR NEW.revision<>OLD.revision OR NEW.after_unix_ms<OLD.after_unix_ms
BEGIN SELECT RAISE(ABORT,'routine cursor cannot move backwards'); END;
UPDATE store_meta SET schema_version=16;
PRAGMA user_version=16;
