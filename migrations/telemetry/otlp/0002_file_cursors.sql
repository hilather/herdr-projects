-- DG4c: replayable SDK file positions; no raw payload or owner paths.
CREATE TABLE gemini_file_cursors (
    source_digest TEXT PRIMARY KEY,
    device INTEGER NOT NULL,
    inode INTEGER NOT NULL,
    byte_offset INTEGER NOT NULL CHECK(byte_offset >= 0)
) STRICT;
