-- Every acceptance policy of the contract revision is rechecked on the
-- integrated candidate. One row per policy that ran, with its verdict; the
-- first failure stops the check. No backfill: earlier integrations ran only
-- one policy.
CREATE TABLE integration_policy_checks (
    operation_id TEXT NOT NULL REFERENCES integration_operations(operation_id),
    policy_id TEXT NOT NULL CHECK (length(policy_id) BETWEEN 1 AND 128),
    policy_digest TEXT NOT NULL CHECK (length(policy_digest) = 64),
    passed INTEGER NOT NULL CHECK (passed IN (0, 1)),
    PRIMARY KEY (operation_id, policy_id)
) STRICT;
UPDATE store_meta SET schema_version = 46;
PRAGMA user_version = 46;
