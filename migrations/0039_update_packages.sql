-- Immutable package of unresolved obligations for one binding generation.
-- An acknowledgment lists those change ids. It is not a sequence watermark.
-- seen is not applied. Schema 24 worker receipts stay on memory_update_receipts.
CREATE TABLE update_packages (
    package_id TEXT PRIMARY KEY CHECK (length(package_id) = 64),
    binding_id TEXT NOT NULL REFERENCES consumer_bindings(binding_id),
    consumer_binding_generation INTEGER NOT NULL CHECK (consumer_binding_generation > 0),
    manifest_hash TEXT NOT NULL CHECK (length(manifest_hash) = 64),
    created_seq INTEGER NOT NULL REFERENCES events(sequence)
) STRICT;

CREATE TABLE update_package_members (
    package_id TEXT NOT NULL REFERENCES update_packages(package_id),
    position INTEGER NOT NULL CHECK (position >= 0),
    change_id TEXT NOT NULL REFERENCES memory_delivery_intents(id),
    PRIMARY KEY (package_id, change_id),
    UNIQUE (package_id, position)
) STRICT;

-- Logical receipt key: (consumer_binding_generation, change_id, disposition).
CREATE TABLE memory_change_receipts (
    consumer_binding_generation INTEGER NOT NULL CHECK (consumer_binding_generation > 0),
    change_id TEXT NOT NULL REFERENCES memory_delivery_intents(id),
    disposition TEXT NOT NULL CHECK (disposition IN ('seen', 'applied')),
    binding_id TEXT NOT NULL REFERENCES consumer_bindings(binding_id),
    package_id TEXT NOT NULL REFERENCES update_packages(package_id),
    sequence INTEGER NOT NULL REFERENCES events(sequence),
    PRIMARY KEY (consumer_binding_generation, change_id, disposition)
) STRICT;
CREATE INDEX memory_change_receipts_by_generation
    ON memory_change_receipts(consumer_binding_generation, change_id, disposition);

CREATE TRIGGER update_packages_no_update
BEFORE UPDATE ON update_packages
BEGIN SELECT RAISE(ABORT, 'update package is immutable'); END;
CREATE TRIGGER update_packages_no_delete
BEFORE DELETE ON update_packages
BEGIN SELECT RAISE(ABORT, 'update package is immutable'); END;
CREATE TRIGGER update_package_members_no_update
BEFORE UPDATE ON update_package_members
BEGIN SELECT RAISE(ABORT, 'update package member is immutable'); END;
CREATE TRIGGER update_package_members_no_delete
BEFORE DELETE ON update_package_members
BEGIN SELECT RAISE(ABORT, 'update package member is immutable'); END;
CREATE TRIGGER memory_change_receipts_no_update
BEFORE UPDATE ON memory_change_receipts
BEGIN SELECT RAISE(ABORT, 'memory change receipt is immutable'); END;
CREATE TRIGGER memory_change_receipts_no_delete
BEFORE DELETE ON memory_change_receipts
BEGIN SELECT RAISE(ABORT, 'memory change receipt is immutable'); END;

-- Applied package id for a coordinator binding. Not a sequence watermark.
ALTER TABLE consumer_bindings ADD COLUMN applied_cursor TEXT
    CHECK (applied_cursor IS NULL OR length(applied_cursor) = 64)
    REFERENCES update_packages(package_id);

UPDATE store_meta SET schema_version = 39;
PRAGMA user_version = 39;
