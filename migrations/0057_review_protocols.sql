-- Review protocols and experiments (TM3.4, docs/telemetry/contracts-review.md
-- §7). `protocol_log` extends the one ordering of `finding_log` and `fix_log`
-- (0056): its `seq` values take the next number of the same sequence, so a
-- protocol, a skeptical pass's prior-coverage cutoff, a preregistered
-- experiment and an experiment assignment each sit at one place relative to
-- every finding submission and triage decision. Every row is append-only.
-- Only the project owner (`operator:cli`, `operator_owner.v1`) registers or
-- assigns; nothing here opens, assigns or launches a review. Metadata and
-- identifiers only: protocols name challenge and failure classes as tokens,
-- never prompt or review text.
CREATE TABLE protocol_log (
    seq INTEGER PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('protocol_registered', 'pass_bound', 'experiment_registered', 'unit_assigned', 'unit_excluded')),
    principal TEXT NOT NULL CHECK (principal = 'operator:cli'),
    authority TEXT NOT NULL CHECK (authority = 'operator_owner.v1'),
    expected_seq INTEGER CHECK (expected_seq IS NULL OR expected_seq >= 0),
    recorded_unix_ms INTEGER NOT NULL
) STRICT;
-- A versioned review protocol (`review_protocol.v1`): the method, artifact
-- scope, reviewer role, assigned budget and outcome criteria an opportunity
-- run under it must share. A new version is a new identifier.
CREATE TABLE review_protocols (
    seq INTEGER PRIMARY KEY REFERENCES protocol_log(seq),
    protocol TEXT NOT NULL UNIQUE CHECK (length(protocol) BETWEEN 1 AND 64),
    kind TEXT NOT NULL CHECK (kind IN ('code', 'skeptical', 'security', 'test', 'architecture')),
    scope TEXT NOT NULL CHECK (scope IN ('candidate_diff', 'candidate_tree', 'contract_scope')),
    role TEXT NOT NULL CHECK (role IN ('gate', 'evaluation', 'advisory')),
    budget_ms INTEGER NOT NULL CHECK (budget_ms > 0),
    prior_disclosure TEXT NOT NULL CHECK (prior_disclosure IN ('withheld', 'disclosed')),
    evidence_min INTEGER NOT NULL CHECK (evidence_min BETWEEN 0 AND 64),
    min_severity TEXT NOT NULL CHECK (min_severity IN ('critical', 'high', 'medium', 'low', 'informational')),
    reviewer_configuration_id TEXT REFERENCES agent_configurations(configuration_id),
    definition_digest TEXT NOT NULL CHECK (length(definition_digest) = 71 AND substr(definition_digest, 1, 7) = 'sha256:'),
    canonical_json TEXT NOT NULL CHECK (json_valid(canonical_json) AND length(canonical_json) <= 8192)
) STRICT;
-- A second review (the pass) of one opportunity after the ordinary reviews
-- it names (`priors`), bound before its first session. `cutoff_seq` is the
-- ledger head when it was bound: findings validated by then are known.
-- `comparability` and `prior_coverage` are frozen from the priors' exact
-- candidates, scopes and statuses at binding.
CREATE TABLE skeptical_passes (
    seq INTEGER PRIMARY KEY REFERENCES protocol_log(seq),
    opportunity_id TEXT NOT NULL UNIQUE REFERENCES review_opportunities(opportunity_id),
    protocol_seq INTEGER NOT NULL REFERENCES review_protocols(seq),
    cutoff_seq INTEGER NOT NULL CHECK (cutoff_seq >= 0 AND cutoff_seq < seq),
    priors TEXT NOT NULL CHECK (json_valid(priors) AND json_array_length(priors) BETWEEN 1 AND 16),
    comparability TEXT NOT NULL CHECK (comparability IN ('same_artifact', 'changed_artifact', 'different_scope')),
    prior_coverage TEXT NOT NULL CHECK (prior_coverage IN ('complete', 'incomplete'))
) STRICT;
-- A preregistered experiment (`review_experiment.v1`), frozen when
-- registered: design, exact eligibility of a unit (a base review
-- opportunity), arms, assignment rule (randomized: recorded seed; matched:
-- owner-chosen arm within a block), outcome, horizon and minimum units.
CREATE TABLE review_experiments (
    seq INTEGER PRIMARY KEY REFERENCES protocol_log(seq),
    experiment TEXT NOT NULL UNIQUE CHECK (length(experiment) BETWEEN 1 AND 64),
    design TEXT NOT NULL CHECK (design IN ('randomized', 'matched')),
    seed TEXT CHECK (seed IS NULL OR length(seed) = 64),
    eligible_kind TEXT NOT NULL CHECK (eligible_kind IN ('code', 'skeptical', 'security', 'test', 'architecture')),
    eligible_scope TEXT NOT NULL CHECK (eligible_scope IN ('candidate_diff', 'candidate_tree', 'contract_scope')),
    -- Required production gates are never withheld to balance an experiment.
    eligible_role TEXT NOT NULL CHECK (eligible_role IN ('evaluation', 'advisory')),
    eligible_protocol TEXT NOT NULL CHECK (length(eligible_protocol) BETWEEN 1 AND 64),
    arms TEXT NOT NULL CHECK (json_valid(arms) AND json_array_length(arms) BETWEEN 2 AND 4),
    min_units INTEGER NOT NULL CHECK (min_units >= 2),
    horizon_ms INTEGER NOT NULL CHECK (horizon_ms > 0),
    definition_digest TEXT NOT NULL CHECK (length(definition_digest) = 71 AND substr(definition_digest, 1, 7) = 'sha256:'),
    canonical_json TEXT NOT NULL CHECK (json_valid(canonical_json) AND length(canonical_json) <= 8192),
    CHECK ((design = 'randomized') = (seed IS NOT NULL))
) STRICT;
-- One unit per exact artifact: a base opportunity assigned to an arm before
-- it has any completion.
CREATE TABLE experiment_units (
    seq INTEGER PRIMARY KEY REFERENCES protocol_log(seq),
    experiment_seq INTEGER NOT NULL REFERENCES review_experiments(seq),
    opportunity_id TEXT NOT NULL REFERENCES review_opportunities(opportunity_id),
    submission_id TEXT NOT NULL,
    arm TEXT NOT NULL CHECK (length(arm) BETWEEN 1 AND 64),
    block TEXT CHECK (block IS NULL OR length(block) BETWEEN 1 AND 64),
    UNIQUE (experiment_seq, opportunity_id),
    UNIQUE (experiment_seq, submission_id),
    UNIQUE (experiment_seq, block, arm)
) STRICT;
-- A unit excluded after assignment; it stays listed with its arm.
CREATE TABLE experiment_exclusions (
    seq INTEGER PRIMARY KEY REFERENCES protocol_log(seq),
    unit_seq INTEGER NOT NULL UNIQUE REFERENCES experiment_units(seq),
    reason TEXT NOT NULL CHECK (reason IN ('ineligible_discovered', 'artifact_withdrawn', 'protocol_violation', 'operator_error'))
) STRICT;
CREATE INDEX experiment_units_by_opportunity ON experiment_units(opportunity_id);
-- One ordering across the three ledgers.
DROP TRIGGER finding_log_one_order;
DROP TRIGGER fix_log_one_order;
CREATE TRIGGER finding_log_one_order AFTER INSERT ON finding_log
WHEN NEW.seq <= coalesce((SELECT max(seq) FROM fix_log), 0) OR NEW.seq <= coalesce((SELECT max(seq) FROM protocol_log), 0)
BEGIN SELECT RAISE(ABORT, 'finding and fix history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER fix_log_one_order AFTER INSERT ON fix_log
WHEN NEW.seq <= coalesce((SELECT max(seq) FROM finding_log), 0) OR NEW.seq <= coalesce((SELECT max(seq) FROM protocol_log), 0)
    OR EXISTS (SELECT 1 FROM fix_log WHERE seq > NEW.seq)
BEGIN SELECT RAISE(ABORT, 'finding and fix history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER protocol_log_one_order AFTER INSERT ON protocol_log
WHEN NEW.seq <= coalesce((SELECT max(seq) FROM finding_log), 0) OR NEW.seq <= coalesce((SELECT max(seq) FROM fix_log), 0)
    OR EXISTS (SELECT 1 FROM protocol_log WHERE seq > NEW.seq)
BEGIN SELECT RAISE(ABORT, 'finding and fix history are one ordering: seq must follow the ledger head'); END;
CREATE TRIGGER protocol_log_no_update BEFORE UPDATE ON protocol_log BEGIN SELECT RAISE(ABORT, 'protocol history is append-only'); END;
CREATE TRIGGER protocol_log_no_delete BEFORE DELETE ON protocol_log BEGIN SELECT RAISE(ABORT, 'protocol history is append-only'); END;
CREATE TRIGGER review_protocols_no_update BEFORE UPDATE ON review_protocols BEGIN SELECT RAISE(ABORT, 'protocol history is append-only'); END;
CREATE TRIGGER review_protocols_no_delete BEFORE DELETE ON review_protocols BEGIN SELECT RAISE(ABORT, 'protocol history is append-only'); END;
CREATE TRIGGER skeptical_passes_no_update BEFORE UPDATE ON skeptical_passes BEGIN SELECT RAISE(ABORT, 'protocol history is append-only'); END;
CREATE TRIGGER skeptical_passes_no_delete BEFORE DELETE ON skeptical_passes BEGIN SELECT RAISE(ABORT, 'protocol history is append-only'); END;
CREATE TRIGGER review_experiments_no_update BEFORE UPDATE ON review_experiments BEGIN SELECT RAISE(ABORT, 'protocol history is append-only'); END;
CREATE TRIGGER review_experiments_no_delete BEFORE DELETE ON review_experiments BEGIN SELECT RAISE(ABORT, 'protocol history is append-only'); END;
CREATE TRIGGER experiment_units_no_update BEFORE UPDATE ON experiment_units BEGIN SELECT RAISE(ABORT, 'protocol history is append-only'); END;
CREATE TRIGGER experiment_units_no_delete BEFORE DELETE ON experiment_units BEGIN SELECT RAISE(ABORT, 'protocol history is append-only'); END;
CREATE TRIGGER experiment_exclusions_no_update BEFORE UPDATE ON experiment_exclusions BEGIN SELECT RAISE(ABORT, 'protocol history is append-only'); END;
CREATE TRIGGER experiment_exclusions_no_delete BEFORE DELETE ON experiment_exclusions BEGIN SELECT RAISE(ABORT, 'protocol history is append-only'); END;
-- Each detail row needs its own history row of the matching kind.
CREATE TRIGGER review_protocols_kind BEFORE INSERT ON review_protocols
WHEN NOT EXISTS (SELECT 1 FROM protocol_log l WHERE l.seq = NEW.seq AND l.kind = 'protocol_registered')
BEGIN SELECT RAISE(ABORT, 'protocol registry needs its history row'); END;
CREATE TRIGGER skeptical_passes_kind BEFORE INSERT ON skeptical_passes
WHEN NOT EXISTS (SELECT 1 FROM protocol_log l WHERE l.seq = NEW.seq AND l.kind = 'pass_bound')
BEGIN SELECT RAISE(ABORT, 'protocol registry needs its history row'); END;
CREATE TRIGGER review_experiments_kind BEFORE INSERT ON review_experiments
WHEN NOT EXISTS (SELECT 1 FROM protocol_log l WHERE l.seq = NEW.seq AND l.kind = 'experiment_registered')
BEGIN SELECT RAISE(ABORT, 'protocol registry needs its history row'); END;
CREATE TRIGGER experiment_units_kind BEFORE INSERT ON experiment_units
WHEN NOT EXISTS (SELECT 1 FROM protocol_log l WHERE l.seq = NEW.seq AND l.kind = 'unit_assigned')
BEGIN SELECT RAISE(ABORT, 'protocol registry needs its history row'); END;
CREATE TRIGGER experiment_exclusions_kind BEFORE INSERT ON experiment_exclusions
WHEN NOT EXISTS (SELECT 1 FROM protocol_log l WHERE l.seq = NEW.seq AND l.kind = 'unit_excluded')
BEGIN SELECT RAISE(ABORT, 'protocol registry needs its history row'); END;
-- A pass runs under its protocol's method, scope, role and budget, and is
-- bound before any session of it starts.
CREATE TRIGGER skeptical_passes_exact BEFORE INSERT ON skeptical_passes
WHEN NOT EXISTS (SELECT 1 FROM review_opportunities o JOIN review_protocols p ON p.seq = NEW.protocol_seq
        WHERE o.opportunity_id = NEW.opportunity_id AND o.protocol = p.protocol AND o.kind = p.kind AND o.scope = p.scope
            AND o.role = p.role AND o.budget_ms = p.budget_ms)
    OR EXISTS (SELECT 1 FROM review_sessions s WHERE s.opportunity_id = NEW.opportunity_id)
BEGIN SELECT RAISE(ABORT, 'a pass runs under its registered protocol and is bound before its review starts'); END;
-- A unit is an exactly eligible opportunity of its own submission, assigned
-- to a registered arm before any completion (the outcome) exists.
CREATE TRIGGER experiment_units_before_outcome BEFORE INSERT ON experiment_units
WHEN NOT EXISTS (SELECT 1 FROM review_opportunities o JOIN review_experiments e ON e.seq = NEW.experiment_seq
        WHERE o.opportunity_id = NEW.opportunity_id AND o.submission_id = NEW.submission_id AND o.kind = e.eligible_kind
            AND o.scope = e.eligible_scope AND o.role = e.eligible_role AND o.protocol = e.eligible_protocol
            AND ((e.design = 'randomized') = (NEW.block IS NULL))
            AND EXISTS (SELECT 1 FROM json_each(e.arms) a WHERE json_extract(a.value, '$.arm') = NEW.arm))
    OR EXISTS (SELECT 1 FROM review_sessions s JOIN review_completions c ON c.session_id = s.session_id WHERE s.opportunity_id = NEW.opportunity_id)
BEGIN SELECT RAISE(ABORT, 'an experiment unit is an eligible opportunity assigned to a registered arm before any outcome'); END;
UPDATE store_meta SET schema_version = 57;
PRAGMA user_version = 57;
