-- A pending result job whose task revision fence no longer holds can never be
-- claimed. It is retired (`<lane>.job_retired`, reason `task_revision_changed`)
-- and replaced by a job fenced on the current revision, so uniqueness includes
-- the fence. The enqueue transaction still admits at most one unretired job.
DROP INDEX verification_jobs_by_policy;
CREATE UNIQUE INDEX verification_jobs_by_policy ON operations(
    json_extract(payload,'$.submission_id'),json_extract(payload,'$.policy_id'),expected_revision
) WHERE kind='verification.run';
DROP INDEX integration_jobs_by_submission;
CREATE UNIQUE INDEX integration_jobs_by_submission ON operations(json_extract(payload,'$.submission_id'),expected_revision)
WHERE kind='integration.run';
UPDATE store_meta SET schema_version = 47;
PRAGMA user_version = 47;
