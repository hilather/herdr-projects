//! One-use local Git worktree provisioning for already reserved, approved launches.
//! Native launch integration is separate; this service never starts an agent.
use crate::{
    domain::*,
    operations::DeliveryState,
    runner::{Cancellation, Cmd, InheritedLock},
};
use anyhow::{ensure, Context, Result};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant, UNIX_EPOCH},
};

pub(crate) struct Git {
    pub(crate) deadline: Instant,
    pub(crate) cancellation: Cancellation,
    pub(crate) locks: Vec<InheritedLock>,
}
impl Git {
    fn check(&self) -> Result<()> {
        ensure!(
            !self.cancellation.is_cancelled() && Instant::now() < self.deadline,
            "worktree operation cancelled or deadline expired"
        );
        Ok(())
    }
    pub(crate) fn capture(
        &self,
        path: &Path,
        args: &[&str],
        stdin: Option<String>,
        limit: usize,
    ) -> Result<Vec<u8>> {
        self.check()?;
        let mut cmd = Cmd::new("/usr/bin/git", Duration::from_secs(20))
            .args([
                "--no-pager",
                "--no-optional-locks",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "maintenance.auto=false",
                "-c",
                "gc.auto=0",
                "-c",
                "submodule.recurse=false",
                "-c",
                "core.sparseCheckout=false",
                "-c",
                "core.autocrlf=false",
                "-c",
                "core.ignoreStat=false",
                "-c",
                "core.fsync=all",
                "-c",
                "core.fsyncMethod=fsync",
            ])
            .args(args.iter().copied());
        cmd.cwd = Some(path.to_str().context("worktree path is not UTF-8")?.into());
        cmd.env_clear = true;
        cmd.capture_limit = limit;
        cmd.stdin = stdin;
        cmd.env = [
            ("PATH", "/usr/bin:/bin"),
            ("LANG", "C"),
            ("LC_ALL", "C"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_NO_LAZY_FETCH", "1"),
            ("GIT_NO_REPLACE_OBJECTS", "1"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        let output =
            crate::supervision::run(cmd, self.deadline, self.cancellation.clone(), &self.locks)?;
        ensure!(
            output.success() && !output.stdout_truncated && !output.stderr_truncated,
            "worktree Git operation failed or exceeded output bounds (output withheld)"
        );
        self.check()?;
        Ok(output.stdout_bytes)
    }
    fn run(&self, path: &Path, args: &[&str]) -> Result<String> {
        let bytes = self.capture(path, args, None, 8192)?;
        Ok(std::str::from_utf8(&bytes)
            .context("worktree Git output is not UTF-8")?
            .trim_end_matches('\n')
            .into())
    }
    fn source(&self, plan: &WorktreePlan) -> Result<PathBuf> {
        let path = Path::new(&plan.source.repository);
        canonical_directory(path)?;
        let config = self.run(
            path,
            &["config", "--includes", "--list", "--name-only", "--null"],
        )?;
        ensure!(
            !config.split('\0').any(|key| {
                let key = key.to_ascii_lowercase();
                key.starts_with("filter.")
                    || key == "extensions.partialclone"
                    || (key.starts_with("remote.") && key.ends_with(".promisor"))
            }),
            "worktree source requires unsupported filters or partial-clone fetching"
        );
        ensure!(
            self.run(path, &["rev-parse", "--show-toplevel"])? == plan.source.repository,
            "worktree source is not its repository root"
        );
        ensure!(
            self.run(
                path,
                &[
                    "rev-parse",
                    "--verify",
                    &format!("{}^{{tree}}", plan.source.commit)
                ]
            )? == plan.source.tree,
            "approved repository tree changed or is unavailable"
        );
        let common = PathBuf::from(self.run(
            path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?);
        canonical_directory(&common)?;
        Ok(common)
    }
}
fn canonical_directory(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute() && path.canonicalize()? == path && fs::symlink_metadata(path)?.is_dir(),
        "worktree directory is not canonical"
    );
    Ok(())
}
fn identity(path: &Path) -> Result<ResourceIdentity> {
    canonical_directory(path)?;
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.uid() == unsafe { libc::geteuid() },
        "worktree directory has another owner"
    );
    let born = m.created()?.duration_since(UNIX_EPOCH)?;
    Ok(ResourceIdentity {
        device: m.dev(),
        inode: m.ino(),
        born_secs: born.as_secs(),
        born_nanos: born.subsec_nanos(),
    })
}
fn pin(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    ensure!(file.as_raw_fd() >= 0, "worktree directory pin unavailable");
    Ok(file)
}
fn absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
        Ok(_) => anyhow::bail!("worktree creation path already exists"),
    }
}
fn parent_preflight(project: &Path, plans: &[WorktreePlan]) -> Result<PathBuf> {
    let root = project.join(".state/worktrees");
    match fs::symlink_metadata(&root) {
        Ok(_) => {
            identity(&root)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let parent = Path::new(
        &plans
            .first()
            .context("no repository worktrees selected")?
            .path,
    )
    .parent()
    .context("worktree parent missing")?
    .to_owned();
    ensure!(
        parent.parent() == Some(root.as_path())
            && plans
                .iter()
                .all(|p| Path::new(&p.path).parent() == Some(parent.as_path())),
        "worktree plans escaped project state"
    );
    absent(&parent)?;
    Ok(parent)
}
fn open_regular(p: &Path) -> Result<File> {
    // Validate the opened descriptor, not only the pathname. O_NONBLOCK keeps
    // a FIFO replacement from wedging recovery while it holds root ownership.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(p)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file() && metadata.nlink() == 1, "invalid worktree regular file");
    Ok(file)
}
fn read_small(p: &Path) -> Result<String> {
    let mut f = open_regular(p)?;
    let m = f.metadata()?;
    ensure!(
        m.is_file() && m.len() <= 8192 && m.nlink() == 1,
        "invalid worktree metadata file"
    );
    let mut bytes = Vec::new();
    Read::by_ref(&mut f).take(8193).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 8192, "worktree metadata exceeds bounds");
    Ok(String::from_utf8(bytes)?.trim_end_matches('\n').into())
}

struct Observation {
    lock: String,
    receipt: WorktreeReceipt,
    _pins: Vec<File>,
}
impl Observation {
    fn check(&self) -> Result<()> {
        let r = &self.receipt;
        ensure!(
            identity(Path::new(&r.plan.path))? == r.directory
                && identity(Path::new(&r.git_directory))? == r.git_identity
                && identity(Path::new(&r.common_directory))? == r.common_identity,
            "worktree directory replaced during observation"
        );
        let path = Path::new(&r.plan.path);
        let git = Path::new(&r.git_directory);
        ensure!(
            read_small(&path.join(".git"))? == format!("gitdir: {}", git.display())
                && read_small(&git.join("gitdir"))?
                    == path
                        .join(".git")
                        .to_str()
                        .context("invalid worktree path")?
                && git
                    .join(read_small(&git.join("commondir"))?)
                    .canonicalize()?
                    == Path::new(&r.common_directory)
                && git.parent() == Some(Path::new(&r.common_directory).join("worktrees").as_path())
                && read_small(&git.join("locked"))? == self.lock
                && read_small(&git.join("HEAD"))?
                    == format!("ref: refs/heads/{}", r.plan.branch),
            "worktree association changed during observation"
        );
        Ok(())
    }
}
fn observe(git: &Git, intent: &WorktreeCreation, plan: &WorktreePlan) -> Result<Observation> {
    observe_mode(git,intent,plan,true)
}
fn observe_mode(git: &Git, intent: &WorktreeCreation, plan: &WorktreePlan, pristine:bool) -> Result<Observation> {
    let source = git.source(plan)?;
    let common_identity = identity(&source)?;
    let source_pin = pin(&source)?;
    let path = Path::new(&plan.path);
    let directory = identity(path)?;
    let directory_pin = pin(path)?;
    // Explicit directory arguments prevent core.worktree from redirecting any
    // observation to unrelated files. Check the native association both ways.
    let git_directory = PathBuf::from(git.run(path, &["rev-parse", "--absolute-git-dir"])?);
    ensure!(
        git_directory.parent() == Some(source.join("worktrees").as_path()),
        "worktree Git directory belongs to another repository"
    );
    let git_identity = identity(&git_directory)?;
    let git_pin = pin(&git_directory)?;
    ensure!(
        read_small(&path.join(".git"))? == format!("gitdir: {}", git_directory.display())
            && read_small(&git_directory.join("gitdir"))?
                == path
                    .join(".git")
                    .to_str()
                    .context("worktree path is not UTF-8")?,
        "worktree association changed"
    );
    ensure!(
        git_directory
            .join(read_small(&git_directory.join("commondir"))?)
            .canonicalize()?
            == source,
        "worktree common directory changed"
    );
    ensure!(
        read_small(&git_directory.join("locked"))?
            == format!(
                "herdr-projects:{}:{}",
                intent.operation.as_str(),
                intent.token
            ),
        "worktree creation lock does not match intent"
    );
    let prefix = format!("core.worktree={}", path.display());
    ensure!(
        git.run(path, &["-c", &prefix, "symbolic-ref", "HEAD"])?
            == format!("refs/heads/{}", plan.branch),
        "worktree branch changed"
    );
    if pristine {
    ensure!(
        git.run(
            path,
            &["-c", &prefix, "rev-parse", "--verify", "HEAD^{commit}"]
        )? == plan.source.commit,
        "worktree base commit changed"
    );
    ensure!(
        git.run(
            path,
            &[
                "-c",
                &prefix,
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--ignore-submodules=none"
            ]
        )?
        .is_empty(),
        "worktree checkout is incomplete or modified"
    );
    verify_checkout(git, plan)?;
    }
    let result = Observation {
        lock: format!(
            "herdr-projects:{}:{}",
            intent.operation.as_str(),
            intent.token
        ),
        receipt: WorktreeReceipt {
            plan: plan.clone(),
            directory,
            git_directory: git_directory
                .to_str()
                .context("Git directory is not UTF-8")?
                .into(),
            git_identity,
            common_directory: source
                .to_str()
                .context("common directory is not UTF-8")?
                .into(),
            common_identity,
        },
        _pins: vec![source_pin, directory_pin, git_pin],
    };
    result.check()?;
    Ok(result)
}

/// Creates fresh branch/worktree resources exactly once for a reserved launch.
/// A repeat call only observes the retained creation intent. Missing or incomplete
/// resources remain uncertain; this service never retries Git add or deletes data.
pub fn prepare(
    project: &Path,
    operation: &OperationId,
    expected_revision: u64,
    deadline: Instant,
    cancellation: Cancellation,
) -> Result<Vec<WorktreeReceipt>> {
    let deadline = deadline.min(Instant::now() + Duration::from_secs(45));
    let project = project.canonicalize()?;
    let guard = crate::execution_guard::RootGuard::exclusive(
        project.parent().context("project root missing")?,
    )?;
    let mut git = Git {
        deadline,
        cancellation,
        locks: guard.inherit()?,
    };
    git.check()?;
    let mut db = crate::migration::open_active(&project)?;
    let state = db.read_snapshot(None)?;
    let delivery = state
        .deliveries
        .iter()
        .find(|d| &d.operation == operation)
        .context("launch delivery missing")?;
    ensure!(
        delivery.revision == expected_revision,
        "worktree launch revision changed"
    );
    let record = state
        .attempt_inputs
        .iter()
        .find(|r| &r.operation == operation)
        .context("worktree launch inputs missing")?;
    let plans = worktree_plans(&record.inputs, &record.attempt).map_err(anyhow::Error::msg)?;
    ensure!(!plans.is_empty(), "launch has no repository inputs");
    let prior: Vec<_> = state
        .events
        .iter()
        .filter(|e| e.entity == operation.as_str() && e.kind == "runtime.worktrees_creation")
        .collect();
    ensure!(prior.len() <= 1, "duplicate worktree creation intent");
    let intent = if let Some(event) = prior.first() {
        ensure!(
            delivery.attempts == 1
                && matches!(
                    delivery.state,
                    DeliveryState::Claimed | DeliveryState::Ambiguous
                ),
            "worktree observation has no original claim"
        );
        let intent: WorktreeCreation = serde_json::from_value(event.payload.clone())?;
        ensure!(
            intent.plans == plans
                && intent.operation == *operation
                && intent.attempt == record.attempt,
            "worktree creation provenance changed"
        );
        intent
    } else {
        ensure!(
            delivery.state == DeliveryState::Pending && delivery.attempts == 0,
            "worktree creation already claimed without recovery intent"
        );
        let brief =
            crate::memory::render_attempt_brief_held(&project, record.attempt.as_str(), &mut db)?;
        crate::canonical_worker::validate_preparation_inputs(
            record
                .inputs
                .effective_profile
                .as_ref()
                .context("worktree launch profile missing")?,
            &project,
            brief.prompt_chars,
            git.deadline,
            &git.cancellation,
        )?;
        crate::canonical_worker::inventory::check_worktrees(
            &project,
            &record.inputs.binding,
            &plans,
            git.deadline,
            git.cancellation.clone(),
        )?;
        let binding = state
            .runtime_bindings
            .iter()
            .find(|b| b.id == record.inputs.binding)
            .context("worktree binding missing")?;
        let (route, selected) =
            worktree_execution_route(&record.inputs, &record.attempt, &binding.identity)
                .map_err(anyhow::Error::msg)?;
        crate::canonical_worker::validate_server_creation(
            record.inputs.effective_profile.as_ref().context("worktree profile missing")?,
            &route, operation, git.deadline, git.cancellation.clone(), guard.inherit()?,
        )?;
        let parent = parent_preflight(&project, &plans)?;
        let mut common = std::collections::BTreeSet::new();
        for plan in &plans {
            ensure!(
                common.insert(git.source(plan)?),
                "multiple inputs refer to the same Git repository"
            );
            let contents = tree_contents(&git, plan)?;
            if selected.as_ref() == Some(plan) {
                let relative = Path::new(&route.cwd).strip_prefix(&plan.path)?;
                ensure!(relative.as_os_str().is_empty() || contents.iter().any(|entry| entry.path != relative && entry.path.starts_with(relative)), "working directory is absent from the approved repository tree");
            }
            ensure!(
                git.run(
                    Path::new(&plan.source.repository),
                    &[
                        "for-each-ref",
                        "--format=%(refname)",
                        &format!("refs/heads/{}", plan.branch)
                    ]
                )?
                .is_empty(),
                "worktree branch already exists"
            );
            absent(Path::new(&plan.path))?;
        }
        let mut token = [0u8; 32];
        File::open("/dev/urandom")?.read_exact(&mut token)?;
        let intent = WorktreeCreation {
            version: 1,
            operation: operation.clone(),
            attempt: record.attempt.clone(),
            plans,
            token: token.iter().map(|b| format!("{b:02x}")).collect(),
        };
        let claim = db.claim_worktree_creation(
            expected_revision,
            &PreparedWorktreeCreation {
                intent: intent.clone(),
            },
            crate::canonical_worker::now(),
            30_000,
        )?;
        git.deadline = git.deadline.min(
            Instant::now()
                + Duration::from_millis(
                    claim
                        .lease_until_ms
                        .saturating_sub(crate::canonical_worker::now())
                        .max(0) as u64,
                ),
        );
        db.validate_claim(&claim, crate::canonical_worker::now())?;
        let root = parent.parent().unwrap();
        if !root.exists() {
            fs::create_dir(root)?;
            fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
            File::open(root.parent().unwrap())?.sync_all()?;
        }
        identity(root)?;
        fs::create_dir(&parent)?;
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
        File::open(root)?.sync_all()?;
        let _parent_pin = pin(&parent)?;
        let parent_identity = identity(&parent)?;
        let mut marker = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(parent.join("creation.json"))?;
        marker.write_all(&serde_json::to_vec(&intent)?)?;
        marker.sync_all()?;
        File::open(&parent)?.sync_all()?;
        for plan in &intent.plans {
            git.check()?;
            ensure!(
                identity(&parent)? == parent_identity,
                "worktree parent replaced"
            );
            git.source(plan)?;
            absent(Path::new(&plan.path))?;
            db.validate_claim(&claim, crate::canonical_worker::now())?;
            git.run(
                Path::new(&plan.source.repository),
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "--lock",
                    "--reason",
                    &format!("herdr-projects:{}:{}", operation.as_str(), intent.token),
                    "-b",
                    &plan.branch,
                    "--",
                    &plan.path,
                    &plan.source.commit,
                ],
            )?;
        }
        intent
    };
    let observed = intent
        .plans
        .iter()
        .map(|p| observe(&git, &intent, p))
        .collect::<Result<Vec<_>>>()?;
    let receipts = observed
        .iter()
        .map(|o| o.receipt.clone())
        .collect::<Vec<_>>();
    for observation in &observed {
        observation.check()?;
    }
    git.check()?;
    let current = db.read_snapshot(None)?;
    let revision = current
        .deliveries
        .iter()
        .find(|d| &d.operation == operation)
        .context("launch delivery disappeared")?
        .revision;
    db.observe_worktrees(
        &PreparedWorktreeReceipts {
            intent,
            receipts: receipts.clone(),
        },
        revision,
        current.head,
        crate::canonical_worker::now(),
    )?;
    Ok(receipts)
}

// Compare actual filesystem bytes, not Git's stat cache or index flags. This
// deliberately refuses checkout transformations and unmaterialized submodules.
struct TreeContent {
    path: PathBuf,
    mode: String,
    bytes: Vec<u8>,
}
fn tree_contents(git: &Git, plan: &WorktreePlan) -> Result<Vec<TreeContent>> {
    let repo = Path::new(&plan.source.repository);
    let listing = git.capture(
        repo,
        &["ls-tree", "-r", "-z", "--full-tree", &plan.source.commit],
        None,
        16 * 1024 * 1024,
    )?;
    let mut entries = Vec::new();
    let mut input = String::new();
    for entry in listing.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        git.check()?;
        ensure!(
            entries.len() < 100_000,
            "worktree tree inventory exceeds bounds"
        );
        let value = std::str::from_utf8(entry).context("worktree tree has non-UTF-8 paths")?;
        let (header, path) = value.split_once('\t').context("invalid Git tree entry")?;
        let words = header.split(' ').collect::<Vec<_>>();
        ensure!(
            words.len() == 3
                && words[1] == "blob"
                && matches!(words[0], "100644" | "100755" | "120000"),
            "worktree requires unsupported tree entry or submodule"
        );
        ensure!(
            path.len() <= 4096
                && !path.is_empty()
                && Path::new(path)
                    .components()
                    .all(|c| matches!(c,std::path::Component::Normal(s) if s!=".git")),
            "unsafe worktree tree path"
        );
        ensure!(
            matches!(words[2].len(), 40 | 64)
                && words[2]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid tree object identity"
        );
        input.push_str(words[2]);
        input.push('\n');
        entries.push((
            PathBuf::from(path),
            words[0].to_owned(),
            words[2].to_owned(),
        ));
    }
    if entries.is_empty() {
        return Ok(vec![]);
    }
    let objects = git.capture(
        repo,
        &["cat-file", "--batch"],
        Some(input),
        64 * 1024 * 1024,
    )?;
    let mut remaining = objects.as_slice();
    let mut result = Vec::new();
    for (path, mode, oid) in entries {
        git.check()?;
        let end = remaining
            .iter()
            .position(|b| *b == b'\n')
            .context("missing Git object header")?;
        let header = std::str::from_utf8(&remaining[..end])?;
        let words = header.split(' ').collect::<Vec<_>>();
        ensure!(
            words.len() == 3 && words[0] == oid && words[1] == "blob",
            "Git object reply differs from approved tree"
        );
        let size = words[2].parse::<usize>()?;
        remaining = &remaining[end + 1..];
        ensure!(
            size < remaining.len() && remaining[size] == b'\n',
            "Git object size exceeds bounded reply"
        );
        result.push(TreeContent {
            path,
            mode,
            bytes: remaining[..size].to_vec(),
        });
        remaining = &remaining[size + 1..];
    }
    ensure!(remaining.is_empty(), "unexpected Git object output");
    Ok(result)
}
fn verify_checkout(git: &Git, plan: &WorktreePlan) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let contents = tree_contents(git, plan)?;
    let root = Path::new(&plan.path);
    let mut allowed = std::collections::BTreeSet::new();
    allowed.insert(PathBuf::from(".git"));
    for entry in &contents {
        git.check()?;
        let path = root.join(&entry.path);
        canonical_directory(path.parent().unwrap())?;
        let mut relative = entry.path.as_path();
        while !relative.as_os_str().is_empty() {
            allowed.insert(relative.to_owned());
            relative = relative.parent().unwrap();
        }
        let meta = fs::symlink_metadata(&path)?;
        if entry.mode == "120000" {
            ensure!(
                meta.file_type().is_symlink()
                    && fs::read_link(&path)?.as_os_str().as_bytes() == entry.bytes,
                "worktree symlink differs from approved tree"
            );
        } else {
            ensure!(
                meta.is_file()
                    && meta.nlink() == 1
                    && meta.len() == entry.bytes.len() as u64
                    && (meta.mode() & 0o111 != 0) == (entry.mode == "100755"),
                "worktree file differs from approved tree"
            );
            let mut file = open_regular(&path)?;
            let before = file.metadata()?;
            ensure!(
                (before.dev(), before.ino()) == (meta.dev(), meta.ino()),
                "worktree file replaced during observation"
            );
            let mut bytes = Vec::new();
            Read::by_ref(&mut file)
                .take(entry.bytes.len() as u64 + 1)
                .read_to_end(&mut bytes)?;
            let after = file.metadata()?;
            ensure!(
                bytes == entry.bytes
                    && before.len() == after.len()
                    && before.mtime() == after.mtime()
                    && before.mtime_nsec() == after.mtime_nsec()
                    && before.ctime() == after.ctime()
                    && before.ctime_nsec() == after.ctime_nsec(),
                "worktree file content changed"
            );
        }
    }
    let mut pending = vec![PathBuf::new()];
    let mut count = 0;
    while let Some(dir) = pending.pop() {
        for child in fs::read_dir(root.join(&dir))? {
            git.check()?;
            count += 1;
            ensure!(
                count <= 200_000,
                "worktree filesystem inventory exceeds bounds"
            );
            let child = child?;
            let relative = dir.join(child.file_name());
            ensure!(
                allowed.contains(&relative),
                "worktree contains unexpected files"
            );
            if child.file_type()?.is_dir() {
                pending.push(relative);
            }
        }
    }
    Ok(())
}

/// Non-deserializable pins retained by native creation/release across the effect.
pub(crate) struct WorktreeProof {
    observations: Vec<Observation>,
}
impl WorktreeProof {
    pub(crate) fn check(&self) -> Result<()> {
        for observation in &self.observations {
            observation.check()?;
        }
        Ok(())
    }
}
pub(crate) fn verify_held(
    project: &Path,
    state: &Snapshot,
    record: &AttemptInputRecord,
    deadline: Instant,
    cancellation: Cancellation,
    locks: Vec<InheritedLock>,
) -> Result<WorktreeProof> {
    if record.inputs.repositories.is_empty() {
        return Ok(WorktreeProof {
            observations: vec![],
        });
    }
    let mut budget = crate::store::identity_inventory::Budget::new(
        50 * 1024 * 1024,
        1024,
        deadline,
        cancellation.clone(),
    )?;
    let _ = crate::migration::read_worktree_inventory(project, &mut budget)?;
    let event = |kind: &str| -> Result<&Event> {
        let values = state
            .events
            .iter()
            .filter(|e| e.kind == kind && e.entity == record.operation.as_str())
            .collect::<Vec<_>>();
        ensure!(
            values.len() == 1,
            "native launch requires exact worktree provenance"
        );
        Ok(values[0])
    };
    let intent: WorktreeCreation =
        serde_json::from_value(event("runtime.worktrees_creation")?.payload.clone())?;
    let receipts: Vec<WorktreeReceipt> =
        serde_json::from_value(event("runtime.worktrees_ready")?.payload.clone())?;
    ensure!(
        intent.operation == record.operation
            && intent.attempt == record.attempt
            && intent.plans
                == worktree_plans(&record.inputs, &record.attempt).map_err(anyhow::Error::msg)?,
        "native worktree plan changed"
    );
    let git = Git {
        deadline,
        cancellation,
        locks,
    };
    let observations = intent
        .plans
        .iter()
        .map(|p| observe(&git, &intent, p))
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        observations.iter().map(|o| &o.receipt).eq(receipts.iter()),
        "native worktree incarnation changed"
    );
    let proof = WorktreeProof { observations };
    proof.check()?;
    Ok(proof)
}

/// After gate release, validate retained incarnations and associations without
/// requiring pristine content: the worker may already be writing its output.
pub(crate) fn pin_started_held(
    project: &Path,
    state: &Snapshot,
    record: &AttemptInputRecord,
    deadline: Instant,
    cancellation: Cancellation,
) -> Result<WorktreeProof> {
    if record.inputs.repositories.is_empty() {
        return Ok(WorktreeProof {
            observations: vec![],
        });
    }
    let mut budget = crate::store::identity_inventory::Budget::new(
        50 * 1024 * 1024,
        1024,
        deadline,
        cancellation,
    )?;
    crate::migration::read_worktree_inventory(project, &mut budget)?;
    let event = |kind: &str| -> Result<&Event> {
        let events = state
            .events
            .iter()
            .filter(|e| e.kind == kind && e.entity == record.operation.as_str())
            .collect::<Vec<_>>();
        ensure!(
            events.len() == 1,
            "start requires exact worktree provenance"
        );
        Ok(events[0])
    };
    let intent: WorktreeCreation =
        serde_json::from_value(event("runtime.worktrees_creation")?.payload.clone())?;
    let receipts: Vec<WorktreeReceipt> =
        serde_json::from_value(event("runtime.worktrees_ready")?.payload.clone())?;
    ensure!(
        intent.operation == record.operation
            && intent.attempt == record.attempt
            && intent.plans
                == worktree_plans(&record.inputs, &record.attempt).map_err(anyhow::Error::msg)?
            && receipts.iter().map(|r| &r.plan).eq(intent.plans.iter()),
        "start worktree plan changed"
    );
    let observations = receipts
        .into_iter()
        .map(|receipt| -> Result<Observation> {
            let pins = [
                &receipt.plan.path,
                &receipt.git_directory,
                &receipt.common_directory,
            ]
            .iter()
            .map(|p| pin(Path::new(p)))
            .collect::<Result<Vec<_>>>()?;
            let observation = Observation {
                receipt,
                _pins: pins,
                lock: format!(
                    "herdr-projects:{}:{}",
                    intent.operation.as_str(),
                    intent.token
                ),
            };
            observation.check()?;
            Ok(observation)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(WorktreeProof { observations })
}

#[cfg(test)]
mod file_tests {
    use super::*;

    #[test]
    fn fifo_probe_child() {
        let Some(path) = std::env::var_os("HP_WORKTREE_FIFO_PROBE") else { return; };
        assert!(read_small(Path::new(&path)).is_err());
        assert!(open_regular(Path::new(&path)).is_err());
    }

    #[test]
    fn metadata_and_checkout_opens_reject_fifo_without_waiting_for_a_writer() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("metadata");
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: name is a live, NUL-terminated path and mode is a valid mode_t.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "worktree_preparation::file_tests::fifo_probe_child"])
            .env("HP_WORKTREE_FIFO_PROBE", &path)
            .spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("worktree FIFO inspection blocked waiting for a writer");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn metadata_reads_accept_bounded_files_and_reject_indirection_and_special_nodes() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("metadata");
        fs::write(&file, b"retained association\n").unwrap();
        assert_eq!(read_small(&file).unwrap(), "retained association");
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(read_small(&link).is_err());
        assert!(read_small(root.path()).is_err());
        fs::write(&file, vec![b'x'; 8193]).unwrap();
        assert!(read_small(&file).is_err());
        fs::write(&file, b"association").unwrap();
        fs::hard_link(&file, root.path().join("alias")).unwrap();
        assert!(read_small(&file).is_err());
        assert!(open_regular(&file).is_err());
    }
}


/// Recover preparation ownership without certifying that checkout completed.
/// Existing ready receipts still fence the original directory incarnations.
pub(crate) fn pin_preparation_held(git:&Git,state:&Snapshot,intent:&WorktreeCreation)->Result<(WorktreeProof,Vec<WorktreeReceipt>,Vec<WorktreePlan>)> {
    git.check()?;
    let ready=state.events.iter().filter(|e|e.kind=="runtime.worktrees_ready"&&e.entity==intent.operation.as_str()).collect::<Vec<_>>();
    ensure!(ready.len()<=1,"duplicate ready worktree evidence");
    let retained:Option<Vec<WorktreeReceipt>>=ready.first().map(|e|serde_json::from_value(e.payload.clone())).transpose()?;
    let mut observations=Vec::new();let mut missing=Vec::new();
    for plan in &intent.plans {
        match fs::symlink_metadata(&plan.path) {
            Err(error) if error.kind()==std::io::ErrorKind::NotFound=>{
                ensure!(retained.is_none(),"recorded worktree disappeared; preservation unresolved");
                verify_uncreated(git,plan)?;missing.push(plan.clone());
            },
            Err(error)=>return Err(error.into()),
            Ok(_)=>observations.push(observe_mode(git,intent,plan,false)?),
        }
    }
    let receipts=observations.iter().map(|o|o.receipt.clone()).collect::<Vec<_>>();
    if let Some(retained)=retained {ensure!(retained==receipts,"prepared worktree incarnation changed");}
    let proof=WorktreeProof{observations};proof.check()?;Ok((proof,receipts,missing))
}
/// A missing path is uncreated only if Git also retains neither its branch nor
/// registration. Ambiguous partial metadata stays owned and blocks retirement.
pub(crate) fn verify_uncreated(git:&Git,plan:&WorktreePlan)->Result<()> {
    git.check()?;let mut parent=Path::new(&plan.path).parent().context("worktree parent missing")?;
    loop {
        match fs::symlink_metadata(parent) {
            Ok(_)=>{canonical_directory(parent)?;break;},
            Err(e) if e.kind()==std::io::ErrorKind::NotFound=>parent=parent.parent().context("worktree ancestor missing")?,
            Err(e)=>return Err(e.into()),
        }
    }
    absent(Path::new(&plan.path))?;
    git.source(plan)?;
    ensure!(git.run(Path::new(&plan.source.repository),&["for-each-ref","--format=%(refname)",&format!("refs/heads/{}",plan.branch)])?.is_empty(),"missing worktree has a retained branch; preservation unresolved");
    let listing=git.capture(Path::new(&plan.source.repository),&["worktree","list","--porcelain","-z"],None,4*1024*1024)?;
    for field in listing.split(|b|*b==0) {
        ensure!(field!=format!("worktree {}",plan.path).as_bytes()&&field!=format!("branch refs/heads/{}",plan.branch).as_bytes(),"missing worktree has retained Git registration; preservation unresolved");
    }
    absent(Path::new(&plan.path))?;git.check()
}
