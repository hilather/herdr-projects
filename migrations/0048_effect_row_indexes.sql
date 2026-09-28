-- Notification enqueue and delivery check only unseen inbox items and
-- unresolved (claimed or ambiguous) deliveries, never retained history.
CREATE INDEX inbox_items_unseen ON inbox_items(id) WHERE seen=0 AND done=0;
CREATE INDEX operation_delivery_unresolved ON operation_delivery(operation_id)
WHERE state IN ('claimed','ambiguous');
-- Finalization resolves preserved outputs from this binding's termination
-- evidence. Payloads without a text binding stay visible as uncertainty.
CREATE INDEX worker_terminations_by_binding ON events(json_extract(payload,'$.binding'),sequence)
WHERE kind='runtime.worker_terminated';
CREATE INDEX worker_terminations_unbound ON events(sequence)
WHERE kind='runtime.worker_terminated' AND json_type(payload,'$.binding') IS NOT 'text';
UPDATE store_meta SET schema_version = 48;
PRAGMA user_version = 48;
