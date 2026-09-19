fn main() -> rusqlite::Result<()> {
    assert!(rusqlite::version_number() >= 3_053_004, "SQLite 3.53.4+ is required by the reviewed engine policy");
    let mut db = rusqlite::Connection::open_in_memory()?;
    db.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE tasks(id TEXT PRIMARY KEY, rev INTEGER NOT NULL); CREATE TABLE events(seq INTEGER PRIMARY KEY AUTOINCREMENT, task TEXT NOT NULL REFERENCES tasks(id)); INSERT INTO tasks VALUES('t',1);")?;
    { let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
      assert_eq!(tx.execute("UPDATE tasks SET rev=2 WHERE id='t' AND rev=1", [])?, 1);
      tx.execute("INSERT INTO events(task) VALUES('t')", [])?;
      tx.commit()?; }
    { let tx = db.transaction()?;
      assert_eq!(tx.execute("UPDATE tasks SET rev=3 WHERE id='t' AND rev=1", [])?, 0); }
    assert_eq!(db.query_row("SELECT count(*) FROM events", [], |r| r.get::<_,u32>(0))?, 1);
    assert_eq!(db.query_row("PRAGMA integrity_check", [], |r| r.get::<_,String>(0))?, "ok");
    println!("SQLite {}; transaction, CAS, rollback, event and integrity checks passed", rusqlite::version());
    Ok(())
}
