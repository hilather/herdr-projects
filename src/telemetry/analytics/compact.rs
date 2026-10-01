//! Share immutable M40 bytes without parsing its whole decision array in SQL.
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use std::ops::Range;

pub(super) struct Reference {
    pub revision: i64,
    pub offset: i64,
    pub bytes: i64,
    pub range: Range<usize>,
}

/// Return a top-level field's byte range in already validated JSON. Walk only
/// delimiters and quoted strings, retaining no parsed tree or event history.
fn field_range(body: &str, name: &str) -> Option<Range<usize>> {
    let b = body.as_bytes();
    let key = format!("\"{name}\"");
    let mut depth = 0usize;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'{' | b'[' => { depth += 1; i += 1; }
            b'}' | b']' => { depth = depth.checked_sub(1)?; i += 1; }
            b'"' => {
                let start = i;
                i = quoted_end(b, i)?;
                if depth != 1 || body[start..i] != key { continue; }
                while b.get(i).is_some_and(u8::is_ascii_whitespace) { i += 1; }
                if b.get(i) != Some(&b':') { continue; }
                i += 1;
                while b.get(i).is_some_and(u8::is_ascii_whitespace) { i += 1; }
                let start = i;
                let mut nested = 0usize;
                while i < b.len() {
                    match b[i] {
                        b'"' => { i = quoted_end(b, i)?; if nested == 0 { break; } }
                        b'{' | b'[' => { nested += 1; i += 1; }
                        b'}' | b']' if nested > 0 => { nested -= 1; i += 1; if nested == 0 { break; } }
                        b',' | b'}' | b']' if nested == 0 => break,
                        _ => i += 1,
                    }
                }
                while i > start && b[i - 1].is_ascii_whitespace() { i -= 1; }
                return Some(start..i);
            }
            _ => i += 1,
        }
    }
    None
}

fn quoted_end(b: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Plan outside the writer lock. Exact byte equality is required; an
/// unmatched body stays inline. The commit rechecks the immutable revision.
pub(super) fn reference(db: &Connection, body: &str) -> Result<Option<Reference>> {
    let Some(range) = field_range(body, "M40") else { return Ok(None) };
    let bytes = range.len() as i64;
    let found = db.prepare_cached("SELECT revision,instr(CAST(body AS BLOB),CAST('\"detail\":' AS BLOB))+9
        FROM analytics_revisions WHERE json_extract(cell,'$.metric')='M40'
        AND instr(CAST(body AS BLOB),CAST('\"detail\":' AS BLOB))>0
        AND substr(CAST(body AS BLOB),instr(CAST(body AS BLOB),CAST('\"detail\":' AS BLOB))+9,?1)=CAST(?2 AS BLOB)
        AND substr(CAST(body AS BLOB),instr(CAST(body AS BLOB),CAST('\"detail\":' AS BLOB))+9+?1,1) IN (X'2c',X'7d')
        ORDER BY revision DESC LIMIT 1")?
        .query_row(rusqlite::params![bytes, &body[range.clone()]], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    Ok(found.map(|(revision, offset)| Reference { revision, offset, bytes, range }))
}
