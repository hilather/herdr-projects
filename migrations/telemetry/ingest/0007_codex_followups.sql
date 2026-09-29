-- A7 (contracts-collection.md "A7"): collector follow-ups. Re-runnable: every
-- table is `IF NOT EXISTS`, and the `coverage_gaps` rebuild starts from a
-- dropped scratch table, so a sidecar whose stream table was lost runs it again.

-- `session_meta.source.subagent.other` (a string tag; live 0.154.0:
-- `guardian`), beside `rollout_metadata.subagent_kind` = `other`. Written
-- from the file's first `session_meta` with `rollout_metadata`. A source with
-- a `rollout_sources` row but none here was read before A7; the next collect
-- reads it again from byte 0.
CREATE TABLE IF NOT EXISTS rollout_subagents (
    path_digest TEXT PRIMARY KEY,
    subagent_detail TEXT
) STRICT;

-- Per rollout source, what its ingest pass carries between passes beyond the
-- cursor: whether any of its envelopes was written while its `cli_version`
-- was uncertified (sticky until a re-read from byte 0), and its last turn (the
-- byte offset and turn id of the `turn_context` or `task_started` that opened
-- it, and whether its `task_complete` was read). Metadata only.
CREATE TABLE IF NOT EXISTS rollout_ingest_state (
    path_digest TEXT PRIMARY KEY,
    uncertified_envelopes INTEGER NOT NULL CHECK (uncertified_envelopes IN (0, 1)),
    last_turn_offset INTEGER CHECK (last_turn_offset IS NULL OR last_turn_offset >= 0),
    last_turn_id TEXT,
    last_turn_completed INTEGER NOT NULL CHECK (last_turn_completed IN (0, 1))
) STRICT;

-- `final_event_missing`: a rollout idle past the threshold whose last turn has
-- no `task_complete`, from the turn's first line to the file's end. SQLite
-- cannot widen a CHECK in place, so the table is rebuilt with its rows.
DROP TABLE IF EXISTS coverage_gaps_0007;
CREATE TABLE coverage_gaps_0007 (
    source TEXT NOT NULL,
    start_offset INTEGER NOT NULL,
    end_offset INTEGER NOT NULL CHECK (end_offset >= start_offset),
    reason TEXT NOT NULL CHECK (reason IN ('sidecar_write_failed', 'predates_ingest', 'final_event_missing')),
    recovery TEXT NOT NULL CHECK (recovery IN ('pending', 'recovered')),
    observed_unix_ms INTEGER NOT NULL,
    PRIMARY KEY (source, start_offset, reason)
) STRICT;
INSERT INTO coverage_gaps_0007 SELECT source,start_offset,end_offset,reason,recovery,observed_unix_ms FROM coverage_gaps;
DROP TABLE coverage_gaps;
ALTER TABLE coverage_gaps_0007 RENAME TO coverage_gaps;
