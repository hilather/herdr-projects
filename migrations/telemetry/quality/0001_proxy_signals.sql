-- Stream `quality` 1: TM3.7 proxy signals (docs/telemetry/contracts-quality.md).
-- Analytics only: never read to accept, verify, integrate or launch.
-- Canonical IDs are copied by value; no content, paths or diff text.
-- IF NOT EXISTS: a sidecar whose `telemetry_streams` was lost re-runs this.
CREATE TABLE IF NOT EXISTS proxy_signals (
    kind TEXT NOT NULL CHECK (kind = 'first_candidate_ci'),
    task_id TEXT NOT NULL,
    submission_id TEXT NOT NULL,
    attempt_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    policy_digest TEXT NOT NULL,
    base_oid TEXT NOT NULL,
    candidate_oid TEXT NOT NULL,
    ci_state TEXT NOT NULL CHECK (ci_state IN ('accepted', 'rejected')),
    verified_unix_ms INTEGER NOT NULL,
    tests_added_lines INTEGER CHECK (tests_added_lines >= 0),
    tests_deleted_lines INTEGER CHECK (tests_deleted_lines >= 0),
    tests_binary_files INTEGER CHECK (tests_binary_files >= 0),
    weakening TEXT NOT NULL CHECK (weakening IN ('flagged', 'clear', 'unavailable')),
    weakening_reason TEXT,
    weakening_rule TEXT NOT NULL,
    source_trust TEXT NOT NULL CHECK (source_trust = 'proxy_observed'),
    observed_unix_ms INTEGER NOT NULL,
    PRIMARY KEY (kind, task_id),
    CHECK ((weakening = 'unavailable') = (tests_deleted_lines IS NULL)),
    CHECK ((weakening = 'unavailable') = (weakening_reason IS NOT NULL))
) STRICT;
