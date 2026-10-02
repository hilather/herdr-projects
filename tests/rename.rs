#![allow(clippy::disallowed_methods)] // Isolated CLI fixtures run outside the library gate.
//! Rename compatibility through the CLI, with isolated installation homes.
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
const BIN: &str = env!("CARGO_BIN_EXE_herdr-farm");
fn run(home: &Path, vars: &[(&str, &Path)], args: &[&str]) -> Output {
    let mut cmd = Command::new(BIN);
    cmd.env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("HERDR_BIN_PATH", "/nonexistent-herdr-farm-fixture");
    for (key, value) in vars {
        cmd.env(key, value);
    }
    cmd.args(args).output().unwrap()
}
fn create(home: &Path, vars: &[(&str, &Path)], slug: &str) {
    let out = run(home, vars, &["new", slug]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
#[test]
fn legacy_environment_is_honored_and_new_environment_wins() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let old = home.join("legacy-root");
    let new = home.join("new-root");
    create(home, &[("HERDR_PROJECTS_ROOT", &old)], "legacy");
    assert!(old.join("legacy/PROJECT.md").is_file());
    create(
        home,
        &[("HERDR_PROJECTS_ROOT", &old), ("HERDR_FARM_ROOT", &new)],
        "modern",
    );
    assert!(new.join("modern/PROJECT.md").is_file());
    assert!(!old.join("modern").exists());
}
#[test]
fn default_roots_and_configs_preserve_existing_installations() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let old = home.join(".herdr-projects");
    let new = home.join(".herdr-farm");
    fs::create_dir(&old).unwrap();
    create(home, &[], "legacy");
    assert!(old.join("legacy/PROJECT.md").is_file());
    fs::create_dir(&new).unwrap();
    create(home, &[], "modern");
    assert!(new.join("modern/PROJECT.md").is_file());
    assert!(!old.join("modern").exists());
    let old_config = home.join(".config/herdr-projects");
    let new_config = home.join(".config/herdr-farm");
    let configured_old = home.join("configured-old");
    let configured_new = home.join("configured-new");
    fs::create_dir_all(&old_config).unwrap();
    fs::write(
        old_config.join("config.toml"),
        format!("root = {:?}\n", configured_old.to_str().unwrap()),
    )
    .unwrap();
    create(home, &[], "configured");
    assert!(configured_old.join("configured/PROJECT.md").is_file());
    fs::create_dir_all(&new_config).unwrap();
    fs::write(
        new_config.join("config.toml"),
        format!("root = {:?}\n", configured_new.to_str().unwrap()),
    )
    .unwrap();
    create(home, &[], "configured");
    assert!(configured_new.join("configured/PROJECT.md").is_file());
    let out = run(home, &[], &["doctor"]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains(&format!("root:       {}", configured_new.display())),
        "{text}"
    );
    assert!(
        text.contains(&format!("config dir: {}", new_config.display())),
        "{text}"
    );
    for path in [&old, &new, &old_config, &new_config] {
        assert!(text.contains(&path.display().to_string()), "{text}");
    }
    assert!(text.contains("[warn] locations: both"), "{text}");
}
