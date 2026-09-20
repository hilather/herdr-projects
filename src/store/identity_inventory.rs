//! Bounded, read-only runtime identity evidence; never a full store snapshot.
use super::*;
use anyhow::{Result,ensure};
use std::time::Instant;
use crate::runner::Cancellation;

pub struct Budget {pub deadline:Instant,pub cancellation:Cancellation,remaining:usize,records:usize,used:usize}
impl Budget {
    pub fn new(bytes:usize,records:usize,deadline:Instant,cancellation:Cancellation)->Result<Self> {
        ensure!(bytes<=50*1024*1024&&records<=1024,"identity budget exceeds supported bounds");
        let budget=Self{deadline:deadline.min(Instant::now()+Duration::from_secs(10)),cancellation,remaining:bytes,records,used:0};budget.check()?;Ok(budget)
    }
    pub fn check(&self)->Result<()> {ensure!(!self.cancellation.is_cancelled()&&Instant::now()<self.deadline,"identity inventory cancelled or expired");Ok(())}
    pub fn used(&self)->usize {self.used}
    pub(crate) fn charge(&mut self,n:usize)->Result<()> {self.check()?;ensure!(n<=self.remaining,"identity inventory exceeds byte budget");self.remaining-=n;self.used+=n;Ok(())}
    pub(crate) fn read(&mut self,path:&Path)->Result<Vec<u8>> {
        use std::{io::Read,os::unix::fs::MetadataExt};
        self.check()?;
        let mut file=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(path)?;
        let before=file.metadata()?;ensure!(before.is_file()&&before.nlink()==1,"identity publication must be a single-link regular file");
        let limit=self.remaining.min(16*1024*1024);ensure!(before.len()<=limit as u64,"identity publication exceeds budget");
        let mut bytes=Vec::new();(&mut file).take(limit as u64+1).read_to_end(&mut bytes)?;ensure!(bytes.len()<=limit,"identity publication exceeds budget");
        let after=file.metadata()?;ensure!(before.len()==after.len()&&before.mtime()==after.mtime()&&before.mtime_nsec()==after.mtime_nsec()&&before.ctime()==after.ctime()&&before.ctime_nsec()==after.ctime_nsec(),"identity publication changed");
        self.charge(bytes.len())?;Ok(bytes)
    }
}
pub(crate) struct Publication {pub digest:String,pub sources:u64,pub tasks:u64,pub operations:u64,pub reconciliation_required:bool}
/// Caller has validated and budgeted the active journal and format marker.
fn read_published<T>(path:&Path,publication:&Publication,budget:&mut Budget,read:impl FnOnce(&rusqlite::Transaction<'_>,&mut Budget)->Result<T>)->Result<T> {
    use std::os::unix::fs::MetadataExt;
    budget.check()?;engine_check()?;
    for suffix in ["-wal","-shm","-journal"] {
        let mut name=path.as_os_str().to_os_string();name.push(suffix);
        match std::fs::symlink_metadata(Path::new(&name)) {
            Ok(m)=>ensure!(m.is_file()&&m.nlink()==1,"identity database sidecar must be a single-link regular file"),
            Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{},Err(e)=>return Err(e.into()),
        }
    }
    let before=std::fs::symlink_metadata(path)?;ensure!(before.is_file()&&before.nlink()==1,"identity database must be a single-link regular file");
    let mut db=Connection::open_with_flags(path,OpenFlags::SQLITE_OPEN_READ_ONLY|OpenFlags::SQLITE_OPEN_NO_MUTEX|OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
    // SQLite also limits whole encoded rows, so allow headroom for a bounded
    // 16 MiB provenance field plus its identity payload and metadata.
    db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH,32*1024*1024)?;
    let deadline=budget.deadline;let cancellation=budget.cancellation.clone();
    db.progress_handler(1000,Some(move||cancellation.is_cancelled()||Instant::now()>=deadline));
    db.busy_timeout(Duration::from_millis(10))?;
    db.execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF;")?;
    let tx=db.transaction()?;check_schema(&tx)?;
    let version:u32=tx.query_row("PRAGMA user_version",[],|r|r.get(0))?;ensure!(version>=5,"identity inventory requires schema v5");
    let receipt:(String,u64,u64,u64)=tx.query_row("SELECT source_digest,source_count,task_count,operation_count FROM migration_receipt WHERE singleton=1 AND octet_length(source_digest)=64",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    ensure!(receipt==(publication.digest.clone(),publication.sources,publication.tasks,publication.operations),"identity migration receipt mismatch");
    let required=if version>=7 {tx.query_row("SELECT reconciliation_required FROM project_control WHERE singleton=1",[],|r|r.get::<_,bool>(0))?}else{true};
    ensure!(required==publication.reconciliation_required,"identity control publication interrupted");
    let value=read(&tx,budget)?;budget.check()?;
    let after=std::fs::symlink_metadata(path)?;ensure!(before.dev()==after.dev()&&before.ino()==after.ino()&&after.is_file()&&after.nlink()==1,"identity database replaced");
    Ok(value)
}

/// Bounded publication-checked revision; no integrity scan or payload inventory.
pub(crate) fn read_head(path:&Path,publication:&Publication,budget:&mut Budget)->Result<u64> {
    read_published(path,publication,budget,|tx,budget|{budget.charge(8)?;Ok(super::head(tx)?)})
}

pub(crate) fn read(path:&Path,publication:&Publication,budget:&mut Budget)->Result<Vec<RuntimeBinding>> {
    read_published(path,publication,budget,|tx,budget|{
    let version:u32=tx.query_row("PRAGMA user_version",[],|r|r.get(0))?;
    for (minimum,table) in [(6,"runtime_observations"),(9,"runtime_ownership")] {
        if version>=minimum {
            let dangling:bool=tx.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table} r LEFT JOIN runtime_bindings b ON b.id=r.binding_id WHERE b.id IS NULL)"),[],|r|r.get(0))?;
            ensure!(!dangling,"identity inventory contains dangling resource references");
        }
    }
    // Measure every field read by runtime::read_all before materializing any
    // payload or provenance blob. The snapshot stays fixed through validation.
    let mut statement=tx.prepare("SELECT octet_length(b.id),coalesce(octet_length(b.task_id),0),coalesce(octet_length(b.source_path),0),octet_length(b.payload),octet_length(b.payload_hash),coalesce(octet_length(s.digest),0),coalesce(length(s.bytes),0) FROM runtime_bindings b LEFT JOIN legacy_sources s ON s.path=b.source_path ORDER BY b.id LIMIT ?1")?;
    let mut rows=statement.query([budget.records as u64+1])?;let mut count=0;
    while let Some(row)=rows.next()? {
        budget.check()?;count+=1;ensure!(count<=budget.records,"identity inventory exceeds reference budget");
        for column in 0..7 {let n:usize=row.get(column)?;ensure!(n<=16*1024*1024,"identity field exceeds 16 MiB");budget.charge(n)?;}
    }
    drop(rows);drop(statement);
    let mut statement=tx.prepare("SELECT octet_length(digest),length(bytes) FROM legacy_sources WHERE path='.state/coordinator.json' AND kind='runtime'")?;
    let mut rows=statement.query([])?;
    if let Some(row)=rows.next()? {for column in 0..2 {let n:usize=row.get(column)?;ensure!(n<=16*1024*1024,"identity session provenance exceeds 16 MiB");budget.charge(n)?;}}
    drop(rows);drop(statement);budget.check()?;
    let bindings=super::runtime::read_all(tx)?;ensure!(bindings.len()==count,"identity inventory changed");budget.check()?;
    budget.records-=count;Ok(bindings)
    })
}
