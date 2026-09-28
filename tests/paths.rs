#![cfg(unix)]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Root and session resolution through the compiled CLI. The root is observed
//! where `new` creates a project; the session, herdr binary and root through
//! what `doctor` reports, against a fake herdr that lists sessions.
use std::{fs, os::unix::fs::PermissionsExt, path::{Path, PathBuf}, process::{Command, Output}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

/// Lists `$HOME/sessions.json` (none when absent); fails the listing while
/// `$HOME/sessions.fail` exists.
const FAKE_HERDR: &str = r#"#!/bin/sh
case "$*" in
"--version") echo "herdr 0.9.1" ;;
"session list --json")
    [ -e "$HOME/sessions.fail" ] && { echo "boom" >&2; exit 1; }
    if [ -e "$HOME/sessions.json" ]; then /bin/cat "$HOME/sessions.json"; else echo '{"sessions":[]}'; fi ;;
*) echo '{"result":{}}' ;;
esac
"#;

struct Home { dir: tempfile::TempDir, bin: PathBuf }

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("herdr"), FAKE_HERDR).unwrap();
        fs::set_permissions(bin.join("herdr"), fs::Permissions::from_mode(0o700)).unwrap();
        Home { dir, bin }
    }
    fn path(&self, rel: &str) -> PathBuf { self.dir.path().join(rel) }
    /// `hp ARGS` with only HOME, a PATH holding the fake herdr, and `vars`.
    fn run(&self, vars: &[(&str, &str)], args: &[&str]) -> Output {
        let mut command = Command::new(BIN);
        command.env_clear().env("HOME", self.dir.path()).env("PATH", &self.bin);
        for (k, v) in vars { command.env(k, v); }
        command.args(args).output().unwrap()
    }
    /// `new SLUG`: the project directory it reports creating.
    fn new_project(&self, vars: &[(&str, &str)], flags: &[&str], slug: &str) -> PathBuf {
        let out = self.run(vars, &[flags, &["new", slug]].concat());
        assert!(out.status.success(), "new {slug}: {}", String::from_utf8_lossy(&out.stderr));
        let text = String::from_utf8(out.stdout).unwrap();
        let dir = text.lines().next().and_then(|l| l.split_once(" at ")).map(|(_, d)| PathBuf::from(d)).unwrap();
        assert!(dir.join("PROJECT.md").is_file(), "{text}");
        dir
    }
    /// One labelled line of the `doctor` report.
    fn doctor(&self, vars: &[(&str, &str)], args: &[&str], label: &str) -> String {
        let out = self.run(vars, &[&["doctor"], args].concat());
        let text = String::from_utf8(out.stdout).unwrap();
        text.lines().find(|l| l.starts_with(label) || l.get(7..).is_some_and(|rest| rest.starts_with(label)))
            .unwrap_or_else(|| panic!("no {label} line in:\n{text}\n{}", String::from_utf8_lossy(&out.stderr))).to_string()
    }
    fn config(&self, text: &str) {
        fs::create_dir_all(self.path(".config/herdr-projects")).unwrap();
        fs::write(self.path(".config/herdr-projects/config.toml"), text).unwrap();
    }
}

fn under(dir: &Path, slug: &str) -> PathBuf { dir.join(slug) }

/// `--root`, then `HERDR_PROJECTS_ROOT` (with `~/`), then `root` in
/// config.toml, then `~/.herdr-projects`. An empty variable counts as unset.
#[test]
fn projects_root_comes_from_flag_then_variable_then_config_then_home() {
    let home = Home::new();
    let flag = home.path("from-flag");
    let flag_arg = ["--root", flag.to_str().unwrap()];
    // No config and no variable: the default root; an empty variable is unset.
    assert_eq!(home.new_project(&[], &[], "a"), under(&home.path(".herdr-projects"), "a"));
    assert_eq!(home.new_project(&[("HERDR_PROJECTS_ROOT", "")], &[], "b"), under(&home.path(".herdr-projects"), "b"));
    assert!(home.doctor(&[("HERDR_PROJECTS_ROOT", "")], &[], "root:").ends_with(&home.path(".herdr-projects").display().to_string()));

    home.config("root = \"~/from-config\"\n");
    assert_eq!(home.new_project(&[], &[], "c"), under(&home.path("from-config"), "c"));
    assert_eq!(home.new_project(&[("HERDR_PROJECTS_ROOT", "")], &[], "d"), under(&home.path("from-config"), "d"));
    let env = [("HERDR_PROJECTS_ROOT", "~/from-env")];
    assert_eq!(home.new_project(&env, &[], "e"), under(&home.path("from-env"), "e"));
    assert_eq!(home.new_project(&env, &flag_arg, "f"), under(&flag, "f"));
    // An empty `root` in config falls through to the default.
    home.config("root = \"\"\n");
    assert_eq!(home.new_project(&[], &[], "g"), under(&home.path(".herdr-projects"), "g"));

    let projects = |root: &str| {
        let mut slugs: Vec<_> = fs::read_dir(home.path(root)).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).filter(|n| !n.starts_with('.')).collect();
        slugs.sort();
        slugs.join(",")
    };
    let roots: Vec<_> = [".herdr-projects", "from-config", "from-env", "from-flag"].map(projects).into();
    assert_eq!(roots, ["a,b,g", "c,d", "e", "f"]);
}

/// A config.toml that does not parse stops every command that would fall
/// back to it, without echoing its contents, and creates nothing; a root
/// given by flag or variable is not affected.
#[test]
fn malformed_root_config_is_refused_unless_the_root_is_given() {
    let home = Home::new();
    home.config("root = [ \"secret-value\"");
    for args in [&["new", "x"][..], &["list"]] {
        let out = home.run(&[], args);
        assert!(!out.status.success(), "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("config.toml does not parse (contents withheld)"), "{stderr}");
        assert!(!stderr.contains("secret-value"), "{stderr}");
    }
    let out = home.run(&[("HERDR_PROJECTS_ROOT", "")], &["new", "x"]);
    assert!(!out.status.success());
    assert!(!home.path(".herdr-projects").exists());
    assert_eq!(home.new_project(&[("HERDR_PROJECTS_ROOT", "~/env")], &[], "y"), under(&home.path("env"), "y"));
    let flag = home.path("flag");
    assert_eq!(home.new_project(&[], &["--root", flag.to_str().unwrap()], "z"), under(&flag, "z"));
}

/// `HERDR_BIN_PATH` names the herdr binary; empty, it counts as unset and
/// `herdr` is looked up on PATH.
#[test]
fn herdr_binary_comes_from_the_variable_or_path() {
    let home = Home::new();
    let custom = home.path("custom-herdr");
    fs::copy(home.bin.join("herdr"), &custom).unwrap();
    let line = home.doctor(&[("HERDR_BIN_PATH", custom.to_str().unwrap())], &[], "herdr:");
    assert!(line.starts_with("[ok  ]") && line.ends_with(&format!("({})", custom.display())), "{line}");
    let line = home.doctor(&[("HERDR_BIN_PATH", "")], &[], "herdr:");
    assert!(line.starts_with("[ok  ]") && line.ends_with("(herdr)"), "{line}");
}

/// `--session`, then `--socket`, then `HERDR_SOCKET_PATH`, then
/// `HERDR_SESSION`, then herdr's default session, then herdr's default socket
/// path. A name is resolved by asking herdr; an unknown name is refused, and
/// `--session` with `--socket` is refused.
#[test]
fn session_comes_from_flags_then_variables_then_herdr_default() {
    let home = Home::new();
    let h = home.dir.path().display().to_string();
    fs::write(home.path("sessions.json"), format!(r#"{{"sessions":[
        {{"default":true,"name":"default","running":true,"session_dir":"{h}","socket_path":"{h}/default.sock"}},
        {{"default":false,"name":"hp-dev","running":true,"session_dir":"{h}/dev","socket_path":"{h}/dev.sock"}}]}}"#)).unwrap();
    let session = |vars: &[(&str, &str)], args: &[&str]| {
        let line = home.doctor(vars, args, "session:");
        line.split_once("session: ").unwrap().1.to_string()
    };
    let both = [("HERDR_SOCKET_PATH", "/env.sock"), ("HERDR_SESSION", "hp-dev")];
    assert_eq!(session(&both, &["--session", "hp-dev"]), format!("{h}/dev.sock (name: hp-dev); not reachable"));
    assert_eq!(session(&both, &["--session", "default"]), format!("{h}/default.sock (name: default); not reachable"));
    assert_eq!(session(&both, &["--socket", "/flag.sock"]), "/flag.sock (name: -); not reachable");
    assert_eq!(session(&both, &[]), "/env.sock (name: -); not reachable");
    assert_eq!(session(&[("HERDR_SOCKET_PATH", ""), ("HERDR_SESSION", "hp-dev")], &[]), format!("{h}/dev.sock (name: hp-dev); not reachable"));
    assert_eq!(session(&[("HERDR_SESSION", "")], &[]), format!("{h}/default.sock (name: -); not reachable"));

    // Unknown names fail the check, by flag or by variable.
    for (vars, args) in [(&[][..], &["--session", "nope"][..]), (&[("HERDR_SESSION", "nope")][..], &[][..])] {
        let out = home.run(vars, &[&["doctor"], args].concat());
        assert!(!out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(text.contains("[FAIL] session: herdr has no session named `nope`; start it with `herdr --session nope`"), "{text}");
    }
    let out = home.run(&[], &["doctor", "--session", "hp-dev", "--socket", "/b.sock"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot be used with"), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty());

    // Without a default session, or when herdr cannot list them, the default socket path.
    fs::write(home.path("sessions.json"), r#"{"sessions":[]}"#).unwrap();
    assert_eq!(session(&[], &[]), format!("{h}/.config/herdr/herdr.sock (name: -); not reachable"));
    fs::write(home.path("sessions.fail"), "").unwrap();
    assert_eq!(session(&[], &[]), format!("{h}/.config/herdr/herdr.sock (name: -); not reachable"));
    // A name still needs the listing.
    let line = home.doctor(&[], &["--session", "hp-dev"], "session:");
    assert!(line.starts_with("[FAIL]") && line.contains("session list` failed"), "{line}");
}
