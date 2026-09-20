CREATE TABLE review_decisions (
    id TEXT PRIMARY KEY NOT NULL,
    proposal_id TEXT NOT NULL REFERENCES memory_proposals(id),
    payload_digest TEXT NOT NULL CHECK (length(payload_digest)=64),
    decision TEXT NOT NULL CHECK (decision IN ('approve','reject','narrow')),
    classification TEXT NOT NULL,
    reviewed_heads TEXT NOT NULL,
    reason TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL
) STRICT;
CREATE TABLE memory_promotions (
    proposal_id TEXT PRIMARY KEY NOT NULL REFERENCES memory_proposals(id),
    decision_id TEXT NOT NULL REFERENCES review_decisions(id),
    payload_digest TEXT NOT NULL CHECK (length(payload_digest)=64),
    sequence INTEGER NOT NULL,
    change_ids TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL
) STRICT;
CREATE TABLE memory_invalidations (
    id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT,
    proposal_id TEXT NOT NULL,
    record_id TEXT,
    severity TEXT NOT NULL CHECK (severity IN ('informational','reconcile_before_completion','stop_at_checkpoint')),
    triggering_seq INTEGER NOT NULL,
    resolved_seq INTEGER,
    reason TEXT NOT NULL
) STRICT;
CREATE TRIGGER review_decisions_no_update BEFORE UPDATE ON review_decisions BEGIN SELECT RAISE(ABORT,'review decision is immutable'); END;
CREATE TRIGGER review_decisions_no_delete BEFORE DELETE ON review_decisions BEGIN SELECT RAISE(ABORT,'review decision is immutable'); END;
CREATE TRIGGER memory_promotions_no_update BEFORE UPDATE ON memory_promotions BEGIN SELECT RAISE(ABORT,'memory promotion is immutable'); END;
CREATE TRIGGER memory_promotions_no_delete BEFORE DELETE ON memory_promotions BEGIN SELECT RAISE(ABORT,'memory promotion is immutable'); END;
CREATE TRIGGER memory_invalidations_no_delete BEFORE DELETE ON memory_invalidations BEGIN SELECT RAISE(ABORT,'memory invalidation is immutable'); END;
UPDATE store_meta SET schema_version=22;
PRAGMA user_version=22;
