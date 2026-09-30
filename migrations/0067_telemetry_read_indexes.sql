-- TM5.1 (docs/telemetry/certificate-scale.md §5): read indexes for the
-- telemetry projections over retained history. Verified results are looked
-- up by submission (acceptance evidence, lifecycle acceptance times), and a
-- submission by its attempt (the attempt projection, once per attempt).
-- Without them each lookup built an automatic index or scanned the table,
-- once per attempt. The first was proposed by TM4.1 (contracts-analytics.md
-- §6). No row changes.
CREATE INDEX verified_results_by_submission ON verified_results(submission_id);
CREATE INDEX result_submissions_by_attempt ON result_submissions(attempt_id, created_unix_ms);

UPDATE store_meta SET schema_version = 67;
PRAGMA user_version = 67;
