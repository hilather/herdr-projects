//! Descriptor-relative, bounded reads of worker-owned artifact sources.
use std::{ffi::{CStr,CString,OsStr,OsString},fs::{File,OpenOptions,Metadata},io::{self,Read},os::{fd::{AsRawFd,FromRawFd,IntoRawFd},unix::{ffi::{OsStrExt,OsStringExt},fs::{OpenOptionsExt,MetadataExt}}},path::{Path,Component},time::{Duration,Instant}};
use anyhow::{Result,Context,ensure};
use crate::runner::Cancellation;

pub const BYTE_LIMIT:u64=50*1024*1024;
pub const ENTRY_LIMIT:usize=10_000;
pub const DEPTH_LIMIT:usize=64;
pub struct Budget {remaining:u64,entries:usize,pub deadline:Instant,pub cancellation:Cancellation}
impl Budget {
    pub fn new()->Self {Self{remaining:BYTE_LIMIT,entries:ENTRY_LIMIT,deadline:Instant::now()+Duration::from_secs(10),cancellation:Cancellation::default()}}
    pub fn check(&self)->Result<()> {
        ensure!(!self.cancellation.is_cancelled(),"artifact source read cancelled");
        ensure!(Instant::now()<self.deadline,"artifact source read deadline elapsed");Ok(())
    }
    pub fn entry(&mut self,depth:usize)->Result<()> {
        self.check()?;ensure!(depth<=DEPTH_LIMIT,"artifact directory nesting exceeds 64 levels");
        self.entries=self.entries.checked_sub(1).context("artifact source exceeds 10000 entries")?;Ok(())
    }
    pub fn read(&mut self,file:&mut File,buffer:&mut [u8])->Result<usize> {
        self.check()?;
        let limit=buffer.len().min(self.remaining.saturating_add(1) as usize);
        let n=file.read(&mut buffer[..limit])?;
        self.remaining=self.remaining.checked_sub(n as u64).context("artifact source exceeds 50 MiB")?;
        self.check()?;Ok(n)
    }
    pub fn size(&self,file:&File)->Result<()> {
        self.check()?;let metadata=file.metadata()?;
        ensure!(metadata.is_file()&&metadata.nlink()==1,"artifact source is not a regular file with one link");
        ensure!(metadata.len()<=self.remaining,"artifact source exceeds 50 MiB");Ok(())
    }
}

pub struct Directory(File);
impl Directory {
    pub fn open(path:&Path)->Result<Self> {
        let path=std::path::absolute(path)?;
        ensure!(path.as_os_str().len()<=4096&&path.components().count()<=256,"artifact source path exceeds bounds");
        let mut directory=Self(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_NONBLOCK).open("/")?);
        for part in path.components() {
            match part {Component::RootDir=>{},Component::Normal(name)=>directory=Self::from_file(directory.child(name)?)?,_=>anyhow::bail!("artifact source path contains non-normal components")}
        }
        Ok(directory)
    }
    pub fn matches_path(&self,path:&Path)->Result<()> {
        let before=self.0.metadata()?;let after=Self::open(path)?.0.metadata()?;
        ensure!(before.dev()==after.dev()&&before.ino()==after.ino(),"artifact source directory identity changed");Ok(())
    }
    pub fn from_file(file:File)->Result<Self> {ensure!(file.metadata()?.is_dir(),"artifact source is not a directory");Ok(Self(file))}
    pub fn child(&self,name:&OsStr)->Result<File> {
        ensure!(Path::new(name).components().count()==1&&matches!(Path::new(name).components().next(),Some(Component::Normal(_))),"invalid artifact path component");
        let name=CString::new(name.as_bytes())?;
        let fd=unsafe{libc::openat(self.0.as_raw_fd(),name.as_ptr(),libc::O_RDONLY|libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_NONBLOCK)};
        if fd<0 {return Err(io::Error::last_os_error().into());}
        // SAFETY: successful openat returned a new, exclusively owned descriptor.
        let file=unsafe{File::from_raw_fd(fd)};
        let metadata=file.metadata()?;ensure!(metadata.is_dir()||metadata.is_file(),"unsupported artifact entry type");Ok(file)
    }
    pub fn optional(&self,name:&OsStr)->Result<Option<File>> {
        match self.child(name) {Ok(file)=>Ok(Some(file)),Err(e) if e.downcast_ref::<io::Error>().is_some_and(|e|e.kind()==io::ErrorKind::NotFound)=>Ok(None),Err(e)=>Err(e)}
    }
    pub fn file(&self,path:&Path)->Result<File> {
        let mut parts=path.components().peekable();let mut directory=None;
        while let Some(part)=parts.next() {
            let Component::Normal(name)=part else {anyhow::bail!("invalid artifact relative path");};
            let current=directory.as_ref().unwrap_or(self);let file=current.child(name)?;
            if parts.peek().is_none(){ensure!(file.metadata()?.is_file(),"artifact source is not a regular file");return Ok(file);}
            directory=Some(Self::from_file(file)?);
        }
        anyhow::bail!("empty artifact relative path")
    }
    pub fn names(&self,budget:&Budget)->Result<Vec<OsString>> {
        budget.check()?;
        // Opening '.' obtains an independent directory offset; dup would share
        // the original offset and make repeated enumeration silently incomplete.
        let fd=unsafe{libc::openat(self.0.as_raw_fd(),c".".as_ptr(),libc::O_RDONLY|libc::O_CLOEXEC|libc::O_DIRECTORY)};
        if fd<0{return Err(io::Error::last_os_error().into());}
        let file=unsafe{File::from_raw_fd(fd)};
        let stream=unsafe{libc::fdopendir(file.as_raw_fd())};
        if stream.is_null(){return Err(io::Error::last_os_error().into());}
        let _=file.into_raw_fd(); // fdopendir now owns it; closedir closes it.
        struct Stream(*mut libc::DIR);
        impl Drop for Stream {fn drop(&mut self){unsafe{libc::closedir(self.0);}}}
        let stream=Stream(stream);let mut names=Vec::new();
        loop {
            budget.check()?;
            // Each stream is confined to this call. readdir storage is copied
            // before the next call; errno distinguishes EOF from read failure.
            unsafe{*errno()=0;}
            let entry=unsafe{libc::readdir(stream.0)};
            if entry.is_null(){let code=unsafe{*errno()};if code!=0{return Err(io::Error::from_raw_os_error(code).into());}break;}
            let name=unsafe{CStr::from_ptr((*entry).d_name.as_ptr())}.to_bytes();
            if name==b"."||name==b".."{continue;}
            ensure!(names.len()<budget.entries,"artifact source exceeds 10000 entries");
            names.push(OsString::from_vec(name.to_vec()));
        }
        names.sort();Ok(names)
    }
}
#[cfg(any(target_os="linux",target_os="android"))]
unsafe fn errno()->*mut libc::c_int {unsafe{libc::__errno_location()}}
#[cfg(any(target_os="macos",target_os="ios",target_os="freebsd"))]
unsafe fn errno()->*mut libc::c_int {unsafe{libc::__error()}}

pub fn report(path:&Path)->Result<Option<Vec<u8>>> {
    let directory=match Directory::open(path){Ok(dir)=>dir,Err(error) if error.downcast_ref::<io::Error>().is_some_and(|e|e.kind()==io::ErrorKind::NotFound)=>return Ok(None),Err(error)=>return Err(error)};
    let Some(mut file)=directory.optional(OsStr::new("report.md"))? else{return Ok(None);};
    let mut budget=Budget::new();budget.size(&file)?;let before=file.metadata()?;let mut bytes=Vec::new();let mut buffer=[0;64*1024];
    loop {let n=budget.read(&mut file,&mut buffer)?;if n==0 {break;}bytes.extend_from_slice(&buffer[..n]);}
    unchanged(&file,&before)?;directory.matches_path(path)?;Ok(Some(bytes))
}

pub fn unchanged(file:&File,before:&Metadata)->Result<()> {
    let after=file.metadata()?;
    ensure!(after.is_file()&&after.nlink()==1&&before.dev()==after.dev()&&before.ino()==after.ino()&&before.len()==after.len()
        &&before.mtime()==after.mtime()&&before.mtime_nsec()==after.mtime_nsec()&&before.ctime()==after.ctime()&&before.ctime_nsec()==after.ctime_nsec(),"artifact source changed during read");Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs,io::Write,os::unix::fs::symlink};

    #[test]
    fn enumeration_restarts_and_open_handles_survive_ancestor_replacement() {
        let temp=tempfile::tempdir().unwrap();
        let parent=temp.path().join("parent");let source=parent.join("source");
        fs::create_dir_all(&source).unwrap();fs::write(source.join("report.md"),b"original").unwrap();
        let opened=Directory::open(&source).unwrap();
        let expected=vec![OsString::from("report.md")];
        assert_eq!(opened.names(&Budget::new()).unwrap(),expected);
        assert_eq!(opened.names(&Budget::new()).unwrap(),expected);
        let replacement=temp.path().join("replacement");
        fs::create_dir_all(replacement.join("source")).unwrap();
        fs::write(replacement.join("source/report.md"),b"outside").unwrap();
        fs::rename(&parent,temp.path().join("old-parent")).unwrap();
        symlink(&replacement,&parent).unwrap();
        let mut bytes=Vec::new();opened.file(Path::new("report.md")).unwrap().read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes,b"original");
        assert!(opened.matches_path(&source).is_err());assert!(Directory::open(&source).is_err());
        assert!(opened.file(Path::new("../report.md")).is_err());
    }

    #[test]
    fn missing_report_is_distinct_from_unsafe_or_oversize_report() {
        let temp=tempfile::tempdir().unwrap();let path=temp.path().join("report.md");
        assert_eq!(report(temp.path()).unwrap(),None);
        assert_eq!(report(&temp.path().join("missing")).unwrap(),None);
        for kind in ["symlink","hardlink","fifo","oversize","directory"] {
            match kind {
                "symlink"=>symlink("missing",&path).unwrap(),
                "hardlink"=>{fs::write(temp.path().join("original"),b"data").unwrap();fs::hard_link(temp.path().join("original"),&path).unwrap();},
                "fifo"=>{let name=CString::new(path.as_os_str().as_bytes()).unwrap();assert_eq!(unsafe{libc::mkfifo(name.as_ptr(),0o600)},0);},
                "oversize"=>File::create(&path).unwrap().set_len(BYTE_LIMIT+1).unwrap(),
                "directory"=>fs::create_dir(&path).unwrap(),
                _=>unreachable!(),
            }
            assert!(report(temp.path()).is_err(),"{kind}");
            if kind=="directory" {fs::remove_dir(&path).unwrap();} else {fs::remove_file(&path).unwrap();}
        }
    }

    #[test]
    fn growth_and_aggregate_bytes_are_bounded_during_reads() {
        let temp=tempfile::tempdir().unwrap();let path=temp.path().join("file");
        fs::write(&path,b"ab").unwrap();let mut file=File::open(&path).unwrap();
        let before=file.metadata().unwrap();let mut budget=Budget::new();budget.remaining=3;
        budget.size(&file).unwrap();let mut buffer=[0;2];
        assert_eq!(budget.read(&mut file,&mut buffer).unwrap(),2);
        OpenOptions::new().append(true).open(&path).unwrap().write_all(b"cd").unwrap();
        assert!(budget.read(&mut file,&mut buffer).is_err());
        assert!(unchanged(&file,&before).is_err());
        assert!(budget.size(&File::open(&path).unwrap()).is_err());
    }

    #[test]
    fn enumeration_depth_deadline_and_cancellation_limits_fail_closed() {
        let temp=tempfile::tempdir().unwrap();fs::write(temp.path().join("a"),b"").unwrap();fs::write(temp.path().join("b"),b"").unwrap();
        let directory=Directory::open(temp.path()).unwrap();let mut budget=Budget::new();budget.entries=1;
        assert!(directory.names(&budget).is_err());budget.entry(0).unwrap();assert!(budget.entry(0).is_err());
        assert!(Budget::new().entry(DEPTH_LIMIT+1).is_err());
        let mut expired=Budget::new();expired.deadline=Instant::now();assert!(directory.names(&expired).is_err());
        let cancelled=Budget::new();cancelled.cancellation.cancel();assert!(directory.names(&cancelled).is_err());
    }
}
