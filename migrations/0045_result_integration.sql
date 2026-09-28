-- Automatic integration switch, off by default like `verify`. Enqueueing an
-- integration job grants no authority; the integrator rechecks everything.
ALTER TABLE result_automation_control ADD COLUMN integrate INTEGER NOT NULL DEFAULT 0 CHECK(integrate IN (0,1));
-- One automatic integration job per submission. At most one unfinished job per
-- (repository, ref) is enforced in the enqueue transaction.
CREATE UNIQUE INDEX integration_jobs_by_submission ON operations(json_extract(payload,'$.submission_id'))
WHERE kind='integration.run';
UPDATE store_meta SET schema_version = 45;
PRAGMA user_version = 45;
