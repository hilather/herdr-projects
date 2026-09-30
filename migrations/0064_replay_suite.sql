-- Replay evaluation suite (TM4.6, docs/telemetry/contracts-replay.md).
-- The replay owner (`operator:cli`, recorded as `replay_owner.v1`) turns
-- accepted historical tasks into versioned replay cases: the source task and
-- its accepted submission, the contract revision, the base commit before the
-- accepted change, a content-addressed reference to hidden checks held
-- outside the project (never their content), the reference solution's oid,
-- the classification and a metadata-only contamination scan. A replay run
-- creates ordinary tasks, registered here as replay candidates; they launch,
-- verify and consume budget like any other work but never integrate and
-- never release dependents. Every row is append-only.
CREATE TABLE replay_log (
    seq INTEGER PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('suite', 'retired', 'run')),
    principal TEXT NOT NULL CHECK (principal = 'operator:cli'),
    authority TEXT NOT NULL CHECK (authority = 'replay_owner.v1'),
    recorded_unix_ms INTEGER NOT NULL
) STRICT;
-- One immutable suite version with its extraction exclusions (counts only).
CREATE TABLE replay_suites (
    seq INTEGER PRIMARY KEY REFERENCES replay_log(seq),
    suite_version TEXT NOT NULL UNIQUE CHECK (length(suite_version) BETWEEN 1 AND 32),
    extractor TEXT NOT NULL CHECK (length(extractor) BETWEEN 1 AND 64),
    manifest_digest TEXT NOT NULL CHECK (length(manifest_digest) = 71 AND substr(manifest_digest, 1, 7) = 'sha256:'),
    exclusions TEXT NOT NULL CHECK (json_valid(exclusions) AND json_type(exclusions) = 'object' AND length(exclusions) <= 4096)
) STRICT;
-- A case: recorded with its suite, in the suite's own transaction.
CREATE TABLE replay_cases (
    seq INTEGER NOT NULL REFERENCES replay_suites(seq),
    suite_version TEXT NOT NULL REFERENCES replay_suites(suite_version),
    case_id TEXT NOT NULL CHECK (length(case_id) BETWEEN 1 AND 160),
    ordinal INTEGER NOT NULL CHECK (ordinal BETWEEN 1 AND 4096),
    source_task_id TEXT NOT NULL REFERENCES tasks(id),
    source_submission_id TEXT NOT NULL REFERENCES result_submissions(submission_id),
    source_result_id TEXT NOT NULL REFERENCES verified_results(result_id),
    contract_revision INTEGER NOT NULL CHECK (contract_revision > 0),
    contract_digest TEXT NOT NULL CHECK (length(contract_digest) = 64),
    repository TEXT NOT NULL CHECK (length(repository) BETWEEN 1 AND 4096),
    object_format TEXT NOT NULL CHECK (object_format IN ('sha1', 'sha256')),
    base_oid TEXT NOT NULL,
    reference_oid TEXT NOT NULL,
    integrated_oid TEXT NOT NULL,
    hidden_check_ref TEXT NOT NULL CHECK (length(hidden_check_ref) = 71 AND substr(hidden_check_ref, 1, 7) = 'sha256:'),
    hidden_checks TEXT NOT NULL CHECK (json_valid(hidden_checks) AND json_type(hidden_checks) = 'array' AND json_array_length(hidden_checks) BETWEEN 1 AND 8),
    solution_paths TEXT NOT NULL CHECK (json_valid(solution_paths) AND json_type(solution_paths) = 'array' AND json_array_length(solution_paths) BETWEEN 1 AND 64),
    classification TEXT NOT NULL CHECK (json_valid(classification) AND json_type(classification) = 'object'),
    stratum TEXT NOT NULL CHECK (length(stratum) BETWEEN 1 AND 64),
    contamination TEXT NOT NULL CHECK (json_valid(contamination) AND json_type(contamination) = 'object' AND length(contamination) <= 8192),
    status TEXT NOT NULL CHECK (status IN ('eligible', 'contaminated')),
    PRIMARY KEY (suite_version, case_id),
    UNIQUE (suite_version, ordinal),
    CHECK ((object_format = 'sha1' AND length(base_oid) = 40 AND length(reference_oid) = 40 AND length(integrated_oid) = 40)
        OR (object_format = 'sha256' AND length(base_oid) = 64 AND length(reference_oid) = 64 AND length(integrated_oid) = 64))
) STRICT;
-- Retires one case whose checks no longer apply; the case stays readable.
CREATE TABLE replay_retirements (
    seq INTEGER PRIMARY KEY REFERENCES replay_log(seq),
    suite_version TEXT NOT NULL,
    case_id TEXT NOT NULL,
    reason TEXT NOT NULL CHECK (length(reason) BETWEEN 1 AND 160),
    UNIQUE (suite_version, case_id),
    FOREIGN KEY (suite_version, case_id) REFERENCES replay_cases(suite_version, case_id)
) STRICT;
-- One replay run: a configuration label over a reproducible subset.
CREATE TABLE replay_runs (
    seq INTEGER PRIMARY KEY REFERENCES replay_log(seq),
    run_id TEXT NOT NULL UNIQUE CHECK (length(run_id) = 71 AND substr(run_id, 1, 7) = 'sha256:'),
    suite_version TEXT NOT NULL REFERENCES replay_suites(suite_version),
    configuration TEXT NOT NULL CHECK (length(configuration) BETWEEN 1 AND 64),
    subset TEXT NOT NULL CHECK (length(subset) BETWEEN 1 AND 64),
    seed TEXT NOT NULL CHECK (length(seed) BETWEEN 1 AND 64),
    cases TEXT NOT NULL CHECK (json_valid(cases) AND json_type(cases) = 'array' AND json_array_length(cases) BETWEEN 1 AND 256)
) STRICT;
-- A replay candidate: an ordinary task created by a run for one case. It is
-- an evaluation artefact: it never integrates and never releases dependents.
CREATE TABLE replay_candidates (
    task_id TEXT PRIMARY KEY REFERENCES tasks(id),
    run_id TEXT NOT NULL REFERENCES replay_runs(run_id),
    suite_version TEXT NOT NULL,
    case_id TEXT NOT NULL,
    repository TEXT NOT NULL CHECK (length(repository) BETWEEN 1 AND 4096),
    registered_unix_ms INTEGER NOT NULL,
    UNIQUE (run_id, case_id),
    FOREIGN KEY (suite_version, case_id) REFERENCES replay_cases(suite_version, case_id)
) STRICT;
CREATE INDEX replay_candidates_by_run ON replay_candidates(run_id, case_id);
CREATE TRIGGER replay_log_no_update BEFORE UPDATE ON replay_log BEGIN SELECT RAISE(ABORT, 'replay history is append-only'); END;
CREATE TRIGGER replay_log_no_delete BEFORE DELETE ON replay_log BEGIN SELECT RAISE(ABORT, 'replay history is append-only'); END;
CREATE TRIGGER replay_suites_no_update BEFORE UPDATE ON replay_suites BEGIN SELECT RAISE(ABORT, 'replay history is append-only'); END;
CREATE TRIGGER replay_suites_no_delete BEFORE DELETE ON replay_suites BEGIN SELECT RAISE(ABORT, 'replay history is append-only'); END;
CREATE TRIGGER replay_cases_no_update BEFORE UPDATE ON replay_cases BEGIN SELECT RAISE(ABORT, 'replay history is append-only'); END;
CREATE TRIGGER replay_cases_no_delete BEFORE DELETE ON replay_cases BEGIN SELECT RAISE(ABORT, 'replay history is append-only'); END;
CREATE TRIGGER replay_retirements_no_update BEFORE UPDATE ON replay_retirements BEGIN SELECT RAISE(ABORT, 'replay history is append-only'); END;
CREATE TRIGGER replay_retirements_no_delete BEFORE DELETE ON replay_retirements BEGIN SELECT RAISE(ABORT, 'replay history is append-only'); END;
CREATE TRIGGER replay_runs_no_update BEFORE UPDATE ON replay_runs BEGIN SELECT RAISE(ABORT, 'replay history is append-only'); END;
CREATE TRIGGER replay_runs_no_delete BEFORE DELETE ON replay_runs BEGIN SELECT RAISE(ABORT, 'replay history is append-only'); END;
CREATE TRIGGER replay_candidates_no_update BEFORE UPDATE ON replay_candidates BEGIN SELECT RAISE(ABORT, 'replay history is append-only'); END;
CREATE TRIGGER replay_candidates_no_delete BEFORE DELETE ON replay_candidates BEGIN SELECT RAISE(ABORT, 'replay history is append-only'); END;
-- Each detail row needs its own history row of the matching kind.
CREATE TRIGGER replay_suites_kind BEFORE INSERT ON replay_suites
WHEN NOT EXISTS (SELECT 1 FROM replay_log l WHERE l.seq = NEW.seq AND l.kind = 'suite')
BEGIN SELECT RAISE(ABORT, 'a replay suite needs its own suite history row'); END;
CREATE TRIGGER replay_cases_with_suite BEFORE INSERT ON replay_cases
WHEN NEW.seq <> (SELECT max(seq) FROM replay_log) OR NOT EXISTS (SELECT 1 FROM replay_suites s WHERE s.seq = NEW.seq AND s.suite_version = NEW.suite_version)
BEGIN SELECT RAISE(ABORT, 'replay cases are recorded with their suite version'); END;
CREATE TRIGGER replay_retirements_kind BEFORE INSERT ON replay_retirements
WHEN NOT EXISTS (SELECT 1 FROM replay_log l WHERE l.seq = NEW.seq AND l.kind = 'retired')
BEGIN SELECT RAISE(ABORT, 'a retirement needs its own retired history row'); END;
CREATE TRIGGER replay_runs_kind BEFORE INSERT ON replay_runs
WHEN NOT EXISTS (SELECT 1 FROM replay_log l WHERE l.seq = NEW.seq AND l.kind = 'run')
BEGIN SELECT RAISE(ABORT, 'a replay run needs its own run history row'); END;
-- A run draws only eligible, unretired cases of its suite.
CREATE TRIGGER replay_candidates_eligible BEFORE INSERT ON replay_candidates
WHEN NOT EXISTS (SELECT 1 FROM replay_runs r, json_each(r.cases) j WHERE r.run_id = NEW.run_id AND r.suite_version = NEW.suite_version AND j.value = NEW.case_id)
  OR NOT EXISTS (SELECT 1 FROM replay_cases c WHERE c.suite_version = NEW.suite_version AND c.case_id = NEW.case_id AND c.status = 'eligible')
  OR EXISTS (SELECT 1 FROM replay_retirements x WHERE x.suite_version = NEW.suite_version AND x.case_id = NEW.case_id)
BEGIN SELECT RAISE(ABORT, 'a replay candidate draws an eligible, unretired case of its run'); END;
-- Registered on a fresh task: before any contract, attempt, submission or dependent.
CREATE TRIGGER replay_candidates_fresh BEFORE INSERT ON replay_candidates
WHEN EXISTS (SELECT 1 FROM task_contracts c WHERE c.task_id = NEW.task_id)
  OR EXISTS (SELECT 1 FROM attempts a WHERE a.task_id = NEW.task_id)
  OR EXISTS (SELECT 1 FROM result_submissions s WHERE s.task_id = NEW.task_id)
  OR EXISTS (SELECT 1 FROM task_dependencies d WHERE d.predecessor_id = NEW.task_id)
  OR EXISTS (SELECT 1 FROM replay_cases c WHERE c.source_task_id = NEW.task_id)
BEGIN SELECT RAISE(ABORT, 'a replay candidate is registered on a fresh task'); END;
-- A replay case never draws on a replay candidate's own history.
CREATE TRIGGER replay_cases_not_replayed BEFORE INSERT ON replay_cases
WHEN EXISTS (SELECT 1 FROM replay_candidates c WHERE c.task_id = NEW.source_task_id)
BEGIN SELECT RAISE(ABORT, 'a replay candidate is never a replay source'); END;
-- Dependency guard: nothing depends on a replay candidate.
CREATE TRIGGER replay_candidate_no_dependents BEFORE INSERT ON task_dependencies
WHEN EXISTS (SELECT 1 FROM replay_candidates c WHERE c.task_id = NEW.predecessor_id)
BEGIN SELECT RAISE(ABORT, 'a replay candidate never releases dependents'); END;
CREATE TRIGGER replay_candidate_no_satisfaction BEFORE INSERT ON dependency_satisfactions
WHEN EXISTS (SELECT 1 FROM replay_candidates c WHERE c.task_id = NEW.predecessor_task)
BEGIN SELECT RAISE(ABORT, 'a replay candidate never releases dependents'); END;
-- Integration guard: no integration job, lease or operation for a replay candidate.
CREATE TRIGGER replay_candidate_no_integration_operation BEFORE INSERT ON integration_operations
WHEN EXISTS (SELECT 1 FROM verified_results r JOIN result_submissions s ON s.submission_id = r.submission_id
    JOIN replay_candidates c ON c.task_id = s.task_id WHERE r.result_id = NEW.verified_result_id)
BEGIN SELECT RAISE(ABORT, 'a replay candidate never integrates'); END;
CREATE TRIGGER replay_candidate_no_integration_job BEFORE INSERT ON operations
WHEN NEW.kind IN ('integration.run', 'integration.lease') AND (EXISTS (SELECT 1 FROM replay_candidates c WHERE c.task_id = NEW.task_id)
    OR EXISTS (SELECT 1 FROM result_submissions s JOIN replay_candidates c ON c.task_id = s.task_id WHERE s.submission_id = json_extract(NEW.payload, '$.submission_id'))
    OR EXISTS (SELECT 1 FROM verified_results r JOIN result_submissions s ON s.submission_id = r.submission_id
        JOIN replay_candidates c ON c.task_id = s.task_id WHERE r.result_id = json_extract(NEW.payload, '$.result_id')))
BEGIN SELECT RAISE(ABORT, 'a replay candidate never integrates'); END;
UPDATE store_meta SET schema_version = 64;
PRAGMA user_version = 64;
