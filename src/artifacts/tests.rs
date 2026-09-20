use super::*;
use crate::project;

pub(super) fn fixture() -> (tempfile::TempDir, Project, Thread) {
    let root = tempfile::tempdir().unwrap();
    let project = project::create(root.path(), "demo", "", vec![]).unwrap();
    let source = root.path().join("source");
    fs::create_dir_all(source.join("library/empty")).unwrap();
    fs::write(source.join("report.md"), b"report\0\xff").unwrap();
    fs::write(source.join("library/artifact"), b"version A").unwrap();
    let record = Thread { id: "t-0001".into(), thread_dir: source.to_str().unwrap().into(), ..Thread::default() };
    (root, project, record)
}

#[test]
fn published_snapshots_preserve_versions_and_reuse_identical_content() {
    let (_root, project, record) = fixture();
    let first = capture_local(&project, &record).unwrap();
    assert_eq!(first.id, capture_local(&project, &record).unwrap().id);
    let source = Path::new(&record.thread_dir).join("library/artifact");
    let stamp = fs::metadata(&source).unwrap().modified().unwrap();
    fs::write(&source, b"version B").unwrap();
    File::options().write(true).open(&source).unwrap().set_modified(stamp).unwrap();
    let second = capture_local(&project, &record).unwrap();
    assert_ne!(first.id, second.id);
    for (snapshot, expected) in [(&first, b"version A"), (&second, b"version B")] {
        let dir = project.state_dir().join("artifacts/t-0001").join(&snapshot.id);
        assert_eq!(fs::read(dir.join("library/artifact")).unwrap(), expected);
        assert!(dir.join("library/empty").is_dir());
        assert_eq!(load(&project, &record, &snapshot.id).unwrap(), snapshot.manifest);
    }
}

#[test]
fn source_change_and_staged_corruption_never_replace_good_evidence() {
    for corrupt_stage in [false, true] {
        let (_root, project, record) = fixture();
        let good = capture_local(&project, &record).unwrap();
        let parent = project.state_dir().join("artifacts/t-0001");
        let result = capture(&project, &record, || {
            let path = if corrupt_stage {
                fs::read_dir(&parent)?.collect::<std::io::Result<Vec<_>>>()?.into_iter()
                    .find(|e| e.file_name().to_string_lossy().starts_with(".stage-")).unwrap().path().join("library/artifact")
            } else { Path::new(&record.thread_dir).join("library/artifact") };
            fs::write(path, b"version B")?;
            Ok(())
        });
        assert!(result.is_err());
        load(&project, &record, &good.id).unwrap();
        assert_eq!(fs::read_dir(parent).unwrap().count(), 1, "failed staging is removed; old snapshot remains");
    }
}

#[test]
fn links_special_files_missing_sources_and_oversize_files_are_rejected() {
    for kind in ["symlink", "hardlink", "oversize", "missing", "fifo"] {
        let (_root, project, record) = fixture();
        let path = Path::new(&record.thread_dir).join("library/artifact");
        fs::remove_file(&path).unwrap();
        match kind {
            "symlink" => std::os::unix::fs::symlink("/etc/passwd", &path).unwrap(),
            "hardlink" => fs::hard_link(Path::new(&record.thread_dir).join("report.md"), &path).unwrap(),
            "oversize" => File::create(&path).unwrap().set_len(BYTE_LIMIT + 1).unwrap(),
            "missing" => fs::remove_dir_all(&record.thread_dir).unwrap(),
            "fifo" => {
                let path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
                // SAFETY: valid NUL-terminated fixture path.
                assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
            }
            _ => unreachable!(),
        }
        assert!(capture_local(&project, &record).is_err(), "{kind}");
    }
}

#[test]
fn tampered_receipts_and_destination_links_are_not_reused() {
    let (root, project, record) = fixture();
    let good = capture_local(&project, &record).unwrap();
    let dir = project.state_dir().join("artifacts/t-0001").join(&good.id);
    fs::write(dir.join("library/artifact"), b"tampered!").unwrap();
    assert!(load(&project, &record, &good.id).is_err());
    assert!(capture_local(&project, &record).is_err());
    fs::remove_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(root.path(), &dir).unwrap();
    assert!(capture_local(&project, &record).is_err());
    assert!(load(&project, &record, "../../source").is_err());
}

#[test]
fn entry_limit_is_enforced_without_unbounded_directory_collection() {
    let (_root, project, record) = fixture();
    for n in 0..ENTRY_LIMIT { File::create(Path::new(&record.thread_dir).join(format!("library/{n}"))).unwrap(); }
    let error = capture_local(&project, &record).err().unwrap();
    assert!(error.to_string().contains("entries"), "{error:#}");
}

#[test]
fn identical_replacement_directory_cannot_publish_under_original_identity() {
    let (root,project,record)=fixture();let good=capture_local(&project,&record).unwrap();
    let result=capture(&project,&record,|| {
        fs::rename(&record.thread_dir,root.path().join("old-source"))?;
        fs::create_dir_all(Path::new(&record.thread_dir).join("library/empty"))?;
        fs::write(Path::new(&record.thread_dir).join("report.md"),b"report\0\xff")?;
        fs::write(Path::new(&record.thread_dir).join("library/artifact"),b"version A")?;
        Ok(())
    });
    assert!(result.is_err());load(&project,&record,&good.id).unwrap();
    assert_eq!(fs::read_dir(project.state_dir().join("artifacts/t-0001")).unwrap().count(),1);
}

#[test]
fn excessive_source_depth_does_not_leave_a_snapshot() {
    let (_root,project,record)=fixture();let mut path=Path::new(&record.thread_dir).join("library");
    for _ in 0..=crate::source_tree::DEPTH_LIMIT {path=path.join("d");fs::create_dir(&path).unwrap();}
    let error=capture_local(&project,&record).err().unwrap();assert!(error.to_string().contains("nesting"),"{error:#}");
    assert_eq!(fs::read_dir(project.state_dir().join("artifacts/t-0001")).unwrap().count(),0);
}

#[test]
fn historical_schema_one_root_entry_types_remain_readable() {
    let (_root,project,record)=fixture();
    let source=Path::new(&record.thread_dir);
    fs::remove_file(source.join("report.md")).unwrap();
    fs::create_dir(source.join("report.md")).unwrap();
    fs::write(source.join("report.md/old"),b"old report").unwrap();
    fs::remove_dir_all(source.join("library")).unwrap();
    fs::write(source.join("library"),b"old library").unwrap();
    let snapshot=capture_local(&project,&record).unwrap();
    assert_eq!(load(&project,&record,&snapshot.id).unwrap(),snapshot.manifest);
    verify_source(&record,&snapshot.manifest).unwrap();
    assert!(snapshot.manifest.entries.iter().any(|entry|entry.path=="report.md"&&entry.directory));
    assert!(snapshot.manifest.entries.iter().any(|entry|entry.path=="library"&&!entry.directory));
}
