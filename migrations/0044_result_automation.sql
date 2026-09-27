-- Per-project result automation switches. Off by default; enqueueing a job
-- grants no execution authority and records no verification evidence.
CREATE TABLE result_automation_control (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    revision INTEGER NOT NULL CHECK(revision>0),
    verify INTEGER NOT NULL CHECK(verify IN (0,1))
) STRICT;
INSERT INTO result_automation_control VALUES(1,1,0);
-- One verification job per (submission, acceptance policy).
CREATE UNIQUE INDEX verification_jobs_by_policy ON operations(
    json_extract(payload,'$.submission_id'),json_extract(payload,'$.policy_id')
) WHERE kind='verification.run';
UPDATE store_meta SET schema_version = 44;
PRAGMA user_version = 44;
