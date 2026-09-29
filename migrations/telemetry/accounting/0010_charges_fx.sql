-- Stream `accounting` 10 (docs/telemetry/contracts-accounting.md §13):
-- provider-reported charges and invoices, and dated exchange-rate tables.
-- Each is a cost basis or conversion input of its own, imported from local
-- files that must be marked synthetic (fixture-only: no real provider export
-- or price ships). They are never added to published-rate estimates.
-- Append-only: a correction is the next revision of the same charge or
-- invoice, a changed rate the next table version. Money and rates are exact
-- decimal strings, never floats. Identifiers, amounts and times only; no
-- content (contracts §7). `IF NOT EXISTS` and no `ALTER`: re-running this
-- migration is harmless.

-- One provider-reported charge revision (basis `provider_billed`); matched to
-- ledger entries by the native `response_id` (and `session_id`, when given).
CREATE TABLE IF NOT EXISTS provider_charges (
    charge_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    digest TEXT NOT NULL,
    provider TEXT NOT NULL,
    product TEXT NOT NULL,
    currency TEXT NOT NULL CHECK (length(currency) = 3 AND currency = upper(currency)),
    amount TEXT NOT NULL,
    response_id TEXT,
    session_id TEXT,
    charged_unix_ms INTEGER,
    source TEXT NOT NULL,
    synthetic INTEGER NOT NULL CHECK (synthetic = 1),
    imported_unix_ms INTEGER NOT NULL,
    PRIMARY KEY (charge_id, revision)
) STRICT;
-- One invoice or subscription bill revision for a half-open UTC-ms period;
-- allocated to attempts only by a named, versioned rule at read time.
CREATE TABLE IF NOT EXISTS provider_invoices (
    invoice_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    digest TEXT NOT NULL,
    provider TEXT NOT NULL,
    product TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('usage', 'subscription')),
    currency TEXT NOT NULL CHECK (length(currency) = 3 AND currency = upper(currency)),
    amount TEXT NOT NULL,
    period_from_unix_ms INTEGER NOT NULL,
    period_to_unix_ms INTEGER NOT NULL CHECK (period_to_unix_ms > period_from_unix_ms),
    source TEXT NOT NULL,
    synthetic INTEGER NOT NULL CHECK (synthetic = 1),
    imported_unix_ms INTEGER NOT NULL,
    PRIMARY KEY (invoice_id, revision)
) STRICT;
-- A dated exchange-rate table version and its rates (`from` → `to`, one unit
-- of `from` in `to`), effective over half-open UTC-ms intervals.
CREATE TABLE IF NOT EXISTS fx_tables (
    table_id TEXT NOT NULL,
    version INTEGER NOT NULL CHECK (version > 0),
    digest TEXT NOT NULL,
    source TEXT NOT NULL,
    synthetic INTEGER NOT NULL CHECK (synthetic = 1),
    imported_unix_ms INTEGER NOT NULL,
    PRIMARY KEY (table_id, version)
) STRICT;
CREATE TABLE IF NOT EXISTS fx_rates (
    table_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    from_currency TEXT NOT NULL CHECK (length(from_currency) = 3 AND from_currency = upper(from_currency)),
    to_currency TEXT NOT NULL CHECK (length(to_currency) = 3 AND to_currency = upper(to_currency) AND to_currency <> from_currency),
    rate TEXT NOT NULL,
    effective_from_unix_ms INTEGER NOT NULL,
    effective_to_unix_ms INTEGER CHECK (effective_to_unix_ms IS NULL OR effective_to_unix_ms > effective_from_unix_ms),
    PRIMARY KEY (table_id, version, from_currency, to_currency, effective_from_unix_ms),
    FOREIGN KEY (table_id, version) REFERENCES fx_tables(table_id, version)
) STRICT;
CREATE TRIGGER IF NOT EXISTS provider_charges_append_only_u BEFORE UPDATE ON provider_charges BEGIN SELECT RAISE(ABORT, 'provider charges are append-only'); END;
CREATE TRIGGER IF NOT EXISTS provider_charges_append_only_d BEFORE DELETE ON provider_charges BEGIN SELECT RAISE(ABORT, 'provider charges are append-only'); END;
CREATE TRIGGER IF NOT EXISTS provider_invoices_append_only_u BEFORE UPDATE ON provider_invoices BEGIN SELECT RAISE(ABORT, 'provider invoices are append-only'); END;
CREATE TRIGGER IF NOT EXISTS provider_invoices_append_only_d BEFORE DELETE ON provider_invoices BEGIN SELECT RAISE(ABORT, 'provider invoices are append-only'); END;
CREATE TRIGGER IF NOT EXISTS fx_tables_append_only_u BEFORE UPDATE ON fx_tables BEGIN SELECT RAISE(ABORT, 'exchange-rate tables are append-only'); END;
CREATE TRIGGER IF NOT EXISTS fx_tables_append_only_d BEFORE DELETE ON fx_tables BEGIN SELECT RAISE(ABORT, 'exchange-rate tables are append-only'); END;
CREATE TRIGGER IF NOT EXISTS fx_rates_append_only_u BEFORE UPDATE ON fx_rates BEGIN SELECT RAISE(ABORT, 'exchange-rate tables are append-only'); END;
CREATE TRIGGER IF NOT EXISTS fx_rates_append_only_d BEFORE DELETE ON fx_rates BEGIN SELECT RAISE(ABORT, 'exchange-rate tables are append-only'); END;
