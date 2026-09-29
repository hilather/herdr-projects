-- Stream `accounting` 9 (docs/telemetry/contracts-accounting.md §4, §12):
-- valuation revisions stored as deltas. A revision appended from this version
-- on stores only the entries whose valuation changed since the previous
-- revision (added or different) and a tombstone for each entry that left the
-- ledger; `accounting cost --revision N` replays the full-copy revision below
-- it (`valuations` + `valuation_bases`, written before this version) and every
-- delta up to N. Revisions written before this version stay full copies and
-- read back unchanged: nothing is moved, altered or dropped. Prices, counters,
-- identifiers and enums only; no content (contracts §7).
-- `IF NOT EXISTS` and no `ALTER`: re-running this migration is harmless.

-- One row per revision stored as a delta, with its size.
CREATE TABLE IF NOT EXISTS valuation_delta_revisions (
    revision INTEGER PRIMARY KEY REFERENCES valuation_revisions(revision),
    changed INTEGER NOT NULL CHECK (changed >= 0),
    removed INTEGER NOT NULL CHECK (removed >= 0)
) STRICT;
-- The `valuations` columns plus the version-6 bases, per changed entry;
-- `removed = 1`: the entry is not in this revision (every other column NULL).
CREATE TABLE IF NOT EXISTS valuation_deltas (
    revision INTEGER NOT NULL REFERENCES valuation_delta_revisions(revision),
    entry_id TEXT NOT NULL,
    removed INTEGER NOT NULL CHECK (removed IN (0, 1)),
    session_id TEXT,
    role TEXT,
    attempt_id TEXT,
    model TEXT,
    usage_from_unix_ms INTEGER,
    usage_to_unix_ms INTEGER,
    new_input_tokens INTEGER,
    cache_read_tokens INTEGER,
    cache_write_tokens INTEGER,
    output_tokens INTEGER,
    status TEXT CHECK (status IS NULL OR status IN ('priced', 'unavailable')),
    reason TEXT,
    card_id TEXT,
    card_version INTEGER,
    currency TEXT,
    amount TEXT,
    components TEXT CHECK (components IS NULL OR json_valid(components)),
    usage_basis TEXT CHECK (usage_basis IS NULL OR usage_basis IN ('record_time', 'session_start..first_observed')),
    provider_check TEXT CHECK (provider_check IS NULL OR provider_check IN ('matched', 'provider_unverified')),
    PRIMARY KEY (revision, entry_id),
    CHECK ((removed = 1) = (status IS NULL)),
    CHECK (removed = 1 OR (session_id IS NOT NULL AND role IS NOT NULL)),
    CHECK (status IS NULL OR (status = 'priced') = (reason IS NULL)),
    CHECK (status IS NULL OR (status = 'priced') = (amount IS NOT NULL AND currency IS NOT NULL AND card_id IS NOT NULL))
) STRICT;
CREATE TRIGGER IF NOT EXISTS valuation_delta_revisions_append_only_u BEFORE UPDATE ON valuation_delta_revisions BEGIN SELECT RAISE(ABORT, 'valuations are append-only'); END;
CREATE TRIGGER IF NOT EXISTS valuation_delta_revisions_append_only_d BEFORE DELETE ON valuation_delta_revisions BEGIN SELECT RAISE(ABORT, 'valuations are append-only'); END;
CREATE TRIGGER IF NOT EXISTS valuation_deltas_append_only_u BEFORE UPDATE ON valuation_deltas BEGIN SELECT RAISE(ABORT, 'valuations are append-only'); END;
CREATE TRIGGER IF NOT EXISTS valuation_deltas_append_only_d BEFORE DELETE ON valuation_deltas BEGIN SELECT RAISE(ABORT, 'valuations are append-only'); END;

-- The digest of the inputs (rate card versions and the ledger rows a reprice
-- reads) at the last reprice, so the ticker reprices only when they changed.
-- Operational state, not a valuation: replaced on each reprice.
CREATE TABLE IF NOT EXISTS valuation_inputs (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    digest TEXT NOT NULL,
    revision INTEGER,
    recorded_unix_ms INTEGER NOT NULL
) STRICT;
