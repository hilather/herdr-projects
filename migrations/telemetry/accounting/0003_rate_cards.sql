-- Stream `accounting` 3 (docs/telemetry/contracts-accounting.md §4): versioned
-- rate cards and published-rate estimates. Both are append-only: a changed
-- price is a new card version, a repricing a new valuation revision; measured
-- tokens are never changed. Money is an exact decimal string, never a float.
-- Counters, prices and identifiers only; no content (contracts §7).
CREATE TABLE IF NOT EXISTS rate_cards (
    card_id TEXT NOT NULL,
    version INTEGER NOT NULL CHECK (version > 0),
    digest TEXT NOT NULL,
    provider TEXT NOT NULL,
    product TEXT NOT NULL,
    currency TEXT NOT NULL CHECK (length(currency) = 3 AND currency = upper(currency)),
    -- Tokens per quoted rate: a power of ten, so every amount is an exact decimal.
    rate_unit INTEGER NOT NULL CHECK (rate_unit > 0),
    -- Half-open `[from, to)`, UTC Unix ms; `to` NULL is open-ended.
    effective_from_unix_ms INTEGER NOT NULL,
    effective_to_unix_ms INTEGER CHECK (effective_to_unix_ms IS NULL OR effective_to_unix_ms > effective_from_unix_ms),
    includes_discounts INTEGER NOT NULL CHECK (includes_discounts IN (0, 1)),
    includes_taxes INTEGER NOT NULL CHECK (includes_taxes IN (0, 1)),
    includes_fees INTEGER NOT NULL CHECK (includes_fees IN (0, 1)),
    source TEXT NOT NULL,
    imported_unix_ms INTEGER NOT NULL,
    PRIMARY KEY (card_id, version)
) STRICT;
CREATE TABLE IF NOT EXISTS rate_card_models (
    card_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    model TEXT NOT NULL,
    PRIMARY KEY (card_id, version, model),
    FOREIGN KEY (card_id, version) REFERENCES rate_cards(card_id, version)
) STRICT;
-- Disjoint billable categories: `input` is new (uncached) input, `output`
-- includes reasoning. `rate` is a canonical decimal per `rate_unit` tokens.
CREATE TABLE IF NOT EXISTS rate_card_rates (
    card_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    category TEXT NOT NULL CHECK (category IN ('input', 'cache_read', 'cache_write', 'output')),
    cache_tier TEXT NOT NULL DEFAULT '',
    rate TEXT NOT NULL,
    PRIMARY KEY (card_id, version, category, cache_tier),
    FOREIGN KEY (card_id, version) REFERENCES rate_cards(card_id, version)
) STRICT;
-- One calculation per `accounting reprice` whose result differs from the last.
CREATE TABLE IF NOT EXISTS valuation_revisions (
    revision INTEGER PRIMARY KEY CHECK (revision > 0),
    basis TEXT NOT NULL CHECK (basis = 'published_rate_estimate'),
    policy TEXT NOT NULL,
    ledger_synced_unix_ms INTEGER NOT NULL,
    digest TEXT NOT NULL,
    computed_unix_ms INTEGER NOT NULL
) STRICT;
-- One row per delta usage entry per revision, with the quantities it priced
-- (copied, so a later ledger sync cannot change an earlier revision).
CREATE TABLE IF NOT EXISTS valuations (
    revision INTEGER NOT NULL REFERENCES valuation_revisions(revision),
    entry_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    role TEXT NOT NULL,
    attempt_id TEXT,
    model TEXT,
    usage_from_unix_ms INTEGER,
    usage_to_unix_ms INTEGER,
    new_input_tokens INTEGER,
    cache_read_tokens INTEGER,
    cache_write_tokens INTEGER,
    output_tokens INTEGER,
    status TEXT NOT NULL CHECK (status IN ('priced', 'unavailable')),
    reason TEXT,
    card_id TEXT,
    card_version INTEGER,
    currency TEXT,
    amount TEXT,
    components TEXT CHECK (components IS NULL OR json_valid(components)),
    PRIMARY KEY (revision, entry_id),
    CHECK ((status = 'priced') = (reason IS NULL)),
    CHECK ((status = 'priced') = (amount IS NOT NULL AND currency IS NOT NULL AND card_id IS NOT NULL))
) STRICT;
CREATE TRIGGER IF NOT EXISTS rate_cards_append_only_u BEFORE UPDATE ON rate_cards BEGIN SELECT RAISE(ABORT, 'rate cards are append-only'); END;
CREATE TRIGGER IF NOT EXISTS rate_cards_append_only_d BEFORE DELETE ON rate_cards BEGIN SELECT RAISE(ABORT, 'rate cards are append-only'); END;
CREATE TRIGGER IF NOT EXISTS rate_card_models_append_only_u BEFORE UPDATE ON rate_card_models BEGIN SELECT RAISE(ABORT, 'rate cards are append-only'); END;
CREATE TRIGGER IF NOT EXISTS rate_card_models_append_only_d BEFORE DELETE ON rate_card_models BEGIN SELECT RAISE(ABORT, 'rate cards are append-only'); END;
CREATE TRIGGER IF NOT EXISTS rate_card_rates_append_only_u BEFORE UPDATE ON rate_card_rates BEGIN SELECT RAISE(ABORT, 'rate cards are append-only'); END;
CREATE TRIGGER IF NOT EXISTS rate_card_rates_append_only_d BEFORE DELETE ON rate_card_rates BEGIN SELECT RAISE(ABORT, 'rate cards are append-only'); END;
CREATE TRIGGER IF NOT EXISTS valuation_revisions_append_only_u BEFORE UPDATE ON valuation_revisions BEGIN SELECT RAISE(ABORT, 'valuations are append-only'); END;
CREATE TRIGGER IF NOT EXISTS valuation_revisions_append_only_d BEFORE DELETE ON valuation_revisions BEGIN SELECT RAISE(ABORT, 'valuations are append-only'); END;
CREATE TRIGGER IF NOT EXISTS valuations_append_only_u BEFORE UPDATE ON valuations BEGIN SELECT RAISE(ABORT, 'valuations are append-only'); END;
CREATE TRIGGER IF NOT EXISTS valuations_append_only_d BEFORE DELETE ON valuations BEGIN SELECT RAISE(ABORT, 'valuations are append-only'); END;
