-- Sanitized native observations only; no cross-surface aggregation.
CREATE TABLE otlp_records (
    identity TEXT PRIMARY KEY,
    adapter TEXT NOT NULL,
    attempt_id TEXT,
    binding TEXT NOT NULL CHECK(binding IN ('exact','unbound','unknown_attempt')),
    kind TEXT NOT NULL CHECK(kind IN ('usage','tool','unmapped')),
    source_trust TEXT NOT NULL CHECK(source_trust='collector_observed'),
    certified TEXT NOT NULL CHECK(certified='fixture'),
    record TEXT NOT NULL CHECK(json_valid(record)),
    observed_unix_ms INTEGER NOT NULL
) STRICT;
CREATE INDEX otlp_records_attempt ON otlp_records(attempt_id,kind);
