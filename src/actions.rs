//! What herdr's action menu and the four popup panes run. An action that needs
//! to ask the user something opens its popup with `herdr plugin pane open`,
//! passing what it already knows through a small file in the plugin state dir.

use std::io::{BufRead, Write as _};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::adopt::{self, AdoptWorkspace};
use crate::coordinator::{self, OpenOptions};
use crate::herdr::{CALL_TIMEOUT, Herdr};
use crate::paths::{Ctx, SessionFlags};
use crate::project::{self, Status};
use crate::{doctor, lifecycle, overview};

const PLUGIN_ID: &str = "herdr-farm";
mod handoff;

/// What an action hands to the popup it opens.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Handoff {
    /// The subcommand the `pick` pane should run: `open`, `pause` or `resume`.
    pub command: String,
    pub slug: String,
    /// Captured by the action, before any popup opens.
    pub pane_id: String,
    pub workspace_label: String,
    pub workspace_cwd: String,
    pub socket: String,
    /// `fleet`: identity (`device:inode`) of the named project's canonical
    /// store when the action ran, so the popup reads that project and no other.
    pub store: String,
}

/// The originating pane and workspace, from the action's own environment or
/// from `HERDR_PLUGIN_CONTEXT_JSON`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ActionContext {
    workspace_id: String,
    workspace_label: String,
    workspace_cwd: String,
    focused_pane_id: String,
}

fn action_context(ctx: &Ctx) -> ActionContext {
    ctx.env.var("HERDR_PLUGIN_CONTEXT_JSON").and_then(|json| serde_json::from_str(json).ok()).unwrap_or_default()
}

fn socket(ctx: &Ctx) -> Result<String> {
    ctx.env.var("HERDR_SOCKET_PATH").map(str::to_string).context("HERDR_SOCKET_PATH is not set: this command is meant to be run by herdr")
}

fn open_pane(ctx: &Ctx, entrypoint: &str, handoff: &Handoff) -> Result<()> {
    let id = handoff::create(ctx, entrypoint, handoff)?;
    let binding = format!("{}={id}", handoff::ENV);
    let root_binding = format!("HERDR_FARM_ROOT={}", std::path::absolute(&ctx.root)?.display());
    let herdr = Herdr::new(ctx.env.herdr_bin(), socket(ctx)?, ctx.runner);
    herdr.call(&["plugin", "pane", "open", "--plugin", PLUGIN_ID, "--entrypoint", entrypoint, "--env", &binding, "--env", &root_binding], CALL_TIMEOUT).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

/// The project of the workspace the action was invoked from, if any.
fn current_slug(ctx: &Ctx) -> Option<String> {
    let context = action_context(ctx);
    let workspace = ctx.env.var("HERDR_WORKSPACE_ID").map(str::to_string).unwrap_or(context.workspace_id);
    overview::project_for_workspace(ctx, &workspace, ctx.env.var("HERDR_SOCKET_PATH").unwrap_or(""))
}

pub fn run_action(ctx: &Ctx, id: &str) -> Result<()> {
    let context = action_context(ctx);
    let base = Handoff { socket: socket(ctx).unwrap_or_default(), ..Handoff::default() };
    match id {
        "new" => open_pane(ctx, "new", &base),
        "overview" => open_pane(ctx, "overview", &Handoff { slug: current_slug(ctx).unwrap_or_default(), ..base }),
        // The read-only fleet popup, scoped to the invoking workspace's project if any.
        // TM4.8: the refreshing fleet pane and the three owner popups, each bound
        // to the invoking workspace's project (store identity) like `fleet`.
        "fleet" | "fleet-watch" | "fleet-race" | "fleet-select" | "fleet-replay" => {
            let slug = current_slug(ctx).unwrap_or_default();
            let store = fleet_store(ctx, &slug);
            open_pane(ctx, id, &Handoff { slug, store, ..base })
        }
        "open" | "pause" | "resume" => match current_slug(ctx) {
            Some(slug) => run_on_slug(ctx, id, &slug),
            None => open_pane(ctx, "pick", &Handoff { command: id.to_string(), ..base }),
        },
        "focus" => match current_slug(ctx) {
            Some(slug) => overview::focus(ctx, Some(&slug)),
            None => bail!("this workspace does not belong to a project; run `focus <slug>` from a terminal"),
        },
        "unfocus" => overview::unfocus(ctx, &SessionFlags::default()),
        "adopt-workspace" => {
            // The originating pane is captured here, before any popup opens.
            let pane = ctx.env.var("HERDR_PANE_ID").map(str::to_string).unwrap_or(context.focused_pane_id);
            if pane.is_empty() {
                bail!("herdr did not say which pane this action was invoked from");
            }
            let herdr = Herdr::new(ctx.env.herdr_bin(), socket(ctx)?, ctx.runner);
            adopt::adoptable_agent(ctx, &herdr, &socket(ctx)?, &pane)?;
            open_pane(ctx, "adopt", &Handoff { pane_id: pane, workspace_label: context.workspace_label, workspace_cwd: context.workspace_cwd, ..base })
        }
        "doctor" => {
            let healthy = doctor::run(ctx, &SessionFlags::default())?;
            let herdr = Herdr::new(ctx.env.herdr_bin(), socket(ctx)?, ctx.runner);
            let body = if healthy { "All required checks passed. Details: herdr plugin log --plugin herdr-farm" } else { "Some checks FAILED. Details: herdr plugin log --plugin herdr-farm" };
            let _ = herdr.notification_show("herdr-farm doctor", body);
            Ok(())
        }
        other => bail!("unknown action `{other}`"),
    }
}

fn run_on_slug(ctx: &Ctx, command: &str, slug: &str) -> Result<()> {
    match command {
        "open" => coordinator::open(ctx, slug, &OpenOptions { session: SessionFlags { session: None, socket: Some(PathBuf::from(socket(ctx)?)) }, reprime: false, rebind: false }),
        "pause" => lifecycle::set_status(ctx, slug, Status::Paused),
        "resume" => lifecycle::set_status(ctx, slug, Status::Active),
        other => bail!("`{other}` cannot be run from the picker"),
    }
}

fn ask(question: &str, default: &str) -> Result<String> {
    if default.is_empty() {
        print!("{question}: ");
    } else {
        print!("{question} [{default}]: ");
    }
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let answer = line.trim();
    Ok(if answer.is_empty() { default.to_string() } else { answer.to_string() })
}

fn hold_open() {
    let _ = ask("\nPress Enter to close", "");
}

/// A popup's body. Errors are printed and the popup is held open, so the user
/// can read them before it closes.
pub fn run_pane(ctx: &Ctx, id: &str) -> Result<()> {
    if id == "fleet" {
        fleet(ctx);
        hold_open();
        return Ok(());
    }
    if id == "fleet-watch" {
        return fleet_watch(ctx);
    }
    let handoff = match handoff::consume(ctx, id) {
        Ok(handoff) => handoff,
        Err(error) => { println!("error: {error:#}"); hold_open(); return Err(error); }
    };
    let result = match id {
        "overview" => return overview::run(ctx, Some(handoff.slug.as_str()).filter(|s| !s.is_empty()), true),
        "new" => (|| {
            println!("New project\n");
            let name = ask("Name", "")?;
            if name.is_empty() {
                bail!("no name given");
            }
            let goal = ask("Goal (one line, optional)", "")?;
            let project = project::create(&ctx.root, &name, &goal, Vec::new())?;
            println!("created `{}` at {}", project.slug, project.dir().display());
            run_on_slug(ctx, "open", &project.slug)
        })(),
        "pick" => (|| {
            println!("Which project should `{}` act on?\n", handoff.command);
            let slug = overview::pick(ctx)?;
            run_on_slug(ctx, &handoff.command, &slug)
        })(),
        "adopt" => (|| {
            println!("Continue this workspace as a project\n");
            let name = ask("Project name", &handoff.workspace_label)?;
            if name.is_empty() {
                bail!("no name given");
            }
            adopt::adopt_workspace(
                ctx,
                &AdoptWorkspace { name, goal: String::new(), pane: handoff.pane_id.clone(), workspace_cwd: handoff.workspace_cwd.clone(), session: SessionFlags { session: None, socket: Some(PathBuf::from(socket(ctx)?)) } },
            )
        })(),
        "fleet-race" | "fleet-select" | "fleet-replay" => owner_popup(ctx, id, &handoff),
        other => bail!("unknown pane `{other}`"),
    };
    if let Err(error) = &result {
        println!("\nerror: {error:#}");
    }
    hold_open();
    result
}

/// The store identity a `fleet` handoff binds for `slug` (empty without a project).
#[cfg(feature = "state-store")]
fn fleet_store(ctx: &Ctx, slug: &str) -> String {
    use herdr_farm::telemetry::views;
    if slug.is_empty() { return String::new(); }
    views::scope(&ctx.root, slug).and_then(|scope| views::store_identity(&scope)).unwrap_or_default()
}

#[cfg(not(feature = "state-store"))]
fn fleet_store(_ctx: &Ctx, _slug: &str) -> String { String::new() }

/// A `fleet` handoff reads only the project it was issued for (TM4.2): its
/// slug must resolve inside the root to the store the action saw, and, when
/// the popup can tell its own workspace's project, name that project.
#[cfg(feature = "state-store")]
fn check_fleet_handoff(ctx: &Ctx, handoff: &Handoff) -> Result<()> {
    use herdr_farm::telemetry::views;
    let scope = views::scope(&ctx.root, &handoff.slug)?;
    anyhow::ensure!(!handoff.store.is_empty() && views::store_identity(&scope)? == handoff.store,
        "the handoff names project `{}` but was not issued for its store", handoff.slug);
    if let Some(own) = current_slug(ctx) && own != handoff.slug {
        bail!("the handoff names project `{}` but this workspace belongs to `{own}`", handoff.slug);
    }
    Ok(())
}

/// Read-only fleet view (telemetry S7, TM4.2 views): the project of the
/// invoking workspace (or of an optional handoff), else every project with a
/// canonical store. Writes nothing: a handoff is consumed only when one was
/// passed. `[telemetry] views = false` in config.toml turns it off.
fn fleet(ctx: &Ctx) {
    #[cfg(not(feature = "state-store"))]
    { let _ = ctx; println!("fleet panel unavailable: this build lacks the `state-store` feature; rebuild with `cargo build --release --locked --features state-store`"); }
    #[cfg(feature = "state-store")]
    {
        use herdr_farm::telemetry::{panel, views, workspace};
        let handoff = ctx.env.var(handoff::ENV).map(|_| handoff::consume(ctx, "fleet"));
        let slug = match handoff {
            Some(Err(error)) => { println!("error: {error:#}"); return; }
            Some(Ok(handoff)) if !handoff.slug.is_empty() => match check_fleet_handoff(ctx, &handoff) {
                Ok(()) => Some(handoff.slug),
                Err(error) => { println!("error: fleet handoff refused: {error:#}"); return; }
            },
            _ => current_slug(ctx),
        };
        match views::enabled(&ctx.config_dir) {
            Ok(true) => {}
            Ok(false) => { println!("{}", views::disabled_message(&ctx.config_dir)); return; }
            Err(error) => { println!("error: {error:#}"); return; }
        }
        let now = jiff::Timestamp::now();
        for slug in slug.map_or_else(|| project::list_slugs(&ctx.root), |slug| vec![slug]) {
            println!("── {slug} · fleet · as of {} ──", now.strftime("%Y-%m-%d %H:%M:%S UTC"));
            if !ctx.root.join(&slug).join(".state/state.db").is_file() {
                println!("no canonical store; telemetry n/a\n");
                continue;
            }
            // Each section reads its own project only (root-confined, no symlinked store).
            let scope = match views::scope(&ctx.root, &slug) { Ok(scope) => scope, Err(error) => { println!("error: {error:#}\n"); continue; } };
            // TM4.8: one workspace snapshot; when the query service cannot answer the
            // project shows `unavailable` and no number at all.
            let snapshot = workspace::snapshot(&scope.dir, &slug);
            if snapshot["status"] != "available" {
                println!("{}", workspace::text(&snapshot));
                continue;
            }
            match panel::render(&scope.dir, now.as_millisecond()).and_then(|text| Ok(text + "\n" + &panel::views(&scope.dir, &slug)?)) {
                Ok(text) => println!("{text}\n{}", workspace::text(&snapshot)),
                Err(error) => println!("error: {error:#}\n"),
            }
        }
    }
}

/// The project a TM4.8 pane acts on: the handoff's (bound to its store and to
/// the invoking workspace's project, as `fleet`), else the operator's answer.
#[cfg(feature = "state-store")]
fn workspace_scope(ctx: &Ctx, handoff: &Handoff) -> Result<herdr_farm::telemetry::views::Scope> {
    use herdr_farm::telemetry::views;
    if !handoff.slug.is_empty() {
        check_fleet_handoff(ctx, handoff).context("fleet handoff refused")?;
        return views::scope(&ctx.root, &handoff.slug);
    }
    let slug = ask("Project", "")?;
    views::scope(&ctx.root, &slug)
}

/// `fleet-watch` (split pane): `telemetry <slug> watch` for the handed-off
/// project until the pane is closed. Read-only.
fn fleet_watch(ctx: &Ctx) -> Result<()> {
    #[cfg(not(feature = "state-store"))]
    { let _ = ctx; println!("fleet pane unavailable: this build lacks the `state-store` feature"); hold_open(); Ok(()) }
    #[cfg(feature = "state-store")]
    {
        use herdr_farm::telemetry::workspace;
        let scope = handoff::consume(ctx, "fleet-watch").and_then(|handoff| workspace_scope(ctx, &handoff));
        let scope = match scope { Ok(scope) => scope, Err(error) => { println!("error: {error:#}"); hold_open(); return Err(error); } };
        let interval = ctx.env.var("HERDR_FARM_FLEET_INTERVAL").and_then(|s| s.parse().ok()).filter(|s| (1..=workspace::WATCH_MAX_SECS).contains(s))
            .unwrap_or(workspace::WATCH_DEFAULT_SECS);
        let iterations = ctx.env.var("HERDR_FARM_FLEET_ITERATIONS").and_then(|s| s.parse().ok());
        workspace::watch(&ctx.root, &scope.slug, &ctx.config_dir, &workspace::WatchArgs { interval_secs: interval, iterations })
    }
}

/// `fleet-race`, `fleet-select`, `fleet-replay` (TM4.8): a proposal built from
/// the operator's answers, shown with the exact owner command, and run only on
/// an explicit `y`. The owner command is the only writer and keeps its own
/// checks: it refuses a worker execution context and worker principals.
fn owner_popup(ctx: &Ctx, id: &str, handoff: &Handoff) -> Result<()> {
    #[cfg(not(feature = "state-store"))]
    { let _ = (ctx, id, handoff); bail!("candidate groups and the replay suite need the `state-store` feature") }
    #[cfg(feature = "state-store")]
    {
        use herdr_farm::telemetry::workspace::owner::{self, Owner};
        let scope = workspace_scope(ctx, handoff)?;
        let slug = scope.slug.as_str();
        let owner = match id {
            "fleet-race" => {
                println!("Propose a candidate group (race) on `{slug}`: nothing is written until you confirm.\n");
                let task = ask("Task id", "")?;
                let arms = ask("Arms: retained worker profile names, comma-separated (2-8)", "")?;
                let mut args = vec!["groups".to_string(), "create".into(), task];
                for arm in arms.split(',').map(str::trim).filter(|a| !a.is_empty()) { args.push("--arm".into()); args.push(arm.into()); }
                println!("\nEach arm is an ordinary attempt of the task, reserved through the launch path under its own profile's budget; the group grants no launch.");
                Owner::Quality(args)
            }
            "fleet-select" => {
                println!("Record a candidate-group selection on `{slug}`: nothing is written until you confirm.\n");
                let shown = owner::run(&scope.dir, &Owner::Quality(vec!["groups".into(), "show".into()]))?;
                let groups: serde_json::Value = serde_json::from_str(&shown)?;
                let groups = groups["groups"].as_array().cloned().unwrap_or_default();
                for (i, g) in groups.iter().enumerate().filter(|(_, g)| g["status"] == "open") {
                    let arms: Vec<String> = g["arms"].as_array().into_iter().flatten().map(|a| format!("arm {} {}", a["arm"], a["outcome"].as_str().unwrap_or("?"))).collect();
                    println!("  race#{} {} task {}: {}", i + 1, g["group_id"].as_str().unwrap_or("?"), g["task_id"].as_str().unwrap_or("?"), arms.join(" · "));
                }
                let chosen = ask("Group (race#N or group id)", "")?;
                let group = match chosen.strip_prefix("race#").and_then(|n| n.parse::<usize>().ok()) {
                    Some(n) => groups.get(n.wrapping_sub(1)).and_then(|g| g["group_id"].as_str()).map(str::to_owned).with_context(|| format!("no candidate group {chosen}"))?,
                    None => chosen,
                };
                let arm = ask("Winning arm number, or `none`", "")?;
                let reason = ask("Reason code", "unspecified")?;
                let mut args = vec!["groups".to_string(), "select".into(), group];
                if arm == "none" { args.push("--none".into()); } else { args.push("--arm".into()); args.push(arm); }
                args.push("--reason".into());
                args.push(reason);
                println!("\nA selection verifies, integrates and moves nothing: the winner still takes the ordinary verification and integration path.");
                Owner::Quality(args)
            }
            _ => {
                println!("Launch a replay-suite run on `{slug}`: replay tasks are created only when you confirm; each still needs its own signed contract and launch approval.\n");
                let suite = ask("Suite version", "")?;
                let configuration = ask("Configuration label", "")?;
                let subset = ask("Subset", "stratified:4")?;
                let seed = ask("Seed", "")?;
                let preview = Owner::Replay(vec!["subset".into(), "--suite".into(), suite.clone(), "--subset".into(), subset.clone(), "--seed".into(), seed.clone()]);
                println!("\nPreview: {}", preview.command_line(slug));
                print!("{}", owner::run(&scope.dir, &preview)?);
                #[cfg(target_os = "linux")]
                let head = herdr_farm::runtime::snapshot(&scope.dir)?.head.to_string();
                #[cfg(not(target_os = "linux"))]
                let head = String::from("0");
                Owner::Replay(vec!["run".into(), "--suite".into(), suite, "--configuration".into(), configuration, "--subset".into(), subset, "--seed".into(), seed,
                    "--expected-head".into(), head])
            }
        };
        println!("Runs: {}", owner.command_line(slug));
        if ask("Confirm (y/N)", "N")? != "y" {
            println!("nothing written");
            return Ok(());
        }
        print!("{}", owner::run(&scope.dir, &owner)?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Env;
    use crate::runner::fake::ok;
    use crate::scenarios::World;

    fn plugin_env(world: &World, extra: &[(&str, &str)]) -> Env {
        let state = world.home.path().join("state");
        let socket = world.home.path().join("a.sock");
        let mut vars = vec![("HERDR_PLUGIN_STATE_DIR", state.to_str().unwrap().to_string()), ("HERDR_SOCKET_PATH", socket.to_str().unwrap().to_string())];
        vars.extend(extra.iter().map(|(k, v)| (*k, v.to_string())));
        let refs: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (*k, v.as_str())).collect();
        Env::for_test(world.home.path(), &refs)
    }

    fn consume_opened(world: &World, entrypoint: &str) -> Handoff {
        let calls = world.runner.calls.borrow();
        let call = calls.iter().rev().find(|c| c.display().contains("plugin pane open")).unwrap();
        let binding = call.args.windows(2).find(|args| args[0] == "--env").unwrap()[1].clone();
        drop(calls);
        let id = binding.strip_prefix(&format!("{}=", handoff::ENV)).unwrap();
        let env = plugin_env(world, &[(handoff::ENV, id)]);
        handoff::consume(&Ctx { env: &env, ..world.ctx() }, entrypoint).unwrap()
    }

    #[test]
    fn an_action_without_a_project_opens_the_picker_with_the_requested_command() {
        let world = World::new();
        world.project("demo", "a.sock");
        world.runner.on("plugin pane open", ok(r#"{"result":{"type":"ok"}}"#));
        let env = plugin_env(&world, &[("HERDR_WORKSPACE_ID", "w42")]);
        let ctx = Ctx { env: &env, ..world.ctx() };
        run_action(&ctx, "pause").unwrap();
        let calls = world.runner.calls.borrow();
        let opened = calls.iter().find(|c| c.display().contains("plugin pane open")).unwrap();
        assert!(opened.display().contains("--plugin herdr-farm --entrypoint pick"));
        assert!(opened.args.iter().any(|a| a == &format!("HERDR_FARM_ROOT={}", world.root.display())));
        drop(calls);
        assert_eq!(consume_opened(&world, "pick").command, "pause");
    }

    #[test]
    fn an_action_inside_a_project_workspace_acts_on_that_project() {
        let world = World::new();
        let project = world.project("demo", "a.sock");
        let env = plugin_env(&world, &[("HERDR_WORKSPACE_ID", "w1")]);
        let ctx = Ctx { env: &env, ..world.ctx() };
        run_action(&ctx, "pause").unwrap();
        assert_eq!(project.status(), Status::Paused);
        assert_eq!(world.runner.count("plugin pane open"), 0);
        run_action(&ctx, "resume").unwrap();
        assert_eq!(project.status(), Status::Active);
    }

    #[test]
    fn adopt_workspace_captures_the_pane_in_the_action_and_refuses_without_an_agent() {
        let world = World::new();
        world.runner.on("plugin pane open", ok(r#"{"result":{"type":"ok"}}"#));
        let context = r#"{"workspace_id":"w5","workspace_label":"My Repo","workspace_cwd":"/work","focused_pane_id":"w5:p1"}"#;
        let env = plugin_env(&world, &[("HERDR_PLUGIN_CONTEXT_JSON", context)]);
        let ctx = Ctx { env: &env, ..world.ctx() };
        // No agent in the pane: refused before any popup opens.
        assert!(run_action(&ctx, "adopt-workspace").is_err());
        assert_eq!(world.runner.count("plugin pane open"), 0);

        *world.agents.borrow_mut() = format!("[{}]", crate::scenarios::agent_json("w5", "w5:t1", "w5:p1", "/work", "", "idle"));
        run_action(&ctx, "adopt-workspace").unwrap();
        let handoff = consume_opened(&world, "adopt");
        assert_eq!((handoff.pane_id.as_str(), handoff.workspace_label.as_str(), handoff.workspace_cwd.as_str()), ("w5:p1", "My Repo", "/work"));
    }
}
