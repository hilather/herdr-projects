//! Explicit SQL execution controls. Aggregate decoded allocation is not yet bounded.
use super::*;
use std::{os::unix::fs::MetadataExt,sync::{Arc,atomic::{AtomicU8,Ordering}},time::Instant};
use crate::runner::Cancellation;

#[derive(Clone)]
pub struct ReadControl {deadline:Instant,cancellation:Cancellation,row_bytes:i32}
impl ReadControl {
    pub fn new(deadline:Instant,cancellation:Cancellation)->Self {Self{deadline,cancellation,row_bytes:32*1024*1024}}
    pub fn deadline(&self)->Instant {self.deadline}
    pub fn cancellation(&self)->Cancellation {self.cancellation.clone()}
    pub fn check(&self)->Result<()> {match self.reason(){1=>Err(StoreError::Cancelled),2=>Err(StoreError::Deadline),_=>Ok(())}}
    fn reason(&self)->u8 {if self.cancellation.is_cancelled(){1}else if Instant::now()>=self.deadline{2}else{0}}
    pub fn with_row_limit(mut self,bytes:usize)->Result<Self> {
        if !(1024*1024..=32*1024*1024).contains(&bytes){return Err(StoreError::Invalid("encoded row limit must be 1–32 MiB".into()));}
        self.row_bytes=bytes as i32;Ok(self)
    }
}

/// Only explicitly controlled entry points are exposed; no raw-connection or
/// Deref escape permits callers to replace hooks or renew an expired deadline.
pub struct ControlledStore {store:SqliteStore,control:ReadControl,interrupted:Arc<AtomicU8>}
impl ControlledStore {
    pub fn open(path:&Path,control:ReadControl)->Result<Self> {
        control.check()?;engine_check()?;
        let before=std::fs::symlink_metadata(path).map_err(|e|StoreError::Io(e.to_string()))?;
        if !before.is_file()||before.nlink()!=1{return Err(StoreError::Invalid("controlled database must be a single-link regular file".into()));}
        for suffix in ["-wal","-shm","-journal"] {
            let mut name=path.as_os_str().to_os_string();name.push(suffix);
            match std::fs::symlink_metadata(Path::new(&name)) {
                Ok(m) if !m.is_file()||m.nlink()!=1=>return Err(StoreError::Invalid("controlled database sidecar must be a single-link regular file".into())),
                Ok(_)=>{},Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{},Err(e)=>return Err(StoreError::Io(e.to_string())),
            }
        }
        control.check()?;
        let connection=Connection::open_with_flags(path,OpenFlags::SQLITE_OPEN_READ_WRITE|OpenFlags::SQLITE_OPEN_NO_MUTEX|OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
        let interrupted=Arc::new(AtomicU8::new(0));
        connection.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH,control.row_bytes)?;
        let c=control.clone();let reason=interrupted.clone();
        connection.progress_handler(1000,Some(move||{let n=c.reason();if n!=0{reason.store(n,Ordering::SeqCst);}n!=0}));
        let c=control.clone();let reason=interrupted.clone();
        connection.commit_hook(Some(move||{let n=c.reason();if n!=0{reason.store(n,Ordering::SeqCst);}n!=0}));
        connection.busy_timeout(Duration::from_millis(10).min(control.deadline.saturating_duration_since(Instant::now())))?;
        let store=Self{store:SqliteStore{connection},control,interrupted};
        store.read(|s|{s.connection.execute_batch("PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF; PRAGMA synchronous=FULL;")?;Ok(())})?;
        store.read(|s|check_schema(&s.connection))?;
        store.read(|s|enable_wal(&s.connection))?;
        store.read(SqliteStore::integrity_check)?;
        let after=std::fs::symlink_metadata(path).map_err(|e|StoreError::Io(e.to_string()))?;
        if !after.is_file()||after.nlink()!=1||(before.dev(),before.ino())!=(after.dev(),after.ino()){return Err(StoreError::Invalid("controlled database changed during open".into()));}
        store.control.check()?;Ok(store)
    }
    fn error(&self,error:StoreError)->StoreError {
        match self.interrupted.load(Ordering::SeqCst) {1=>StoreError::Cancelled,2=>StoreError::Deadline,_=>error}
    }
    fn read<T>(&self,read:impl FnOnce(&SqliteStore)->Result<T>)->Result<T> {
        self.control.check()?;let value=read(&self.store).map_err(|e|self.error(e))?;self.control.check()?;Ok(value)
    }
    pub fn project_control(&self)->Result<Option<ProjectControl>> {self.read(SqliteStore::project_control)}
    pub fn import_operation_count(&self)->Result<u64> {self.read(SqliteStore::import_operation_count)}
    pub fn import_receipt(&self)->Result<(String,u64,u64)> {self.read(SqliteStore::import_receipt)}
    pub fn read_snapshot(&mut self,at:Option<u64>)->Result<Snapshot> {
        self.control.check()?;let value=self.store.read_snapshot(at).map_err(|e|self.error(e))?;self.control.check()?;Ok(value)
    }
    pub(crate) fn schedule_routine(&mut self,prepared:&PreparedRoutineTick,head:u64)->Result<Option<RoutineOccurrence>> {
        self.mutation(|store|store.schedule_routine(prepared,head))
    }
    fn mutation<T>(&mut self,write:impl FnOnce(&mut SqliteStore)->Result<T>)->Result<T> {
        self.control.check()?;
        // A successful commit remains success even if cancellation arrives
        // during final fsync. Only a latched hook veto explains a failed commit.
        write(&mut self.store).map_err(|e|self.error(e))
    }
}

#[cfg(test)]
mod tests;
