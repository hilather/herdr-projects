#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! The worker sandbox authenticates as the owner's logged-in agent CLI by
//! binding the owner's single login file (never a copy) into the isolated
//! execution home, while the rest of the owner's agent directories stays hidden.
//! Runs the real sandbox with a probe script standing in for the agent.
use herdr_projects::{
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

    // Claude Code: its credentials file, and again nothing else.
    let report = probe(&world, &isolation(&world).with_shared_login("claude", &world.home, None).unwrap());
    assert_eq!(report["home_claude_login"], "claude-login", "{report:?}");
    assert_eq!(report["home_claude_dir"].split_whitespace().filter(|n| *n != ".credentials.json").count(), 0, "{report:?}");
    for hidden in ["owner_claude_session", "owner_claude_settings", "owner_claude_login", "owner_claude_state", "owner_codex_history", "owner_codex_config", "owner_claude_dir"] {
        assert_eq!(report[hidden], "", "{hidden} must stay hidden from the worker: {report:?}");
    }
    assert_eq!(fs::read_to_string(world.owner.join(".claude/.credentials.json")).unwrap(), "token-refreshed-in-place");
    assert_eq!(fs::read_to_string(world.owner.join(".claude/settings.json")).unwrap(), "owner-claude-settings");

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
