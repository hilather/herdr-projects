-- P5: one btree for the disposition key and its row; exact fields/checks retained.
CREATE TABLE usage_dispositions_0016 (
    entry_id TEXT NOT NULL REFERENCES usage_entries(entry_id),
    path_digest TEXT NOT NULL,
    disposition TEXT NOT NULL CHECK (disposition IN ('accepted', 'duplicate', 'conflict', 'unresolved')),
    reason TEXT,
    PRIMARY KEY (entry_id, path_digest)
) STRICT, WITHOUT ROWID;
INSERT INTO usage_dispositions_0016 SELECT * FROM usage_dispositions;
DROP TABLE usage_dispositions;
ALTER TABLE usage_dispositions_0016 RENAME TO usage_dispositions;
