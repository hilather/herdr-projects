//! ssh, scp and rsync to saved machines, and the one quoting helper. No other
//! code builds a string that a shell will parse.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::runner::{Cmd, Output, Runner};

pub const SSH_TIMEOUT: Duration = Duration::from_secs(10);
pub const SSH_START_TIMEOUT: Duration = Duration::from_secs(25);
pub const COPY_TIMEOUT: Duration = Duration::from_secs(60);
const SSH_OPTIONS: [&str; 4] = ["-o", "ConnectTimeout=5", "-o", "BatchMode=yes"];

/// Single-quote escaping: safe for any value in an `sh` command string. Plain
/// words are left bare so printed commands stay readable and stable.
pub fn quote(value: &str) -> String {
    if is_plain(value) {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

/// Only characters that no shell, and neither scp nor rsync in any of their
/// remote-path modes, treat specially.
pub fn is_plain(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':' | '@' | '+' | ','))
}

#[derive(Debug, Clone, Deserialize)]
struct SavedMachine {
    #[serde(default)]
    id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    target: String,
    #[serde(default = "enabled_by_default")]
    enabled: bool,
}

fn enabled_by_default()->bool {true}

/// The SSH target of a saved machine: from `herdr machine list --json`, else
/// `[machines.<label>] ssh` in `config.toml`.
pub fn ssh_target(runner: &dyn Runner, herdr_bin: &str, config_dir: &Path, machine: &str) -> Result<String> {
    let output=runner.run(&Cmd::new(herdr_bin, SSH_TIMEOUT).args(["machine", "list", "--json"])).ok();
    target_from_listing(output,||configured_target(config_dir,machine),machine)
}
pub(crate) fn target_from_listing(output:Option<Output>,fallback:impl FnOnce()->Option<String>,machine:&str)->Result<String> {
    let listed = output
        .filter(Output::success)
        .and_then(|out| serde_json::from_str::<Vec<SavedMachine>>(&out.stdout).ok())
        .unwrap_or_default();
    let mut ids=std::collections::BTreeSet::new();
    anyhow::ensure!(listed.iter().filter(|m|!m.id.is_empty()).all(|m|ids.insert(&m.id)),"duplicate saved machine ID");
    // Match Herdr's selector: an opaque ID takes precedence over labels.
    // Ambiguous labels or disabled profiles must never become fallback authority.
    let by_id:Vec<_>=listed.iter().filter(|m|!m.id.is_empty()&&m.id==machine).collect();
    anyhow::ensure!(by_id.len()<=1,"duplicate saved machine ID");
    let found=if let Some(found)=by_id.first(){Some(*found)}else{
        let labels:Vec<_>=listed.iter().filter(|m|m.label==machine).collect();
        anyhow::ensure!(labels.len()<=1,"ambiguous machine label; use its profile ID");labels.first().copied()
    };
    if let Some(found)=found {
        anyhow::ensure!(found.enabled,"saved machine is disabled");
        anyhow::ensure!(!found.target.is_empty(),"saved machine has no SSH target");
        return Ok(found.target.clone());
    }
    fallback()
        .with_context(|| format!("machine `{machine}` has no SSH target: it is not in `herdr machine list`, and config.toml has no [machines.{machine}] ssh"))
}

fn configured_target(config_dir: &Path, machine: &str) -> Option<String> {
    configured_target_bytes(crate::paths::read_root_config(&config_dir.join("config.toml")).ok()??.as_bytes(),machine)
}
pub(crate) fn configured_target_bytes(bytes:&[u8],machine:&str)->Option<String> {
    #[derive(Deserialize, Default)]
    struct Entry {
        #[serde(default)]
        ssh: String,
    }
    #[derive(Deserialize, Default)]
    struct Config {
        #[serde(default)]
        machines: std::collections::BTreeMap<String, Entry>,
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let mut config: Config = toml::from_str(text).ok()?;
    config.machines.remove(machine).map(|e| e.ssh).filter(|s| !s.is_empty())
}

fn check_target(target: &str) -> Result<()> {
    if let Some(authority)=target.strip_prefix("ssh://") {
        let host=if let Some((user,host))=authority.rsplit_once('@') {
            anyhow::ensure!(!user.is_empty()&&user.bytes().all(|b|b.is_ascii_alphanumeric()||matches!(b,b'.'|b'_'|b'-')),"invalid SSH URI user");host
        }else{authority};
        let port=if let Some(rest)=host.strip_prefix('[') {
            let (ip,suffix)=rest.split_once(']').context("invalid SSH URI IPv6 host")?;ip.parse::<std::net::Ipv6Addr>().context("invalid SSH URI IPv6 host")?;
            if suffix.is_empty(){None}else{Some(suffix.strip_prefix(':').context("invalid SSH URI port")?)}
        }else{
            let (name,port)=host.split_once(':').map_or((host,None),|(name,port)|(name,Some(port)));
            anyhow::ensure!(!name.is_empty()&&!name.starts_with('-')&&name.bytes().all(|b|b.is_ascii_alphanumeric()||matches!(b,b'.'|b'_'|b'-')),"invalid SSH URI host");port
        };
        if let Some(port)=port {anyhow::ensure!(!port.is_empty()&&port.bytes().all(|b|b.is_ascii_digit())&&port.parse::<u16>().is_ok_and(|p|p>0),"invalid SSH URI port");}
        return Ok(());
    }
    // A target is `user@host` or a host alias; it must never look like an option.
    if target.is_empty() || target.starts_with('-') || !target.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '-' | ':' | '[' | ']')) {
        bail!("`{target}` is not a usable SSH target");
    }
    Ok(())
}

/// Runs `script` on the machine with `sh -c`. The script is one argument; every
/// value inside it must already have gone through `quote`.
pub fn ssh(runner: &dyn Runner, target: &str, script: &str, stdin: Option<&str>, timeout: Duration) -> Result<Output> {
    let mut cmd = ssh_command(target, script, timeout)?;
    if let Some(text) = stdin {
        cmd = cmd.stdin(text);
    }
    runner.run(&cmd)
}

pub fn ssh_command(target: &str, script: &str, timeout: Duration) -> Result<Cmd> {
    check_target(target)?;
    Ok(Cmd::new("ssh", timeout).args(SSH_OPTIONS).args(["--", target, &format!("sh -c {}", quote(script))]))
}

/// Origin URL and base ref of a repository on the machine, after a fetch whose
/// failure is not an error. One ssh call.
pub fn repo_info(runner: &dyn Runner, target: &str, repo: &str, base: &str) -> Result<(String, String)> {
    let script = format!(
        "cd {repo} && git rev-parse --show-toplevel >/dev/null || exit 3\n\
         o=$(git remote get-url origin 2>/dev/null || true)\n\
         if [ -n \"$o\" ]; then git fetch origin >/dev/null 2>&1 || true; fi\n\
         b={base}\n\
         if [ -z \"$b\" ]; then b=$(git symbolic-ref --short refs/remotes/origin/HEAD 2>/dev/null || git rev-parse --abbrev-ref HEAD); fi\n\
         if [ \"$b\" = HEAD ]; then b=$(git rev-parse HEAD); fi\n\
         printf '%s\\n%s\\n' \"$o\" \"$b\"",
        repo = quote(repo),
        base = quote(base),
    );
    let out = ssh(runner, target, &script, None, SSH_START_TIMEOUT)?;
    if !out.success() {
        bail!("{repo} on {target} is not a usable git repository: {}", out.error_text());
    }
    let mut lines = out.stdout.lines();
    let origin = lines.next().unwrap_or("").trim().to_string();
    let base = lines.next().unwrap_or("").trim().to_string();
    if base.is_empty() {
        bail!("could not find a base ref in {repo} on {target}");
    }
    Ok((origin, base))
}

/// Creates the thread directory, keeps it out of git, and writes the brief
/// from standard input. One ssh call, so handshakes do not eat the start budget.
pub fn write_brief(runner: &dyn Runner, target: &str, cwd: &str, thread_dir: &str, brief: &str) -> Result<()> {
    let script = format!(
        "set -e\n\
         d={dir}\n\
         mkdir -p \"$d/library\"\n\
         cd {cwd}\n\
         if ex=$(git rev-parse --git-path info/exclude 2>/dev/null); then\n\
           mkdir -p \"$(dirname \"$ex\")\"\n\
           grep -qxF '.herdr-project/' \"$ex\" 2>/dev/null || printf '%s\\n' '.herdr-project/' >> \"$ex\"\n\
         fi\n\
         cat > \"$d/brief.md\"",
        dir = quote(thread_dir),
        cwd = quote(cwd),
    );
    let out = ssh(runner, target, &script, Some(brief), SSH_START_TIMEOUT)?;
    if !out.success() {
        bail!("could not write the brief on {target}: {}", out.error_text());
    }
    Ok(())
}

/// Whether a branch exists in a repository on the machine.
pub fn branch_exists(runner: &dyn Runner, target: &str, repo: &str, branch: &str) -> Result<bool> {
    let script = format!("cd {} && git rev-parse --verify --quiet {} >/dev/null", quote(repo), quote(&format!("refs/heads/{branch}")));
    Ok(ssh(runner, target, &script, None, SSH_TIMEOUT)?.success())
}

/// Report hashes for every given thread on one machine, in one ssh call. Only
/// a regular file inside a real (not symlinked) directory is hashed; anything
/// else yields no hash. `sha256sum`, falling back to `shasum -a 256`.
pub fn report_hashes(runner: &dyn Runner, target: &str, threads: &[(String, String)]) -> Result<std::collections::BTreeMap<String, String>> {
    let mut script = String::from("h() { if command -v sha256sum >/dev/null 2>&1; then sha256sum \"$1\"; else shasum -a 256 \"$1\"; fi | cut -d' ' -f1; }\n");
    for (id, dir) in threads {
        script.push_str(&format!(
            "d={dir}; if [ -d \"$d\" ] && [ ! -L \"$d\" ] && [ -f \"$d/report.md\" ] && [ ! -L \"$d/report.md\" ]; then printf '%s %s\\n' {id} \"$(h \"$d/report.md\")\"; else printf '%s -\\n' {id}; fi\n",
            dir = quote(dir),
            id = quote(id),
        ));
    }
    let out = ssh(runner, target, &script, None, SSH_TIMEOUT)?;
    if !out.success() {
        bail!("ssh {target}: {}", out.error_text());
    }
    Ok(out
        .stdout
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(_, hash)| hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
        .map(|(id, hash)| (id.to_string(), hash.to_string()))
        .collect())
}

/// What the machine says about a thread directory before anything is copied.
#[derive(Debug, Default, PartialEq)]
pub struct RemoteLayout {
    pub absent: bool,
    pub dir_ok: bool,
    pub report_ok: bool,
    pub report_is_other: bool,
    pub library_ok: bool,
    pub library_is_link: bool,
    pub library_kb: u64,
    pub symlinks: Vec<String>,
}

pub fn layout(runner: &dyn Runner, target: &str, thread_dir: &str) -> Result<RemoteLayout> {
    let script = format!(
        "d={dir}\n\
         if [ ! -e \"$d\" ]; then echo absent; exit 0; fi\n\
         if [ -d \"$d\" ] && [ ! -L \"$d\" ]; then echo dir_ok; else exit 0; fi\n\
         if [ -f \"$d/report.md\" ] && [ ! -L \"$d/report.md\" ]; then echo report_ok; elif [ -e \"$d/report.md\" ] || [ -L \"$d/report.md\" ]; then echo report_other; fi\n\
         if [ -L \"$d/library\" ]; then echo library_link; elif [ -d \"$d/library\" ]; then\n\
           size=$(du -sk \"$d/library\") || exit 4\n\
           size=${{size%%[!0-9]*}}\n\
           [ -n \"$size\" ] || exit 4\n\
           echo library_ok; printf 'kb %s\\n' \"$size\"\n\
           find \"$d/library\" -type l -exec printf 'link symbolic-link-in-library\\n' \\; | head -20\n\
         fi",
        dir = quote(thread_dir),
    );
    let out = ssh(runner, target, &script, None, SSH_TIMEOUT)?;
    if !out.success() {
        bail!("ssh {target}: {}", out.error_text());
    }
    let mut found = RemoteLayout::default();
    let mut size_seen = false;
    for line in out.stdout.lines() {
        match line {
            "absent" => found.absent = true,
            "dir_ok" => found.dir_ok = true,
            "report_ok" => found.report_ok = true,
            "report_other" => found.report_is_other = true,
            "library_ok" => found.library_ok = true,
            "library_link" => found.library_is_link = true,
            other => {
                if let Some(kb) = other.strip_prefix("kb ") {
                    found.library_kb = kb.trim().parse().context("remote library size is not a valid integer")?;
                    size_seen = true;
                } else if let Some(link) = other.strip_prefix("link ") {
                    found.symlinks.push(pr_safe(link));
                }
            }
        }
    }
    if found.library_ok && !size_seen { bail!("remote library size is missing; refusing to copy"); }
    Ok(found)
}

/// Remote file names are outside text; keep them printable and short.
fn pr_safe(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(200).collect()
}

/// Copies a remote file through a bounded quoted SSH stream and atomic staging.
pub fn fetch_file(runner: &dyn Runner, target: &str, remote_path: &str, local_path: &Path) -> Result<()> {
    check_target(target)?;
    let parent = local_path.parent().context("file destination has no parent")?;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let staged = parent.join(format!(".fetch-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    if staged.try_exists()? { bail!("stale transfer staging file {}; inspect it before retrying", staged.display()); }
    let mut cmd = ssh_command(target, &format!("test -f {0} && test ! -L {0} && cat -- {0}", quote(remote_path)), COPY_TIMEOUT)?;
    cmd.stdout_file = Some((staged.clone(), 50 * 1024 * 1024));
    let result = (|| {
        let output = runner.run(&cmd)?;
        if !output.success() { bail!("ssh {target} file stream: {}", output.error_text()); }
        std::fs::rename(&staged, local_path)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&staged);
    result?;
    Ok(())
}

/// Checksum-based `rsync -rt` over ssh, without `-l`, so symbolic links are skipped.
pub fn fetch_dir(runner: &dyn Runner, target: &str, remote_dir: &str, local_dir: &Path) -> Result<()> {
    check_target(target)?;
    anyhow::ensure!(!target.starts_with("ssh://"),"[transport-unsupported] legacy rsync cannot represent SSH URI targets");
    if remote_dir.is_empty() || remote_dir.contains('\0') { bail!("remote library path is empty or contains NUL"); }
    for (place, output) in [
        ("local host".to_string(), runner.run(&Cmd::new("rsync", SSH_TIMEOUT).arg("--help"))?),
        (target.to_string(), ssh(runner, target, "rsync --help", None, SSH_TIMEOUT)?),
    ] {
        if !output.success() || !(output.stdout.contains("--secluded-args") || output.stdout.contains("--protect-args")) {
            bail!("[transport-unsupported] rsync on {place} does not confirm protected-argument support; install rsync 3.0 or later on both hosts before retrying");
        }
    }
    // -s still expands wildcard source arguments. Change directory through a
    // quoted shell value instead, and transfer literal ./ through the protocol.
    let directory = if remote_dir.starts_with('/') { remote_dir.to_string() } else { format!("./{remote_dir}") };
    let server = format!("cd {} && rsync", quote(&directory));
    let out = runner.run(&Cmd::new("rsync", COPY_TIMEOUT).args([
        "-rt".to_string(),
        "-s".to_string(),
        "--checksum".to_string(),
        format!("--rsync-path={server}"),
        "-e".to_string(),
        format!("ssh {}", SSH_OPTIONS.join(" ")),
        "--".to_string(),
        format!("{target}:./"),
        format!("{}/", local_dir.to_string_lossy()),
    ]))?;
    if !out.success() {
        bail!("rsync from {target}: {}", out.error_text());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::RealRunner;
    use crate::runner::fake::{FakeRunner, fail, ok};

    #[test]
    fn plain_words_stay_bare() {
        assert_eq!(quote("/Users/me/.dev-root"), "/Users/me/.dev-root");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote("-n"), "'-n'");
    }

    const HOSTILE: [&str; 10] = ["$(touch /tmp/hp-pwned)", "`id`", "a'; rm -rf ~; echo '", "x\ny", "~/x", "-n", "a\\b\"c", "*", "!!", "a b  c"];

    #[test]
    fn hostile_values_survive_a_real_shell_unchanged() {
        for hostile in HOSTILE {
            let out = RealRunner.run(&Cmd::new("sh", Duration::from_secs(5)).args(["-c".to_string(), format!("printf %s {}", quote(hostile))])).unwrap();
            assert_eq!(out.stdout, hostile);
        }
    }

    #[test]
    fn hostile_values_survive_the_double_shell_of_an_ssh_command() {
        // ssh hands its argument to the remote login shell, which runs our
        // `sh -c <quoted script>`: two layers of parsing. `sh -c` stands in for ssh.
        for hostile in HOSTILE {
            let script = format!("printf %s {}", quote(hostile));
            let remote_command = format!("sh -c {}", quote(&script));
            let out = RealRunner.run(&Cmd::new("sh", Duration::from_secs(5)).args(["-c", &remote_command])).unwrap();
            assert_eq!(out.stdout, hostile, "{remote_command}");
        }
    }

    #[test]
    fn the_brief_script_works_against_a_real_repository_with_a_hostile_path() {
        // The same script, run locally through `sh -c` instead of ssh.
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("it's a $(repo)");
        std::fs::create_dir(&repo).unwrap();
        std::process::Command::new("git").arg("-C").arg(&repo).args(["init", "-q"]).output().unwrap();
        let cwd = repo.to_string_lossy().into_owned();
        let dir = format!("{cwd}/.herdr-project/demo-t-0001");
        let runner = FakeRunner::new();
        runner.on_fn(
            |cmd| cmd.program == "ssh",
            |cmd| RealRunner.run(&Cmd { program: "sh".into(), args: vec!["-c".into(), cmd.args.last().unwrap().clone()], ..cmd.clone() }),
        );
        write_brief(&runner, "box", &cwd, &dir, "the brief").unwrap();
        write_brief(&runner, "box", &cwd, &dir, "the brief, again").unwrap();
        assert_eq!(std::fs::read_to_string(format!("{dir}/brief.md")).unwrap(), "the brief, again");
        assert!(Path::new(&format!("{dir}/library")).is_dir());
        let exclude = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches(".herdr-project/").count(), 1);

        let hashes = report_hashes(&runner, "box", &[("t-0001".into(), dir.clone())]).unwrap();
        assert!(hashes.is_empty());
        std::fs::write(format!("{dir}/report.md"), "r").unwrap();
        let hashes = report_hashes(&runner, "box", &[("t-0001".into(), dir.clone())]).unwrap();
        assert_eq!(hashes["t-0001"], crate::thread::sha256_hex(b"r"));

        std::os::unix::fs::symlink("/etc/passwd", format!("{dir}/library/link")).unwrap();
        let found = layout(&runner, "box", &dir).unwrap();
        assert!(found.dir_ok && found.report_ok && found.library_ok && !found.library_is_link);
        assert_eq!(found.symlinks.len(), 1);

        // A symlinked report is never hashed.
        std::fs::remove_file(format!("{dir}/report.md")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", format!("{dir}/report.md")).unwrap();
        assert!(report_hashes(&runner, "box", &[("t-0001".into(), dir.clone())]).unwrap().is_empty());
        assert!(layout(&runner, "box", &dir).unwrap().report_is_other);
    }

    #[test]
    fn ssh_always_uses_batch_mode_a_connect_timeout_and_a_separator() {
        let runner = FakeRunner::new();
        runner.on("ssh", ok(""));
        ssh(&runner, "user@host", "true", None, SSH_TIMEOUT).unwrap();
        let calls = runner.calls.borrow();
        assert_eq!(&calls[0].args[..6], ["-o", "ConnectTimeout=5", "-o", "BatchMode=yes", "--", "user@host"]);
        drop(calls);
        assert!(ssh(&runner, "-oProxyCommand=evil", "true", None, SSH_TIMEOUT).is_err());
        assert!(ssh(&runner, "host; rm -rf ~", "true", None, SSH_TIMEOUT).is_err());
    }

    #[test]
    fn target_comes_from_herdr_then_from_config() {
        let config = tempfile::tempdir().unwrap();
        std::fs::write(config.path().join("config.toml"), "[machines.box]\nssh = \"me@box.local\"\n").unwrap();
        let runner = FakeRunner::new();
        runner.on("machine list --json", ok(r#"[{"id":"abc","label":"m1","target":"m1.local","session":"default"}]"#));
        assert_eq!(ssh_target(&runner, "herdr", config.path(), "m1").unwrap(), "m1.local");
        assert_eq!(ssh_target(&runner, "herdr", config.path(), "abc").unwrap(), "m1.local");
        assert_eq!(ssh_target(&runner, "herdr", config.path(), "box").unwrap(), "me@box.local");
        assert!(ssh_target(&runner, "herdr", config.path(), "nope").is_err());

        let broken = FakeRunner::new();
        broken.on("machine list --json", fail(1, "no"));
        assert_eq!(ssh_target(&broken, "herdr", config.path(), "box").unwrap(), "me@box.local");
    }

    #[test]
    fn quoted_file_transport_preserves_binary_bytes_and_rejects_truncation() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("report");
        for truncated in [false, true] {
            let runner = FakeRunner::new();
            let bytes = vec![0, 255, 254, b'\n'];
            let mut output = ok(&String::from_utf8_lossy(&bytes));
            output.stdout_bytes = bytes.clone();
            output.stdout_truncated = truncated;
            runner.on("ssh", output);
            std::fs::write(&path, b"last good report").unwrap();
            let result = fetch_file(&runner, "box", "/repo with spaces/report.md", &path);
            if truncated {
                assert!(result.is_err());
                assert_eq!(std::fs::read(&path).unwrap(), b"last good report");
            } else {
                result.unwrap();
                assert_eq!(std::fs::read(&path).unwrap(), bytes);
            }
        }
    }

    #[test]
    fn remote_library_transfer_compares_content_without_following_links() {
        let runner = FakeRunner::new();
        runner.on("--help", ok("--protect-args"));
        runner.on("rsync", ok(""));
        let root = tempfile::tempdir().unwrap();
        fetch_dir(&runner, "box", "/repo/library", root.path()).unwrap();
        let calls = runner.calls.borrow();
        let args = &calls.last().unwrap().args;
        assert!(args.iter().any(|arg| arg == "--checksum"));
        assert!(args.iter().any(|arg| arg == "-rt"));
        assert!(!args.iter().any(|arg| ["--links", "--copy-links", "-l", "-L"].contains(&arg.as_str())));
    }

    #[test]
    fn quoted_file_paths_use_ssh_and_unconfirmed_rsync_never_transfers() {
        let runner = FakeRunner::new();
        runner.on("ssh", ok("file body"));
        runner.on("scp", ok(""));
        let dir = tempfile::tempdir().unwrap();
        fetch_file(&runner, "box", "/wt/my repo/report.md", &dir.path().join("r")).unwrap();
        assert_eq!(runner.count("scp"), 0);
        assert_eq!(std::fs::read_to_string(dir.path().join("r")).unwrap(), "file body");
        fetch_file(&runner, "box", "/wt/repo/report.md", &dir.path().join("r2")).unwrap();
        assert_eq!(runner.count("scp"), 0);
        assert!(fetch_dir(&runner, "box", "/wt/my repo/library", dir.path()).is_err());
        assert!(!runner.calls.borrow().iter().any(|c| c.program == "rsync" && c.args.iter().any(|a| a == "-rt")));
    }

    #[test]
    fn protected_transfers_preserve_literal_hostile_paths_through_a_real_shell() {
        use std::os::unix::fs::PermissionsExt;
        struct Loopback { root: std::path::PathBuf, shell: String }
        impl Runner for Loopback {
            fn socket_request(&self, _: &Path, _: &str, _: Duration) -> Result<String> { bail!("unexpected socket request in transport fixture") }
            fn run(&self, cmd: &Cmd) -> Result<Output> {
                if cmd.program == "ssh" {
                    let mut local = Cmd::new("sh", cmd.timeout).args(["-c", cmd.args.last().unwrap()]).cwd(&self.root);
                    local.stdout_file = cmd.stdout_file.clone();
                    return RealRunner.run(&local);
                }
                let mut cmd = cmd.clone();
                if cmd.program == "rsync" && let Some(index) = cmd.args.iter().position(|a| a == "-e") {
                    cmd.args[index + 1] = self.shell.clone();
                }
                cmd.cwd = Some(self.root.clone());
                RealRunner.run(&cmd)
            }
        }
        let root = tempfile::tempdir().unwrap();
        let shell = root.path().join("loopback-ssh");
        std::fs::write(&shell, "#!/bin/sh\n[ \"$1\" = fixture ] || exit 99\nshift\nexec sh -c \"$*\"\n").unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).unwrap();
        let runner = Loopback { root: root.path().into(), shell: shell.to_str().unwrap().into() };
        for (index, name) in ["space dir", "unicode λ", "quote's", "$(touch PWNED)", "`touch PWNED`", "wild*[x]?", "-leading", "line\nbreak"].into_iter().enumerate() {
            let source = root.path().join(name);
            std::fs::create_dir_all(source.join("empty")).unwrap();
            std::fs::write(source.join("report.md"), [0, 255, 254, b'X']).unwrap();
            std::os::unix::fs::symlink("/etc/passwd", source.join("link")).unwrap();
            let destination = root.path().join(format!("result-{index}"));
            std::fs::create_dir(&destination).unwrap();
            fetch_dir(&runner, "fixture", name, &destination).unwrap();
            assert_eq!(std::fs::read(destination.join("report.md")).unwrap(), [0, 255, 254, b'X']);
            assert!(destination.join("empty").is_dir());
            assert!(!destination.join("link").exists());
            let fetched = root.path().join(format!("file-{index}"));
            fetch_file(&runner, "fixture", &format!("{name}/report.md"), &fetched).unwrap();
            assert_eq!(std::fs::read(fetched).unwrap(), [0, 255, 254, b'X']);
        }
        assert!(!root.path().join("PWNED").exists());
        let deceptive = root.path().join("deceptive\nreport_ok");
        std::fs::create_dir_all(deceptive.join("library")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", deceptive.join("library/link\nreport_ok\nkb 0")).unwrap();
        let found = layout(&runner, "fixture", deceptive.to_str().unwrap()).unwrap();
        assert!(found.dir_ok && found.library_ok);
        assert!(!found.report_ok, "filenames must not inject layout protocol fields");
        assert_eq!(found.symlinks.len(), 1);
    }

    #[test]
    fn unknown_or_invalid_remote_library_size_refuses_copy() {
        for reply in ["dir_ok\nlibrary_ok\n", "dir_ok\nlibrary_ok\nkb nonsense\n", "dir_ok\nlibrary_ok\nkb 18446744073709551616\n"] {
            let runner = FakeRunner::new();
            runner.on("ssh", ok(reply));
            assert!(layout(&runner, "box", "/repo/library").is_err());
        }
    }
    #[test]
    fn saved_route_selection_matches_id_precedence_and_refuses_ambiguous_or_disabled_profiles() {
        use crate::runner::fake::ok;
        let listed=ok(r#"[{"id":"chosen","label":"first","target":"correct"},{"id":"other","label":"chosen","target":"wrong"}]"#);
        assert_eq!(target_from_listing(Some(listed),||None,"chosen").unwrap(),"correct");
        for text in [r#"[{"id":"a","label":"same","target":"a"},{"id":"b","label":"same","target":"b"}]"#,r#"[{"id":"a","label":"same","target":"a","enabled":false}]"#,r#"[{"id":"a","label":"same","target":"a"},{"id":"a","label":"different","target":"b"}]"#] {
            assert!(target_from_listing(Some(ok(text)),||panic!("must not fall back"),"same").is_err());
        }
    }

    #[test]
    fn ssh_uri_targets_remain_one_argument_and_refuse_passwords_paths_or_options() {
        let runner=FakeRunner::new();
        assert!(fetch_dir(&runner,"ssh://user@host:2222","/library",Path::new("/tmp/unused")).is_err());
        assert!(runner.calls.borrow().is_empty());
        for target in ["ssh://host", "ssh://user@host:2222", "ssh://[::1]:2222"] {let cmd=ssh_command(target,"true",SSH_TIMEOUT).unwrap();assert_eq!(cmd.args[5],target);}
        for target in ["ssh://", "ssh://-host", "ssh://user:password@host", "ssh://host/path", "ssh://host:0", "ssh://host:65536", "ssh://[bad]:22", "ssh://host?command", "ssh://host:22:23"] {assert!(ssh_command(target,"true",SSH_TIMEOUT).is_err(),"{target}");}
    }

}
