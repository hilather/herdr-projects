#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Owner-signed routines run by a foreground `ticker run` over migrated
//! projects. Routines are signed with `ssh-keygen` and installed with
//! `routine-store import`; migration, activation and runtime bindings use the
//! public library API, as in tests/cli.rs, because they have no CLI verb.
use herdr_projects::{authority, domain::*, migration, reconcile::ResourceState, runtime};
use sha2::{Digest, Sha256};
use std::{fs, os::unix::fs::PermissionsExt, path::{Path, PathBuf}, process::{Command, Stdio}, time::{Duration, Instant}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

struct Root { home: tempfile::TempDir, key: PathBuf, config: PathBuf }

impl Root {
    /// An owner key and config allowing routine commands in each of `slugs`,
    /// which are created, migrated and activated.
    fn new(slugs: &[&str]) -> Self {
        let home = tempfile::tempdir().unwrap();
        let key = home.path().join("owner");
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).output().unwrap().status.success());
        let public = fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let config = home.path().join(".config/herdr-projects/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let root = Root { home, key, config };
        let mut text = format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n");
        for slug in slugs {
            for command in ["new", "pause"] { assert!(root.command().args([command, slug]).status().unwrap().success()); }
            text += &format!("[safety.{:?}]\nroutine_commands=true\n", root.project(slug).canonicalize().unwrap().display().to_string());
        }
        fs::write(&root.config, text).unwrap();
        for slug in slugs {
            let project = root.project(slug);
            migration::apply(&project, &migration::inspect_with_config(&project, &root.config).unwrap(), true).unwrap();
            let s = runtime::snapshot(&project).unwrap();
            runtime::set_state(&project, s.head, s.control.unwrap().revision, ProjectState::Active, &root.config).unwrap();
        }
        root
    }
    fn path(&self, name: &str) -> PathBuf { self.home.path().join(name) }
    fn project(&self, slug: &str) -> PathBuf { self.path("root").join(slug) }
    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command.env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", self.path("herdr")).arg("--root").arg(self.path("root"));
        command
    }
    /// Signs and imports routine `name` of `slug`, due now and hourly, whose script is `script`.
    fn routine(&self, slug: &str, name: &str, script: &str) {
        let project = self.project(slug).canonicalize().unwrap();
        let path = project.join(format!("{name}.sh"));
        fs::write(&path, script).unwrap();
        let definition = RoutineDefinition { version: 1, name: name.into(), revision: 1, project_store: project.join(".state/state.db").display().to_string(),
            authority: authority::policy_reference(&project).unwrap(), config: migration::config_reference(&self.config).unwrap(), enabled: true,
            schedule: "every 1h".into(), timezone: "UTC".into(), start_unix_ms: jiff::Timestamp::now().as_millisecond() - 1000, missed: MissedRunPolicy::CoalesceLatest,
            overlap: OverlapPolicy::Skip, script: path.display().to_string(), script_sha256: format!("{:x}", Sha256::digest(script)), cwd: project.display().to_string(),
            deadline_ms: 1000, output_cap_bytes: 4000 };
        let document = self.path(&format!("{slug}-{name}.json"));
        fs::write(&document, serde_json::to_vec(&definition).unwrap()).unwrap();
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(&self.key).args(["-n", authority::ROUTINE_SIGNATURE_NAMESPACE]).arg(&document).output().unwrap().status.success());
        let head = runtime::snapshot(&project).unwrap().head.to_string();
        let out = self.command().args(["routine-store", slug, "import", document.to_str().unwrap(), &format!("{}.sig", document.display()), "--expected-head", &head]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    fn ticker(&self) -> Ticker<'_> {
        Ticker { root: self, child: self.command().args(["ticker", "run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap() }
    }
}

struct Ticker<'a> { root: &'a Root, child: std::process::Child }
impl Ticker<'_> {
    fn wait_for(&mut self, what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(90);
        while !done() {
            assert!(self.child.try_wait().unwrap().is_none(), "ticker exited while waiting for {what}");
            assert!(Instant::now() < deadline, "timed out waiting for {what}: {}", fs::read_to_string(self.root.path("root/.ticker.log")).unwrap_or_default());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Ticker<'_> { fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); } }

fn receipts(project: &Path) -> usize { runtime::snapshot(project).unwrap().routine_receipts.len() }

/// Replaces `project_fairness_advances_on_admission_not_ticker_cadence`, and
/// with `ticker_canonical_routine_admits_from_hint_and_restart_keeps_one_execution`
/// in tests/cli.rs `ticker_admits_signed_work_once_and_restart_does_not_replay`.
///
/// Two projects each have two routines queued. The ticker admits one routine
/// at a time and turns to the other project after each admission, so the
/// runs alternate between the projects, and each routine runs once.
#[test]
fn routines_of_two_projects_run_alternately_and_once_each() {
    let root = Root::new(&["first", "second"]);
    let order = root.path("order");
    for slug in ["first", "second"] {
        for name in ["a", "b"] { root.routine(slug, name, &format!("printf '%s\\n' {slug} >> '{}'\n", order.display())); }
        // Both routines are queued before the ticker starts: each project has a backlog.
        for name in ["a", "b"] {
            let head = runtime::snapshot(&root.project(slug)).unwrap().head.to_string();
            assert!(root.command().args(["routine-store", slug, "schedule", name, "--expected-head", &head]).status().unwrap().success());
        }
    }
    let mut ticker = root.ticker();
    ticker.wait_for("all four routines", || ["first", "second"].iter().all(|slug| receipts(&root.project(slug)) == 2));
    let runs: Vec<String> = fs::read_to_string(&order).unwrap().lines().map(str::to_owned).collect();
    assert_eq!(runs.len(), 4, "{runs:?}");
    assert!(runs.windows(2).all(|pair| pair[0] != pair[1]), "a project ran twice in a row: {runs:?}");
    for slug in ["first", "second"] {
        let snapshot = runtime::snapshot(&root.project(slug)).unwrap();
        assert!(snapshot.deliveries.iter().all(|d| d.attempts == 1), "{:?}", snapshot.deliveries);
    }
}

/// Replaces `canonical_withdrawn_routine_does_not_discard_valid_observations`.
///
/// A routine whose script changed after it was signed is withdrawn: it is
/// never scheduled or run. The ticker still records what the project's
/// runtime binding observes.
#[test]
fn a_changed_routine_script_is_not_run_but_the_binding_is_still_observed() {
    let root = Root::new(&["demo"]);
    let project = root.project("demo").canonicalize().unwrap();
    root.routine("demo", "first", "touch MUST_NOT_EXECUTE\n");
    fs::write(project.join("first.sh"), "touch WITHDRAWN_SCRIPT_RAN\n").unwrap();
    let socket = root.path("session.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let route = RuntimeRoute { socket: socket.display().to_string(), workspace_id: "w".into(), tab_id: "t".into(), pane_id: "p".into(), cwd: "/fixture".into(), ..Default::default() };
    runtime::create_binding(&project, None, None, runtime::snapshot(&project).unwrap().head, &route).unwrap();
    fs::write(root.path("herdr"), "#!/usr/bin/python3\nimport sys,json\nif sys.argv[1:]==['--version']:print('herdr 0.9.1');sys.exit(0)\n\
p={'pane_id':'p','workspace_id':'w','tab_id':'t','cwd':'/fixture','agent':'claude','name':'fixture','agent_status':'idle'}\n\
kind={('pane','list'):'panes',('agent','list'):'agents'}.get(tuple(sys.argv[1:]))\nif not kind:sys.exit(4)\nprint(json.dumps({'result':{kind:[p]}}))\n").unwrap();
    fs::set_permissions(root.path("herdr"), fs::Permissions::from_mode(0o700)).unwrap();
    let mut ticker = root.ticker();
    ticker.wait_for("the binding's observation", || runtime::snapshot(&project).unwrap().observations.iter().any(|o| o.pane == ResourceState::Present));
    // Two more passes, each of which republishes the metrics file.
    use std::os::unix::fs::MetadataExt;
    let metrics = || fs::metadata(root.path("root/.ticker-metrics.json")).map(|m| m.ino()).ok();
    for _ in 0..2 { let seen = metrics(); ticker.wait_for("another pass", || metrics() != seen); }
    let snapshot = runtime::snapshot(&project).unwrap();
    assert!(snapshot.routine_occurrences.is_empty() && snapshot.routine_receipts.is_empty(), "{:?}", snapshot.routine_occurrences);
    assert!(!project.join("MUST_NOT_EXECUTE").exists() && !project.join("WITHDRAWN_SCRIPT_RAN").exists());
}
