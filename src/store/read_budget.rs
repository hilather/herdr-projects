//! Accounting before application copies/decodes SQLite-owned values. These
//! weights bound admitted input and JSON structure, not exact peak heap usage.
use super::{controlled::ReadControl, Result, StoreError};
use rusqlite::{types::ValueRef, Row};
use std::cell::Cell;

const MAX_UNITS: usize = 50 * 1024 * 1024;
const MAX_ROWS: usize = 100_000;
const MAX_FIELD: usize = 16 * 1024 * 1024;
const MAX_COLUMNS: usize = 64;
const STRUCTURE_WEIGHT: usize = 128;

pub(super) struct ReadBudget {
    control: ReadControl,
    units: Cell<usize>,
    rows: Cell<usize>,
}
impl ReadBudget {
    pub(super) fn new(control: ReadControl) -> Self {
        Self { control, units: Cell::new(0), rows: Cell::new(0) }
    }
    fn charge(&self, units: usize) -> Result<()> {
        let next = self.units.get().checked_add(units)
            .filter(|n| *n <= MAX_UNITS)
            .ok_or_else(|| StoreError::Limit("snapshot input/structure accounting exceeds 50 MiB".into()))?;
        self.units.set(next);
        Ok(())
    }
    /// Called before row.get or JSON decoding. JSON columns specify the number
    /// of parse/copy passes; raw provenance is deliberately not treated as JSON.
    pub(super) fn row(&self, row: &Row<'_>, json: &[(usize, usize)]) -> Result<()> {
        self.control.check()?;
        let count = self.rows.get().checked_add(1).filter(|n| *n <= MAX_ROWS)
            .ok_or_else(|| StoreError::Limit("snapshot exceeds 100000 returned rows".into()))?;
        self.rows.set(count);
        let columns = row.as_ref().column_count();
        if columns > MAX_COLUMNS { return Err(StoreError::Limit("snapshot row exceeds 64 columns".into())); }
        self.charge(STRUCTURE_WEIGHT * (1 + columns))?;
        for column in 0..columns {
            let bytes = match row.get_ref(column)? {
                ValueRef::Text(bytes) | ValueRef::Blob(bytes) => bytes,
                _ => &[],
            };
            if bytes.len() > MAX_FIELD { return Err(StoreError::Limit("snapshot field exceeds 16 MiB".into())); }
            self.charge(bytes.len())?;
        }
        for &(column, passes) in json {
            let bytes = match row.get_ref(column)? {
                ValueRef::Text(bytes) | ValueRef::Blob(bytes) => bytes,
                ValueRef::Null => continue,
                _ => return Err(StoreError::Corrupt("JSON column is not text/blob".into())),
            };
            self.json(bytes, passes)?;
        }
        self.control.check()
    }
    fn json(&self, bytes: &[u8], passes: usize) -> Result<()> {
        self.control.check()?;
        if passes == 0 { return Err(StoreError::Invalid("JSON accounting requires at least one pass".into())); }
        let mut quoted = false;
        let mut escaped = false;
        // Count opening quotes (keys and strings) plus EVERY non-whitespace
        // byte outside strings. This conservatively overcounts valid JSON
        // nodes, including empty containers. No allocation or recursion.
        for (index, byte) in bytes.iter().copied().enumerate() {
            if index % 4096 == 0 { self.control.check()?; }
            if quoted {
                if escaped { escaped = false; }
                else if byte == b'\\' { escaped = true; }
                else if byte == b'"' { quoted = false; }
            } else if !matches!(byte, b' ' | b'\n' | b'\r' | b'\t') {
                self.charge(STRUCTURE_WEIGHT.checked_mul(passes)
                    .ok_or_else(|| StoreError::Limit("JSON accounting overflow".into()))?)?;
                if byte == b'"' { quoted = true; }
            }
        }
        // The row already charged the first raw copy; additional passes also
        // charge the encoded bytes. Serde retains its own syntax/depth checks.
        self.charge(bytes.len().checked_mul(passes.saturating_sub(1))
            .ok_or_else(|| StoreError::Limit("JSON accounting overflow".into()))?)?;
        self.control.check()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::Cancellation;
    use std::time::{Duration, Instant};
    fn budget() -> ReadBudget { ReadBudget::new(ReadControl::new(Instant::now()+Duration::from_secs(10), Cancellation::default())) }
    #[test]
    fn lexical_scan_counts_empty_dense_and_escaped_values() {
        for (json, nodes) in [(r#""""#,1), ("[]",2), ("{}",2), ("[0,0]",5),
            (r#"{"":"","a":[]}"#,10), (r#""a\"b\\c\u1234""#,1), (" \n true\t",4)] {
            let b=budget(); b.json(json.as_bytes(),1).unwrap();
            assert_eq!(b.units.get(),nodes*STRUCTURE_WEIGHT,"{json}");
        }
    }
    #[test]
    fn dense_json_and_repeated_passes_consume_shared_budget() {
        let b=budget(); let payload=format!("[{}0]","0,".repeat(150_000));
        b.json(payload.as_bytes(),1).unwrap();
        assert!(matches!(b.json(payload.as_bytes(),1),Err(StoreError::Limit(_))));
        let b=budget(); assert!(matches!(b.json(payload.as_bytes(),2),Err(StoreError::Limit(_))));
    }
    #[test]
    fn long_strings_remain_cancellable_without_structure_expansion() {
        let b=budget();let bytes=format!("\"{}\"","x".repeat(1_000_000));
        b.json(bytes.as_bytes(),1).unwrap();assert_eq!(b.units.get(),128);
        b.control.cancellation().cancel();
        assert!(matches!(b.json(bytes.as_bytes(),1),Err(StoreError::Cancelled)));
    }
    #[test]
    fn actual_rows_charge_repeated_join_values_before_decode() {
        let db=rusqlite::Connection::open_in_memory().unwrap();
        let mut stmt=db.prepare("SELECT zeroblob(10000000) FROM (SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6)").unwrap();
        let mut rows=stmt.query([]).unwrap();let b=budget();
        for _ in 0..5 {b.row(rows.next().unwrap().unwrap(),&[]).unwrap();}
        assert!(matches!(b.row(rows.next().unwrap().unwrap(),&[]),Err(StoreError::Limit(_))));
    }
    #[test]
    fn field_column_row_and_arithmetic_limits_fail_closed() {
        let db=rusqlite::Connection::open_in_memory().unwrap();
        let mut stmt=db.prepare("SELECT zeroblob(16777217)").unwrap();
        assert!(matches!(budget().row(stmt.query([]).unwrap().next().unwrap().unwrap(),&[]),Err(StoreError::Limit(_))));
        let mut stmt=db.prepare(&format!("SELECT {}",vec!["0";65].join(","))).unwrap();
        assert!(matches!(budget().row(stmt.query([]).unwrap().next().unwrap().unwrap(),&[]),Err(StoreError::Limit(_))));
        let b=budget();b.rows.set(MAX_ROWS);let mut stmt=db.prepare("SELECT 0").unwrap();
        assert!(matches!(b.row(stmt.query([]).unwrap().next().unwrap().unwrap(),&[]),Err(StoreError::Limit(_))));
        assert!(matches!(budget().json(b"[]",usize::MAX),Err(StoreError::Limit(_))));
    }
}
