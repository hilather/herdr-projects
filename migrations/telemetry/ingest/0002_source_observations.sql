-- Ingest ledger (TM1.2, contracts-collection.md A2): one sanitized envelope per
-- allowlisted source record (doc 03 §1 subset), written in the same sidecar
-- transaction as the adapter's rows. `IF NOT EXISTS`: a sidecar whose stream
-- table was lost re-runs this migration.
CREATE TABLE IF NOT EXISTS source_observations (
    event_id TEXT PRIMARY KEY,
    schema_version INTEGER NOT NULL,
    producer_id TEXT NOT NULL,
    producer_epoch TEXT NOT NULL,
    producer_sequence INTEGER NOT NULL CHECK (producer_sequence >= 0),
    event_kind TEXT NOT NULL,
    occurred_unix_ms INTEGER,
    observed_unix_ms INTEGER NOT NULL,
    identity TEXT NOT NULL,
    provenance TEXT NOT NULL,
    measurement TEXT NOT NULL,
    payload TEXT NOT NULL,
    payload_digest TEXT NOT NULL,
    envelope_bytes INTEGER NOT NULL CHECK (envelope_bytes BETWEEN 1 AND 65536),
    UNIQUE (producer_epoch, producer_sequence)
) STRICT;

-- Source positions that yield no observation, with why. Nothing of the line is kept.
CREATE TABLE IF NOT EXISTS ingest_quarantine (
    source TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    reason TEXT NOT NULL CHECK (reason IN ('line_oversized', 'envelope_oversized', 'digest_conflict')),
    bytes INTEGER NOT NULL,
    event_id TEXT,
    first_digest TEXT,
    new_digest TEXT,
    observed_unix_ms INTEGER NOT NULL,
    PRIMARY KEY (source, sequence)
) STRICT;

-- Byte ranges of a source not ingested, and whether a later collect read them.
CREATE TABLE IF NOT EXISTS coverage_gaps (
    source TEXT NOT NULL,
    start_offset INTEGER NOT NULL,
    end_offset INTEGER NOT NULL CHECK (end_offset >= start_offset),
    reason TEXT NOT NULL CHECK (reason IN ('sidecar_write_failed', 'predates_ingest')),
    recovery TEXT NOT NULL CHECK (recovery IN ('pending', 'recovered')),
    observed_unix_ms INTEGER NOT NULL,
    PRIMARY KEY (source, start_offset, reason)
) STRICT;

-- How far each source's envelopes are written; advanced with the adapter offset.
CREATE TABLE IF NOT EXISTS source_cursors (
    source TEXT PRIMARY KEY,
    producer_id TEXT,
    byte_offset INTEGER NOT NULL CHECK (byte_offset >= 0),
    observations INTEGER NOT NULL CHECK (observations >= 0),
    updated_unix_ms INTEGER NOT NULL
) STRICT;
