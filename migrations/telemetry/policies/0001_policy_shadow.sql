-- Stream `policies` 1 (docs/telemetry/contracts-evaluation.md §9, TM4.7):
-- what each configured assignment policy would have chosen at a canonical
-- dispatch decision, beside the canonical choice. Written by admission after
-- the canonical commit; never read to choose, grant or reserve. Identifiers,
-- policy parameters, exact ppm probabilities and seeds only.
CREATE TABLE IF NOT EXISTS policy_shadow_decisions (
    attempt_id TEXT NOT NULL,
    policy_digest TEXT NOT NULL CHECK (length(policy_digest) = 71),
    policy TEXT NOT NULL CHECK (policy IN ('deterministic.v1', 'uniform.v1', 'epsilon.v1', 'thompson.v1')),
    spec TEXT NOT NULL CHECK (json_valid(spec)),
    settings_revision INTEGER NOT NULL CHECK (settings_revision > 0),
    mode TEXT NOT NULL CHECK (mode IN ('shadow', 'suggest', 'assign')),
    task_id TEXT NOT NULL,
    task_class TEXT NOT NULL,
    -- `[{configuration_id, probability_ppm, excluded?}]` over the approved arms, in evaluation order.
    arms TEXT NOT NULL CHECK (json_valid(arms)),
    suggested_configuration_id TEXT,
    abstained TEXT,
    actual_configuration_id TEXT NOT NULL,
    seed TEXT NOT NULL CHECK (length(seed) = 16),
    draw_ppm INTEGER NOT NULL CHECK (draw_ppm BETWEEN 0 AND 999999),
    recorded_unix_ms INTEGER NOT NULL,
    PRIMARY KEY (attempt_id, policy_digest),
    CHECK ((suggested_configuration_id IS NULL) = (abstained IS NOT NULL))
) STRICT;
