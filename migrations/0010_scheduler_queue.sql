CREATE TABLE scheduler_policy (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    revision INTEGER NOT NULL CHECK(revision>0),
    max_active_workers INTEGER NOT NULL CHECK(max_active_workers BETWEEN 0 AND 1024),
    max_attempts_per_task INTEGER NOT NULL CHECK(max_attempts_per_task BETWEEN 1 AND 32)
) STRICT;
INSERT INTO scheduler_policy VALUES(1,1,0,3);
CREATE TABLE task_queue (
    task_id TEXT PRIMARY KEY NOT NULL REFERENCES tasks(id),
    priority INTEGER NOT NULL CHECK(priority BETWEEN -20 AND 20),
    enqueued_unix_ms INTEGER NOT NULL CHECK(enqueued_unix_ms>=0),
    enqueue_sequence INTEGER NOT NULL CHECK(enqueue_sequence>0)
) STRICT;
CREATE TABLE task_dependencies (
    task_id TEXT NOT NULL REFERENCES tasks(id),
    predecessor_id TEXT NOT NULL REFERENCES tasks(id),
    requirement TEXT NOT NULL CHECK(requirement IN ('verified_result','integration_candidate','landed_commit')),
    PRIMARY KEY(task_id,predecessor_id),
    CHECK(task_id<>predecessor_id)
) STRICT;
UPDATE store_meta SET schema_version=10;
PRAGMA user_version=10;
