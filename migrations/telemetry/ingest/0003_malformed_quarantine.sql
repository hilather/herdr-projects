-- Conformance (TM1.6, contracts-collection.md A3): a complete line that does
-- not parse is quarantined with a reason instead of skipped silently. SQLite
-- cannot widen a CHECK in place, so the table is rebuilt with its rows.
CREATE TABLE ingest_quarantine_0003 (
    source TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    reason TEXT NOT NULL CHECK (reason IN ('line_oversized', 'envelope_oversized', 'digest_conflict', 'line_malformed', 'record_malformed')),
    bytes INTEGER NOT NULL,
    event_id TEXT,
    first_digest TEXT,
    new_digest TEXT,
    observed_unix_ms INTEGER NOT NULL,
    PRIMARY KEY (source, sequence)
) STRICT;
INSERT INTO ingest_quarantine_0003 SELECT source,sequence,reason,bytes,event_id,first_digest,new_digest,observed_unix_ms FROM ingest_quarantine;
DROP TABLE ingest_quarantine;
ALTER TABLE ingest_quarantine_0003 RENAME TO ingest_quarantine;
