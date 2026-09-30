-- TM4.5 health stream (docs/telemetry/contracts-health.md §4): health-rule
-- evaluations, per-rule state and deduplicated alerts. Derived only from the
-- query service and lane read paths; never read to grant launch, change
-- budgets, profiles or model access, or accept results. Re-runnable.
-- One row per evaluation pass (`health evaluate` or the ticker); the newest
-- `KEEP_EVALUATIONS` are kept.
CREATE TABLE IF NOT EXISTS health_evaluations (
    evaluation INTEGER PRIMARY KEY AUTOINCREMENT,
    evaluated_unix_ms INTEGER NOT NULL,
    rules_version TEXT NOT NULL,
    source TEXT NOT NULL CHECK (source IN ('cli', 'tick')),
    summary TEXT NOT NULL CHECK (json_valid(summary))
) STRICT;
CREATE INDEX IF NOT EXISTS health_evaluations_time ON health_evaluations(evaluated_unix_ms);
-- The latest state of each rule key (the canonical JSON of its bounded
-- labels). `known` records whether the rule ever had its source (an outage
-- alerts only after that); `suppressed` counts conditions held back by the cooldown.
CREATE TABLE IF NOT EXISTS health_rule_states (
    rule_key TEXT PRIMARY KEY CHECK (json_valid(rule_key)),
    rule TEXT NOT NULL CHECK (length(rule) BETWEEN 1 AND 64),
    state TEXT NOT NULL CHECK (state IN ('ok', 'warn', 'critical', 'unknown')),
    reasons TEXT NOT NULL CHECK (json_valid(reasons)),
    known INTEGER NOT NULL CHECK (known IN (0, 1)),
    suppressed INTEGER NOT NULL DEFAULT 0 CHECK (suppressed >= 0),
    evaluated_unix_ms INTEGER NOT NULL
) STRICT;
-- One alert per condition episode. At most one open alert per rule key
-- (deduplication); a condition that returns within the rule's cooldown after
-- its alert resolved opens none. Labels are bounded (project, family, rule,
-- service kind, role): never task, attempt, session or account identities.
CREATE TABLE IF NOT EXISTS health_alerts (
    alert_id INTEGER PRIMARY KEY AUTOINCREMENT,
    rule_key TEXT NOT NULL CHECK (json_valid(rule_key)),
    rule TEXT NOT NULL CHECK (length(rule) BETWEEN 1 AND 64),
    labels TEXT NOT NULL CHECK (json_valid(labels)),
    state TEXT NOT NULL CHECK (state IN ('warn', 'critical', 'unknown')),
    reasons TEXT NOT NULL CHECK (json_valid(reasons)),
    metric TEXT NOT NULL CHECK (json_valid(metric)),
    evidence_window TEXT NOT NULL CHECK (json_valid(evidence_window)),
    evidence TEXT NOT NULL CHECK (json_valid(evidence)),
    rules_version TEXT NOT NULL,
    opened_unix_ms INTEGER NOT NULL,
    last_seen_unix_ms INTEGER NOT NULL CHECK (last_seen_unix_ms >= opened_unix_ms),
    occurrences INTEGER NOT NULL CHECK (occurrences > 0),
    resolved_unix_ms INTEGER CHECK (resolved_unix_ms IS NULL OR resolved_unix_ms >= opened_unix_ms),
    notified_unix_ms INTEGER,
    notice_id TEXT
) STRICT;
CREATE UNIQUE INDEX IF NOT EXISTS health_alerts_open ON health_alerts(rule_key) WHERE resolved_unix_ms IS NULL;
CREATE INDEX IF NOT EXISTS health_alerts_key ON health_alerts(rule_key, alert_id);
