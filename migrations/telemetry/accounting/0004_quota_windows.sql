-- Stream `accounting` 4 (docs/telemetry/contracts-accounting.md §5): quota
-- window observations and the windows they identify, rebuilt whole with the
-- usage ledger by each sync from `codex_rate_limits` (read by SQL only).
-- Identifiers, enums, native percent values and timestamps only; no content
-- (contracts §7). Values are exact decimal strings in the native unit.
-- One row per collected rate-limit snapshot. `account` is the rollout's
-- `home_digest` (one execution home = one login); `window_kind` is the provider's
-- window kind (only `primary` is collected). A reset starts a new window, so a
-- snapshot never subtracts across one; a `used` below the window's high-water
-- mark without a reset is flagged, never subtracted.
CREATE TABLE IF NOT EXISTS quota_observations (
    session_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    service TEXT NOT NULL CHECK (service = 'codex'),
    account TEXT,
    limit_id TEXT,
    window_kind TEXT NOT NULL CHECK (window_kind IN ('primary', 'secondary')),
    unit TEXT NOT NULL CHECK (unit = 'percent'),
    window_minutes INTEGER,
    resets_unix_ms INTEGER,
    used TEXT,
    remaining TEXT,
    plan_type TEXT,
    observed_unix_ms INTEGER NOT NULL,
    trust TEXT NOT NULL CHECK (trust IN ('trusted', 'incomplete', 'unparseable', 'account_ambiguous',
        'used_decreased_without_reset', 'window_regressed', 'window_conflict')),
    window_id TEXT,
    PRIMARY KEY (session_id, ordinal),
    CHECK (trust <> 'trusted' OR (window_id IS NOT NULL AND used IS NOT NULL AND remaining IS NOT NULL))
) STRICT;
CREATE INDEX IF NOT EXISTS quota_observations_account ON quota_observations(account, limit_id, window_kind, observed_unix_ms);
-- One row per window identity (service, account, limit, window kind, reset
-- time). `start_evidence`: the account's first snapshot of the limit, or a
-- later reset time (`reset_elapsed` when observed at or after the previous
-- reset, `reset_moved` before it). `used` is the high-water trusted value;
-- `observed_increase` = `used` − `first_used`, account-wide (never attributed
-- to a task: the window's invocation scope is not certified).
CREATE TABLE IF NOT EXISTS quota_windows (
    window_id TEXT PRIMARY KEY,
    service TEXT NOT NULL CHECK (service = 'codex'),
    account TEXT NOT NULL,
    limit_id TEXT NOT NULL,
    window_kind TEXT NOT NULL CHECK (window_kind IN ('primary', 'secondary')),
    unit TEXT NOT NULL CHECK (unit = 'percent'),
    window_minutes INTEGER NOT NULL CHECK (window_minutes > 0),
    window_start_unix_ms INTEGER NOT NULL,
    resets_unix_ms INTEGER NOT NULL CHECK (resets_unix_ms > window_start_unix_ms),
    start_evidence TEXT NOT NULL CHECK (start_evidence IN ('first_observation', 'reset_elapsed', 'reset_moved')),
    first_observed_unix_ms INTEGER NOT NULL,
    last_observed_unix_ms INTEGER NOT NULL CHECK (last_observed_unix_ms >= first_observed_unix_ms),
    first_used TEXT NOT NULL,
    used TEXT NOT NULL,
    remaining TEXT NOT NULL,
    observed_increase TEXT NOT NULL,
    plan_type TEXT,
    observations INTEGER NOT NULL CHECK (observations > 0),
    flagged INTEGER NOT NULL CHECK (flagged >= 0)
) STRICT;
-- A ledger synced before this version has no quota windows: it reads as not
-- synced until the next sync rebuilds everything.
DELETE FROM usage_ledger;
