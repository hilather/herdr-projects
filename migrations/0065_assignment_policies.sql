-- Propensity-logged assignment policies (TM4.7,
-- docs/telemetry/contracts-evaluation.md §9; factory plan F2.5 for the
-- randomized-assignment authority). Numbered 0065 beside TM4.6's 0064; the
-- steward renumbers if needed. Nothing here grants a launch: an assignment
-- policy only chooses among profiles automatic admission already found
-- approved, and every ordinary reservation check still applies.
--
-- An owner-signed `randomized_assignment` grant: the policies it permits,
-- the largest exploration share, the arms a policy may assign with a cap
-- each, a validity interval and the effects it never grants. Verified with
-- the owner's key at import and again when the operator enables assignment;
-- the exact signed bytes are kept and every field is re-derived from them.
CREATE TABLE assignment_authority_grants (
    grant_id TEXT PRIMARY KEY CHECK (length(grant_id) = 71 AND substr(grant_id, 1, 7) = 'sha256:'),
    raw_bytes BLOB NOT NULL CHECK (length(raw_bytes) BETWEEN 1 AND 65536),
    signature BLOB NOT NULL CHECK (length(signature) BETWEEN 1 AND 8192),
    scope TEXT NOT NULL CHECK (scope = 'randomized_assignment'),
    issuer TEXT NOT NULL CHECK (issuer = 'owner'),
    project_store TEXT NOT NULL CHECK (length(project_store) BETWEEN 1 AND 4096),
    policies TEXT NOT NULL CHECK (json_valid(policies) AND json_array_length(policies) BETWEEN 1 AND 4),
    max_exploration_ppm INTEGER NOT NULL CHECK (max_exploration_ppm BETWEEN 0 AND 1000000),
    arm_caps TEXT NOT NULL CHECK (json_valid(arm_caps) AND json_type(arm_caps) = 'object'),
    valid_from_unix_ms INTEGER NOT NULL CHECK (valid_from_unix_ms >= 0),
    expires_unix_ms INTEGER NOT NULL,
    authority_revision INTEGER NOT NULL CHECK (authority_revision > 0),
    authority_digest TEXT NOT NULL CHECK (length(authority_digest) = 64),
    installed_unix_ms INTEGER NOT NULL CHECK (installed_unix_ms >= 0),
    CHECK (expires_unix_ms > valid_from_unix_ms)
) STRICT;
CREATE TRIGGER assignment_authority_grants_no_update BEFORE UPDATE ON assignment_authority_grants
BEGIN SELECT RAISE(ABORT, 'assignment authority grant is immutable'); END;
CREATE TRIGGER assignment_authority_grants_no_delete BEFORE DELETE ON assignment_authority_grants
BEGIN SELECT RAISE(ABORT, 'assignment authority grant is immutable'); END;

-- The operator switch, append-only. No row (the default) is `off`. `shadow`
-- records what each policy would choose; `suggest` also makes the first
-- policy the coordinator's default suggestion; `assign` lets the first
-- policy choose, and only under an owner-signed grant.
CREATE TABLE assignment_policy_settings (
    revision INTEGER PRIMARY KEY CHECK (revision > 0),
    mode TEXT NOT NULL CHECK (mode IN ('off', 'shadow', 'suggest', 'assign')),
    policies TEXT NOT NULL CHECK (json_valid(policies) AND json_array_length(policies) BETWEEN 0 AND 8),
    grant_id TEXT REFERENCES assignment_authority_grants(grant_id),
    set_unix_ms INTEGER NOT NULL CHECK (set_unix_ms >= 0),
    CHECK ((mode = 'assign') = (grant_id IS NOT NULL)),
    CHECK (mode = 'off' OR json_array_length(policies) >= 1)
) STRICT;
CREATE TRIGGER assignment_policy_settings_no_update BEFORE UPDATE ON assignment_policy_settings
BEGIN SELECT RAISE(ABORT, 'assignment policy settings are append-only'); END;
CREATE TRIGGER assignment_policy_settings_no_delete BEFORE DELETE ON assignment_policy_settings
BEGIN SELECT RAISE(ABORT, 'assignment policy settings are append-only'); END;

-- The policy, seed and draw of a dispatch decision a policy made, in the
-- same transaction. `dispatch_decisions.policy`/`seed` keep their 0050
-- `NULL` check; this row carries them. The decision's logged probabilities
-- must sum to 1000000 with the chosen entry positive.
CREATE TABLE dispatch_policy_assignments (
    attempt_id TEXT PRIMARY KEY REFERENCES dispatch_decisions(attempt_id),
    settings_revision INTEGER NOT NULL REFERENCES assignment_policy_settings(revision),
    policy TEXT NOT NULL CHECK (policy IN ('deterministic.v1', 'uniform.v1', 'epsilon.v1', 'thompson.v1')),
    policy_digest TEXT NOT NULL CHECK (length(policy_digest) = 71 AND substr(policy_digest, 1, 7) = 'sha256:'),
    spec TEXT NOT NULL CHECK (json_valid(spec) AND length(spec) <= 8192),
    seed TEXT NOT NULL CHECK (length(seed) = 16),
    draw_ppm INTEGER NOT NULL CHECK (draw_ppm BETWEEN 0 AND 999999),
    grant_id TEXT NOT NULL REFERENCES assignment_authority_grants(grant_id),
    constraints TEXT NOT NULL CHECK (json_valid(constraints) AND json_array_length(constraints) <= 256),
    assigned_unix_ms INTEGER NOT NULL
) STRICT;
CREATE INDEX dispatch_policy_assignments_revision ON dispatch_policy_assignments(settings_revision);
CREATE TRIGGER dispatch_policy_assignments_no_update BEFORE UPDATE ON dispatch_policy_assignments
BEGIN SELECT RAISE(ABORT, 'policy assignment is immutable'); END;
CREATE TRIGGER dispatch_policy_assignments_no_delete BEFORE DELETE ON dispatch_policy_assignments
BEGIN SELECT RAISE(ABORT, 'policy assignment is immutable'); END;
CREATE TRIGGER dispatch_policy_assignments_probabilities BEFORE INSERT ON dispatch_policy_assignments
WHEN (SELECT coalesce(sum(json_extract(e.value, '$.probability_ppm')), 0) FROM dispatch_decisions d, json_each(d.eligible) e
      WHERE d.attempt_id = NEW.attempt_id) <> 1000000
  OR NOT EXISTS (SELECT 1 FROM dispatch_decisions d, json_each(d.eligible) e WHERE d.attempt_id = NEW.attempt_id
      AND json_extract(e.value, '$.status') = 'chosen' AND json_extract(e.value, '$.configuration_id') = d.chosen_configuration_id
      AND json_extract(e.value, '$.probability_ppm') > 0)
BEGIN SELECT RAISE(ABORT, 'policy assignment probabilities are invalid'); END;

UPDATE store_meta SET schema_version = 65;
PRAGMA user_version = 65;
