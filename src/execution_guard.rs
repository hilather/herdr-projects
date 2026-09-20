//! Cooperative ownership shared by the CLI and canonical library services.
//! Order: root barrier, project effect ownership, then any short record lock.
use std::{fs::{File,OpenOptions},os::unix::fs::OpenOptionsExt,path::Path};
use anyhow::{Result,Context,ensure};

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

/// Excludes effects within one project while preserving the root-wide barrier.
/// Never acquire an exclusive root guard while retaining this shared guard.
pub struct ProjectGuard {_project:File,_root:RootGuard,root:std::path::PathBuf}
impl ProjectGuard {
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
        Ok(Self{_project:file,_root:root,root:root_path})
    }
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
}
