//! T00.1: executable counterexamples from the architecture review.
//!
//! These assert the repaired behavior and run in the normal suite. Run alone
//! with `cargo test --locked review_regressions -- --nocapture`.
//! Historical counterexamples are recorded in docs/review-baseline.md.

use super::*;
use crate::runner::{RealRunner, Runner};
use crate::thread::CopyOutcome;
use std::fs::{File, FileTimes};
use std::time::{Duration, Instant, UNIX_EPOCH};

#[test]
fn f01_artifact_copy_preserves_changed_bytes_with_identical_metadata() {
    let world = World::new();
    let project = world.project("demo", "a.sock");
    let t = world.thread(&project, world.home.path(), |_| {});
    let library = Path::new(&t.thread_dir).join("library");
    std::fs::create_dir_all(&library).unwrap();
    let source = library.join("artifact.bin");
    let destination = project.dir().join("library").join(&t.id).join("artifact.bin");
    let modified = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let write_version = |bytes: &[u8]| {
        std::fs::write(&source, bytes).unwrap();
        File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_times(FileTimes::new().set_modified(modified))
            .unwrap();
    };

    write_version(b"version A\x00\xff");
    let first = thread::copy_home_local(&project, &t, true, &RealRunner);
    assert_eq!(first.outcome, CopyOutcome::Complete, "initial real rsync copy");
    assert_eq!(std::fs::read(&destination).unwrap(), b"version A\x00\xff");
    let old_metadata = std::fs::metadata(&source).unwrap();

    write_version(b"version B\x00\xfe");
    let new_metadata = std::fs::metadata(&source).unwrap();
    assert_eq!(old_metadata.len(), new_metadata.len());
    assert_eq!(old_metadata.modified().unwrap(), new_metadata.modified().unwrap());
    let second = thread::copy_home_local(&project, &t, true, &RealRunner);
    assert_eq!(second.outcome, CopyOutcome::Complete, "second real rsync copy");
    assert_eq!(
        std::fs::read(&destination).unwrap(),
        std::fs::read(&source).unwrap(),
        "F01: a complete copy must contain the latest artifact bytes"
    );
}

#[test]
fn f04_deadline_includes_descendant_held_pipes() {
    // The descendant exits by itself after two seconds even on the broken
    // runner, keeping this negative fixture finite and leaving no daemon behind.
    let start = Instant::now();
    let out = RealRunner.run(
        &Cmd::new("sh", Duration::from_millis(150)).args(["-c", "sleep 2 &"]).own_group(),
    ).unwrap();
    let elapsed = start.elapsed();
    assert!(out.timed_out && !out.success() && elapsed < Duration::from_secs(1),
        "F04: expected timeout within 1s including cleanup; elapsed={elapsed:?}, output={out:?}");
}
