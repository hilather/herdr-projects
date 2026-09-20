-- Existing v1 attempt inputs remain immutable historical records. New records
-- require version 2 effective profile evidence; old clients refuse schema 12.
CREATE TRIGGER attempt_inputs_effective_profile BEFORE INSERT ON attempt_inputs
WHEN COALESCE(json_extract(NEW.payload,'$.inputs.version'),0)<>2
 OR COALESCE(json_type(NEW.payload,'$.inputs.effective_profile'),'missing')<>'object'
BEGIN SELECT RAISE(ABORT,'effective profile evidence is required'); END;
UPDATE store_meta SET schema_version=12;
PRAGMA user_version=12;
