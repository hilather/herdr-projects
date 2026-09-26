-- Levels a retained profile or the fake adapter has already shown.
-- Launchable is not workflow-certified. A fixture row is not live evidence.
-- Provider selection, effort, and process environment are not mapped here.
CREATE TABLE capability_evidence (
    evidence_id TEXT PRIMARY KEY CHECK (length(evidence_id) = 64),
    adapter_kind TEXT NOT NULL CHECK (adapter_kind IN ('native', 'fake')),
    binary_digest TEXT NOT NULL CHECK (length(binary_digest) = 64),
    os_name TEXT NOT NULL CHECK (length(os_name) BETWEEN 1 AND 32),
    profile_digest TEXT NOT NULL CHECK (length(profile_digest) = 64),
    profile_kind TEXT NOT NULL CHECK (length(profile_kind) BETWEEN 1 AND 64),
    level TEXT NOT NULL CHECK (level IN (
        'discovered',
        'launchable',
        'repository-capable',
        'memory-protocol-capable',
        'workflow-certified'
    )),
    test_id TEXT NOT NULL CHECK (length(test_id) BETWEEN 1 AND 128),
    observed_unix_ms INTEGER NOT NULL CHECK (observed_unix_ms >= 0),
    expires_unix_ms INTEGER NOT NULL CHECK (expires_unix_ms > observed_unix_ms),
    live INTEGER NOT NULL CHECK (live IN (0, 1)),
    -- Certification is a later live decision. Fixtures cannot take that row.
    CHECK (live = 1 OR level != 'workflow-certified'),
    CHECK (adapter_kind != 'fake' OR (live = 0 AND level IN ('discovered', 'launchable')))
) STRICT;
-- No UNIQUE on (adapter, digest, level): a later observation window is another immutable row.
CREATE INDEX capability_evidence_by_profile ON capability_evidence(profile_digest, level);
CREATE INDEX capability_evidence_by_kind ON capability_evidence(profile_kind, adapter_kind, observed_unix_ms);
CREATE TRIGGER capability_evidence_no_update BEFORE UPDATE ON capability_evidence
BEGIN SELECT RAISE(ABORT, 'capability evidence is immutable'); END;
CREATE TRIGGER capability_evidence_no_delete BEFORE DELETE ON capability_evidence
BEGIN SELECT RAISE(ABORT, 'capability evidence is immutable'); END;
UPDATE store_meta SET schema_version = 33;
PRAGMA user_version = 33;
