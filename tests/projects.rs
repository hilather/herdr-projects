#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Project creation, discovery, PROJECT.md settings and per-project safety
//! overrides through the compiled CLI: `new`, `list`, `context` and
//! `safety show`, asserting what they print and the files they leave.
use std::{fs, path::PathBuf, process::{Command, Output}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-farm");

struct Home(tempfile::TempDir);

impl Home {
    fn new() -> Self { Home(tempfile::tempdir().unwrap()) }
    fn root(&self) -> PathBuf { self.0.path().join("root") }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(BIN).env_clear().env("HOME", self.0.path()).env("PATH", "/usr/bin:/bin")
            .args(["--root", self.root().to_str().unwrap()]).args(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }
    fn refused(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        assert!(!out.status.success(), "{args:?} was accepted: {}", String::from_utf8_lossy(&out.stdout));
        String::from_utf8(out.stderr).unwrap()
    }
    fn listing(&self) -> Vec<String> {
        self.ok(&["list"]).lines().map(|l| l.split('\t').next().unwrap().to_string()).collect()
    }
    fn tree(&self) -> Vec<PathBuf> {
        let mut all = Vec::new();
        let mut stack = vec![self.root()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
                if entry.path().is_dir() { stack.push(entry.path()); }
                all.push(entry.path());
            }
        }
        all.sort();
        all
    }
}

/// `new` derives the slug from the name, writes the whole skeleton with
/// PROJECT.md settings, keeps `@` inside a path, and refuses a second project
/// of the same slug and names that would escape the root.
#[test]
fn new_writes_the_skeleton_and_refuses_duplicates_and_escapes() {
    let home = Home::new();
    // Nothing exists yet: an absent root lists no projects.
    assert_eq!(home.ok(&["list"]), "");
    let out = home.ok(&["new", "My Demo  Project!", "--goal", "Ship \"it\"", "--repo", "/srv/app@box", "--repo", "/a@b/c", "--repo", "/no/such/repo"]);
    let dir = home.root().join("my-demo-project");
    assert!(out.starts_with(&format!("created `my-demo-project` at {}\n", dir.display())), "{out}");
    for sub in ["memory", "scratch", "routines", "threads", "inbox/done", "library", ".state"] {
        assert!(dir.join(sub).is_dir(), "{sub}");
    }
    assert!(fs::read_to_string(dir.join("MEMORY.md")).unwrap().starts_with("# Memory\n"));
    assert!(dir.join("TASKS.md").is_file() && dir.join(".state/project.json").is_file());
    let md = fs::read_to_string(dir.join("PROJECT.md")).unwrap();
    let (front, body) = md.strip_prefix("+++\n").unwrap().split_once("+++\n").unwrap();
    let settings: toml::Value = toml::from_str(front).unwrap();
    let expected: toml::Value = toml::from_str(r#"
        name = "My Demo  Project!"
        goal = 'Ship "it"'
        coordinator_agent = "claude"
        thread_agent = "claude"
        max_parallel_threads = 3
        auto_resolve_days = 7
        nudge = false
        repos = [{ path = "/srv/app", machine = "box" }, { path = "/a@b/c" }, { path = "/no/such/repo" }]
    "#).unwrap();
    assert_eq!(settings, expected);
    assert!(body.trim_start().starts_with("# Instructions"));
    let context = home.ok(&["context", "my-demo-project"]);
    for line in ["Project: my-demo-project (active)", "Goal: Ship \"it\"", "Repo: /srv/app (machine box)", "Repo: /a@b/c\n", "Repo: /no/such/repo"] {
        assert!(context.contains(line), "missing {line:?} in {context}");
    }

    // Folding to the same slug is a duplicate; the original is untouched.
    let before = home.tree();
    assert!(home.refused(&["new", "my-demo-project"]).contains("`my-demo-project` already exists"));
    assert_eq!(fs::read_to_string(dir.join("PROJECT.md")).unwrap(), md);
    for (name, error) in [("../x", "may not contain"), ("a/b", "may not contain"), ("a\\b", "may not contain"), ("..", "may not contain"),
                          ("!!!", "no letters or digits"), ("", "no letters or digits")] {
        assert!(home.refused(&["new", name]).contains(error), "{name:?}");
    }
    assert_eq!(home.tree(), before, "a refused name created something");

    // Non-ASCII letters drop out and long names are cut to 40 characters.
    assert!(home.ok(&["new", "  Ünï 42 "]).starts_with("created `n-42`"));
    let long = home.ok(&["new", &"x".repeat(60)]);
    assert!(long.starts_with(&format!("created `{}`", "x".repeat(40))), "{long}");
    assert_eq!(home.listing(), ["my-demo-project", "n-42", &"x".repeat(40)]);
}

/// Only valid slugs are projects: `list` skips dot folders, folders without
/// PROJECT.md and invalid names, and every command refuses an invalid slug.
#[test]
fn list_and_commands_accept_only_folders_with_project_md_and_valid_slugs() {
    let home = Home::new();
    home.ok(&["new", "b"]);
    for name in ["a", "0x", "demo-2", ".trash", "Not_A_Slug", "empty", &"a".repeat(41)] {
        fs::create_dir_all(home.root().join(name)).unwrap();
        if name != "empty" { fs::write(home.root().join(name).join("PROJECT.md"), "").unwrap(); }
    }
    assert_eq!(home.listing(), ["0x", "a", "b", "demo-2"]);
    // Forty characters is a well-formed slug that is looked up; forty-one is refused.
    assert!(home.refused(&["safety", "show", &"a".repeat(40)]).contains(&format!("no project `{}`", "a".repeat(40))));
    for bad in ["", "-a", "A", "a_b", "a/b", "../x", "a b", ".", "..", &"a".repeat(41)] {
        assert!(home.refused(&["safety", "show", "--", bad]).contains("is not a valid slug"), "{bad:?}");
    }
    assert!(home.refused(&["safety", "show", "zz"]).contains("no project `zz`"));
    assert!(home.ok(&["safety", "show", "b"]).starts_with("Effective safety settings for `b`:"));
}

/// Settings come from the TOML between the `+++` lines; the body may itself
/// contain `+++`. Malformed front matter is reported rather than guessed at.
#[test]
fn context_reads_front_matter_and_reports_malformed_project_md() {
    let home = Home::new();
    home.ok(&["new", "demo"]);
    let md = home.root().join("demo/PROJECT.md");
    fs::write(&md, "+++\nname = \"X\"\nnudge = true\n+++\n\nBody\n+++\nmore\n").unwrap();
    let context = home.ok(&["context", "demo"]);
    assert!(context.contains("Name: X\n"), "{context}");
    assert!(context.contains("Settings: thread_agent=claude max_parallel_threads=3 auto_resolve_days=7 nudge=true"), "{context}");
    assert!(context.contains("14 characters)\nBody\n+++\nmore\n"), "{context}");

    fs::write(&md, "+++\nname = \"X\"\n+++").unwrap();
    assert!(home.ok(&["context", "demo"]).contains("; 0 characters)"));

    for (text, error) in [("no front matter", "must start with a `+++` line"), ("+++\nname = \n+++\n", "front matter does not parse"),
                          ("+++\nname = \"X\"\n", "front matter has no closing `+++` line")] {
        fs::write(&md, text).unwrap();
        let context = home.ok(&["context", "demo"]);
        assert!(context.contains(&format!("config-error: PROJECT.md: PROJECT.md {error}")), "{text:?}: {context}");
        assert!(!context.contains("Name: X"), "{text:?}");
    }
}

/// Safety overrides live in the user's config.toml, keyed by the project's
/// canonical path; other projects keep the defaults and a bad value is refused.
#[test]
fn safety_overrides_are_keyed_by_canonical_project_path() {
    let home = Home::new();
    home.ok(&["new", "demo"]);
    home.ok(&["new", "other"]);
    let defaults = "  start_threads = \"propose\"\n  coordinator_agent_args = []\n  thread_agent_args = []\n";
    assert!(home.ok(&["safety", "show", "demo"]).contains(defaults));

    let canonical = fs::canonicalize(home.root().join("demo")).unwrap();
    let config = home.0.path().join(".config/herdr-farm");
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("config.toml"), format!("[safety.{:?}]\nstart_threads = \"auto\"\nthread_agent_args = [\"--x\"]\n", canonical.to_str().unwrap())).unwrap();
    let shown = home.ok(&["safety", "show", "demo"]);
    assert!(shown.contains("  start_threads = \"auto\"\n  coordinator_agent_args = []\n  thread_agent_args = [\"--x\"]\n"), "{shown}");
    assert!(shown.contains("  routine_commands = false\n"), "{shown}");
    assert!(shown.ends_with(&format!("[safety.{:?}]\n", canonical.to_str().unwrap())), "{shown}");
    assert!(home.ok(&["context", "demo"]).contains("Safety: start_threads=auto routine_commands=false thread_agent_args=[\"--x\"]"));
    assert!(home.ok(&["safety", "show", "other"]).contains(defaults));

    fs::write(config.join("config.toml"), format!("[safety.{:?}]\nstart_threads = \"yolo\"\n", canonical.to_str().unwrap())).unwrap();
    let error = home.refused(&["safety", "show", "demo"]);
    assert!(error.contains("start_threads must be \"propose\" or \"auto\", not \"yolo\""), "{error}");
}
