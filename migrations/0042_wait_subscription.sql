-- A replay without a wake advances the subscription; only a wake is terminal.
DROP TRIGGER wait_conditions_replay_once;
-- Older replay treated every wait.wake as its own. Preserve only wakes whose
-- original event actually addressed this registration. A wake is advisory;
-- this never changes any result, dependency or authority evidence.
UPDATE wait_conditions SET wake_requested=0
WHERE wake_requested=1 AND NOT EXISTS (
    SELECT 1 FROM events e
    WHERE e.kind='wait.wake' AND e.entity=wait_conditions.wait_id
      AND e.sequence>wait_conditions.cursor_sequence
      AND e.sequence<=wait_conditions.replayed_through
);
CREATE TRIGGER wait_conditions_replay_monotonic
BEFORE UPDATE OF state, replayed_through, wake_requested ON wait_conditions
WHEN OLD.wake_requested = 1 OR NEW.state != 'replayed'
  OR NEW.replayed_through IS NULL
  OR NEW.replayed_through < COALESCE(OLD.replayed_through, OLD.cursor_sequence)
BEGIN SELECT RAISE(ABORT, 'wait replay must advance an unresolved subscription'); END;
UPDATE store_meta SET schema_version = 42;
PRAGMA user_version = 42;
