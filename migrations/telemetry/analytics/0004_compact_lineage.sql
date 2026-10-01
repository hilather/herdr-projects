-- P5: exact attrs and entity identities shared across buckets/revisions.
-- No JSON is rewritten, and ordinal/key order remains unchanged.
CREATE TABLE analytics_lineage_values (
 id INTEGER PRIMARY KEY,
 entity_kind TEXT NOT NULL CHECK(entity_kind IN ('task','attempt')),
 entity_id TEXT NOT NULL,
 attrs TEXT NOT NULL CHECK(json_valid(attrs))
) STRICT;
CREATE INDEX analytics_lineage_values_entity ON analytics_lineage_values(entity_kind,entity_id);
INSERT INTO analytics_lineage_values(entity_kind,entity_id,attrs)
 SELECT DISTINCT entity_kind,entity_id,attrs FROM analytics_lineage;
CREATE TABLE analytics_lineage_rows (
 revision INTEGER NOT NULL REFERENCES analytics_revisions(revision),
 bucket TEXT NOT NULL CHECK(length(bucket) BETWEEN 1 AND 128),
 ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
 value_id INTEGER NOT NULL REFERENCES analytics_lineage_values(id),
 PRIMARY KEY(revision,bucket,ordinal)
) STRICT, WITHOUT ROWID;
INSERT INTO analytics_lineage_rows SELECT revision,bucket,ordinal,
 (SELECT id FROM analytics_lineage_values v WHERE v.entity_kind=l.entity_kind AND v.entity_id=l.entity_id AND v.attrs=l.attrs)
 FROM analytics_lineage l;
DROP TABLE analytics_lineage;
CREATE VIEW analytics_lineage AS SELECT r.revision,r.bucket,r.ordinal,v.entity_kind,v.entity_id,v.attrs
 FROM analytics_lineage_rows r JOIN analytics_lineage_values v ON v.id=r.value_id;
CREATE TRIGGER analytics_lineage_insert INSTEAD OF INSERT ON analytics_lineage BEGIN
 INSERT INTO analytics_lineage_values(entity_kind,entity_id,attrs)
 SELECT NEW.entity_kind,NEW.entity_id,NEW.attrs WHERE NOT EXISTS(
  SELECT 1 FROM analytics_lineage_values WHERE entity_kind=NEW.entity_kind AND entity_id=NEW.entity_id AND attrs=NEW.attrs);
 INSERT INTO analytics_lineage_rows VALUES(NEW.revision,NEW.bucket,NEW.ordinal,
  (SELECT id FROM analytics_lineage_values WHERE entity_kind=NEW.entity_kind AND entity_id=NEW.entity_id AND attrs=NEW.attrs));
END;
CREATE TRIGGER analytics_lineage_no_update INSTEAD OF UPDATE ON analytics_lineage
BEGIN SELECT RAISE(ABORT,'analytics lineage is immutable'); END;
CREATE TRIGGER analytics_lineage_no_delete INSTEAD OF DELETE ON analytics_lineage
BEGIN SELECT RAISE(ABORT,'analytics lineage is immutable'); END;
CREATE TRIGGER analytics_lineage_delete INSTEAD OF DELETE ON analytics_lineage BEGIN
 DELETE FROM analytics_lineage_rows WHERE revision=OLD.revision AND bucket=OLD.bucket AND ordinal=OLD.ordinal;
END;

-- A central provider's M40 is exactly the detail of its recorded revision.
-- Share that immutable body; on eviction discard the disposable cache so the
-- original evaluator remains the fallback. Unmatched/legacy bodies stay whole.
CREATE TABLE analytics_provider_rows (
 provider TEXT NOT NULL,
 window_key TEXT NOT NULL,
 inputs TEXT NOT NULL,
 body TEXT NOT NULL,
 suffix TEXT NOT NULL,
 m40_revision INTEGER REFERENCES analytics_revisions(revision),
 m40_offset INTEGER,
 m40_bytes INTEGER,
 CHECK(m40_revision IS NOT NULL OR json_valid(body)),
 CHECK((m40_revision IS NULL AND m40_offset IS NULL AND m40_bytes IS NULL) OR (m40_revision IS NOT NULL AND m40_offset>0 AND m40_bytes>0)),
 PRIMARY KEY(provider,window_key)
) STRICT, WITHOUT ROWID;
-- Existing bodies are copied byte for byte, including noncanonical legacy JSON.
-- The ordinary validated refresh may later share an exactly equal M40 range.
INSERT INTO analytics_provider_rows
 SELECT provider,window_key,inputs,body,'',NULL,NULL,NULL FROM analytics_provider_aggregates;
DROP TABLE analytics_provider_aggregates;
CREATE VIEW analytics_provider_aggregates AS SELECT p.provider,p.window_key,p.inputs,
 CASE WHEN p.m40_revision IS NULL THEN p.body
 ELSE p.body||CAST(substr(CAST(r.body AS BLOB),p.m40_offset,p.m40_bytes) AS TEXT)||p.suffix END AS body
 FROM analytics_provider_rows p LEFT JOIN analytics_revisions r ON r.revision=p.m40_revision;
-- Direct logical-view writes keep their original bytes. Only the production
-- writer, after exact range comparison, creates a shared-body reference.
CREATE TRIGGER analytics_provider_insert INSTEAD OF INSERT ON analytics_provider_aggregates BEGIN
 INSERT INTO analytics_provider_rows VALUES(NEW.provider,NEW.window_key,NEW.inputs,NEW.body,'',NULL,NULL,NULL);
END;
CREATE TRIGGER analytics_provider_update INSTEAD OF UPDATE ON analytics_provider_aggregates BEGIN
 UPDATE analytics_provider_rows SET provider=NEW.provider,window_key=NEW.window_key,
 inputs=NEW.inputs,body=NEW.body,suffix='',m40_revision=NULL,m40_offset=NULL,m40_bytes=NULL
 WHERE provider=OLD.provider AND window_key=OLD.window_key;
END;
CREATE TRIGGER analytics_provider_delete INSTEAD OF DELETE ON analytics_provider_aggregates BEGIN
 DELETE FROM analytics_provider_rows WHERE provider=OLD.provider AND window_key=OLD.window_key;
END;
CREATE TRIGGER analytics_provider_revision_delete AFTER DELETE ON analytics_revisions BEGIN
 DELETE FROM analytics_provider_rows WHERE m40_revision=OLD.revision;
END;
