-- Automatic integration switch, off by default like `verify`. Enqueueing an
-- integration job grants no authority; the integrator rechecks everything.
ALTER TABLE result_automation_control ADD COLUMN integrate INTEGER NOT NULL DEFAULT 0 CHECK(integrate IN (0,1));
-- One automatic integration job per submission. At most one unfinished job per
-- (repository, ref) is enforced in the enqueue transaction.
CREATE UNIQUE INDEX integration_jobs_by_submission ON operations(json_extract(payload,'$.submission_id'))
WHERE kind='integration.run';
-- Rebuildable candidates for automatic integration, so the producer never
-- scans submission history. A submission enters when any verified result is
-- recorded for it (or its repository gains a target) and leaves when an
-- integration job or operation names it; the producer rechecks full
-- eligibility and drops rows that are not eligible now.
CREATE TABLE pending_integration_work (
    submission_id TEXT PRIMARY KEY REFERENCES result_submissions(submission_id),
    created_unix_ms INTEGER NOT NULL
) STRICT;
CREATE INDEX pending_integration_oldest ON pending_integration_work(created_unix_ms,submission_id);
INSERT INTO pending_integration_work
SELECT s.submission_id,s.created_unix_ms FROM result_submissions s
WHERE EXISTS (SELECT 1 FROM verified_results r WHERE r.submission_id=s.submission_id)
  AND NOT EXISTS (SELECT 1 FROM verified_results r JOIN integration_operations i ON i.verified_result_id=r.result_id WHERE r.submission_id=s.submission_id);
CREATE TRIGGER pending_integration_on_verified AFTER INSERT ON verified_results
BEGIN INSERT OR IGNORE INTO pending_integration_work
  SELECT s.submission_id,s.created_unix_ms FROM result_submissions s WHERE s.submission_id=NEW.submission_id; END;
CREATE TRIGGER pending_integration_on_target AFTER INSERT ON integration_targets
BEGIN INSERT OR IGNORE INTO pending_integration_work
  SELECT s.submission_id,s.created_unix_ms FROM result_submissions s WHERE s.repository=NEW.repository
    AND EXISTS (SELECT 1 FROM verified_results r WHERE r.submission_id=s.submission_id)
    AND NOT EXISTS (SELECT 1 FROM operations o WHERE o.kind='integration.run' AND json_extract(o.payload,'$.submission_id')=s.submission_id)
    AND NOT EXISTS (SELECT 1 FROM verified_results r JOIN integration_operations i ON i.verified_result_id=r.result_id WHERE r.submission_id=s.submission_id); END;
CREATE TRIGGER pending_integration_on_job AFTER INSERT ON operations WHEN NEW.kind='integration.run'
BEGIN DELETE FROM pending_integration_work WHERE submission_id=json_extract(NEW.payload,'$.submission_id'); END;
CREATE TRIGGER pending_integration_on_operation AFTER INSERT ON integration_operations
BEGIN DELETE FROM pending_integration_work WHERE submission_id=(SELECT submission_id FROM verified_results WHERE result_id=NEW.verified_result_id); END;
UPDATE store_meta SET schema_version = 45;
PRAGMA user_version = 45;
