#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! The worker sandbox authenticates a Codex worker as the owner's logged-in CLI
//! by binding the owner's single `auth.json` (never a copy) into the isolated
//! execution home, while the rest of the owner's agent directories stays hidden.
//! Codex rewrites that file in place, so the owner's own refreshes stay visible
//! to the worker. Claude Code replaces its credentials file by rename, which a
//! bind mount cannot follow, so a Claude worker gets a long-lived setup token
//! file instead: handed to the agent alone as an environment variable, never
//! bound, copied into the home or put in any argument.
//! Runs the real sandbox with a probe script standing in for the agent.
use herdr_farm::{
    execution_guard::GatedSpawn,
    worker_supervision::{Isolation, isolated_gated_command},
};
use std::{
    fs,
    io::Write as _,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

struct World {
    _guard: tempfile::TempDir,
    owner: PathBuf,
    project: PathBuf,
    home: PathBuf,
    agent: PathBuf,
}

/// An owner home (outside the private `/tmp`) with logins and private agent
/// data, a projects root with one project, an execution home and a probe agent.
fn world(base: &Path) -> World {
    let guard = tempfile::tempdir_in(base).unwrap();
    let top = guard.path().canonicalize().unwrap();
    let owner = top.join("owner");
    for (path, text) in [
        (".codex/auth.json", "codex-login"),
        (".codex/history.jsonl", "codex-private-history"),
        (".codex/config.toml", "owner-codex-config"),
        (".claude/.credentials.json", "claude-login"),
        (".claude/projects/p/session.jsonl", "claude-private-session"),
        (".claude/settings.json", "owner-claude-settings"),
        (".claude.json", "owner-claude-state"),
        (".gemini/oauth_creds.json", "gemini-login"),
        (".grok/auth.json", "grok-login"),
    ] {
        fs::create_dir_all(owner.join(path).parent().unwrap()).unwrap();
        fs::write(owner.join(path), text).unwrap();
        fs::set_permissions(owner.join(path), fs::Permissions::from_mode(0o600)).unwrap();
    }
    let root = top.join("root");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(root.join(".execution.lock"), b"").unwrap();
    let home = top.join("exec-home");
    fs::create_dir(&home).unwrap();
    // The probe reports what the worker can reach; nothing in it is a secret.
    let agent = top.join("probe-agent");
    let owner_text = owner.display();
    fs::write(
        &agent,
        format!(
            r#"#!/bin/sh
out="$HOME/probe.txt"; : > "$out"
show() {{ printf '%s=%s\n' "$1" "$(cat "$2" 2>/dev/null | head -c 64)" >> "$out"; }}
show home_codex_login "$HOME/.codex/auth.json"
show home_claude_login "$HOME/.claude/.credentials.json"
show owner_codex_history "{owner_text}/.codex/history.jsonl"
show owner_codex_config "{owner_text}/.codex/config.toml"
show owner_codex_login "{owner_text}/.codex/auth.json"
show owner_claude_session "{owner_text}/.claude/projects/p/session.jsonl"
show owner_claude_settings "{owner_text}/.claude/settings.json"
show owner_claude_login "{owner_text}/.claude/.credentials.json"
show owner_claude_state "{owner_text}/.claude.json"
show owner_gemini "{owner_text}/.gemini/oauth_creds.json"
show owner_grok "{owner_text}/.grok/auth.json"
show setup_token_file "{owner_text}/../claude-setup-token"
printf 'token_env=%s\n' "${{CLAUDE_CODE_OAUTH_TOKEN-unset}}" >> "$out"
if [ -e /proc/self/fd/9 ]; then echo fd9=open >> "$out"; else echo fd9=closed >> "$out"; fi
printf 'home_codex_dir=%s\n' "$(ls -A "$HOME/.codex" 2>/dev/null | tr '\n' ' ')" >> "$out"
printf 'home_claude_dir=%s\n' "$(ls -A "$HOME/.claude" 2>/dev/null | tr '\n' ' ')" >> "$out"
printf 'owner_codex_dir=%s\n' "$(ls -A "{owner_text}/.codex" 2>/dev/null | tr '\n' ' ')" >> "$out"
printf 'owner_claude_dir=%s\n' "$(ls -A "{owner_text}/.claude" 2>/dev/null | tr '\n' ' ')" >> "$out"
for f in "$HOME/.codex/auth.json" "$HOME/.claude/.credentials.json"; do
  if [ -f "$f" ]; then printf 'inode_%s=%s\n' "$(basename "$f")" "$(stat -c %i "$f")" >> "$out"; printf 'token-refreshed-in-place' > "$f" 2>/dev/null && printf 'wrote_%s=yes\n' "$(basename "$f")" >> "$out"; fi
done
"#
        ),
    )
    .unwrap();
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o700)).unwrap();
    World { _guard: guard, owner, project, home, agent }
}

/// Run the probe once under `isolation`; its report as (name, value) pairs.
fn probe(world: &World, isolation: &Isolation) -> std::collections::BTreeMap<String, String> {
    let _ = fs::remove_file(world.home.join("probe.txt"));
    let argv = isolated_gated_command(&world.agent, &[], 30, "release-probe", &world.home, isolation).unwrap();
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(&world.project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn_gated()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"release-probe\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "isolated probe failed: {}", String::from_utf8_lossy(&output.stderr));
    fs::read_to_string(world.home.join("probe.txt"))
        .unwrap()
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, v)| (k.to_owned(), v.trim().to_owned())))
        .collect()
}

fn isolation(world: &World) -> Isolation {
    Isolation::for_agent(&world.project, &world.home, &world.project, &world.agent, &[], &[], None, None, &[]).unwrap()
}

#[test]
fn the_worker_shares_the_owners_single_login_file_and_nothing_else_of_the_agent_directories() {
    let base = Path::new(env!("CARGO_TARGET_TMPDIR"));
    fs::create_dir_all(base).unwrap();
    let world = world(base);
    // The fixture owner home is declared explicitly: the real owner's home
    // (passwd entry or HOME) is never consulted for logins or hidden paths.
    // SAFETY: this binary has one test and no other thread reads the environment.
    unsafe { std::env::set_var("HERDR_PROJECTS_OWNER_HOME", &world.owner) };

    // Codex: the login is the owner's file itself, written in place by the
    // worker; every other file under ~/.codex and ~/.claude, and the other
    // agents' directories, stay hidden.
    let report = probe(&world, &isolation(&world).with_shared_login("codex", &world.home, None).unwrap());
    assert_eq!(report["home_codex_login"], "codex-login", "{report:?}");
    assert_eq!(report["home_codex_dir"], "auth.json", "only the login file is visible in the home's .codex: {report:?}");
    assert_eq!(report["home_claude_login"], "", "{report:?}");
    for hidden in [
        "owner_codex_history", "owner_codex_config", "owner_codex_login", "owner_claude_session", "owner_claude_settings",
        "owner_claude_login", "owner_claude_state", "owner_gemini", "owner_grok", "owner_codex_dir", "owner_claude_dir",
    ] {
        assert_eq!(report[hidden], "", "{hidden} must stay hidden from the worker: {report:?}");
    }
    assert_eq!(report["wrote_auth.json"], "yes", "{report:?}");
    // Same file, not a copy: the worker's write is the owner's file, same inode.
    let login = world.owner.join(".codex/auth.json");
    assert_eq!(fs::read_to_string(&login).unwrap(), "token-refreshed-in-place");
    assert_eq!(report["inode_auth.json"], fs::metadata(&login).unwrap().ino().to_string());
    // No copy was left in the execution home for a later launch to find: only
    // the empty mount point the sandbox created.
    assert_eq!(fs::read_to_string(world.home.join(".codex/auth.json")).unwrap(), "", "the home holds only the mount point of the bound file");
    assert!(fs::read_to_string(world.owner.join(".codex/history.jsonl")).unwrap() == "codex-private-history");

    an_owner_refresh_of_the_codex_login_written_in_place_is_visible_to_a_running_worker(&world);
    fs::write(world.owner.join(".codex/auth.json"), "codex-login").unwrap();

    // Claude Code: no credentials file is bound any more, so the owner's
    // refresh (a rename onto the owner's path) can never leave the worker with a
    // stale inode, nor the worker's own refresh fight a mount point.
    let report = probe(&world, &isolation(&world).with_shared_login("claude", &world.home, None).unwrap());
    assert_eq!(report["home_claude_login"], "", "no bound Claude credentials: {report:?}");
    assert_eq!(report["token_env"], "unset", "{report:?}");
    assert_eq!(report["owner_claude_login"], "", "{report:?}");

    // The Claude login is a setup-token file: its content reaches the agent's
    // environment alone, through a descriptor the wrapper closes.
    let token = world.owner.parent().unwrap().join("claude-setup-token");
    fs::write(&token, "sk-ant-oat01-fixture-setup-token\n").unwrap();
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
    let with_token = isolation(&world).with_login_token_file(&token).unwrap();
    let argv = isolated_gated_command(&world.agent, &[], 30, "release-probe", &world.home, &with_token).unwrap();
    assert!(argv.iter().all(|a| !a.contains("fixture-setup-token")), "the token never appears in an argument");
    assert!(argv.iter().any(|a| a == &format!("tokensrc:{}", token.display())), "{argv:?}");
    let report = probe(&world, &with_token);
    assert_eq!(report["token_env"], "sk-ant-oat01-fixture-setup-token", "{report:?}");
    assert_eq!(report["fd9"], "closed", "{report:?}");
    assert_eq!(report["setup_token_file"], "", "the agent cannot read the token file itself: {report:?}");
    assert_eq!(report["home_claude_login"], "", "{report:?}");
    assert_eq!(report["home_claude_dir"], "", "nothing is bound or written under the home's .claude: {report:?}");
    for hidden in ["owner_claude_session", "owner_claude_settings", "owner_claude_login", "owner_claude_state", "owner_codex_history", "owner_codex_config", "owner_claude_dir"] {
        assert_eq!(report[hidden], "", "{hidden} must stay hidden from the worker: {report:?}");
    }
    assert_eq!(fs::read_to_string(world.owner.join(".claude/.credentials.json")).unwrap(), "claude-login", "the owner's credentials are untouched");
    let leaked = Command::new("/usr/bin/grep").args(["-r", "-l", "--exclude=probe.txt", "fixture-setup-token"]).arg(&world.home).output().unwrap();
    assert!(leaked.stdout.is_empty(), "the token is in no file of the execution home: {}", String::from_utf8_lossy(&leaked.stdout));
    // (The probe can read that file when nothing hides it.)
    assert_eq!(probe(&world, &isolation(&world))["setup_token_file"], "sk-ant-oat01-fixture-setup-token");
    // A Codex worker never sees the variable.
    let report = probe(&world, &isolation(&world).with_shared_login("codex", &world.home, None).unwrap());
    assert_eq!(report["token_env"], "unset", "{report:?}");

    // A token file that others can read, that lies in an agent directory, in
    // the project, or that is empty or holds more than a token is refused.
    let loose = world.owner.parent().unwrap().join("loose-token");
    fs::write(&loose, "t\n").unwrap();
    fs::set_permissions(&loose, fs::Permissions::from_mode(0o644)).unwrap();
    let inside_agent_dir = world.owner.join(".claude/token");
    fs::write(&inside_agent_dir, "t\n").unwrap();
    fs::set_permissions(&inside_agent_dir, fs::Permissions::from_mode(0o600)).unwrap();
    let inside_project = world.project.join("token");
    fs::write(&inside_project, "t\n").unwrap();
    fs::set_permissions(&inside_project, fs::Permissions::from_mode(0o600)).unwrap();
    let empty = world.owner.parent().unwrap().join("empty-token");
    fs::write(&empty, "\n").unwrap();
    fs::set_permissions(&empty, fs::Permissions::from_mode(0o600)).unwrap();
    let multiline = world.owner.parent().unwrap().join("multiline-token");
    fs::write(&multiline, "one\ntwo\n").unwrap();
    fs::set_permissions(&multiline, fs::Permissions::from_mode(0o600)).unwrap();
    for bad in [&loose, &inside_agent_dir, &inside_project, &empty, &multiline, &world.owner.parent().unwrap().join("missing-token")] {
        assert!(isolation(&world).with_login_token_file(bad).is_err(), "{} must be refused", bad.display());
    }

    // An explicit override binds another token file instead of the owner's default.
    let token = world.owner.parent().unwrap().join("setup-token");
    fs::write(&token, "override-token").unwrap();
    let report = probe(&world, &isolation(&world).with_shared_login("codex", &world.home, Some(&token)).unwrap());
    assert_eq!(report["home_codex_login"], "override-token", "{report:?}");
    assert_eq!(report["owner_codex_login"], "", "{report:?}");

    // Without a shared login (sharing off) the worker has no login at all.
    let _ = fs::remove_dir_all(world.home.join(".codex"));
    let report = probe(&world, &isolation(&world));
    assert_eq!(report["home_codex_login"], "", "{report:?}");
    assert_eq!(report["owner_codex_history"], "", "{report:?}");

    // A kind whose owner login does not exist starts without one.
    fs::remove_file(world.owner.join(".codex/auth.json")).unwrap();
    let report = probe(&world, &isolation(&world).with_shared_login("codex", &world.home, None).unwrap());
    assert_eq!(report["home_codex_login"], "", "{report:?}");
}

/// Codex rewrites `auth.json` in place (truncate and write, the same inode), so
/// the bind stays current: a refresh the owner's own session makes while the
/// worker runs is visible to the worker.
fn an_owner_refresh_of_the_codex_login_written_in_place_is_visible_to_a_running_worker(world: &World) {
    let watcher = world.home.parent().unwrap().join("watch-agent");
    fs::write(
        &watcher,
        r#"#!/bin/sh
: > "$HOME/watching"
n=0
while [ "$n" -lt 200 ]; do
  if [ "$(cat "$HOME/.codex/auth.json" 2>/dev/null)" = owner-refreshed ]; then echo seen > "$HOME/refresh.txt"; exit 0; fi
  n=$((n+1)); sleep 0.1
done
echo missed > "$HOME/refresh.txt"
"#,
    )
    .unwrap();
    fs::set_permissions(&watcher, fs::Permissions::from_mode(0o700)).unwrap();
    let isolation = Isolation::for_agent(&world.project, &world.home, &world.project, &watcher, &[], &[], None, None, &[]).unwrap().with_shared_login("codex", &world.home, None).unwrap();
    let argv = isolated_gated_command(&watcher, &[], 60, "release-probe", &world.home, &isolation).unwrap();
    let mut child = Command::new(&argv[0]).args(&argv[1..]).current_dir(&world.project).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn_gated().unwrap();
    child.stdin.take().unwrap().write_all(b"release-probe\n").unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !world.home.join("watching").exists() {
        assert!(std::time::Instant::now() < until, "the worker never started");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // The owner's refresh: in place, same inode.
    let login = world.owner.join(".codex/auth.json");
    let inode = fs::metadata(&login).unwrap().ino();
    fs::OpenOptions::new().write(true).truncate(true).open(&login).unwrap().write_all(b"owner-refreshed").unwrap();
    assert_eq!(fs::metadata(&login).unwrap().ino(), inode);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(fs::read_to_string(world.home.join("refresh.txt")).unwrap().trim(), "seen", "the worker must see the owner's in-place refresh");
}
