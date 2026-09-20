//! Explicit, conservative orchestration migration. No live resource adoption or dispatch.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fs::{self, File, OpenOptions}, io::{Read, Write}, os::unix::fs::{OpenOptionsExt, PermissionsExt}, path::{Component, Path}};
use crate::{domain::{Task, TaskId, TaskState, Operation}, store::{ImportedSource, SqliteStore}};

const LIMIT: u64 = 16 * 1024 * 1024;
const TOTAL: u64 = 128 * 1024 * 1024;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source { pub path: String, pub digest: String, pub bytes: u64, pub kind: String }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub version: u32,
    pub project: String,
    #[serde(default, skip_serializing_if="Option::is_none")]
    pub config: Option<ConfigReference>,
    pub digest: String,
    pub sources: Vec<Source>,
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub operations: Vec<Operation>,
    pub blockers: Vec<String>,
    pub warnings: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum Phase { Prepared, Imported, Verified, CutoverPending, Active }
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journal { pub version: u32, pub phase: Phase, pub plan: Plan }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Format {
    pub version: u32,
    pub runtime: String,
    pub memory: String,
    pub migration: String,
    pub reconciliation_required: bool,
}

pub(crate) fn hash(bytes: &[u8]) -> String { format!("{:x}",Sha256::digest(bytes)) }
pub(crate) fn safe_relative(path: &str) -> bool {
    !path.is_empty() && Path::new(path).components().all(|p|matches!(p,Component::Normal(_))) && !path.contains('\\')
}
fn checked_project(project: &Path) -> Result<std::path::PathBuf> {
    ensure!(!fs::symlink_metadata(project)?.file_type().is_symlink(),"project path must not be a symlink");
    let project = project.canonicalize()?;
    ensure!(fs::symlink_metadata(project.join(".state"))?.is_dir(),".state must be a real directory");
    Ok(project)
}
pub fn read_plan_file(path: &Path) -> Result<Vec<u8>> { read(path) }
pub(crate) fn read(path: &Path) -> Result<Vec<u8>> {
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(path)?;
    ensure!(file.metadata()?.is_file(),"{} is not a regular file",path.display());
    let mut bytes = Vec::new(); file.take(LIMIT+1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= LIMIT,"{} exceeds 16 MiB",path.display());
    Ok(bytes)
}
fn safe_join(root:&Path,relative:&str)->Result<std::path::PathBuf> {
    ensure!(safe_relative(relative),"unsafe relative path");
    ensure!(fs::symlink_metadata(root)?.is_dir(),"controlled root is not a real directory");
    let mut path=root.to_path_buf();
    for part in Path::new(relative).components() {
        path.push(part);
        match fs::symlink_metadata(&path) {
            Ok(meta)=>ensure!(!meta.file_type().is_symlink(),"symlink in controlled path: {}",path.display()),
            Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{},
            Err(e)=>return Err(e.into()),
        }
    }
    Ok(path)
}
fn exists(path: &Path) -> bool { fs::symlink_metadata(path).is_ok() }
fn inventory(project: &Path, relative: &Path, sources: &mut Vec<Source>, total: &mut u64) -> Result<()> {
    ensure!(relative.components().count()<=64,"source tree exceeds depth 64");
    let mut entries = fs::read_dir(project.join(relative))?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e|e.file_name());
    for entry in entries {
        let rel = relative.join(entry.file_name());
        let name = rel.to_str().context("non-UTF-8 source path cannot be migrated")?;
        ensure!(safe_relative(name),"unsupported source path");
        if name.starts_with(".state/migration-aborted-") { continue; }
        if matches!(name,".state/migration"|".state/projections"|".state/lock"|".state/effect.lock"|".state/format.json"|".state/state.db"|".state/state.db-wal"|".state/state.db-shm") { continue; }
        let meta = entry.file_type()?;
        ensure!(!meta.is_symlink(),"symlink source blocks migration: {name}");
        if meta.is_dir() { inventory(project,&rel,sources,total)?; }
        else {
            let bytes = read(&entry.path()).with_context(||format!("source {name}"))?;
            *total += bytes.len() as u64; ensure!(*total<=TOTAL,"source inventory exceeds 128 MiB");
            ensure!(sources.len()<10_000,"source inventory exceeds 10,000 files");
            let kind = if name=="TASKS.md" {"task"} else if name.starts_with("threads/") && name.ends_with(".toml") {"thread"} else if name.starts_with("inbox/") {"inbox"} else if name.starts_with(".state/") && name.ends_with(".json") {"runtime"} else {"backup"};
            sources.push(Source { path:name.into(),digest:hash(&bytes),bytes:bytes.len() as u64,kind:kind.into() });
        }
    }
    Ok(())
}
fn front(text: &str) -> Result<toml::Value> {
    let rest=text.strip_prefix("+++\n").context("missing TOML front matter")?;
    let (header,_)=rest.split_once("\n+++\n").or_else(||rest.strip_suffix("\n+++").map(|h|(h,""))).context("unclosed TOML front matter")?;
    Ok(toml::from_str(header)?)
}
fn analyze(project: &Path, source: &Source, tasks: &mut Vec<Task>, blockers: &mut Vec<String>, warnings: &mut Vec<String>, ids: &mut BTreeSet<String>) -> Result<()> {
    let bytes=read(&project.join(&source.path))?;
    ensure!(hash(&bytes)==source.digest,"source changed during inspect: {}",source.path);
    let text=std::str::from_utf8(&bytes).context("record is not UTF-8")?;
    match source.kind.as_str() {
        "thread" => {
            let value:toml::Value=toml::from_str(text)?;
            validate_thread(&value)?;
            let id=value.get("id").and_then(|v|v.as_str()).context("missing thread id")?;
            ensure!(Path::new(&source.path).file_stem().and_then(|s|s.to_str())==Some(id),"thread filename/id mismatch");
            ensure!(ids.insert(id.into()),"duplicate thread identity");
            let status=value.get("status").and_then(|v|v.as_str()).context("missing thread status")?;
            ensure!(matches!(status,"starting"|"open"|"failed"|"resolved"),"unknown thread status");
            if matches!(status,"starting"|"open") { blockers.push(format!("{}: active/uncertain execution; quiesce and resolve before migration",source.path)); }
            for field in ["pane_id","machine"] { if value.get(field).and_then(|v|v.as_str()).is_some_and(|s|!s.is_empty()) { blockers.push(format!("{}: {field} identity requires live reconciliation; this importer cannot certify it",source.path)); } }
            let title=value.get("title").and_then(|v|v.as_str()).unwrap_or(id);
            tasks.push(Task { id:TaskId::new(format!("legacy-{id}")).map_err(anyhow::Error::msg)?,revision:1,state:if status=="failed" {TaskState::Failed}else{TaskState::AwaitingReview},title:title.into(),active_attempt:None });
        },
        "runtime" => {
            let value:serde_json::Value=serde_json::from_str(text)?;
            validate_runtime(&source.path,&value)?;
            if source.path==".state/project.json" { ensure!(matches!(value.get("status").and_then(|v|v.as_str()),Some("paused"|"archived")),"pause/archive project before migration"); }
            if source.path==".state/coordinator.json" && value.get("pane_id").and_then(|v|v.as_str()).is_some_and(|s|!s.is_empty()) { blockers.push(".state/coordinator.json: coordinator identity requires live reconciliation; remove no evidence to bypass this check".into()); }
            warnings.push(format!("{}: preserved losslessly; pending obligations require reconciliation before dispatch",source.path));
        },
        "inbox" => { if source.path.ends_with(".md") {
            let value=front(text)?;
            let id=value.get("id").and_then(|v|v.as_str()).filter(|s|!s.is_empty()).context("inbox item has no id")?;
            ensure!(Path::new(&source.path).file_stem().and_then(|s|s.to_str())==Some(id),"inbox filename/id mismatch");
            crate::domain::InboxContent{id:id.into(),..Default::default()}.validate().map_err(anyhow::Error::msg)?;
            ensure!(source.path==format!("inbox/{id}.md")||source.path==format!("inbox/done/{id}.md"),"unsupported inbox record location");
            ensure!(ids.insert(format!("inbox:{id}")),"duplicate inbox identity");
            for field in ["kind","subject","created","summary"] { if let Some(v)=value.get(field) { ensure!(v.is_str(),"invalid inbox string field"); } }
        } },
        "task" => {
            for (line_number,line) in text.lines().enumerate() {
                let line=line.trim_start();
                let (state,title)=if let Some(s)=line.strip_prefix("- [ ] "){(TaskState::Draft,s)}else if let Some(s)=line.strip_prefix("- [x] ").or_else(||line.strip_prefix("- [X] ")){(TaskState::AwaitingReview,s)}else{continue};
                tasks.push(Task{id:TaskId::new(format!("legacy-task-{}",line_number+1)).map_err(anyhow::Error::msg)?,revision:1,state,title:title.into(),active_attempt:None});
            }
            warnings.push("TASKS.md: checkboxes map to draft/awaiting_review; complete original text is retained, never inferred as verified success".into());
        }, _=>{},
    }
    Ok(())
}
/// Read-only inventory and deterministic plan. Diagnoses malformed records without
/// returning their contents (which may contain credentials or untrusted prompts).
pub fn inspect(project: &Path) -> Result<Plan> {
    let project=checked_project(project)?;
    let mut sources=Vec::new(); inventory(&project,Path::new(""),&mut sources,&mut 0)?;
    sources.sort_by(|a,b|a.path.cmp(&b.path));
    let mut blockers=Vec::new(); let mut warnings=Vec::new(); let mut tasks=Vec::new(); let mut ids=BTreeSet::new();
    if !sources.iter().any(|s|s.path==".state/project.json") { blockers.push("legacy status is active by default; pause project first".into()); }
    match read(&project.join("PROJECT.md")).and_then(|b|front(std::str::from_utf8(&b)?)) { Ok(value)=>if validate_settings(&value).is_err(){blockers.push("PROJECT.md: invalid settings types".into());},Err(_)=>blockers.push("PROJECT.md: invalid settings/front matter".into()) }
    for source in &sources {
        if source.kind!="backup" && analyze(&project,source,&mut tasks,&mut blockers,&mut warnings,&mut ids).is_err() { blockers.push(format!("{}: invalid or unsupported record; repair before migration",source.path)); }
    }
    if tasks.len()>10_000 { blockers.push("more than 10,000 imported tasks".into()); }
    let operations=match obligations::convert(&project,&sources,&mut tasks) {
        Ok(operations)=>operations,
        Err(_)=>{blockers.push("legacy records: invalid delivery obligations; repair before migration".into());Vec::new()},
    };
    let digest=hash(&serde_json::to_vec(&sources)?);
    Ok(Plan{version:1,config:None,project:project.to_str().context("non-UTF-8 project path")?.into(),digest,sources,tasks,operations,blockers,warnings})
}
pub(crate) fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file=OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(path)?;
    file.write_all(bytes)?; file.sync_all()?;
    sync_dir(path.parent().context("no parent")?)
}
pub(crate) fn sync_dir(path:&Path)->Result<()> { File::open(path)?.sync_all()?; Ok(()) }
fn mkdir(path:&Path)->Result<()> { fs::create_dir(path)?; fs::set_permissions(path,fs::Permissions::from_mode(0o700))?; sync_dir(path.parent().unwrap()) }
fn journal_path(project:&Path)->std::path::PathBuf { project.join(".state/migration/journal.json") }
fn save(project:&Path,journal:&Journal)->Result<()> {
    let path=journal_path(project); let tmp=path.with_extension("next");
    // A previous interrupted publication leaves a disposable journal.next only.
    if exists(&tmp) { ensure!(fs::symlink_metadata(&tmp)?.is_file(),"invalid journal temporary file"); fs::remove_file(&tmp)?; }
    write_new(&tmp,&serde_json::to_vec_pretty(journal)?)?; fs::rename(tmp,&path)?; sync_dir(path.parent().unwrap())?;
    #[cfg(test)]
    if std::env::var("HP_MIGRATION_CRASH_PHASE").ok().as_deref()==Some(&format!("{:?}",journal.phase)) {
        if let Some(root)=std::env::var_os("HP_MIGRATION_CRASH_ROOT") {
            let root=std::path::PathBuf::from(root);
            if project==root.join("demo") {
                fs::write(root.join("crash-ready"),b"ready")?;
                loop { std::thread::sleep(std::time::Duration::from_secs(1)); }
            }
        }
    }
    Ok(())
}
fn load(project:&Path)->Result<Journal> {
    ensure!(fs::symlink_metadata(project.join(".state/migration"))?.is_dir(),"migration directory must not be a symlink");
    let journal:Journal=serde_json::from_slice(&read(&journal_path(project))?)?;
    ensure!(journal.version==1,"unknown migration journal version");
    ensure!(Path::new(&journal.plan.project)==project,"journal project identity mismatch");
    ensure!(references::plan_digest(&journal.plan)?==journal.plan.digest,"journal inventory digest mismatch");
    ensure!(journal.plan.sources.iter().all(|s|safe_relative(&s.path)),"unsafe journal path");
    Ok(journal)
}
pub(crate) struct Maintenance { _locks:Vec<File>,_project:Option<crate::execution_guard::ProjectGuard>,_root:Option<crate::execution_guard::RootGuard> }
impl Maintenance {
    fn runtime(project:&Path)->Result<Self> {
        let guard=crate::execution_guard::ProjectGuard::acquire(project)?;
        let record=crate::execution_guard::exclusive_file(&project.join(".state/lock"))?;
        Ok(Self{_locks:vec![record],_project:Some(guard),_root:None})
    }
    fn acquire(project:&Path)->Result<Self> {
        let root=project.parent().context("project has no root")?;
        let ticker=crate::execution_guard::exclusive_file(&root.join(".ticker.lock"))?;
        let barrier=crate::execution_guard::RootGuard::exclusive(root)?;
        let record=crate::execution_guard::exclusive_file(&project.join(".state/lock"))?;
        Ok(Self{_locks:vec![record,ticker],_project:None,_root:Some(barrier)})
    }
}
fn backup(project:&Path,plan:&Plan)->Result<()> {
    let dest=project.join(".state/migration/backup");
    if !exists(&dest) { mkdir(&dest)?; }
    for source in &plan.sources {
        let bytes=read(&project.join(&source.path))?; ensure!(hash(&bytes)==source.digest,"source changed before backup: {}",source.path);
        let path=safe_join(&dest,&source.path)?;
        fs::create_dir_all(path.parent().unwrap())?;
        if !exists(&path) { write_new(&path,&bytes)?; }
        ensure!(hash(&read(&path)?)==source.digest,"backup verification failed: {}",source.path);
    }
    sync_dir(&dest)
}
fn verify_sources(project:&Path,plan:&Plan)->Result<()> {
    let current=match &plan.config { Some(config)=>inspect_with_config(project,Path::new(&config.path))?,None=>inspect(project)? }; ensure!(current==*plan,"source or mapping changed since plan; regenerate plan before cutover"); Ok(())
}
fn expected_import(project:&Path,plan:&Plan)->Result<Vec<ImportedSource>> {
    plan.sources.iter().filter(|s|s.kind!="backup").map(|s| {
        let bytes=read(&safe_join(&project.join(".state/migration/backup"),&s.path)?)?;
        ensure!(hash(&bytes)==s.digest,"backup hash mismatch: {}",s.path);
        Ok(ImportedSource { path:s.path.clone(),kind:s.kind.clone(),digest:s.digest.clone(),bytes })
    }).collect()
}
fn verify_db(project:&Path,path:&Path,plan:&Plan)->Result<()> {
    let mut db=SqliteStore::open(path)?; db.integrity_check()?;
    let imported=expected_import(project,plan)?;
    ensure!(db.import_receipt()?==(plan.digest.clone(),imported.len() as u64,plan.tasks.len() as u64),"import counts/digest mismatch");
    ensure!(db.import_operation_count()?==plan.operations.len() as u64,"import operation count mismatch");
    ensure!(db.imported_sources()?==imported,"import source bytes differ from backup");
    let snapshot=db.read_snapshot(None)?;
    let mut tasks=plan.tasks.clone(); tasks.sort_by(|a,b|a.id.cmp(&b.id));
    ensure!(snapshot.tasks==tasks && snapshot.attempts.is_empty() && snapshot.operations==plan.operations,"imported entities differ from plan");
    ensure!(snapshot.head==imported.len() as u64+tasks.len() as u64+plan.operations.len() as u64,"store changed since migration; recovery requires newer-state export");
    Ok(())
}
/// Apply a previously inspected plan. The explicit confirmation covers known
/// manual writers; live/remote runtime identities remain blockers, not waived.
pub fn apply(project:&Path,plan:&Plan,writers_stopped:bool)->Result<Journal> {
    ensure!(writers_stopped,"confirm known writers are stopped with --writers-stopped");
    let project=checked_project(project)?; let _maintenance=Maintenance::acquire(&project)?;
    ensure!(plan.blockers.is_empty(),"migration plan has blockers"); verify_sources(&project,plan)?;
    let storage=storage::inspect(&project.join(".state"),plan.sources.iter().map(|s|s.bytes).sum())?;
    ensure!(storage.supported_type,"filesystem type is unverified for SQLite migration: {}",storage.filesystem);
    ensure!(storage.sufficient_space,"insufficient estimated migration space: need {}, available {}",storage.required_bytes,storage.available_bytes);
    ensure!(!exists(&project.join(".state/state.db")) && !exists(&project.join(".state/format.json")),"existing store/format marker; inspect or recover instead");
    let dir=project.join(".state/migration"); if exists(&dir) {
        use std::os::unix::fs::MetadataExt;
        ensure!(fs::symlink_metadata(&dir)?.dev()==fs::symlink_metadata(project.join(".state"))?.dev(),"staging directory must share the checked .state filesystem");
        ensure!(fs::symlink_metadata(&dir)?.is_dir(),"invalid migration directory");
        ensure!(!exists(&journal_path(&project)),"migration already prepared; use recover");
        ensure!(fs::read_dir(&dir)?.all(|e|e.is_ok_and(|e|e.file_name()=="journal.next")),"unrecognized migration reservation; preserve and inspect it");
    } else { mkdir(&dir)?; }
    let mut journal=Journal{version:1,phase:Phase::Prepared,plan:plan.clone()}; save(&project,&journal)?;
    advance(&project,&mut journal)?; Ok(journal)
}
pub fn recover(project:&Path,writers_stopped:bool)->Result<Journal> {
    ensure!(writers_stopped,"confirm known writers are stopped with --writers-stopped");
    let project=checked_project(project)?; let _maintenance=Maintenance::acquire(&project)?;
    let mut journal=load(&project)?; advance(&project,&mut journal)?; Ok(journal)
}
fn advance(project:&Path,journal:&mut Journal)->Result<()> {
    let staged=project.join(".state/migration/state.db"); let published=project.join(".state/state.db");
    if journal.phase==Phase::Prepared {
        verify_sources(project,&journal.plan)?; backup(project,&journal.plan)?;
        let mut db=if exists(&staged) {
            SqliteStore::open(&staged).context("staged store initialization interrupted or invalid; use migration abort to preserve it and return to legacy mode")?
        } else { SqliteStore::create(&staged)? };
        if !db.has_import()? {
            db.import_legacy_with_operations(&journal.plan.digest,&expected_import(project,&journal.plan)?,&journal.plan.tasks,&journal.plan.operations)?;
        }
        drop(db);
        verify_db(project,&staged,&journal.plan)?;
        journal.phase=Phase::Imported; save(project,journal)?;
    }
    if journal.phase==Phase::Imported {
        verify_db(project,&staged,&journal.plan)?;
        journal.phase=Phase::Verified; save(project,journal)?;
    }
    if journal.phase==Phase::Verified {
        verify_sources(project,&journal.plan)?;
        journal.phase=Phase::CutoverPending; save(project,journal)?;
    }
    if journal.phase==Phase::CutoverPending {
        verify_sources(project,&journal.plan)?;
        ensure!(!(exists(&staged)&&exists(&published)),"both staged and published stores exist; manual inspection required");
        if exists(&staged) {
            verify_db(project,&staged,&journal.plan)?;
            // Closing the last SQLite connection checkpoints its WAL. Verify no
            // sidecar remains before moving the single sealed database file.
            ensure!(!exists(&staged.with_extension("db-wal")),"staged WAL remains; cannot publish database alone");
            fs::rename(&staged,&published)?; sync_dir(&project.join(".state"))?; sync_dir(&project.join(".state/migration"))?;
        }
        verify_db(project,&published,&journal.plan)?;
        let marker=Format{version:1,runtime:"sqlite-v2".into(),memory:"legacy-markdown".into(),migration:journal.plan.digest.clone(),reconciliation_required:true};
        let path=project.join(".state/format.json");
        if exists(&path) { ensure!(serde_json::from_slice::<Format>(&read(&path)?)?==marker,"format marker mismatch"); }
        else {
            let tmp=project.join(".state/migration/format.next");
            if exists(&tmp) { ensure!(fs::symlink_metadata(&tmp)?.is_file(),"invalid marker temporary file"); fs::remove_file(&tmp)?; }
            write_new(&tmp,&serde_json::to_vec_pretty(&marker)?)?;
            fs::rename(&tmp,&path)?; sync_dir(&project.join(".state"))?;
        }
        journal.phase=Phase::Active; save(project,journal)?;
    }
    ensure!(journal.phase==Phase::Active,"unknown recovery state");
    let marker:Format=serde_json::from_slice(&read(&project.join(".state/format.json"))?)?;
    ensure!(marker==Format{version:1,runtime:"sqlite-v2".into(),memory:"legacy-markdown".into(),migration:journal.plan.digest.clone(),reconciliation_required:marker.reconciliation_required},"active ownership marker mismatch");
    // After ownership publication, tasks/events may legitimately have advanced.
    let mut db=open_published(project,false)?;
    ensure!(db.imported_sources()?==expected_import(project,&journal.plan)?,"imported provenance changed");
    publish_control_marker(project,&db)?;
    crate::projections::export(project,&mut db)?;
    Ok(())
}
/// Restore the exact pre-cutover bytes into a NEW recovery directory. Never
/// downgrade the live project or overwrite newer state/worktrees.
pub fn restore_backup(project:&Path,destination:&Path)->Result<()> {
    let project=checked_project(project)?; let _maintenance=Maintenance::acquire(&project)?;
    let journal=load(&project)?;
    ensure!(!exists(destination),"recovery destination already exists"); mkdir(destination)?;
    for source in &journal.plan.sources {
        let bytes=read(&safe_join(&project.join(".state/migration/backup"),&source.path)?)?;
        ensure!(hash(&bytes)==source.digest,"backup corrupt: {}",source.path);
        let path=destination.join(&source.path); fs::create_dir_all(path.parent().unwrap())?; write_new(&path,&bytes)?;
    }
    sync_dir(destination)
}
/// Cancel only before database publication. Retain every backup/staged byte in
/// an archive; original legacy files were never replaced.
pub fn abort(project:&Path)->Result<std::path::PathBuf> {
    let project=checked_project(project)?; let _maintenance=Maintenance::acquire(&project)?;
    let journal=load(&project)?;
    ensure!(matches!(journal.phase,Phase::Prepared|Phase::Imported|Phase::Verified),"cutover started; recover forward or restore to a separate root");
    ensure!(!exists(&project.join(".state/state.db"))&&!exists(&project.join(".state/format.json")),"published state prevents abort");
    let archive=project.join(format!(".state/migration-aborted-{}-{}",&journal.plan.digest[..12],std::process::id()));
    ensure!(!exists(&archive),"abort archive already exists");
    fs::rename(project.join(".state/migration"),&archive)?; sync_dir(&project.join(".state"))?; Ok(archive)
}
pub fn status(project:&Path)->Result<Journal> { let project=checked_project(project)?; load(&project) }

#[cfg(test)]
mod tests;

fn validate_thread(value:&toml::Value)->Result<()> {
    for field in ["id", "title", "error", "repo", "origin", "branch", "base", "machine", "worktree_path", "thread_dir", "workspace_id", "tab_id", "pane_id", "agent", "agent_name", "cwd", "created", "updated", "last_state", "last_state_change", "last_group", "report_hash", "last_report_change", "last_review_item_hash", "acked_report_hash", "pr", "pr_state", "pr_review", "resolved_reason", "suppressed_merged_pr", "last_finalization", "artifact_snapshot"] { if let Some(v)=value.get(field) { ensure!(v.is_str(),"invalid thread string field"); } }
    for field in ["prompt_pending"] { if let Some(v)=value.get(field) { ensure!(v.is_bool(),"invalid thread boolean field"); } }
    for field in ["launch_attempts", "lifecycle_generation", "status_notice_sequence"] { if let Some(v)=value.get(field) { ensure!(v.as_integer().is_some_and(|n|n>=0),"invalid thread integer field"); } }
    if let Some(kind)=value.get("kind") { ensure!(matches!(kind.as_str(),Some("worktree"|"tab"|"adopted")),"invalid thread kind"); }
    if value.get("removal").is_some() { anyhow::bail!("pending/historical removal requires reconciliation before migration"); }
    Ok(())
}

fn validate_runtime(path:&str,value:&serde_json::Value)->Result<()> {
    match path {
        ".state/inbox-counter.json"=>ensure!(value.is_u64(),"invalid inbox counter"),
        ".state/inbox-seen.json"=>ensure!(value.as_array().is_some_and(|a|a.iter().all(|v|v.is_string())),"invalid seen IDs"),
        ".state/project.json"=>ensure!(value.is_object(),"invalid lifecycle record"),
        ".state/coordinator.json"=>{
            ensure!(value.is_object(),"invalid coordinator");
            for field in ["socket","session","workspace_id","tab_id","pane_id","agent_name","cwd","updated"] { if let Some(v)=value.get(field) { ensure!(v.is_string(),"invalid coordinator string field"); } }
            if let Some(v)=value.get("prime_pending") { ensure!(v.is_boolean(),"invalid prime_pending"); }
            if let Some(v)=value.get("launch_attempts") { ensure!(v.as_u64().is_some_and(|n|n<=u32::MAX as u64),"invalid launch_attempts"); }
        },
        ".state/ticker.json"=>{
            ensure!(value.is_object(),"invalid ticker record");
            for (key,v) in value.as_object().unwrap() {
                match key.as_str() {
                    "last_pr_check"|"nudged"=>ensure!(v.is_string(),"invalid ticker string field"),
                    "session_item_written"=>ensure!(v.is_boolean(),"invalid ticker flag"),
                    "event_sequence"=>ensure!(v.is_u64(),"invalid ticker sequence"),
                    "config_errors"=>ensure!(v.as_array().is_some_and(|a|a.iter().all(|v|v.is_string())),"invalid diagnostic hashes"),
                    "prs"|"pr_urls"|"pr_ignored"|"pr_line_noted"|"routines"|"gh_outages"|"machine_outages"=>ensure!(v.as_object().is_some_and(|o|o.is_empty()),"nonempty or invalid ticker obligations require T03.3 conversion"),
                    "pending_events"|"finalizations"|"notification_retry"=>ensure!(v.is_object(),"delivery obligations must be objects"),
                    _=>anyhow::bail!("unsupported ticker field; explicit mapping required"),
                }
            }
        },
        _=>anyhow::bail!("unknown runtime record; explicit mapping required"),
    }
    Ok(())
}

fn validate_settings(value:&toml::Value)->Result<()> {
    for field in ["name","goal","coordinator_agent","thread_agent"] { if let Some(v)=value.get(field) { ensure!(v.is_str(),"invalid settings string field"); } }
    for field in ["max_parallel_threads","auto_resolve_days"] { if let Some(v)=value.get(field) { ensure!(v.as_integer().is_some_and(|n|n>=0 && n<=u32::MAX as i64),"invalid settings integer"); } }
    if let Some(v)=value.get("nudge") { ensure!(v.is_bool(),"invalid nudge flag"); }
    if let Some(v)=value.get("repos") {
        for repo in v.as_array().context("repos must be an array")? {
            ensure!(repo.get("path").is_some_and(|v|v.is_str()),"invalid repository path");
            if let Some(v)=repo.get("machine") { ensure!(v.is_str(),"invalid repository machine"); }
        }
    }
    Ok(())
}

pub(crate) mod obligations;

pub(crate) fn maintenance(project:&Path)->Result<Maintenance> {
    let project=checked_project(project)?; Maintenance::acquire(&project)
}
/// Published runtime mutations serialize with effects, without stopping the ticker.
/// Migration/restore/upgrade still require the stronger maintenance barrier.
pub(crate) fn runtime_mutation(project:&Path)->Result<Maintenance> {
    let project=checked_project(project)?;Maintenance::runtime(&project)
}
/// Validates published authority without requiring tasks to remain at import
/// revisions. Accepted post-cutover edits must never trigger a legacy rollback.
pub fn open_active(project:&Path)->Result<SqliteStore> {open_published(project,true)}
fn open_published(project:&Path,enforce_control:bool)->Result<SqliteStore> {
    let project=checked_project(project)?;
    let journal=load(&project)?;
    ensure!(journal.phase==Phase::Active,"migration has not completed; recover before using the store");
    let marker:Format=serde_json::from_slice(&read(&project.join(".state/format.json"))?)?;
    ensure!(marker==Format{version:1,runtime:"sqlite-v2".into(),memory:"legacy-markdown".into(),migration:journal.plan.digest.clone(),reconciliation_required:marker.reconciliation_required},"active ownership marker mismatch");
    let db=SqliteStore::open(&project.join(".state/state.db"))?;
    ensure!(db.import_operation_count()?==journal.plan.operations.len() as u64,"store imported operation count mismatch");
    let receipt=db.import_receipt()?;
    ensure!(receipt==(journal.plan.digest,journal.plan.sources.iter().filter(|s|s.kind!="backup").count() as u64,journal.plan.tasks.len() as u64),"store import identity mismatch");
    if enforce_control {ensure!(marker.reconciliation_required==db.project_control()?.map(|c|c.reconciliation_required).unwrap_or(true),"control/format publication interrupted; run migration recover before runtime commands");}
    Ok(db)
}

/// Explicitly upgrade a supported published database; dispatch remains frozen.
pub fn upgrade_active(project:&Path)->Result<()> {
    let _maintenance=maintenance(project)?;
    open_active(project)?.upgrade_v1()?;
    Ok(())
}

pub mod storage;

mod references;
pub use references::{ConfigReference, config_reference, inspect_with_config, require_config_path};

/// The DB commit is authoritative. A crash between commit and marker publication
/// blocks ordinary opens; active-journal recovery republishes this derived marker.
pub(crate) fn publish_control_marker(project:&Path,db:&SqliteStore)->Result<()> {
    let journal=load(project)?;
    let expected=Format{version:1,runtime:"sqlite-v2".into(),memory:"legacy-markdown".into(),migration:journal.plan.digest,reconciliation_required:db.project_control()?.map(|c|c.reconciliation_required).unwrap_or(true)};
    let path=project.join(".state/format.json");
    if serde_json::from_slice::<Format>(&read(&path)?)?==expected{return Ok(());}
    let temporary=project.join(".state/migration/control-format.next");
    if exists(&temporary){ensure!(fs::symlink_metadata(&temporary)?.is_file(),"invalid control marker temporary");fs::remove_file(&temporary)?;}
    write_new(&temporary,&serde_json::to_vec_pretty(&expected)?)?;
    fs::rename(temporary,path)?;sync_dir(&project.join(".state"))?;sync_dir(&project.join(".state/migration"))?;Ok(())
}
