//! Content-addressed object rows. Bytes live beside the database under objects/sha256.
use super::*;

pub(super) fn schema(db:&Connection)->Result<()> {
    check_schema(db)?;let n:u32=db.query_row("PRAGMA user_version",[],|r|r.get(0))?;
    if n<18 {return Err(StoreError::UnsupportedSchema(n));}Ok(())
}
fn invalid(s:&str)->StoreError {StoreError::Invalid(s.into())}
pub(super) fn get(db:&Connection,hash:&str)->Result<Option<(i64,String,String,i64,i64)>> {
    let mut stmt=db.prepare("SELECT size,availability,collection,pin_count,fencing_token FROM objects WHERE hash=?1")?;
    let mut rows=stmt.query([hash])?;
    match rows.next()? {
        Some(row)=>Ok(Some((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))),
        None=>Ok(None),
    }
}
pub(super) fn require_available(db:&Connection,hash:&str)->Result<()> {
    match get(db,hash)? {
        Some((_,availability,collection,_,_)) if availability=="available" && collection!="gc_deleting" => Ok(()),
        _ => Err(invalid("missing object")),
    }
}
pub(super) fn upsert_available(db:&Connection,hash:&str,size:i64)->Result<()> {
    if let Some((old_size,_,_,_,_))=get(db,hash)? {
        if old_size!=size {return Err(invalid("object size mismatch"));}
        // Caller publishes verified bytes under the object I/O lock before this update.
        db.execute("UPDATE objects SET availability='available',collection='unclaimed',fencing_token=fencing_token+1 WHERE hash=?1",[hash])?;
        return Ok(());
    }
    db.execute("INSERT INTO objects VALUES(?1,?2,'available','unclaimed',0,0)",params![hash,size])?;
    Ok(())
}
pub(super) fn pin(db:&Connection,hash:&str)->Result<()> {
    require_available(db,hash)?;
    db.execute("UPDATE objects SET pin_count=pin_count+1,collection='unclaimed' WHERE hash=?1",[hash])?;
    Ok(())
}
// Proposal JSON is immutable. Retain all accepted body/evidence references even
// after review or promotion, including proposals created before retention was fixed.
fn proposal_reference_sql(db: &Connection, hash: &str) -> Result<String> {
    let version: u32 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 21 { return Ok("0".into()); }
    let candidates = if version>=23 {format!(" OR EXISTS(SELECT 1 FROM memory_import_candidates WHERE body_hash={hash} OR provenance_hash={hash})")} else {String::new()};
    Ok(format!("EXISTS (SELECT 1 FROM memory_proposals p, json_each(p.payload,'$.changes') c \
        WHERE p.review_state='validated' AND (\
        json_extract(c.value,'$.body_object') IN ({hash}, 'sha256:' || {hash}) OR \
        EXISTS (SELECT 1 FROM json_each(c.value,'$.evidence') e WHERE \
        json_extract(e.value,'$.object') IN ({hash}, 'sha256:' || {hash})))) {candidates}"))
}
pub(super) fn referenced(db:&Connection,hash:&str)->Result<bool> {
    let proposal = proposal_reference_sql(db, "?1")?;
    Ok(db.query_row(&format!("SELECT EXISTS(SELECT 1 FROM memory_revisions WHERE body_hash=?1 OR provenance_hash=?1) OR {proposal}"),[hash],|r|r.get(0))?)
}
pub(super) fn claim_unreferenced(db:&Connection,limit:usize)->Result<Vec<(String,i64)>> {
    let proposal = proposal_reference_sql(db, "objects.hash")?;
    let mut stmt=db.prepare(&format!("SELECT hash,fencing_token FROM objects WHERE pin_count=0 AND availability='available' AND NOT EXISTS (SELECT 1 FROM memory_revisions WHERE body_hash=objects.hash OR provenance_hash=objects.hash) AND NOT ({proposal}) ORDER BY hash LIMIT ?1"))?;
    let candidates = stmt.query_map([limit as i64], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut claimed=Vec::new();
    for (hash, token) in candidates {
        // Reclaim interrupted pending/deleting states with a fresh fence. The
        // service holds the I/O lock, so no previous collector can still unlink.
        db.execute("UPDATE objects SET collection='gc_pending',fencing_token=fencing_token+1 WHERE hash=?1 AND fencing_token=?2 AND pin_count=0",params![hash,token])?;
        if db.changes()==1 {claimed.push((hash,token+1));}
    }
    Ok(claimed)
}
pub(super) fn begin_delete(db:&Connection,hash:&str,token:i64)->Result<bool> {
    if referenced(db,hash)? {return Ok(false);}
    let Some((_,availability,collection,pins,current))=get(db,hash)? else {return Ok(false)};
    if availability!="available"||collection!="gc_pending"||pins!=0||current!=token {return Ok(false);}
    db.execute("UPDATE objects SET collection='gc_deleting' WHERE hash=?1 AND collection='gc_pending' AND fencing_token=?2 AND pin_count=0",params![hash,token])?;
    Ok(db.changes()==1)
}
pub(super) fn finish_purge(db:&Connection,hash:&str,token:i64)->Result<()> {
    db.execute("UPDATE objects SET availability='purged',collection='unclaimed' WHERE hash=?1 AND collection='gc_deleting' AND fencing_token=?2",params![hash,token])?;
    Ok(())
}
pub(super) fn cancel_pending(db:&Connection,hash:&str)->Result<()> {
    db.execute("UPDATE objects SET collection='unclaimed' WHERE hash=?1 AND collection='gc_pending'",[hash])?;
    Ok(())
}
impl SqliteStore {
    pub fn upsert_object(&mut self,hash:&str,size:i64)->Result<()> {
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;
        upsert_available(&tx,hash,size)?;tx.commit()?;Ok(())
    }
    pub fn object_available(&mut self,hash:&str)->Result<bool> {
        let tx=self.connection.transaction()?;schema(&tx)?;
        Ok(get(&tx,hash)?.is_some_and(|(_,availability,collection,_,_)| availability=="available" && collection!="gc_deleting"))
    }
    pub fn pin_object(&mut self,hash:&str)->Result<()> {
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;
        pin(&tx,hash)?;tx.commit()?;Ok(())
    }
    pub fn claim_gc(&mut self,limit:usize)->Result<Vec<(String,i64)>> {
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;
        let claimed=claim_unreferenced(&tx,limit)?;tx.commit()?;Ok(claimed)
    }
    pub fn begin_gc_delete(&mut self,hash:&str,token:i64)->Result<bool> {
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;
        let ok=begin_delete(&tx,hash,token)?;tx.commit()?;Ok(ok)
    }
    pub fn finish_gc_purge(&mut self,hash:&str,token:i64)->Result<()> {
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;
        finish_purge(&tx,hash,token)?;tx.commit()?;Ok(())
    }
}
