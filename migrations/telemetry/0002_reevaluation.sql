-- Contracts §5 re-evaluation: a source whose rows were stored while its
-- `cli_version` was uncertified is re-read after certification; when its
-- rollout is no longer found the rows stay uncertified and this says why.
ALTER TABLE rollout_sources ADD COLUMN reevaluation TEXT CHECK (reevaluation IS NULL OR reevaluation = 'rollout_unavailable');
PRAGMA user_version = 2;
