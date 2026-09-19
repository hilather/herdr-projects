//! Single-use popup context, bound to an entrypoint, root and Herdr socket.
use super::*;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;

pub const ENV: &str = "HERDR_PROJECTS_HANDOFF";
const TTL: i64 = 600;
const LIMIT: u64 = 64 * 1024;

#[derive(Serialize, Deserialize)]
struct Envelope {
    schema: u32,
    id: String,
    entrypoint: String,
    root: PathBuf,
    expires: i64,
    handoff: Handoff,
}

fn directory(ctx: &Ctx) -> Result<PathBuf> {
    let base = ctx.env.var("HERDR_PLUGIN_STATE_DIR").context("HERDR_PLUGIN_STATE_DIR is not set")?;
    let path = PathBuf::from(base).join("handoffs");
    fs::create_dir_all(&path)?;
    anyhow::ensure!(fs::symlink_metadata(&path)?.is_dir(), "handoff directory is not a real directory");
    Ok(path)
}

fn root(ctx: &Ctx) -> Result<PathBuf> { Ok(std::path::absolute(&ctx.root)?) }

fn read(path: &std::path::Path) -> Result<Envelope> {
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    anyhow::ensure!(file.metadata()?.is_file(), "handoff is not a regular file");
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() as u64 <= LIMIT, "handoff exceeds 64 KiB");
    Ok(serde_json::from_slice(&bytes)?)
}

pub(super) fn create(ctx: &Ctx, entrypoint: &str, handoff: &Handoff) -> Result<String> {
    let dir = directory(ctx)?;
    let now = jiff::Timestamp::now().as_second();
    // Only remove expired files in this versioned protocol; never guess at
    // malformed or unrelated state. Consumption also removes successful files.
    for entry in fs::read_dir(&dir)?.take(1024) {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "json" || e == "consumed")
            && let Ok(saved) = read(&path)
            && saved.schema == 1 && saved.expires <= now
        { let _ = fs::remove_file(path); }
    }
    let mut random = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let id: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let envelope = Envelope { schema: 1, id: id.clone(), entrypoint: entrypoint.into(), root: root(ctx)?, expires: now + TTL, handoff: handoff.clone() };
    let bytes = serde_json::to_vec(&envelope)?;
    anyhow::ensure!(bytes.len() as u64 <= LIMIT, "handoff exceeds 64 KiB");
    let mut file = OpenOptions::new().create_new(true).write(true).mode(0o600).open(dir.join(format!("{id}.json")))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(id)
}

pub(super) fn consume(ctx: &Ctx, entrypoint: &str) -> Result<Handoff> {
    let id = ctx.env.var(ENV).context("popup has no handoff ID; reopen it from the Herdr action menu")?;
    anyhow::ensure!(id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()), "invalid popup handoff ID");
    let dir = directory(ctx)?;
    let source = dir.join(format!("{id}.json"));
    // Atomic claim. Exactly one consumer can remove the original name. A bad
    // action/schema/context consumes the handoff too, rather than replaying it.
    let claimed = dir.join(format!("{id}.consumed"));
    fs::rename(&source, &claimed).context("popup handoff is missing or already consumed; reopen the action")?;
    let result = (|| -> Result<Handoff> {
        let envelope = read(&claimed)?;
        anyhow::ensure!(envelope.schema == 1 && envelope.id == id, "unsupported popup handoff schema or identity");
        anyhow::ensure!(envelope.entrypoint == entrypoint, "popup handoff belongs to a different action");
        let now = jiff::Timestamp::now().as_second();
        anyhow::ensure!(envelope.expires > now && envelope.expires <= now + TTL, "popup handoff expired or has an invalid lifetime");
        anyhow::ensure!(envelope.root == root(ctx)? && envelope.handoff.socket == socket(ctx)?, "popup handoff belongs to a different root or session");
        Ok(envelope.handoff)
    })();
    let _ = fs::remove_file(claimed);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenarios::World;
    use crate::paths::Env;

    fn env(world: &World, id: &str) -> Env {
        let path = world.home.path().join("state");
        Env::for_test(world.home.path(), &[("HERDR_PLUGIN_STATE_DIR", path.to_str().unwrap()), ("HERDR_SOCKET_PATH", "/a.sock"), (ENV, id)])
    }

    #[test]
    fn concurrent_actions_keep_separate_context_and_cannot_replay() {
        let world = World::new();
        let initial = env(&world, "");
        let ctx = Ctx { env: &initial, ..world.ctx() };
        let one = create(&ctx, "pick", &Handoff { command: "pause".into(), socket: "/a.sock".into(), ..Default::default() }).unwrap();
        let two = create(&ctx, "adopt", &Handoff { pane_id: "origin:pane".into(), socket: "/a.sock".into(), ..Default::default() }).unwrap();
        assert_ne!(one, two);
        for (id, action) in [(&two, "adopt"), (&one, "pick")] {
            let env = env(&world, id);
            let ctx = Ctx { env: &env, ..world.ctx() };
            let found = consume(&ctx, action).unwrap();
            if action == "pick" { assert_eq!(found.command, "pause"); } else { assert_eq!(found.pane_id, "origin:pane"); }
            assert!(consume(&ctx, action).is_err());
        }
    }

    #[test]
    fn two_consumers_can_claim_the_same_handoff_only_once() {
        let world = World::new();
        let initial = env(&world, "");
        let ctx = Ctx { env: &initial, ..world.ctx() };
        let id = create(&ctx, "pick", &Handoff { socket: "/a.sock".into(), ..Default::default() }).unwrap();
        let env = env(&world, &id);
        let mut workers = Vec::new();
        for _ in 0..2 {
            let env = env.clone();
            let root = world.root.clone();
            let config_dir = world.home.path().join("cfg");
            workers.push(std::thread::spawn(move || {
                let runner = crate::runner::fake::FakeRunner::new();
                let ctx = Ctx { env: &env, root, config_dir, runner: &runner, detached_ticker: false };
                consume(&ctx, "pick").is_ok()
            }));
        }
        assert_eq!(workers.into_iter().map(|worker| usize::from(worker.join().unwrap())).sum::<usize>(), 1);
    }

    #[test]
    fn wrong_action_schema_expiry_session_and_root_are_refused() {
        let world = World::new();
        for invalid in ["action", "schema", "expiry", "session", "root"] {
            let initial = env(&world, "");
            let ctx = Ctx { env: &initial, ..world.ctx() };
            let id = create(&ctx, "pick", &Handoff { socket: "/a.sock".into(), ..Default::default() }).unwrap();
            let path = directory(&ctx).unwrap().join(format!("{id}.json"));
            let mut data = read(&path).unwrap();
            match invalid {
                "action" => data.entrypoint = "adopt".into(),
                "schema" => data.schema = 42,
                "expiry" => data.expires = 0,
                "session" => data.handoff.socket = "/b.sock".into(),
                "root" => data.root = "/other".into(),
                _ => unreachable!(),
            }
            fs::write(path, serde_json::to_vec(&data).unwrap()).unwrap();
            let env = env(&world, &id);
            let ctx = Ctx { env: &env, ..world.ctx() };
            assert!(consume(&ctx, "pick").is_err(), "{invalid}");
            assert!(consume(&ctx, "pick").is_err(), "failed validation must not allow replay");
        }
    }
}
