//! Child side of isolation. `unshare` has already started the new namespaces.
//! The same-namespace check returns before any mount.
use std::{
    ffi::CString,
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Command,
};

use crate::verification::{parse_checks, program_allowed};

const EXIT_SAME_NS: i32 = 71;
const EXIT_SETUP: i32 = 72;
const EXIT_TAMPER: i32 = 73;
const EXIT_LEFTOVER: i32 = 74;
const EXIT_POLICY: i32 = 75;
const EXIT_CHECKS: i32 = 76;

pub fn setup_main() -> i32 {
    setup_from_args(&std::env::args().collect::<Vec<_>>())
}

pub fn setup_from_args(args: &[String]) -> i32 {
    let Some(parsed) = parse_args(args) else {
        return fail("args", 0);
    };
    let current = fs::read_link("/proc/self/ns/mnt").ok();
    let current = current.as_ref().and_then(|path| path.to_str());
    // The caller is not inside the new mount namespace. Do not mount.
    if current == Some(parsed.host_mnt.as_str()) {
        eprintln!("hp-verify same-namespace");
        return EXIT_SAME_NS;
    }
    enter(&parsed)
}

struct Args {
    host_mnt: String,
    checkout: PathBuf,
    policy: PathBuf,
    git: PathBuf,
    checks: Vec<String>,
}

fn parse_args(args: &[String]) -> Option<Args> {
    let start = args.iter().position(|arg| arg == "verification-setup")? + 1;
    let mut host_mnt = None;
    let mut checkout = None;
    let mut policy = None;
    let mut git = None;
    let mut checks = Vec::new();
    let mut index = start;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            checks.extend(args[index + 1..].iter().cloned());
            break;
        }
        let value = args.get(index + 1)?;
        match arg.as_str() {
            "--host-mnt" => host_mnt = Some(value.clone()),
            "--checkout" => checkout = Some(PathBuf::from(value)),
            "--policy" => policy = Some(PathBuf::from(value)),
            "--git" => git = Some(PathBuf::from(value)),
            _ => return None,
        }
        index += 2;
    }
    Some(Args {
        host_mnt: host_mnt?,
        checkout: checkout?,
        policy: policy?,
        git: git?,
        checks,
    })
}

fn enter(parsed: &Args) -> i32 {
    let commit = std::env::var("HP_VERIFY_COMMIT").unwrap_or_default();
    let tree = std::env::var("HP_VERIFY_TREE").unwrap_or_default();
    let policy_digest = std::env::var("HP_VERIFY_POLICY_DIGEST").unwrap_or_default();
    let scratch = std::env::var("HP_VERIFY_SCRATCH").unwrap_or_default();
    if commit.is_empty() || tree.is_empty() || policy_digest.len() != 64 || scratch.is_empty() {
        return fail("env", 0);
    }
    let scratch = PathBuf::from(scratch);
    let libraries = match Command::new("/usr/bin/ldd").arg(&parsed.git).output() {
        Ok(output) if output.status.success() => {
            super::manifest::parse_ldd(&String::from_utf8_lossy(&output.stdout))
        }
        _ => return fail("ldd", 0),
    };
    if libraries.is_empty() {
        return fail("ldd", 0);
    }
    if let Err(errno) = switch_root(&scratch, parsed, &libraries) {
        return fail("root", errno);
    }
    let bytes = match fs::read(&parsed.policy) {
        Ok(bytes) => bytes,
        Err(error) => return fail("policy", error.raw_os_error().unwrap_or(0)),
    };
    if sha256(&bytes) != policy_digest {
        return EXIT_POLICY;
    }
    let checks = match parse_checks(&bytes) {
        Ok(checks) if checks == parsed.checks && program_allowed(&checks[0], &parsed.checkout) => {
            checks
        }
        Ok(_) => return EXIT_CHECKS,
        Err(_) => return EXIT_POLICY,
    };
    let seen_commit = match git_line(&parsed.git, &parsed.checkout, &["rev-parse", "HEAD"]) {
        Some(value) => value,
        None => return fail("rev-parse", 0),
    };
    let seen_tree = match git_line(
        &parsed.git,
        &parsed.checkout,
        &["rev-parse", &format!("{commit}^{{tree}}")],
    ) {
        Some(value) => value,
        None => return fail("rev-parse", 0),
    };
    eprintln!("hp-verify commit={seen_commit}");
    eprintln!("hp-verify tree={seen_tree}");
    if seen_commit != commit || seen_tree != tree {
        return EXIT_TAMPER;
    }
    match git_status(&parsed.git, &parsed.checkout) {
        Some(0) => {}
        Some(1) => return EXIT_TAMPER,
        _ => return fail("diff", 0),
    }
    let output = match Command::new(&checks[0]).args(&checks[1..]).output() {
        Ok(output) => output,
        Err(error) => return fail("exec", error.raw_os_error().unwrap_or(0)),
    };
    let _ = std::io::stdout().write_all(&output.stdout);
    let _ = std::io::stderr().write_all(&output.stderr);
    let code = output.status.code().unwrap_or(72);
    eprintln!("hp-verify checks={code}");
    reap();
    if leftover() {
        return EXIT_LEFTOVER;
    }
    if !(0..71).contains(&code) {
        return EXIT_SETUP;
    }
    code
}

fn git_line(git: &Path, checkout: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new(git)
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-C")
        .arg(checkout)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    if text.is_empty() || text.contains('\n') {
        return None;
    }
    Some(text.to_string())
}

fn git_status(git: &Path, checkout: &Path) -> Option<i32> {
    Command::new(git)
        .args(["-c", "core.hooksPath=/dev/null", "-C"])
        .arg(checkout)
        .args(["diff", "--quiet"])
        .status()
        .ok()?
        .code()
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

fn fail(step: &str, errno: i32) -> i32 {
    eprintln!("hp-verify setup-error={step} errno={errno}");
    EXIT_SETUP
}

fn switch_root(scratch: &Path, parsed: &Args, libraries: &[PathBuf]) -> Result<(), i32> {
    mount_path(
        Some("none"),
        "/",
        None,
        libc::MS_REC | libc::MS_PRIVATE,
        None,
    )?;
    mount_path(
        Some("tmpfs"),
        &scratch.display().to_string(),
        Some("tmpfs"),
        0,
        Some("mode=755"),
    )?;
    for directory in ["old", "dev", "proc", "tmp"] {
        fs::create_dir_all(scratch.join(directory))
            .map_err(|error| error.raw_os_error().unwrap_or(1))?;
    }
    let proc = scratch.join("proc");
    mount_path(
        Some("proc"),
        &proc.display().to_string(),
        Some("proc"),
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        None,
    )?;
    device(scratch, "null", 1, 3, "/dev/null")?;
    device(scratch, "urandom", 1, 9, "/dev/urandom")?;
    bind_ro(scratch, &parsed.git, false)?;
    for library in libraries {
        bind_ro(scratch, library, false)?;
    }
    bind_ro(scratch, &parsed.policy, false)?;
    bind_ro(scratch, &parsed.checkout, true)?;
    let put_old = scratch.join("old");
    pivot(
        &scratch.display().to_string(),
        &put_old.display().to_string(),
    )?;
    if unsafe { libc::chdir(c_str("/")?.as_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(1));
    }
    if unsafe { libc::umount2(c_str("/old")?.as_ptr(), libc::MNT_DETACH) } != 0 {
        return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(1));
    }
    let _ = fs::remove_dir("/old");
    Ok(())
}

fn device(root: &Path, name: &str, major: u32, minor: u32, source: &str) -> Result<(), i32> {
    let dest = root.join("dev").join(name);
    let path = c_str(&dest.display().to_string())?;
    let dev = libc::makedev(major, minor);
    let created = unsafe { libc::mknod(path.as_ptr(), libc::S_IFCHR | 0o666, dev) };
    if created == 0 {
        return Ok(());
    }
    bind_ro(root, Path::new(source), false)
}

fn bind_ro(root: &Path, source: &Path, directory: bool) -> Result<(), i32> {
    let relative = source.strip_prefix("/").map_err(|_| 1)?;
    let dest = root.join(relative);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|error| error.raw_os_error().unwrap_or(1))?;
    }
    if directory {
        fs::create_dir_all(&dest).map_err(|error| error.raw_os_error().unwrap_or(1))?;
    } else {
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o644)
            .open(&dest)
            .map_err(|error| error.raw_os_error().unwrap_or(1))?;
    }
    let source_text = source.display().to_string();
    let dest_text = dest.display().to_string();
    mount_path(Some(&source_text), &dest_text, None, libc::MS_BIND, None)?;
    // Device nodes reject a read-only remount in this user namespace; files do not.
    if source.starts_with("/dev/") {
        return Ok(());
    }
    // This kernel rejects a read-only bind remount unless the implicit nosuid/nodev flags are restated.
    mount_path(
        None,
        &dest_text,
        None,
        libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV,
        None,
    )
}

fn mount_path(
    source: Option<&str>,
    target: &str,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> Result<(), i32> {
    let source = source.map(c_str).transpose()?;
    let target_c = c_str(target)?;
    let fstype = fstype.map(c_str).transpose()?;
    let data = data.map(c_str).transpose()?;
    let rc = unsafe {
        libc::mount(
            source
                .as_ref()
                .map(|value| value.as_ptr())
                .unwrap_or(std::ptr::null()),
            target_c.as_ptr(),
            fstype
                .as_ref()
                .map(|value| value.as_ptr())
                .unwrap_or(std::ptr::null()),
            flags,
            data.as_ref()
                .map(|value| value.as_ptr())
                .unwrap_or(std::ptr::null()) as *const libc::c_void,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(1);
        eprintln!("hp-verify mount-fail target={target} flags={flags:#x} errno={err}");
        Err(err)
    }
}

fn pivot(new_root: &str, put_old: &str) -> Result<(), i32> {
    let new_root = c_str(new_root)?;
    let put_old = c_str(put_old)?;
    let rc = unsafe { libc::syscall(libc::SYS_pivot_root, new_root.as_ptr(), put_old.as_ptr()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(1))
    }
}

fn c_str(text: &str) -> Result<CString, i32> {
    CString::new(text).map_err(|_| 1)
}

fn reap() {
    loop {
        let mut status = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid <= 0 {
            break;
        }
    }
}

fn leftover() -> bool {
    let self_pid = std::process::id();
    let Ok(entries) = fs::read_dir("/proc") else {
        return true;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.chars().all(|character| character.is_ascii_digit()) {
            if name.parse::<u32>().ok() != Some(self_pid) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
#[used]
#[unsafe(link_section = ".init_array")]
static VERIFICATION_SETUP_HOOK: unsafe extern "C" fn() = enter_verification_setup_hook;

#[cfg(test)]
unsafe extern "C" fn enter_verification_setup_hook() {
    if std::env::args().any(|arg| arg == "verification-setup") {
        std::process::exit(setup_from_args(&std::env::args().collect::<Vec<_>>()));
    }
}
