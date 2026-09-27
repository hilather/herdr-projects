//! Test-only historical schemas built from the original migration prefix.
//! Copy only columns present in that schema; never pretend that a newer schema
//! is an older one by changing its version number or maintaining DROP lists.
use rusqlite::{Connection, Result, types::Value};

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
fn columns(db: &Connection, table: &str) -> Result<Vec<String>> {
    db.prepare(&format!("PRAGMA table_info({})", quote(table)))?
        .query_map([], |r| r.get(1))?
        .collect()
}

pub fn historical(db: &Connection, version: u32) -> Result<()> {
    let old = Connection::open_in_memory()?;
    let mut migrations = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "sql"))
        .collect::<Vec<_>>();
    migrations.sort();
    for path in migrations.into_iter().take(version as usize) {
        old.execute_batch(&std::fs::read_to_string(path).unwrap())?;
    }
    let objects: Vec<(String, String, String)> = old.prepare("SELECT type,name,sql FROM sqlite_schema WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY rowid")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_>>()?;
    let mut data = Vec::new();
    for (kind, name, _) in &objects {
        if kind != "table" {
            continue;
        }
        let current = columns(db, name)?;
        let shared = columns(&old, name)?
            .into_iter()
            .filter(|c| current.contains(c))
            .collect::<Vec<_>>();
        if shared.is_empty() {
            continue;
        }
        let sql = format!(
            "SELECT {} FROM {}",
            shared
                .iter()
                .map(|c| quote(c))
                .collect::<Vec<_>>()
                .join(","),
            quote(name)
        );
        let values = db
            .prepare(&sql)?
            .query_map([], |r| {
                (0..shared.len())
                    .map(|i| r.get::<_, Value>(i))
                    .collect::<Result<Vec<_>>>()
            })?
            .collect::<Result<Vec<_>>>()?;
        data.push((name.clone(), shared, values));
    }
    let existing: Vec<(String, String)> = db.prepare("SELECT type,name FROM sqlite_schema WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY CASE type WHEN 'trigger' THEN 0 WHEN 'view' THEN 1 WHEN 'index' THEN 2 ELSE 3 END")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_>>()?;
    db.execute_batch("PRAGMA foreign_keys=OFF; BEGIN IMMEDIATE;")?;
    let result = (|| -> Result<()> {
        for (kind, name) in existing {
            db.execute_batch(&format!("DROP {} IF EXISTS {};", kind, quote(&name)))?;
        }
        for (kind, _, sql) in &objects {
            if kind == "table" {
                db.execute_batch(sql)?;
            }
        }
        for (name, shared, values) in data {
            let sql = format!(
                "INSERT INTO {} ({}) VALUES ({})",
                quote(&name),
                shared
                    .iter()
                    .map(|c| quote(c))
                    .collect::<Vec<_>>()
                    .join(","),
                vec!["?"; shared.len()].join(",")
            );
            for row in values {
                db.execute(&sql, rusqlite::params_from_iter(row))?;
            }
        }
        // Initial singleton rows are supplied only when absent from the source.
        for (kind, name, _) in &objects {
            if kind != "table" {
                continue;
            }
            let cols = columns(&old, name)?;
            let sql = format!("SELECT * FROM {}", quote(name));
            let rows = old
                .prepare(&sql)?
                .query_map([], |r| {
                    (0..cols.len())
                        .map(|i| r.get::<_, Value>(i))
                        .collect::<Result<Vec<_>>>()
                })?
                .collect::<Result<Vec<_>>>()?;
            for row in rows {
                db.execute(
                    &format!(
                        "INSERT OR IGNORE INTO {} VALUES ({})",
                        quote(name),
                        vec!["?"; cols.len()].join(",")
                    ),
                    rusqlite::params_from_iter(row),
                )?;
            }
        }
        for (kind, _, sql) in &objects {
            if kind != "table" {
                db.execute_batch(sql)?;
            }
        }
        db.execute_batch(&format!(
            "UPDATE store_meta SET schema_version={version}; PRAGMA user_version={version};"
        ))?;
        Ok(())
    })();
    db.execute_batch(if result.is_ok() {
        "COMMIT; PRAGMA foreign_keys=ON;"
    } else {
        "ROLLBACK; PRAGMA foreign_keys=ON;"
    })?;
    result
}
