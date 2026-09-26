//! Cooperative ownership shared by the CLI and canonical library services.
//! Order: root barrier, project effect ownership, resource fences sorted by
//! `(class, identity)`, then any short record lock. Never upgrade a shared
//! lock in place, and never take an exclusive root guard while holding one.
use std::{fs::{File,OpenOptions},os::unix::fs::{DirBuilderExt,OpenOptionsExt,MetadataExt},path::{Path,PathBuf}};
use anyhow::{Result,Context,ensure};
use sha2::{Digest,Sha256};

fn lock_file(path:&Path)->Result<File> {
    let file=OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .mode(0o600).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(path)?;
    ensure!(file.metadata()?.is_file(),"execution lock must be a regular file");
    Ok(file)
}
pub(crate) fn exclusive_file(path:&Path)->Result<File> {
    let file=lock_file(path)?;
    file.try_lock().context("another operation owns this lock; retry")?;
    Ok(file)
}

/// Exclusive compatibility barrier for migration, cleanup and terminal effects.
pub struct RootGuard {_file:File}
impl RootGuard {
    #[cfg(feature="state-store")]
    pub(crate) fn inherit(&self)->Result<Vec<crate::runner::InheritedLock>> {
        Ok(vec![crate::runner::InheritedLock::new(self._file.try_clone()?)])
    }
    pub fn exclusive(root:&Path)->Result<Self> {
        let file=exclusive_file(&root.join(".execution.lock"))?;
        Ok(Self{_file:file})
    }
    fn shared(root:&Path)->Result<Self> {
        let file=lock_file(&root.join(".execution.lock"))?;
        file.try_lock_shared().context("root maintenance or exclusive external operation is active; retry")?;
        Ok(Self{_file:file})
    }
}

fn matches_project(guard_project:&Path,identity:(u64,u64),project:&Path)->Result<()> {
    let metadata=std::fs::metadata(project)?;
    ensure!(project.canonicalize()?==guard_project&&(metadata.dev(),metadata.ino())==identity,"execution guard belongs to a different project");Ok(())
}

/// Declared footprint. `git` is a common git directory. `artifact` is one
/// project's publication directory. Unknown classes are not a footprint.
#[derive(Clone,Debug,PartialEq,Eq,PartialOrd,Ord)]
pub struct Resource {pub class:String,pub identity:String}
impl Resource {
    pub fn new(class:impl Into<String>,identity:impl Into<String>)->Result<Self> {
        let class=class.into();let identity=identity.into();
        ensure!(matches!(class.as_str(),"git"|"artifact"),"unknown effect resource");
        ensure!(!identity.is_empty()&&identity.len()<=4096&&!identity.chars().any(char::is_control)&&Path::new(&identity).is_absolute(),"invalid effect resource");
        Ok(Self{class,identity})
    }
}
pub trait ProjectEffect {fn check_project(&self,project:&Path)->Result<()>;}

/// Excludes effects within one project while preserving the root-wide barrier.
/// Never acquire an exclusive root guard while retaining this shared guard.
pub struct ProjectGuard {_project:File,_root:RootGuard,root:std::path::PathBuf,project:std::path::PathBuf,identity:(u64,u64)}
impl ProjectEffect for ProjectGuard {fn check_project(&self,project:&Path)->Result<()> {ProjectGuard::check_project(self,project)}}
impl ProjectGuard {
    pub fn check_project(&self,project:&Path)->Result<()> {matches_project(&self.project,self.identity,project)}
    /// A trusted transfer supervisor keeps these descriptions open until its
    /// descendants finish, even if the caller dies. Retain the returned handles
    /// in the caller through publication. The historical lock name also fences
    /// routine jobs, including those surviving a previous ticker instance.
    pub fn inherit_transfer(&self)->Result<Vec<crate::runner::InheritedLock>> {
        Ok(vec![crate::runner::InheritedLock::new(self._root._file.try_clone()?),crate::runner::InheritedLock::new(self._project.try_clone()?),
            crate::runner::InheritedLock::new(exclusive_file(&self.root.join(".routine-execution.lock"))?)])
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

/// Shared project ownership for an effect that declared its footprint.
/// The same git directory stays exclusive; a different repository does not.
/// Holds shared root and project locks only — never upgrades them.
pub struct ProjectSharedGuard {_project:File,_root:RootGuard,_fences:Vec<File>,_routine:File,project:PathBuf,identity:(u64,u64)}
impl ProjectEffect for ProjectSharedGuard {fn check_project(&self,project:&Path)->Result<()> {ProjectSharedGuard::check_project(self,project)}}
impl ProjectSharedGuard {
    pub fn check_project(&self,project:&Path)->Result<()> {matches_project(&self.project,self.identity,project)}
    /// Retain these descriptors through publication. The routine lock is shared
    /// so two declared footprints can overlap; exclusive routine ownership still
    /// belongs to an undeclared `ProjectGuard`.
    pub fn inherit_transfer(&self)->Result<Vec<crate::runner::InheritedLock>> {
        let mut locks=vec![crate::runner::InheritedLock::new(self._root._file.try_clone()?),crate::runner::InheritedLock::new(self._project.try_clone()?)];
        for fence in &self._fences {locks.push(crate::runner::InheritedLock::new(fence.try_clone()?));}
        locks.push(crate::runner::InheritedLock::new(self._routine.try_clone()?));Ok(locks)
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
        let project_file=lock_file(&project.join(".state/effect.lock"))?;
        project_file.try_lock_shared().context("another operation owns this project; retry")?;
        let mut fences=Vec::with_capacity(ordered.len());
        for resource in &ordered {fences.push(exclusive_file(&fence_path(&root_path,resource)?)?);}
        let routine=lock_file(&root_path.join(".routine-execution.lock"))?;
        routine.try_lock_shared().context("another operation owns this lock; retry")?;
        let metadata=std::fs::metadata(&project)?;
        Ok(Self{_project:project_file,_root:root,_fences:fences,_routine:routine,project,identity:(metadata.dev(),metadata.ino())})
    }
}
fn fence_path(root:&Path,resource:&Resource)->Result<PathBuf> {
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
    Ok(dir.join(format!("{:x}",hasher.finalize())))
}

#[cfg(test)]
mod tests {
    use super::*;
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
