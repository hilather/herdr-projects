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
    if let Some((old_size,availability,collection,pins,token))=get(db,hash)? {
        if old_size!=size {return Err(invalid("object size mismatch"));}
        if collection=="gc_deleting" {return Err(invalid("object is being deleted"));}
        db.execute("UPDATE objects SET availability='available',collection='unclaimed' WHERE hash=?1 AND collection='gc_pending'",[hash])?;
        let _=(availability,pins,token);return Ok(());
    }
    db.execute("INSERT INTO objects VALUES(?1,?2,'available','unclaimed',0,0)",params![hash,size])?;
    Ok(())
}
pub(super) fn pin(db:&Connection,hash:&str)->Result<()> {
    require_available(db,hash)?;
    db.execute("UPDATE objects SET pin_count=pin_count+1,collection='unclaimed' WHERE hash=?1",[hash])?;
    Ok(())
}
pub(super) fn referenced(db:&Connection,hash:&str)->Result<bool> {
    let n:i64=db.query_row("SELECT EXISTS(SELECT 1 FROM memory_revisions WHERE body_hash=?1 OR provenance_hash=?1)",[hash],|r|r.get(0))?;
    Ok(n!=0)
}
pub(super) fn claim_unreferenced(db:&Connection,limit:usize)->Result<Vec<(String,i64)>> {
    let mut stmt=db.prepare("SELECT hash,fencing_token FROM objects WHERE collection='unclaimed' AND pin_count=0 AND availability='available' AND NOT EXISTS (SELECT 1 FROM memory_revisions WHERE body_hash=objects.hash OR provenance_hash=objects.hash) LIMIT ?1")?;
    let mut rows=stmt.query([limit as i64])?;
    let mut claimed=Vec::new();
    while let Some(row)=rows.next()? {
        let hash:String=row.get(0)?;let token:i64=row.get(1)?;
        db.execute("UPDATE objects SET collection='gc_pending',fencing_token=fencing_token+1 WHERE hash=?1 AND collection='unclaimed' AND pin_count=0",[hash.as_str()])?;
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
