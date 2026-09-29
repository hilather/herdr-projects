-- Stream `accounting` 6 (docs/telemetry/contracts-accounting.md §3–§5): consume
-- lane A's A4 Codex metadata (ingest 0004: `rollout_metadata`,
-- `codex_usage_times`, `codex_rate_limit_windows`, all certified `fixture`).
-- Identifiers, enums, counters and times only; no content (contracts §7).
-- `session_graph` and `quota_observations` (versions 2 and 4) are superseded
-- by the two tables below and no longer written or read; they are left as
-- they were (nothing is dropped).

-- §3: a child session links to a collected parent only on its native parent
-- id (`subagent_parent_thread_id`, else `forked_from_id`), never by inference.
-- A linked child is never added to its parent's total; a fork's inclusion is
-- unknown (it may replay its parent's records). Derived: rebuilt by each sync.
CREATE TABLE IF NOT EXISTS session_nodes (
    path_digest TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('primary', 'guardian', 'subagent', 'fork')),
    linkage TEXT NOT NULL CHECK (linkage IN ('root', 'included', 'linked_child', 'unlinked_child', 'unresolved')),
    parent_path_digest TEXT,
    evidence TEXT CHECK (evidence IS NULL OR evidence = 'same_session_prefix'),
    attempt_id TEXT,
    inclusive_total INTEGER CHECK (inclusive_total IS NULL OR inclusive_total >= 0),
    -- The collected parent session of a `linked_child` and the native field linking it.
    parent_session_id TEXT,
    link_basis TEXT CHECK (link_basis IS NULL OR link_basis IN ('parent_thread_id', 'forked_from_id')),
    -- Why an `unlinked_child` has no parent, and the parent id it names when not collected.
    parent_reason TEXT CHECK (parent_reason IS NULL OR parent_reason IN ('no_native_parent_evidence', 'parent_not_collected')),
    claimed_parent_session_id TEXT,
    -- The session names a `forked_from_id` (its records may replay the parent's).
    forked INTEGER NOT NULL DEFAULT 0 CHECK (forked IN (0, 1)),
    CHECK ((linkage = 'included') = (parent_path_digest IS NOT NULL)),
    CHECK ((parent_path_digest IS NULL) = (evidence IS NULL)),
    CHECK ((linkage = 'linked_child') = (parent_session_id IS NOT NULL)),
    CHECK ((parent_session_id IS NULL) = (link_basis IS NULL)),
    CHECK ((linkage = 'unlinked_child') = (parent_reason IS NOT NULL))
) STRICT;

-- §5: the secondary window is a second `window_kind` per snapshot, so the key
-- gains the kind. `not_reported`: the snapshot's window was `null`.
-- `rate_limit_reached_type` is kept as evidence only (semantics not certified).
CREATE TABLE IF NOT EXISTS quota_window_observations (
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
        'used_decreased_without_reset', 'window_regressed', 'window_conflict', 'not_reported')),
    window_id TEXT,
    rate_limit_reached_type TEXT,
    PRIMARY KEY (session_id, ordinal, window_kind),
    CHECK (trust <> 'trusted' OR (window_id IS NOT NULL AND used IS NOT NULL AND remaining IS NOT NULL))
) STRICT;
CREATE INDEX IF NOT EXISTS quota_window_observations_account ON quota_window_observations(account, limit_id, window_kind, observed_unix_ms);

-- §4: per valuation of a revision appended from this version on, how its
-- usage interval was bounded (`record_time` or `session_start..first_observed`)
-- and the provider check of a priced entry (`matched` or `provider_unverified`).
-- Earlier revisions have no rows here and read back unchanged. A side table,
-- not new `valuations` columns, so re-running this migration is harmless.
CREATE TABLE IF NOT EXISTS valuation_bases (
    revision INTEGER NOT NULL REFERENCES valuation_revisions(revision),
    entry_id TEXT NOT NULL,
    usage_basis TEXT CHECK (usage_basis IS NULL OR usage_basis IN ('record_time', 'session_start..first_observed')),
    provider_check TEXT CHECK (provider_check IS NULL OR provider_check IN ('matched', 'provider_unverified')),
    PRIMARY KEY (revision, entry_id)
) STRICT;
CREATE TRIGGER IF NOT EXISTS valuation_bases_append_only_u BEFORE UPDATE ON valuation_bases BEGIN SELECT RAISE(ABORT, 'valuations are append-only'); END;
CREATE TRIGGER IF NOT EXISTS valuation_bases_append_only_d BEFORE DELETE ON valuation_bases BEGIN SELECT RAISE(ABORT, 'valuations are append-only'); END;

-- A ledger synced before this version has no nodes or window observations: it
-- reads as not synced until the next sync rebuilds everything.
DELETE FROM usage_ledger;
