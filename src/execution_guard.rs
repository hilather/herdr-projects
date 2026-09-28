//! Cooperative ownership shared by the CLI and canonical library services.
//! Order: root barrier, project effect ownership, resource fences sorted by
//! `(class, identity)`, then any short record lock. Never upgrade a shared
//! lock in place, and never take an exclusive root guard while holding one.
//! A `CheckGuard` keeps only the root and one fence; it releases the fence
//! before taking project ownership again.
use std::{fs::{File,OpenOptions},mem::ManuallyDrop,os::{fd::{AsRawFd,RawFd},unix::fs::{DirBuilderExt,OpenOptionsExt,MetadataExt}},path::{Path,PathBuf},
    process::{Child,Command,ExitStatus,Output,Stdio},sync::{PoisonError,RwLock,atomic::{AtomicBool,Ordering}}};
use anyhow::{Result,Context,ensure};
use serde::{Deserialize,Serialize};
use sha2::{Digest,Sha256};

/// Process creation and the close of a transferred lock exclude each other.
/// A child forked by any thread holds a copy of every descriptor until it
/// execs (CLOEXEC then closes it), and a flock belongs to the open file
/// description, so closing the parent's last copy of a transferred lock while
/// another thread is between fork and exec would leave that child holding it.
/// Every spawn in this crate goes through [`GatedSpawn`], which holds the gate
/// shared only until the child has exec'd; closing a transferred copy holds it
/// exclusively. Clippy's `disallowed-methods` refuses the ungated calls.
static SPAWN_GATE:RwLock<()>=RwLock::new(());

/// Spawning through the gate. `output_gated` pipes stdout and stderr and
/// closes stdin, as `Command::output` does by default.
pub trait GatedSpawn {
    fn spawn_gated(&mut self)->std::io::Result<Child>;
    fn status_gated(&mut self)->std::io::Result<ExitStatus> {self.spawn_gated()?.wait()}
    fn output_gated(&mut self)->std::io::Result<Output>;
}
impl GatedSpawn for Command {
    #[allow(clippy::disallowed_methods)]
    fn spawn_gated(&mut self)->std::io::Result<Child> {
        // std returns from spawn only after the child has exec'd or failed.
        let _gate=SPAWN_GATE.read().unwrap_or_else(PoisonError::into_inner);self.spawn()
    }
    fn output_gated(&mut self)->std::io::Result<Output> {
        self.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn_gated()?.wait_with_output()
    }
}
/// Close a descriptor of a transferred lock while no fork awaits exec.
fn close_transferred(file:File) {let _gate=SPAWN_GATE.write().unwrap_or_else(PoisonError::into_inner);drop(file);}

/// One copy of a transferred lock's description, closed through the gate.
#[derive(Debug)]
pub(crate) struct TransferredFile(ManuallyDrop<File>);
impl AsRawFd for TransferredFile {fn as_raw_fd(&self)->RawFd {self.0.as_raw_fd()}}
impl Drop for TransferredFile {
    // SAFETY: the file is taken once, here, and never used again.
    fn drop(&mut self) {close_transferred(unsafe{ManuallyDrop::take(&mut self.0)})}
}

/// A held lock. Dropping it unlocks explicitly: a child forked by any thread
/// shares the open file description until it execs, so merely closing the
/// descriptor would leave the lock held for that window. A lock transferred to
/// a supervisor must outlive this handle, so it is never unlocked here; its
/// copies close through the spawn gate instead.
pub(crate) struct LockFile {file:ManuallyDrop<File>,transferred:AtomicBool}
impl LockFile {
    /// A descriptor sharing this lock for a supervisor that keeps it held.
    pub(crate) fn transfer(&self)->Result<TransferredFile> {
        self.transferred.store(true,Ordering::SeqCst);Ok(TransferredFile(ManuallyDrop::new(self.file.try_clone()?)))
    }
}
impl Drop for LockFile {
    fn drop(&mut self) {
        // SAFETY: the file is taken once, here, and never used again.
        let file=unsafe{ManuallyDrop::take(&mut self.file)};
        if self.transferred.load(Ordering::SeqCst) {close_transferred(file)} else {let _=file.unlock();}
    }
}

fn lock_file(path:&Path)->Result<File> {
    let file=OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .mode(0o600).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(path)?;
    ensure!(file.metadata()?.is_file(),"execution lock must be a regular file");
    Ok(file)
}
fn held(file:File)->LockFile {LockFile{file:ManuallyDrop::new(file),transferred:AtomicBool::new(false)}}
pub(crate) fn exclusive_file(path:&Path)->Result<LockFile> {
    let file=lock_file(path)?;
    file.try_lock().with_context(|| format!("another operation owns lock {}; retry", path.display()))?;
    Ok(held(file))
}
fn shared_file(path:&Path,busy:&'static str)->Result<LockFile> {
    let file=lock_file(path)?;
    file.try_lock_shared().context(busy)?;
    Ok(held(file))
}

/// Exclusive compatibility barrier for migration, cleanup and terminal effects.
pub struct RootGuard {_file:LockFile}
impl RootGuard {
    #[cfg(feature="state-store")]
    pub(crate) fn inherit(&self)->Result<Vec<crate::runner::InheritedLock>> {
        Ok(vec![crate::runner::InheritedLock::new(self._file.transfer()?)])
    }
    pub fn exclusive(root:&Path)->Result<Self> {
        let file=exclusive_file(&root.join(".execution.lock"))?;
        Ok(Self{_file:file})
    }
    fn shared(root:&Path)->Result<Self> {
        let file=shared_file(&root.join(".execution.lock"),"root maintenance or exclusive external operation is active; retry")?;
        Ok(Self{_file:file})
    }
}

fn matches_project(guard_project:&Path,identity:(u64,u64),project:&Path)->Result<()> {
    let metadata=std::fs::metadata(project)?;
    ensure!(project.canonicalize()?==guard_project&&(metadata.dev(),metadata.ino())==identity,"execution guard belongs to a different project");Ok(())
}

/// Declared footprint. `git` is a common git directory. `artifact` is one
/// project's publication directory. `scratch` is one job's scratch directory.
/// Unknown classes are not a footprint.
#[derive(Clone,Debug,PartialEq,Eq,PartialOrd,Ord,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resource {pub class:String,pub identity:String}
impl Resource {
    pub fn new(class:impl Into<String>,identity:impl Into<String>)->Result<Self> {
        let class=class.into();let identity=identity.into();
        ensure!(matches!(class.as_str(),"git"|"artifact"|"scratch"),"unknown effect resource");
        ensure!(!identity.is_empty()&&identity.len()<=4096&&!identity.chars().any(char::is_control)&&Path::new(&identity).is_absolute(),"invalid effect resource");
        Ok(Self{class,identity})
    }
}
pub trait ProjectEffect {fn check_project(&self,project:&Path)->Result<()>;}

/// Excludes effects within one project while preserving the root-wide barrier.
/// Never acquire an exclusive root guard while retaining this shared guard.
pub struct ProjectGuard {_project:LockFile,_root:RootGuard,root:std::path::PathBuf,project:std::path::PathBuf,identity:(u64,u64)}
impl ProjectEffect for ProjectGuard {fn check_project(&self,project:&Path)->Result<()> {ProjectGuard::check_project(self,project)}}
impl ProjectGuard {
    pub fn check_project(&self,project:&Path)->Result<()> {matches_project(&self.project,self.identity,project)}
    /// A trusted transfer supervisor keeps these descriptions open until its
    /// descendants finish, even if the caller dies. Retain the returned handles
    /// in the caller through publication. The historical lock name also fences
    /// routine jobs, including those surviving a previous ticker instance.
    pub fn inherit_transfer(&self)->Result<Vec<crate::runner::InheritedLock>> {
        Ok(vec![crate::runner::InheritedLock::new(self._root._file.transfer()?),crate::runner::InheritedLock::new(self._project.transfer()?),
            crate::runner::InheritedLock::new(exclusive_file(&self.root.join(".routine-execution.lock"))?.transfer()?)])
    }
    /// Take `resource`'s fence under this ownership (fences follow the project lock).
    pub fn fence(&self,resource:&Resource)->Result<Fence> {
        Resource::new(&resource.class,&resource.identity)?;
        Ok(Fence{_file:fence_file(&self.root,resource)?})
    }
    /// Keep the shared root and `fence`; release project ownership. Other
    /// project effects may then run, while root-exclusive maintenance waits.
    pub fn narrow(self,fence:Fence)->CheckGuard {
        let ProjectGuard{_project,_root,project,identity,..}=self;drop(_project);
        CheckGuard{_fence:fence,_root,project,identity}
    }
    pub fn acquire(project:&Path)->Result<Self> {
        let project=project.canonicalize()?;
        let root_path=project.parent().context("project has no root")?.to_path_buf();
        let root=RootGuard::shared(&root_path)?;
        ensure!(std::fs::symlink_metadata(project.join(".state"))?.is_dir(),"project state must be a real directory");
        let file=exclusive_file(&project.join(".state/effect.lock"))?;
        let metadata=std::fs::metadata(&project)?;
        Ok(Self{_project:file,_root:root,root:root_path,project,identity:(metadata.dev(),metadata.ino())})
    }
}

/// An exclusive resource fence held beside project or root ownership.
pub struct Fence {_file:LockFile}
fn fence_file(root:&Path,resource:&Resource)->Result<LockFile> {
    let dir=root.join(".resource-fences");
    match std::fs::symlink_metadata(&dir) {
        Ok(meta)=>ensure!(meta.is_dir()&&!meta.file_type().is_symlink(),"resource fence directory must be a real directory"),
        Err(error) if error.kind()==std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new().mode(0o700).create(&dir).or_else(|err| if err.kind()==std::io::ErrorKind::AlreadyExists {Ok(())} else {Err(err)})?;
            let meta=std::fs::symlink_metadata(&dir)?;
            ensure!(meta.is_dir()&&!meta.file_type().is_symlink(),"resource fence directory must be a real directory");
        }
        Err(error)=>return Err(error.into()),
    }
    let mut hasher=Sha256::new();hasher.update(resource.class.as_bytes());hasher.update([0]);hasher.update(resource.identity.as_bytes());
    exclusive_file(&dir.join(format!("{:x}",hasher.finalize())))
}

/// A long isolated check that needs no project ownership: the root stays
/// shared and the check's fence excludes anyone else from its resource.
pub struct CheckGuard {_fence:Fence,_root:RootGuard,project:PathBuf,identity:(u64,u64)}
impl CheckGuard {
    /// Release the fence, then take project ownership again, retrying while
    /// another effect holds it until `wait` passes. The root stays shared throughout.
    pub fn widen(self,wait:std::time::Duration)->Result<ProjectGuard> {
        let CheckGuard{_fence,_root,project,identity}=self;drop(_fence);
        let until=std::time::Instant::now()+wait;
        let guard=loop {
            match ProjectGuard::acquire(&project) {
                Ok(guard)=>break guard,
                Err(error) if std::time::Instant::now()>=until=>return Err(error.context("project ownership was not regained after the check")),
                Err(_)=>std::thread::sleep(std::time::Duration::from_millis(25)),
            }
        };
        guard.check_project(&project)?;ensure!(guard.identity==identity,"execution guard belongs to a different project");
        Ok(guard)
    }
}

/// Shared project ownership for an effect that declared its footprint.
/// The same git directory stays exclusive; a different repository does not.
/// Holds shared root and project locks only — never upgrades them.
pub struct ProjectSharedGuard {_project:LockFile,_root:RootGuard,_fences:Vec<LockFile>,_routine:LockFile,project:PathBuf,identity:(u64,u64)}
impl ProjectEffect for ProjectSharedGuard {fn check_project(&self,project:&Path)->Result<()> {ProjectSharedGuard::check_project(self,project)}}
impl ProjectSharedGuard {
    pub fn check_project(&self,project:&Path)->Result<()> {matches_project(&self.project,self.identity,project)}
    /// Retain these descriptors through publication. The routine lock is shared
    /// so two declared footprints can overlap; exclusive routine ownership still
    /// belongs to an undeclared `ProjectGuard`.
    pub fn inherit_transfer(&self)->Result<Vec<crate::runner::InheritedLock>> {
        let mut locks=vec![crate::runner::InheritedLock::new(self._root._file.transfer()?),crate::runner::InheritedLock::new(self._project.transfer()?)];
        for fence in &self._fences {locks.push(crate::runner::InheritedLock::new(fence.transfer()?));}
        locks.push(crate::runner::InheritedLock::new(self._routine.transfer()?));Ok(locks)
    }
    pub fn acquire(project:&Path,resources:&[Resource])->Result<Self> {
        ensure!(!resources.is_empty(),"shared guard requires a declared footprint");
        let mut ordered=resources.to_vec();ordered.sort();
        ensure!(ordered.windows(2).all(|pair|pair[0]!=pair[1]),"duplicate resource fence");
        for resource in &ordered {Resource::new(&resource.class,&resource.identity)?;}
        let project=project.canonicalize()?;
        let root_path=project.parent().context("project has no root")?.to_path_buf();
        let root=RootGuard::shared(&root_path)?;
        ensure!(std::fs::symlink_metadata(project.join(".state"))?.is_dir(),"project state must be a real directory");
        let project_file=shared_file(&project.join(".state/effect.lock"),"another operation owns this project; retry")?;
        let mut fences=Vec::with_capacity(ordered.len());
        for resource in &ordered {fences.push(fence_file(&root_path,resource)?);}
        let routine=shared_file(&root_path.join(".routine-execution.lock"),"another operation owns this lock; retry")?;
        let metadata=std::fs::metadata(&project)?;
        Ok(Self{_project:project_file,_root:root,_fences:fences,_routine:routine,project,identity:(metadata.dev(),metadata.ino())})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Spawn `/usr/bin/true` on another thread, paused after fork and before
    /// exec (so before CLOEXEC takes effect) until the returned pipe is written.
    fn child_paused_before_exec()->(File,std::thread::JoinHandle<std::process::ExitStatus>) {
        use std::io::Read;
        use std::os::{fd::{AsRawFd, FromRawFd}, unix::process::CommandExt};
        let pipe = || {
            let mut fds = [0; 2];
            assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
            unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
        };
        let (mut ready_read, ready_write) = pipe();
        let (release_read, release_write) = pipe();
        let spawned = std::thread::spawn(move || {
            let ready_fd = ready_write.as_raw_fd();
            let release_fd = release_read.as_raw_fd();
            let mut command = Command::new("/usr/bin/true");
            // Only async-signal-safe syscalls run in the child, with a deadline.
            unsafe { command.pre_exec(move || {
                let mut byte = 1u8;
                if libc::write(ready_fd, (&byte as *const u8).cast(), 1) != 1 { return Err(std::io::Error::last_os_error()); }
                let mut poll = libc::pollfd { fd: release_fd, events: libc::POLLIN, revents: 0 };
                if libc::poll(&mut poll, 1, 5000) != 1 { return Err(std::io::Error::from_raw_os_error(libc::ETIMEDOUT)); }
                if libc::read(release_fd, (&mut byte as *mut u8).cast(), 1) != 1 { return Err(std::io::Error::last_os_error()); }
                Ok(())
            }); }
            let result = command.spawn_gated().unwrap().wait().unwrap();
            drop((ready_write, release_read));
            result
        });
        let mut ready = libc::pollfd { fd: ready_read.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        assert_eq!(unsafe { libc::poll(&mut ready, 1, 5000) }, 1);
        ready_read.read_exact(&mut [0u8]).unwrap();
        (release_write, spawned)
    }

    #[test]
    fn released_lock_is_free_while_a_forked_child_awaits_exec_but_transferred_locks_stay_held() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir_all(project.join(".state")).unwrap();
        let guard = ProjectGuard::acquire(&project).unwrap();
        let (mut release, spawned) = child_paused_before_exec();
        // The child still shares the lock's file description, but dropping the
        // guard unlocks it explicitly rather than waiting for the child's exec.
        drop(guard);
        let reacquired = ProjectGuard::acquire(&project).expect("a released lock must not stay held by a child awaiting exec");
        release.write_all(&[1]).unwrap();
        assert!(spawned.join().unwrap().success());
        // A lock transferred to a supervisor outlives the guard until the
        // supervisor's descriptors close.
        let transferred = reacquired.inherit_transfer().unwrap();
        drop(reacquired);
        let contention = ProjectGuard::acquire(&project).err().expect("a transferred lock must stay held after its guard drops");
        assert!(contention.chain().any(|cause| matches!(cause.downcast_ref::<std::fs::TryLockError>(), Some(std::fs::TryLockError::WouldBlock))));
        // The supervisor side has released; `transferred` is the last copy.
        // Closing it while another thread's child awaits exec must not leave
        // the lock with that child: the next acquisition succeeds at once.
        let (mut release, spawned) = child_paused_before_exec();
        let (closing, closing_started) = std::sync::mpsc::channel();
        let (closed, acquisition) = std::sync::mpsc::channel();
        let closer = std::thread::spawn({
            let project = project.clone();
            move || {
                closing.send(()).unwrap();
                drop(transferred);
                closed.send(ProjectGuard::acquire(&project).map(drop).map_err(|error| format!("{error:#}"))).unwrap();
            }
        });
        closing_started.recv().unwrap();
        // Give an ungated close time to finish while the child is still paused.
        let early = acquisition.recv_timeout(std::time::Duration::from_millis(200)).ok();
        release.write_all(&[1]).unwrap();
        let result = early.unwrap_or_else(|| acquisition.recv().unwrap());
        assert!(result.is_ok(), "a released transferred lock must not stay held by a child awaiting exec: {result:?}");
        closer.join().unwrap();
        assert!(spawned.join().unwrap().success());
    }

    #[test]
    fn projects_are_independent_and_root_barrier_remains_exclusive() {
        let root=tempfile::tempdir().unwrap();
        for name in ["a","b"] {std::fs::create_dir_all(root.path().join(name).join(".state")).unwrap();}
        let a=root.path().join("a");let b=root.path().join("b");
        let first=ProjectGuard::acquire(&a).unwrap();assert!(ProjectGuard::acquire(&a).is_err());
        let second=ProjectGuard::acquire(&b).unwrap();assert!(RootGuard::exclusive(root.path()).is_err());
        drop(first);assert!(ProjectGuard::acquire(&a).is_ok());drop(second);
        let root_guard=RootGuard::exclusive(root.path()).unwrap();assert!(ProjectGuard::acquire(&a).is_err());assert!(ProjectGuard::acquire(&b).is_err());drop(root_guard);
        assert!(ProjectGuard::acquire(&a).is_ok());
    }
    #[test]
    fn failed_project_acquisition_releases_root_and_refuses_symlinks_and_special_files() {
        use std::os::unix::fs::symlink;
        let root=tempfile::tempdir().unwrap();let project=root.path().join("project");std::fs::create_dir_all(project.join(".state")).unwrap();
        let target=root.path().join("target");std::fs::write(&target,"preserve").unwrap();let lock=project.join(".state/effect.lock");
        symlink(&target,&lock).unwrap();assert!(ProjectGuard::acquire(&project).is_err());assert!(RootGuard::exclusive(root.path()).is_ok());assert_eq!(std::fs::read_to_string(&target).unwrap(),"preserve");
        std::fs::remove_file(&lock).unwrap();let name=std::ffi::CString::new(lock.as_os_str().as_encoded_bytes()).unwrap();assert_eq!(unsafe{libc::mkfifo(name.as_ptr(),0o600)},0);
        assert!(ProjectGuard::acquire(&project).is_err());assert!(RootGuard::exclusive(root.path()).is_ok());
    }
    fn project(root:&Path,name:&str)->std::path::PathBuf {
        let path=root.join(name);std::fs::create_dir_all(path.join(".state")).unwrap();path
    }
    #[test]
    fn shared_guards_overlap_across_repos_and_exclude_one_git_dir() {
        let root=tempfile::tempdir().unwrap();let a=project(root.path(),"a");let b=project(root.path(),"b");let c=project(root.path(),"c");
        let git_a=root.path().join("repos/a");let git_b=root.path().join("repos/b");std::fs::create_dir_all(&git_a).unwrap();std::fs::create_dir_all(&git_b).unwrap();
        let git_a=git_a.canonicalize().unwrap();let git_b=git_b.canonicalize().unwrap();
        let footprint=|project:&Path,git:&Path| vec![Resource::new("artifact",project.canonicalize().unwrap().to_str().unwrap()).unwrap(),Resource::new("git",git.to_str().unwrap()).unwrap()];
        let first=ProjectSharedGuard::acquire(&a,&footprint(&a,&git_a)).unwrap();
        let second=ProjectSharedGuard::acquire(&b,&footprint(&b,&git_b)).unwrap();
        assert!(first.inherit_transfer().is_ok()&&second.inherit_transfer().is_ok(),"declared footprints share the routine lock");
        assert!(RootGuard::exclusive(root.path()).is_err());
        assert!(ProjectGuard::acquire(&a).is_err(),"undeclared work keeps the exclusive project guard");
        assert!(ProjectSharedGuard::acquire(&c,&footprint(&c,&git_a)).is_err(),"one git directory stays exclusive");
        assert!(ProjectSharedGuard::acquire(&a,&[]).is_err());
        drop(first);let released=ProjectSharedGuard::acquire(&c,&footprint(&c,&git_a)).unwrap();drop(released);
        assert!(RootGuard::exclusive(root.path()).is_err(),"the other repository still holds the shared root");
        drop(second);assert!(ProjectGuard::acquire(&b).is_ok());assert!(RootGuard::exclusive(root.path()).is_ok());
    }
}
