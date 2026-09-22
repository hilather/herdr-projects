use super::*;
use crate::{memory::MemoryStore, migration, operations::DeliveryState, runtime};
use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::PathBuf,
};

struct Fixture {
    _root: tempfile::TempDir,
    project: PathBuf,
    operation: Operation,
    _socket: UnixListener,
    _worker: Worker,
}
struct Worker(std::process::Child);

// Simulate persisted pre-reboot state only after the real fixture worker has
// stopped. This exercises recovery without rebooting the developer's machine.
fn previous_boot_fixture(f:&Fixture,started:bool)->String {
    let raw=rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
    let kind=if started {"runtime.launch_started"} else {"runtime.launch_target"};
    let payload:String=raw.query_row("SELECT payload FROM events WHERE kind=?1 AND entity=?2",rusqlite::params![kind,f.operation.id.as_str()],|r|r.get(0)).unwrap();
    let mut value:Value=serde_json::from_str(&payload).unwrap();
    let supervisor:crate::worker_supervision::SupervisorIdentity=serde_json::from_value(value["supervisor"].clone()).unwrap();
    assert!(crate::worker_supervision::SupervisorObservation::recover_exited(&supervisor).unwrap());
    let previous="00000000-0000-0000-0000-000000000001";
    assert_ne!(supervisor.boot_id,previous);
    value["supervisor"]["boot_id"]=json!(previous);
    let payload=if started {serde_json::to_string(&serde_json::from_value::<LaunchStartedReceipt>(value).unwrap()).unwrap()}
        else {serde_json::to_string(&serde_json::from_value::<LaunchTarget>(value).unwrap()).unwrap()};
    raw.execute("UPDATE events SET payload=?1 WHERE kind=?2 AND entity=?3",rusqlite::params![payload,kind,f.operation.id.as_str()]).unwrap();
    if started {
        let outcome=crate::operations::Outcome::Confirmed{observed_identity:payload};
        raw.execute("UPDATE operation_delivery SET last_outcome=?1 WHERE operation_id=?2",rusqlite::params![serde_json::to_string(&outcome).unwrap(),f.operation.id.as_str()]).unwrap();
    }
    previous.into()
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl Fixture {
    fn new(mode: &str) -> Self {
        Self::with_handoff(mode, |_, _| {})
    }
    fn with_handoff(mode: &str, handoff: impl FnOnce(&Path, &AttemptId)) -> Self {
        Self::with_native(mode, handoff, None)
    }
    fn with_native(mode: &str, handoff: impl FnOnce(&Path, &AttemptId), native: Option<(&Path, &Path)>) -> Self {
        Self::with_project(mode,handoff,native,None,None)
    }
    fn with_project(mode: &str, handoff: impl FnOnce(&Path, &AttemptId), native: Option<(&Path, &Path)>,project_path:Option<&Path>,signer:Option<&Path>) -> Self {
        let live = native.is_some() && signer.is_some();
        let root = tempfile::tempdir().unwrap();
        let project = project_path.map(Path::to_owned).unwrap_or_else(||root.path().join("project"));
        fs::create_dir(&project).unwrap();
        for name in [".state", "threads", "inbox"] {
            fs::create_dir(project.join(name)).unwrap();
        }
        fs::write(
            project.join("PROJECT.md"),
            "+++\nname='Worker fixture'\n+++\nOriginal instructions",
        )
        .unwrap();
        fs::write(project.join("TASKS.md"), "").unwrap();
        fs::write(project.join("MEMORY.md"), "").unwrap();
        fs::write(
            project.join(".state/project.json"),
            r#"{"status":"paused"}"#,
        )
        .unwrap();
        let config_path = root.path().join("config.toml");
        if mode.starts_with("resource") {
            fs::write(&config_path,"[profiles.fixture]\nkind='claude'\npermission_policy='interactive'\nextra_args=['30']\n[profiles.fixture.budget]\nmax_wall_seconds=20\nunknown_usage='allow_with_warning'\n").unwrap();
        }
        if native.is_some() {
            let config = fs::read_to_string(&config_path).unwrap().replace("max_wall_seconds=20", "max_wall_seconds=60");
            fs::write(&config_path, config).unwrap();
        }
        if mode == "resource-small-input" {
            let mut config = fs::read_to_string(&config_path).unwrap();
            config.push_str("soft_input_tokens=1\n");
            fs::write(&config_path, config).unwrap();
        }
        if mode == "resource-block-usage" {
            let config = fs::read_to_string(&config_path)
                .unwrap()
                .replace("allow_with_warning", "block");
            fs::write(&config_path, config).unwrap();
        }
        if let Some(key)=signer {
            let public=fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
            let mut config=fs::read_to_string(&config_path).unwrap().replace("extra_args=['30']","extra_args=[]");
            config.push_str(&format!("\n[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n"));
            fs::write(&config_path,config).unwrap();
        }
        if live {
            let config=fs::read_to_string(&config_path).unwrap().replace("kind='claude'","kind='codex'").replace("max_wall_seconds=60","max_wall_seconds=300");
            fs::write(&config_path,config).unwrap();
        }
        let plan = if signer.is_some(){migration::inspect_with_config(&project,&config_path).unwrap()}else{migration::inspect(&project).unwrap()};
        migration::apply(&project, &plan, true).unwrap();
        let socket_path = root.path().join("herdr.sock");
        let socket = UnixListener::bind(&socket_path).unwrap();
        let socket_path = native.map_or(socket_path, |(_, socket)| socket.to_path_buf());
        let helper = root.path().join("herdr-fixture");
        let script = format!(
            r#"#!/usr/bin/python3
import json,sys,pathlib,time
if sys.argv[1:]==['--version']:print('herdr 0.9.1');sys.exit(0)
root=pathlib.Path({root:?});project=pathlib.Path({project:?});mode={mode:?}
r=json.loads(sys.stdin.readline())
a=json.loads((root/'agent.json').read_text())
if r['method']=='ping':
 result={{'type':'pong','version':'0.9.1','capabilities':{{'workspace_create_command':True}}}}
 if (root/'server-capability.json').exists():result=json.loads((root/'server-capability.json').read_text())
elif r['method']=='agent.list':
 if mode=='busy':a['agent_status']='working'
 if mode=='foreign':a['terminal_id']='foreign'
 result={{'type':'agent_list','agents':[a]}}
elif r['method']=='agent.explain':
 e={{'agent':'claude','state':'idle','manifest_source':'bundled','manifest_version':'2026.09.14.1',
 'matched_rule':{{'id':'prompt','state':'idle'}},'visible_idle':True,'visible_blocker':False,
 'visible_working':False,'screen_detection_skipped':False,'skip_state_update':False,
 'local_override_shadowing_remote':False,'fallback_reason':None,'warning':None}}
 count_file=root/'readiness-reads'
 count=int(count_file.read_text())+1 if count_file.exists() else 1
 count_file.write_text(str(count))
 if count>1 and (root/'lose-readiness').exists():e['visible_idle']=False
 if (root/'readiness.json').exists():e.update(json.loads((root/'readiness.json').read_text()))
 result={{'type':'agent_explain','explain':e}}
elif r['method']=='agent.rename':
 with open(root/'name-requests','a') as f:f.write(json.dumps(r)+'\n')
 a['name']=r['params']['name']
 (root/'agent.json').write_text(json.dumps(a))
 if (root/'lose-name-reply').exists():sys.exit(1)
 result={{'type':'agent_info','agent':a}}
elif r['method']=='workspace.list':
 requests=root/'workspace-requests'
 rows=[]
 if requests.exists():
  request=json.loads(requests.read_text().splitlines()[0])
  rows=[{{'workspace_id':'w1','label':request['params']['label'],'pane_count':1,'tab_count':1}}]
 if (root/'duplicate-workspaces').exists():rows=rows*2
 result={{'type':'workspace_list','workspaces':rows}}
elif r['method']=='workspace.create_command':
 with open(root/'workspace-requests','a') as f:f.write(json.dumps(r)+'\n')
 if (root/'reject-workspace-command').exists():sys.exit(2)
 (root/'direct-created').write_text('created')
 if (root/'hold-creation-reply').exists():
  until=time.monotonic()+10
  while not (root/'continue-creation-reply').exists():
   if time.monotonic()>until:sys.exit(4)
   time.sleep(0.01)
 if (root/'lose-workspace-reply').exists():sys.exit(1)
 result={{'type':'workspace_created','workspace':{{'workspace_id':'w1'}},'root_pane':{{'pane_id':'w1:p1'}}}}
elif r['method']=='workspace.create':
 with open(root/'workspace-requests','a') as f:f.write(json.dumps(r)+'\n')
 if (root/'lose-workspace-reply').exists():sys.exit(1)
 result={{'type':'workspace_created','workspace':{{'workspace_id':'w1'}},'root_pane':{{'pane_id':'w1:p0'}}}}
elif r['method']=='layout.apply':
 with open(root/'created','a') as f:f.write('created\\n')
 if (root/'hold-creation-reply').exists():
  until=time.monotonic()+10
  while not (root/'continue-creation-reply').exists():
   if time.monotonic()>until:sys.exit(4)
   time.sleep(0.01)
 if mode=='resource-lost' or (root/'lose-creation-reply').exists():sys.exit(1)
 result={{'layout':{{'workspace_id':'w1','tab_id':'w1:t1','focused_pane_id':'w1:p1'}}}}
elif r['method']=='pane.list':
 result={{'panes':[{{'pane_id':'w1:p1','workspace_id':'w1'}}]*(2 if (root/'duplicate').exists() else 1)}}
 if 'workspace' in mode and not (root/'created').exists() and not (root/'direct-created').exists():result['panes']=[{{'pane_id':'w1:p0','workspace_id':'w1'}}]
 if (root/'other-pane').exists():result['panes'].append({{'pane_id':'w1:p2','workspace_id':'w1'}})
elif r['method']=='pane.get':
 count_file=root/'pane-reads'
 count=int(count_file.read_text())+1 if count_file.exists() else 1
 count_file.write_text(str(count))
 if count==2 and (root/'exit-after-observation').exists():
  (root/'exit-request').write_text('ready')
  deadline=time.monotonic()+3
  while not (root/'exit-done').exists():
   if time.monotonic()>deadline:sys.exit(3)
   time.sleep(0.01)
 result={{'pane':{{'workspace_id':'w1','tab_id':'w1:t1','pane_id':'w1:p1','terminal_id':'term1','cwd':str(project)}}}}
 if 'repository' in mode and (root/'workspace-requests').exists():result['pane']['cwd']=json.loads((root/'workspace-requests').read_text().splitlines()[0])['params']['cwd']
 if count>=2 and (root/'replace-terminal').exists():result['pane']['terminal_id']='foreign-terminal'
 if r['params']['pane_id']=='w1:p0':result['pane'].update({{'pane_id':'w1:p0','tab_id':'w1:t0','terminal_id':'term0'}})
 if r['params']['pane_id']=='w1:p2':result['pane'].update({{'pane_id':'w1:p2','tab_id':'w1:t2','terminal_id':'term2','cwd':None}})
elif r['method']=='pane.process_info':
 stage=json.loads((root/'stage.json').read_text())
 result={{'process_info':{{'pane_id':'w1:p1','foreground_processes':[stage]}}}}
 if (root/'no-processes').exists():result['process_info'].pop('foreground_processes')
 if r['params']['pane_id']=='w1:p0':result['process_info']={{'pane_id':'w1:p0','foreground_processes':[json.loads((root/'bootstrap.json').read_text())] if (root/'bootstrap.json').exists() else []}}
 if r['params']['pane_id']=='w1:p2':result['process_info']={{'pane_id':'w1:p2'}}
elif r['method']=='pane.send_input':
 request_path=root/'gate-requests'
 previous=request_path.read_text() if request_path.exists() else ''
 pending=root/'gate-requests.pending'
 pending.write_text(previous+json.dumps(r)+'\n')
 pending.replace(request_path)
 deadline=time.monotonic()+3
 while not (root/'gate-sent').exists():
  if time.monotonic()>deadline:sys.exit(3)
  time.sleep(0.01)
 if mode=='resource-release-lost':sys.exit(1)
 result={{'type':'ok'}}
elif r['method']=='agent.prompt':
 with open(root/'sent','a') as f:f.write(json.dumps(r)+'\n')
 if mode=='lost':sys.exit(1)
 if mode=='stall':time.sleep(10)
 if mode=='wrong-terminal':a['terminal_id']='foreign'
 result={{'type':'agent_prompted','agent':a}}
else:sys.exit(2)
print(json.dumps({{'id':('wrong' if mode=='wrong-id' and r['method']=='agent.prompt' else r['id']),'result':result}}))
"#,
            root = root.path().display().to_string(),project=project.display().to_string()
        );
        fs::write(&helper, script).unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        let time = now();
        let mut db = migration::open_active(&project).unwrap();
        let task = TaskId::new("worker").unwrap();
        let head = db.read_snapshot(None).unwrap().head;
        db.commit(Commit {
            expected_head: head,
            mutations: vec![Mutation::Task {
                expected: None,
                next: Task {
                    id: task.clone(),
                    revision: 1,
                    state: TaskState::Draft,
                    title: "Retained task".into(),
                    active_attempt: None,
                },
            }],
        })
        .unwrap();
        let head = db.read_snapshot(None).unwrap().head;
        let route = RuntimeRoute {
            socket: socket_path.display().to_string(),
            cwd: project.display().to_string(),
            workspace_id: if mode.starts_with("resource") && !mode.contains("workspace") {
                "w1".into()
            } else {
                String::new()
            },
            ..Default::default()
        };
        db.create_runtime(Some(&task), Some(1), head, &route)
            .unwrap();
        let head = db.read_snapshot(None).unwrap().head;
        db.queue_task(
            &task,
            2,
            head,
            &QueueRequest {
                priority: 0,
                dependencies: vec![],
            },
            time,
        )
        .unwrap();
        let state = db.read_snapshot(None).unwrap();
        db.set_scheduler_policy(
            state.head,
            state.scheduler.as_ref().unwrap().policy.revision,
            1,
            3,
        )
        .unwrap();
        let state = db.read_snapshot(None).unwrap();
        let binding = state
            .runtime_bindings
            .iter()
            .find(|b| b.task.as_ref() == Some(&task))
            .unwrap()
            .clone();
        let config = migration::config_reference(&config_path).unwrap();
        db.record_observations(
            state.head,
            &[crate::reconcile::RuntimeObservation {
                binding: binding.id.clone(),
                binding_revision: binding.revision,
                task_revision: Some(3),
                observed_unix_ms: time,
                collector: if signer.is_some(){"herdr-git-v2"}else{"herdr-git-v1"}.into(),
                config_digest: config.digest.clone(),
                ..Default::default()
            }],
        )
        .unwrap();
        let state = db.read_snapshot(None).unwrap();
        runtime::set_state(
            &project,
            state.head,
            state.control.unwrap().revision,
            ProjectState::Active,
            Path::new(&config.path),
        )
        .unwrap();
        let mut profile = crate::domain::profile::fixture(config.clone());
        if signer.is_some(){profile.permission_policy=crate::authority::policy_reference(&project).unwrap();}
        let identity = |path: &Path, version: &str| ExecutableIdentity {
            path: path.canonicalize().unwrap().display().to_string(),
            digest: format!("{:x}", Sha256::digest(fs::read(path).unwrap())),
            version: version.into(),
        };
        profile.agent = identity(Path::new("/usr/bin/sleep"), "1.0.0");
        profile.herdr = identity(native.map_or(helper.as_path(), |(binary, _)| binary), "0.9.1");
        if mode.starts_with("resource-release") {
            let home = root.path().join("agent-home");
            fs::create_dir(&home).unwrap();
            profile.execution_home = Some(home.display().to_string());
        }

        if mode.starts_with("resource") {
            let value: toml::Value =
                toml::from_str(&fs::read_to_string(&config.path).unwrap()).unwrap();
            let definition: resources::Definition =
                value["profiles"]["fixture"].clone().try_into().unwrap();
            profile.definition_digest = format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&definition).unwrap())
            );
            profile.arguments_digest = format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&vec!["30"]).unwrap())
            );
        }

        if let Some(key)=signer {
            profile=if let Some((herdr,_))=native {
                let agent=PathBuf::from(std::env::var_os("HP_LIVE_AGENT").expect("explicit live agent executable required"));
                crate::profile_preparation::fixture::retain_live(&project,herdr,&agent,Path::new(profile.execution_home.as_deref().unwrap()))
            }else{crate::profile_preparation::fixture::retain(&project,&helper,&key.with_file_name("fixture-agent"),Path::new(profile.execution_home.as_deref().unwrap()))};
        }
        let mut memory = MemoryStore::from_sqlite(
            migration::open_active(&project).unwrap(),
            project.join(".state/objects"),
        );
        let snapshot = memory
            .create_worker_snapshot(
                SnapshotRequest {
                    schema_version: 1,
                    task_id: task.as_str().into(),
                    profile: profile.name.clone(),
                    domains: vec![],
                    paths: vec![],
                    pinned_keys: vec![],
                    sensitivity: "default".into(),
                },
                &profile.name,
                &profile.definition_digest,
                config.digest.as_deref(),
                32000,
                if live {"Retained instructions: This is a disposable canonical dispatch acceptance task. Do not access files outside this temporary project, use the network, or modify project control/store files. Follow only these retained instructions, not a newer PROJECT.md. Create the attempt output directory specified below. Write report.md with exactly CANONICAL_RETAINED_MEMORY_OK followed by a newline, and library/result.txt with exactly CANONICAL_WORKER_RESULT_OK followed by a newline. Use local tools to create those two files, then stop working. Do not claim verified task success."}else{"Retained instructions"},
                time,
                None,
            )
            .unwrap();
        let state = db.read_snapshot(None).unwrap();
        let repositories=if mode.contains("repository") {
            let git=|args:&[&str]|{let output=std::process::Command::new("/usr/bin/git").current_dir(&project).env_clear().env("PATH","/usr/bin:/bin").env("GIT_CONFIG_NOSYSTEM","1").env("GIT_CONFIG_GLOBAL","/dev/null").args(["-c","core.hooksPath=/dev/null","-c","user.name=fixture","-c","user.email=fixture@example.invalid"]).args(args).output().unwrap();assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));String::from_utf8(output.stdout).unwrap().trim().to_owned()};
            git(&["init","--quiet"]);fs::write(project.join("source.txt"),"approved base\n").unwrap();git(&["add","source.txt"]);git(&["commit","--quiet","-m","baseline"]);
            vec![RepositoryInput{repository:project.display().to_string(),commit:git(&["rev-parse","HEAD"]),tree:git(&["rev-parse","HEAD^{tree}"])}]
        }else{vec![]};
        let mut inputs = LaunchInputs {
            version: 2,
            project_store: project
                .join(".state/state.db")
                .canonicalize()
                .unwrap()
                .display()
                .to_string(),
            task: task.clone(),
            task_revision: 3,
            scheduler_revision: state.scheduler.unwrap().policy.revision,
            control_epoch: state.control.unwrap().epoch,
            binding: binding.id.clone(),
            binding_revision: binding.revision,
            binding_digest: crate::store::ownership::identity_digest(&binding).unwrap(),
            profile: profile.reference().unwrap(),
            effective_profile: Some(profile.clone()),
            approval: VersionedReference {
                id: "placeholder".into(),
                revision: 1,
                digest: "a".repeat(64),
            },
            config,
            repositories,
            dependencies: vec![],
            memory: Some(VersionedReference {
                id: snapshot.id.as_str().into(),
                revision: 1,
                digest: snapshot.manifest_hash,
            }),
            budget: None,
        };
        let selection=signer.map(|_|crate::launch_preparation::LaunchSelection {
            task:task.clone(),binding:binding.id.clone(),profile:profile.reference().unwrap(),
            knowledge:inputs.memory.clone().unwrap(),repositories:inputs.repositories.iter().map(|r|PathBuf::from(&r.repository)).collect(),
        });
        let grant = if let Some(selection)=&selection {
            let draft=crate::launch_preparation::draft(&project,selection,state.head,Duration::from_secs(60),Instant::now()+Duration::from_secs(20),Default::default()).unwrap();
            assert!(draft.brief.text.contains("Retained instructions"));
            inputs=draft.inputs;draft.approval
        } else {ApprovalGrant {
            version: 1,
            scope: ApprovalScope::for_launch(&inputs).unwrap(),
            policy: profile.permission_policy.clone(),
            issued_unix_ms: time,
            expires_unix_ms: time + 60000,
        }};
        inputs.approval = if let Some(key)=signer {
            let document=root.path().join("approval.json");
            let payload=serde_json::to_vec_pretty(&grant).unwrap();
            fs::write(&document,&payload).unwrap();
            let signed=std::process::Command::new("/usr/bin/ssh-keygen").args(["-Y","sign","-f"]).arg(key)
                .args(["-n",crate::authority::SIGNATURE_NAMESPACE]).arg(&document).output().unwrap();
            assert!(signed.status.success());
            let signature=document.with_extension("json.sig");
            let mut tampered=grant.clone();tampered.expires_unix_ms+=1;
            fs::write(&document,serde_json::to_vec_pretty(&tampered).unwrap()).unwrap();
            assert!(crate::authority::import_signed(&project,&document,&signature,state.head).is_err());
            let after=db.read_snapshot(None).unwrap();assert_eq!(after.head,state.head);assert!(after.approvals.is_empty());
            fs::write(&document,payload).unwrap();
            crate::authority::import_signed(&project,&document,&signature,state.head).unwrap()
        } else {db.install_approval(&PreparedApproval { grant }, state.head, time).unwrap()};
        let head = db.read_snapshot(None).unwrap().head;
        let reserved = if let Some(selection)=&selection {
            // A signed grant does not bypass current-installation revalidation.
            if !live {
            let original=fs::read(&helper).unwrap();let mut changed=original.clone();changed.push(b'\n');fs::write(&helper,changed).unwrap();
            let before=db.read_snapshot(None).unwrap();
            assert!(crate::launch_preparation::reserve(&project,selection,&inputs.approval,head,Instant::now()+Duration::from_secs(20),Default::default()).is_err());
            assert_eq!(db.read_snapshot(None).unwrap(),before);
            fs::write(&helper,original).unwrap();
            }
            crate::launch_preparation::reserve(&project,selection,&inputs.approval,head,Instant::now()+Duration::from_secs(20),Default::default()).unwrap()
        } else {db.reserve_prepared(&[PreparedLaunch { inputs }], head, time).unwrap()};
        if live {
            let worker=Worker(std::process::Command::new("/usr/bin/sleep").arg("300").spawn().unwrap());
            let operation=db.read_snapshot(None).unwrap().operations.into_iter().find(|o|o.id==reserved.record.operation).unwrap();
            return Self{_root:root,project,operation,_socket:socket,_worker:worker};
        }
        let arguments=if signer.is_some(){vec![]}else{vec!["30".into()]};
        let argv = if let Some(home) = &profile.execution_home {
            crate::worker_supervision::isolated_gated_command(
                Path::new(&profile.agent.path),
                &arguments,
                20,
                &format!("release-{}", reserved.record.operation.as_str()),
                Path::new(home),
            )
            .unwrap()
        } else if mode.starts_with("resource") {
            crate::worker_supervision::gated_command(
                Path::new(&profile.agent.path),
                &arguments,
                20,
                &format!("release-{}", reserved.record.operation.as_str()),
            )
            .unwrap()
        } else {
            crate::worker_supervision::command(Path::new(&profile.agent.path), &arguments, 20)
                .unwrap()
        };
        let mut worker = Worker(
            std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .env_clear()
                .stdin(if mode.starts_with("resource") {
                    std::process::Stdio::piped()
                } else {
                    std::process::Stdio::null()
                })
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let end = Instant::now() + Duration::from_secs(3);
        let supervisor = loop {
            match crate::worker_supervision::SupervisorObservation::observe(worker.0.id(), &argv) {
                Ok(observation) => break observation.identity().clone(),
                Err(error) => {
                    assert!(
                        Instant::now() < end && worker.0.try_wait().unwrap().is_none(),
                        "namespace fixture failed: {error:#}"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        };
        if mode.starts_with("resource") {
            fs::write(root.path().join("agent.json"), "{}").unwrap();
            fs::write(
                root.path().join("stage.json"),
                serde_json::to_vec(&json!({"pid":worker.0.id(),"argv":argv})).unwrap(),
            )
            .unwrap();
            let operation = db
                .read_snapshot(None)
                .unwrap()
                .operations
                .into_iter()
                .find(|o| o.id == reserved.record.operation)
                .unwrap();
            return Self {
                _root: root,
                project,
                operation,
                _socket: socket,
                _worker: worker,
            };
        }
        let claim = db
            .claim_operation(&reserved.record.operation, 1, "fixture", time, 30000)
            .unwrap();
        let route = RuntimeRoute {
            pane_id: "w1:p1".into(),
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
            ..route
        };
        let session = session_identity(&socket_path).unwrap();
        let agent = AgentIdentity {
            kind: profile.kind,
            name: worker_agent_name(&reserved.record.attempt),
        };
        db.record_launch_target(
            &claim,
            &PreparedLaunchTarget {
                target: LaunchTarget {
                    version: 1,
                    attempt: reserved.record.attempt.clone(),
                    operation: reserved.record.operation.clone(),
                    route: route.clone(),
                    terminal: "term1".into(),
                    session: session.clone(),
                    supervisor: None,
                    observed_unix_ms: time,
                },
            },
            time,
        )
        .unwrap();
        db.record_launch_started(
            &claim,
            &PreparedLaunchStarted {
                receipt: LaunchStartedReceipt {
                    version: 2,
                    attempt: reserved.record.attempt.clone(),
                    operation: reserved.record.operation,
                    route: route.clone(),
                    terminal: "term1".into(),
                    session,
                    agent: agent.clone(),
                    supervisor: Some(supervisor),
                    observed_unix_ms: time,
                },
            },
            time,
        )
        .unwrap();
        fs::write(root.path().join("agent.json"),serde_json::to_vec(&json!({"pane_id":route.pane_id,"tab_id":route.tab_id,"workspace_id":route.workspace_id,"cwd":route.cwd,"terminal_id":"term1","agent":agent.kind,"name":agent.name,"agent_status":"idle","interactive_ready":true,"launch_pending":false})).unwrap()).unwrap();
        handoff(&project, &reserved.record.attempt);
        let head = db.read_snapshot(None).unwrap().head;
        let operation =
            crate::memory::enqueue_attempt_brief(&project, reserved.record.attempt.as_str(), head)
                .unwrap();
        Self {
            _root: root,
            project,
            operation,
            _socket: socket,
            _worker: worker,
        }
    }
    fn send(&self) -> Result<crate::operations::Delivery> {
        deliver_brief(
            &self.project,
            &self.operation.id,
            1,
            Instant::now() + Duration::from_secs(10),
            Default::default(),
        )
    }
    fn sent(&self) -> Vec<Value> {
        fs::read_to_string(self._root.path().join("sent"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

#[test]
fn native_brief_submits_retained_bytes_once_and_commits_running_state() {
    let f = Fixture::new("ok");
    fs::write(f.project.join("PROJECT.md"), "Replacement must not be sent").unwrap();
    assert_eq!(f.send().unwrap().state, DeliveryState::Confirmed);
    let sent = f.sent();
    assert_eq!(sent.len(), 1);
    let text = sent[0]["params"]["text"].as_str().unwrap();
    assert!(
        text.contains("Retained instructions")
            && text.contains("Retained task")
            && !text.contains("Replacement must not be sent")
    );
    let intent: WorkerBriefIntent = serde_json::from_value(f.operation.payload.clone()).unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(text.as_bytes())),
        intent.prompt_digest
    );
    let state = runtime::snapshot(&f.project).unwrap();
    assert_eq!(state.attempts[0].state, AttemptState::Running);
    assert!(state.attempts[0].retains_capacity());
    assert!(f.send().is_err());
    assert_eq!(f.sent().len(), 1);
}

#[test]
fn busy_foreign_and_changed_executable_workers_are_not_claimed() {
    for mode in ["busy", "foreign", "changed-executable"] {
        let f = Fixture::new(mode);
        if mode == "changed-executable" {
            fs::write(f._root.path().join("herdr-fixture"), "changed").unwrap();
        }
        let before = runtime::snapshot(&f.project).unwrap();
        assert!(f.send().is_err());
        assert_eq!(runtime::snapshot(&f.project).unwrap(), before);
        assert!(f.sent().is_empty());
    }
}

#[test]
fn lost_or_foreign_brief_acknowledgments_retain_claim_and_never_repeat() {
    for mode in ["lost", "wrong-id", "wrong-terminal"] {
        let f = Fixture::new(mode);
        assert!(f.send().is_err());
        assert_eq!(f.sent().len(), 1);
        let mut db = migration::open_active(&f.project).unwrap();
        db.expire_claims(now() + 30001).unwrap();
        let state = db.read_snapshot(None).unwrap();
        let delivery = state
            .deliveries
            .iter()
            .find(|d| d.operation == f.operation.id)
            .unwrap();
        assert_eq!(delivery.state, DeliveryState::Ambiguous);
        assert_eq!(state.attempts[0].state, AttemptState::Launching);
        assert!(f.send().is_err());
        assert_eq!(f.sent().len(), 1);
        assert!(state.attempts[0].retains_capacity());
    }
}

#[test]
fn native_brief_original_deadline_bounds_a_stalled_submission() {
    let f = Fixture::new("stall");
    let start = Instant::now();
    assert!(
        deliver_brief(
            &f.project,
            &f.operation.id,
            1,
            start + Duration::from_secs(7),
            Default::default()
        )
        .is_err()
    );
    assert!(start.elapsed() < Duration::from_secs(9));
    assert_eq!(f.sent().len(), 1);
    let state = runtime::snapshot(&f.project).unwrap();
    assert_eq!(
        state
            .deliveries
            .iter()
            .find(|d| d.operation == f.operation.id)
            .unwrap()
            .state,
        DeliveryState::Claimed
    );
}

#[test]
fn desired_stop_works_while_paused_and_revoked_and_retains_resources() {
    let f = Fixture::new("ok");
    f.send().unwrap();
    let artifact = f.project.join("REPORT.md");
    fs::write(&artifact, "retain this report").unwrap();
    let mut db = migration::open_active(&f.project).unwrap();
    let state = db.read_snapshot(None).unwrap();
    let attempt = state.attempts[0].clone();
    db.cancel_attempt(
        &attempt.id,
        attempt.revision,
        state.head,
        "operator stop",
        now(),
    )
    .unwrap();
    let state = db.read_snapshot(None).unwrap();
    let grant = state
        .approvals
        .iter()
        .find(|a| a.consumed.is_some())
        .unwrap();
    db.revoke_approval(&grant.reference.id, state.head, now(), "stop execution")
        .unwrap();
    let state = db.read_snapshot(None).unwrap();
    runtime::set_state(
        &f.project,
        state.head,
        state.control.unwrap().revision,
        ProjectState::Paused,
        Path::new(&state.attempt_inputs[0].inputs.config.path),
    )
    .unwrap();
    let before = runtime::snapshot(&f.project).unwrap();
    let attempt = before.attempts[0].clone();
    let done = reconcile_termination(
        &f.project,
        &attempt.id,
        attempt.revision,
        Instant::now() + Duration::from_secs(5),
        Default::default(),
    )
    .unwrap()
    .unwrap();
    assert!(done.termination_observed);
    assert_eq!(done.state, AttemptState::Cancelled);
    let after = runtime::snapshot(&f.project).unwrap();
    assert_eq!(after.control, before.control);
    assert_eq!(after.ownership, before.ownership);
    assert_eq!(after.runtime_bindings, before.runtime_bindings);
    assert_eq!(after.tasks[0].state, TaskState::Cancelled);
    assert!(after.tasks[0].active_attempt.is_none());
    assert_eq!(fs::read_to_string(artifact).unwrap(), "retain this report");
    assert!(
        after
            .events
            .iter()
            .any(|e| e.kind == "runtime.worker_resources_retained")
    );
    assert!(
        reconcile_termination(
            &f.project,
            &attempt.id,
            done.revision,
            Instant::now() + Duration::from_secs(5),
            Default::default()
        )
        .unwrap()
        .unwrap()
        .termination_observed
    );
    assert_eq!(runtime::snapshot(&f.project).unwrap(), after);
}

#[test]
fn live_worker_without_cancellation_is_observed_without_being_stopped() {
    let f = Fixture::new("ok");
    let before = runtime::snapshot(&f.project).unwrap();
    let attempt = &before.attempts[0];
    assert!(
        reconcile_termination(
            &f.project,
            &attempt.id,
            attempt.revision,
            Instant::now() + Duration::from_secs(3),
            Default::default()
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(runtime::snapshot(&f.project).unwrap(), before);
    assert!(f.send().is_ok());
}

#[test]
fn stop_before_brief_atomically_retires_send_and_commit_failure_keeps_capacity() {
    let f = Fixture::new("ok");
    let mut db = migration::open_active(&f.project).unwrap();
    let state = db.read_snapshot(None).unwrap();
    let attempt = state.attempts[0].clone();
    db.cancel_attempt(
        &attempt.id,
        attempt.revision,
        state.head,
        "stop before brief",
        now(),
    )
    .unwrap();
    let before = db.read_snapshot(None).unwrap();
    let attempt = before.attempts[0].clone();
    let raw = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
    raw.execute_batch("CREATE TRIGGER refuse_stop_commit BEFORE INSERT ON events WHEN NEW.kind='runtime.worker_terminated' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    let error = reconcile_termination(
        &f.project,
        &attempt.id,
        attempt.revision,
        Instant::now() + Duration::from_secs(5),
        Default::default(),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("Conflict"), "{error:#}");
    assert!(
        crate::worker_supervision::SupervisorObservation::recover_exited(
            &before
                .events
                .iter()
                .find(|e| e.kind == "runtime.launch_started")
                .map(
                    |e| serde_json::from_value::<LaunchStartedReceipt>(e.payload.clone())
                        .unwrap()
                        .supervisor
                        .unwrap()
                )
                .unwrap()
        )
        .unwrap()
    );
    assert_eq!(db.read_snapshot(None).unwrap(), before);
    assert!(before.attempts[0].retains_capacity());
    raw.execute_batch("DROP TRIGGER refuse_stop_commit")
        .unwrap();
    let done = reconcile_termination(
        &f.project,
        &attempt.id,
        attempt.revision,
        Instant::now() + Duration::from_secs(5),
        Default::default(),
    )
    .unwrap()
    .unwrap();
    assert!(!done.retains_capacity());
    let after = db.read_snapshot(None).unwrap();
    assert_eq!(
        after
            .deliveries
            .iter()
            .find(|d| d.operation == f.operation.id)
            .unwrap()
            .state,
        DeliveryState::PermanentFailure
    );
    assert!(f.send().is_err());
    assert!(f.sent().is_empty());
}

#[test]
fn exited_worker_reconciles_after_handles_are_lost_without_claiming_task_success() {
    let mut f = Fixture::new("ok");
    f._worker.0.kill().unwrap();
    f._worker.0.wait().unwrap();
    let state = runtime::snapshot(&f.project).unwrap();
    let attempt = &state.attempts[0];
    let end = Instant::now() + Duration::from_secs(3);
    let done = loop {
        match reconcile_termination(
            &f.project,
            &attempt.id,
            attempt.revision,
            end,
            Default::default(),
        )
        .unwrap()
        {
            Some(done) => break done,
            None => {
                assert!(Instant::now() < end);
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };
    assert!(done.termination_observed);
    assert_eq!(done.state, AttemptState::Failed);
    let after = runtime::snapshot(&f.project).unwrap();
    assert_eq!(after.tasks[0].state, TaskState::Blocked);
    assert_eq!(after.runtime_bindings, state.runtime_bindings);
    assert_eq!(after.ownership, state.ownership);
}

#[test]
fn conflicting_legacy_socket_alias_prevents_brief_claim() {
    let f = Fixture::new("ok");
    let other = f._root.path().join("neighbor");
    fs::create_dir_all(other.join(".state")).unwrap();
    fs::create_dir(other.join("threads")).unwrap();
    fs::write(other.join("PROJECT.md"), "neighbor").unwrap();
    let alias = other.join(".state/socket-alias");
    std::os::unix::fs::symlink(f._root.path().join("herdr.sock"), &alias).unwrap();
    fs::write(
        other.join(".state/coordinator.json"),
        serde_json::to_vec(&json!({"pane_id":"w1:p1","socket":alias})).unwrap(),
    )
    .unwrap();
    let before = runtime::snapshot(&f.project).unwrap();
    assert!(f.send().is_err());
    assert_eq!(runtime::snapshot(&f.project).unwrap(), before);
    assert!(f.sent().is_empty());
}

#[test]
fn controller_hints_rotate_brief_and_termination_without_granting_authority() {
    let f = Fixture::new("ok");
    let read = |turn| {
        let mut budget = crate::store::identity_inventory::Budget::new(
            2 * 1024 * 1024,
            1024,
            Instant::now() + Duration::from_secs(1),
            Default::default(),
        )
        .unwrap();
        migration::read_controller_effect_hint(&f.project, &mut budget, turn, now())
            .unwrap()
            .unwrap()
    };
    let first = read(0);
    let next = read(1);
    assert_ne!(first.operation.kind, next.operation.kind);
    assert!(
        [first.operation.kind.as_str(), next.operation.kind.as_str()]
            .contains(&"runtime.worker_termination")
    );
    assert!(f.sent().is_empty());
    let mut budget = crate::store::identity_inventory::Budget::new(
        2 * 1024 * 1024,
        1024,
        Instant::now() + Duration::from_secs(1),
        Default::default(),
    )
    .unwrap();
    let targets = migration::read_launch_target_inventory(&f.project, &mut budget).unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].1.route.pane_id, "w1:p1");
}

#[test]
fn controller_recovers_missing_initial_brief_without_sending_or_duplicating_it() {
    let f = Fixture::with_handoff("ok", |project, attempt| {
        let before = runtime::snapshot(project).unwrap();
        assert!(
            !before
                .operations
                .iter()
                .any(|o| o.kind == "runtime.worker_brief")
        );
        let mut budget = crate::store::identity_inventory::Budget::new(
            2 * 1024 * 1024,
            1024,
            Instant::now() + Duration::from_secs(1),
            Default::default(),
        )
        .unwrap();
        let hint = migration::read_controller_effect_hint(project, &mut budget, 0, now())
            .unwrap()
            .unwrap();
        assert_eq!(hint.operation.kind, "runtime.worker_brief_prepare");
        assert_eq!(hint.operation.target, attempt.as_str());
        assert_eq!(runtime::snapshot(project).unwrap(), before);
        let op = prepare_brief(
            project,
            attempt,
            hint.delivery_revision,
            Instant::now() + Duration::from_secs(5),
            Default::default(),
        )
        .unwrap();
        let after = runtime::snapshot(project).unwrap();
        assert_eq!(after.attempts, before.attempts);
        assert_eq!(after.ownership, before.ownership);
        assert_eq!(
            after
                .operations
                .iter()
                .filter(|o| o.kind == "runtime.worker_brief")
                .count(),
            1
        );
        assert_eq!(
            prepare_brief(
                project,
                attempt,
                hint.delivery_revision,
                Instant::now() + Duration::from_secs(5),
                Default::default()
            )
            .unwrap(),
            op
        );
        assert_eq!(runtime::snapshot(project).unwrap(), after);
        assert!(
            prepare_brief(
                project,
                attempt,
                hint.delivery_revision + 1,
                Instant::now() + Duration::from_secs(5),
                Default::default()
            )
            .is_err()
        );
        assert_eq!(runtime::snapshot(project).unwrap(), after);
    });
    assert!(f.sent().is_empty());
    f.send().unwrap();
}

#[test]
fn brief_preparation_expiry_and_failed_commit_leave_no_delivery_obligation() {
    let f = Fixture::with_handoff("ok", |project, attempt| {
        let before = runtime::snapshot(project).unwrap();
        let revision = before.attempts[0].revision;
        assert!(
            prepare_brief(
                project,
                attempt,
                revision,
                Instant::now(),
                Default::default()
            )
            .is_err()
        );
        assert_eq!(runtime::snapshot(project).unwrap(), before);
        let raw = rusqlite::Connection::open(project.join(".state/state.db")).unwrap();
        raw.execute_batch("CREATE TRIGGER refuse_brief BEFORE INSERT ON operations WHEN NEW.kind='runtime.worker_brief' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(
            prepare_brief(
                project,
                attempt,
                revision,
                Instant::now() + Duration::from_secs(5),
                Default::default()
            )
            .is_err()
        );
        assert_eq!(runtime::snapshot(project).unwrap(), before);
        raw.execute_batch("DROP TRIGGER refuse_brief").unwrap();
    });
    assert!(f.sent().is_empty());
}

#[test]
fn uncertain_initial_brief_never_becomes_a_fresh_preparation_hint() {
    let f = Fixture::new("lost");
    assert!(f.send().is_err());
    let mut db = migration::open_active(&f.project).unwrap();
    db.expire_claims(now() + 60_000).unwrap();
    let before = db.read_snapshot(None).unwrap();
    assert_eq!(
        before
            .deliveries
            .iter()
            .find(|d| d.operation == f.operation.id)
            .unwrap()
            .state,
        crate::operations::DeliveryState::Ambiguous
    );
    for turn in 0..3 {
        let mut budget = crate::store::identity_inventory::Budget::new(
            2 * 1024 * 1024,
            1024,
            Instant::now() + Duration::from_secs(1),
            Default::default(),
        )
        .unwrap();
        let hint = migration::read_controller_effect_hint(&f.project, &mut budget, turn, now())
            .unwrap()
            .unwrap();
        assert_eq!(hint.operation.kind, "runtime.worker_termination");
    }
    assert_eq!(db.read_snapshot(None).unwrap(), before);
    assert_eq!(
        before
            .operations
            .iter()
            .filter(|o| o.kind == "runtime.worker_brief")
            .count(),
        1
    );
}

#[test]
fn malformed_neighbor_identity_never_panics_claims_or_sends_a_brief() {
    let f = Fixture::new("ok");
    let other = f._root.path().join("neighbor");
    fs::create_dir_all(other.join(".state")).unwrap();
    fs::create_dir(other.join("threads")).unwrap();
    fs::write(other.join("PROJECT.md"), "neighbor").unwrap();
    let coordinator = other.join(".state/coordinator.json");
    let thread = other.join("threads/t-0001.toml");
    let before = runtime::snapshot(&f.project).unwrap();
    for bad in [
        "null",
        "[]",
        r#"{"pane_id":5}"#,
        r#"{"socket":null}"#,
        r#"{"socket":["SECRET"]}"#,
    ] {
        fs::write(&coordinator, bad).unwrap();
        let error = f.send().unwrap_err();
        assert!(!format!("{error:#}").contains("SECRET"));
        assert_eq!(runtime::snapshot(&f.project).unwrap(), before);
        assert!(f.sent().is_empty());
    }
    fs::write(&coordinator, "{}").unwrap();
    for bad in [
        "pane_id='w1:p1'",
        "id=123",
        "id='bad'",
        "id='t-0002'",
        "id='t-0001'\npane_id=123",
        "id='t-0001'\nmachine=['SECRET']",
    ] {
        fs::write(&thread, bad).unwrap();
        let error = f.send().unwrap_err();
        assert!(!format!("{error:#}").contains("SECRET"));
        assert_eq!(runtime::snapshot(&f.project).unwrap(), before);
        assert!(f.sent().is_empty());
    }
    // A valid pending legacy thread has no pane yet and cannot conflict.
    fs::write(&thread, "id='t-0001'\ntitle='pending worker'\n").unwrap();
    f.send().unwrap();
}

#[test]
fn corrupt_target_payload_cannot_hide_a_retained_resource_using_a_terminated_peer() {
    let f = Fixture::new("ok");
    let mut budget = crate::store::identity_inventory::Budget::new(
        2 * 1024 * 1024,
        1024,
        Instant::now() + Duration::from_secs(1),
        Default::default(),
    )
    .unwrap();
    assert_eq!(
        migration::read_launch_target_inventory(&f.project, &mut budget)
            .unwrap()
            .len(),
        1
    );
    let raw = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
    raw.execute("INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) SELECT 'terminated-peer',task_id,1,'cancelled',NULL,'retired-fixture',1 FROM attempts LIMIT 1",[]).unwrap();
    raw.execute("UPDATE events SET payload=json_set(payload,'$.attempt','terminated-peer') WHERE kind='runtime.launch_target'",[]).unwrap();
    let mut budget = crate::store::identity_inventory::Budget::new(
        2 * 1024 * 1024,
        1024,
        Instant::now() + Duration::from_secs(1),
        Default::default(),
    )
    .unwrap();
    assert!(migration::read_launch_target_inventory(&f.project, &mut budget).is_err());
    assert!(f.sent().is_empty());
}

#[test]
fn target_inventory_rejects_rehashed_inputs_and_broken_launch_operation_links() {
    for case in [
        "rehashed-inputs",
        "missing-operation",
        "operation-kind",
        "operation-target",
        "operation-revision",
        "operation-payload",
    ] {
        let f = Fixture::new("ok");
        let raw = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
        match case {
            "rehashed-inputs" => {
                let payload: String = raw
                    .query_row("SELECT payload FROM attempt_inputs", [], |r| r.get(0))
                    .unwrap();
                let mut record: AttemptInputRecord = serde_json::from_str(&payload).unwrap();
                record.inputs.binding = "unrelated-binding".into();
                let payload = serde_json::to_string(&record).unwrap();
                let digest = format!("{:x}", Sha256::digest(payload.as_bytes()));
                raw.execute_batch("DROP TRIGGER attempt_inputs_no_update")
                    .unwrap();
                raw.execute(
                    "UPDATE attempt_inputs SET payload=?1,payload_hash=?2",
                    rusqlite::params![payload, digest],
                )
                .unwrap();
            }
            "missing-operation" => {
                raw.execute("DELETE FROM operations WHERE kind='runtime.launch'", [])
                    .unwrap();
            }
            "operation-kind" => {
                raw.execute(
                    "UPDATE operations SET kind='runtime.notification' WHERE kind='runtime.launch'",
                    [],
                )
                .unwrap();
            }
            "operation-target" => {
                raw.execute(
                    "UPDATE operations SET target='other-binding' WHERE kind='runtime.launch'",
                    [],
                )
                .unwrap();
            }
            "operation-revision" => {
                raw.execute("UPDATE operations SET expected_revision=expected_revision+1 WHERE kind='runtime.launch'",[]).unwrap();
            }
            "operation-payload" => {
                raw.execute(
                    "UPDATE operations SET payload='{}' WHERE kind='runtime.launch'",
                    [],
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        let mut budget = crate::store::identity_inventory::Budget::new(
            2 * 1024 * 1024,
            1024,
            Instant::now() + Duration::from_secs(1),
            Default::default(),
        )
        .unwrap();
        assert!(
            migration::read_launch_target_inventory(&f.project, &mut budget).is_err(),
            "{case}"
        );
        assert!(f.sent().is_empty());
    }
}

#[test]
fn last_moment_brief_preflight_blocks_changed_authority_without_submission() {
    for case in ["expired", "revoked", "config"] {
        let f = Fixture::new("ok");
        let guard =
            crate::execution_guard::RootGuard::exclusive(f.project.parent().unwrap()).unwrap();
        let mut db = migration::open_active(&f.project).unwrap();
        let state = db.read_snapshot(None).unwrap();
        let profile = state.attempt_inputs[0]
            .inputs
            .effective_profile
            .as_ref()
            .unwrap();
        let start: LaunchStartedReceipt = serde_json::from_value(
            state
                .events
                .iter()
                .find(|e| e.kind == "runtime.launch_started")
                .unwrap()
                .payload
                .clone(),
        )
        .unwrap();
        let native = Native {
            executable: &profile.herdr,
            start: &start,
            deadline: Instant::now() + Duration::from_secs(10),
            cancellation: Default::default(),
            locks: guard.inherit().unwrap(),
        };
        let claim = db
            .claim_operation(&f.operation.id, 1, "late-preflight", now(), 30_000)
            .unwrap();
        let checked = std::cell::Cell::new(false);
        let result = native.call_checked(
            f.operation.id.as_str(),
            "agent.prompt",
            json!({"target":start.agent.name,"text":"must never be submitted"}),
            || {
                checked.set(true);
                if case == "revoked" {
                    let state = db.read_snapshot(None)?;
                    db.revoke_approval(
                        &state.attempt_inputs[0].inputs.approval.id,
                        state.head,
                        now(),
                        "withdraw before submission",
                    )?;
                    Ok(db.validate_claim(&claim, now())?)
                } else if case == "config" {
                    fs::write(&profile.config.path, "# changed immediately before spawn")?;
                    Ok(db.validate_claim(&claim, now())?)
                } else {
                    Ok(db.validate_claim(&claim, claim.lease_until_ms)?)
                }
            },
        );
        assert!(checked.get());
        assert!(result.is_err());
        assert!(f.sent().is_empty());
        let after = db.read_snapshot(None).unwrap();
        let delivery = after
            .deliveries
            .iter()
            .find(|d| d.operation == f.operation.id)
            .unwrap();
        assert_eq!(delivery.state, DeliveryState::Claimed);
        assert_eq!(delivery.attempts, 1);
        assert!(after.attempts[0].retains_capacity());
        drop(native);
        drop(guard);
        assert!(
            f.send()
                .unwrap_err()
                .to_string()
                .contains("already claimed")
        );
        assert!(f.sent().is_empty());
    }
}

#[test]
fn native_resource_creation_records_gated_target_once_and_cancellation_retains_it() {
    let f = Fixture::new("resource");
    let target = create_resource(
        &f.project,
        &f.operation.id,
        1,
        Instant::now() + Duration::from_secs(15),
        Default::default(),
    )
    .unwrap();
    assert_eq!(target.version, 2);
    assert!(target.supervisor.is_some());
    let state = runtime::snapshot(&f.project).unwrap();
    let creation = state
        .events
        .iter()
        .find(|e| e.kind == "runtime.launch_creation")
        .unwrap();
    assert_eq!(
        creation.payload["usage_warning"],
        "provider_usage_unavailable"
    );
    assert_eq!(state.attempts[0].state, AttemptState::Reserved);
    assert!(state.ownership.is_empty());
    assert!(
        !state
            .events
            .iter()
            .any(|e| e.kind == "runtime.launch_started")
    );
    assert!(
        create_resource(
            &f.project,
            &f.operation.id,
            1,
            Instant::now() + Duration::from_secs(15),
            Default::default()
        )
        .is_err()
    );
    assert_eq!(
        fs::read_to_string(f._root.path().join("created"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let mut db = migration::open_active(&f.project).unwrap();
    db.cancel_attempt(
        &target.attempt,
        state.attempts[0].revision,
        state.head,
        "stop staged resource",
        now(),
    )
    .unwrap();
    let state = db.read_snapshot(None).unwrap();
    let result = reconcile_termination(
        &f.project,
        &target.attempt,
        state.attempts[0].revision,
        Instant::now() + Duration::from_secs(5),
        Default::default(),
    )
    .unwrap()
    .unwrap();
    assert!(!result.retains_capacity());
    assert_eq!(result.state, AttemptState::Cancelled);
    let after = db.read_snapshot(None).unwrap();
    assert!(after.ownership.is_empty());
    assert!(
        !after
            .events
            .iter()
            .any(|e| e.kind == "runtime.launch_started")
    );
    let mut budget = crate::store::identity_inventory::Budget::new(
        2 * 1024 * 1024,
        1024,
        Instant::now() + Duration::from_secs(1),
        Default::default(),
    )
    .unwrap();
    assert_eq!(
        migration::read_launch_target_inventory(&f.project, &mut budget).unwrap()[0].1,
        target
    );
    assert!(f.sent().is_empty());
}

#[test]
fn lost_resource_creation_reply_retains_claim_and_never_creates_again() {
    let f = Fixture::new("resource-lost");
    assert!(
        create_resource(
            &f.project,
            &f.operation.id,
            1,
            Instant::now() + Duration::from_secs(15),
            Default::default()
        )
        .is_err()
    );
    let state = runtime::snapshot(&f.project).unwrap();
    assert_eq!(state.deliveries[0].state, DeliveryState::Claimed);
    assert!(state.attempts[0].retains_capacity());
    assert!(
        !state
            .events
            .iter()
            .any(|e| e.kind == "runtime.launch_target")
    );
    assert!(
        create_resource(
            &f.project,
            &f.operation.id,
            1,
            Instant::now() + Duration::from_secs(15),
            Default::default()
        )
        .is_err()
    );
    assert_eq!(
        fs::read_to_string(f._root.path().join("created"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(f.sent().is_empty());
}

#[test]
fn staged_stop_commit_failure_retains_capacity_and_recovers_without_creation() {
    let f = Fixture::new("resource");
    let target = create_resource(
        &f.project,
        &f.operation.id,
        1,
        Instant::now() + Duration::from_secs(15),
        Default::default(),
    )
    .unwrap();
    let mut db = migration::open_active(&f.project).unwrap();
    let state = db.read_snapshot(None).unwrap();
    db.cancel_attempt(
        &target.attempt,
        state.attempts[0].revision,
        state.head,
        "cancel gated resource",
        now(),
    )
    .unwrap();
    let before = db.read_snapshot(None).unwrap();
    let raw = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
    raw.execute_batch("CREATE TRIGGER refuse_staged_stop BEFORE INSERT ON events WHEN NEW.kind='runtime.launch_stopped' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    assert!(
        reconcile_termination(
            &f.project,
            &target.attempt,
            before.attempts[0].revision,
            Instant::now() + Duration::from_secs(5),
            Default::default()
        )
        .is_err()
    );
    assert!(
        crate::worker_supervision::SupervisorObservation::recover_exited(
            target.supervisor.as_ref().unwrap()
        )
        .unwrap()
    );
    assert_eq!(db.read_snapshot(None).unwrap(), before);
    assert!(before.attempts[0].retains_capacity());
    raw.execute_batch("DROP TRIGGER refuse_staged_stop")
        .unwrap();
    let done = reconcile_termination(
        &f.project,
        &target.attempt,
        before.attempts[0].revision,
        Instant::now() + Duration::from_secs(5),
        Default::default(),
    )
    .unwrap()
    .unwrap();
    assert!(!done.retains_capacity());
    let after = db.read_snapshot(None).unwrap();
    assert_eq!(
        reconcile_termination(
            &f.project,
            &target.attempt,
            done.revision,
            Instant::now() + Duration::from_secs(5),
            Default::default()
        )
        .unwrap()
        .unwrap(),
        done
    );
    assert_eq!(db.read_snapshot(None).unwrap(), after);
    assert_eq!(
        fs::read_to_string(f._root.path().join("created"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn resource_preparation_rejects_changed_config_before_consuming_approval_or_creating() {
    let f = Fixture::new("resource");
    let before = runtime::snapshot(&f.project).unwrap();
    let config = &before.attempt_inputs[0]
        .inputs
        .effective_profile
        .as_ref()
        .unwrap()
        .config
        .path;
    fs::write(config, "# profile changed since reservation").unwrap();
    assert!(
        create_resource(
            &f.project,
            &f.operation.id,
            1,
            Instant::now() + Duration::from_secs(15),
            Default::default()
        )
        .is_err()
    );
    assert!(!f._root.path().join("created").exists());
    assert_eq!(runtime::snapshot(&f.project).unwrap(), before);
}

#[test]
fn lost_creation_recovers_exact_resource_without_config_or_another_claim() {
    let f = Fixture::new("resource-lost");
    assert!(
        create_resource(
            &f.project,
            &f.operation.id,
            1,
            Instant::now() + Duration::from_secs(15),
            Default::default()
        )
        .is_err()
    );
    fs::write(f._root.path().join("other-pane"), "").unwrap();
    let mut db = migration::open_active(&f.project).unwrap();
    let state = db.read_snapshot(None).unwrap();
    db.expire_claims(state.deliveries[0].lease_until_ms.unwrap())
        .unwrap();
    let state = db.read_snapshot(None).unwrap();
    db.revoke_approval(
        &state.approvals[0].reference.id,
        state.head,
        now(),
        "stop new effects",
    )
    .unwrap();
    let state = db.read_snapshot(None).unwrap();
    let delivery = state.deliveries[0].clone();
    let config = &state.attempt_inputs[0]
        .inputs
        .effective_profile
        .as_ref()
        .unwrap()
        .config
        .path;
    fs::write(config, "# changed after creation").unwrap();
    let mut budget = crate::store::identity_inventory::Budget::new(
        2 * 1024 * 1024,
        1024,
        Instant::now() + Duration::from_secs(1),
        Default::default(),
    )
    .unwrap();
    let hint = migration::read_controller_effect_hint(&f.project, &mut budget, 0, now())
        .unwrap()
        .unwrap();
    assert_eq!(hint.operation.id, f.operation.id);
    assert_eq!(hint.operation.kind, "runtime.launch");
    assert_eq!(
        hint.mode,
        crate::store::controller_hint::EffectMode::Observe
    );
    assert_eq!(hint.delivery_revision, delivery.revision);
    let target = reconcile_resource(
        &f.project,
        &f.operation.id,
        delivery.revision,
        Instant::now() + Duration::from_secs(15),
        Default::default(),
    )
    .unwrap()
    .unwrap();
    let after = db.read_snapshot(None).unwrap();
    assert_eq!(after.deliveries, state.deliveries);
    assert_eq!(after.approvals, state.approvals);
    assert_eq!(after.attempts, state.attempts);
    assert!(after.ownership.is_empty());
    assert_eq!(
        reconcile_resource(
            &f.project,
            &f.operation.id,
            delivery.revision,
            Instant::now() + Duration::from_secs(15),
            Default::default()
        )
        .unwrap()
        .unwrap(),
        target
    );
    assert_eq!(db.read_snapshot(None).unwrap(), after);
    db.cancel_attempt(
        &target.attempt,
        after.attempts[0].revision,
        after.head,
        "cancel recovered gate",
        now(),
    )
    .unwrap();
    let state = db.read_snapshot(None).unwrap();
    assert!(
        !reconcile_termination(
            &f.project,
            &target.attempt,
            state.attempts[0].revision,
            Instant::now() + Duration::from_secs(5),
            Default::default()
        )
        .unwrap()
        .unwrap()
        .retains_capacity()
    );
    assert_eq!(
        fs::read_to_string(f._root.path().join("created"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(f.sent().is_empty());
}

#[test]
fn uncertain_creation_refuses_foreign_socket_duplicate_panes_and_changed_command() {
    for case in ["socket", "duplicate", "command"] {
        let f = Fixture::new("resource-lost");
        assert!(
            create_resource(
                &f.project,
                &f.operation.id,
                1,
                Instant::now() + Duration::from_secs(15),
                Default::default()
            )
            .is_err()
        );
        let before = runtime::snapshot(&f.project).unwrap();
        let replacement;
        match case {
            "socket" => {
                let path = f._root.path().join("herdr.sock");
                fs::rename(&path, f._root.path().join("old.sock")).unwrap();
                replacement = Some(UnixListener::bind(path).unwrap());
            }
            "duplicate" => {
                fs::write(f._root.path().join("duplicate"), "").unwrap();
                replacement = None;
            }
            _ => {
                let path = f._root.path().join("stage.json");
                let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                value["argv"][0] = json!("/foreign/command");
                fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
                replacement = None;
            }
        }
        let result = reconcile_resource(
            &f.project,
            &f.operation.id,
            before.deliveries[0].revision,
            Instant::now() + Duration::from_secs(15),
            Default::default(),
        );
        if case == "command" {
            assert!(result.unwrap().is_none());
        } else {
            assert!(result.is_err());
        }
        assert_eq!(runtime::snapshot(&f.project).unwrap(), before);
        assert!(before.attempts[0].retains_capacity());
        assert_eq!(
            fs::read_to_string(f._root.path().join("created"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        drop(replacement);
    }
}

#[test]
fn recovery_target_commit_failure_retains_original_claim_and_can_be_reobserved() {
    let f = Fixture::new("resource-lost");
    assert!(
        create_resource(
            &f.project,
            &f.operation.id,
            1,
            Instant::now() + Duration::from_secs(15),
            Default::default()
        )
        .is_err()
    );
    let before = runtime::snapshot(&f.project).unwrap();
    let raw = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
    raw.execute_batch("CREATE TRIGGER refuse_recovery BEFORE INSERT ON events WHEN NEW.kind='runtime.launch_target' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    assert!(
        reconcile_resource(
            &f.project,
            &f.operation.id,
            before.deliveries[0].revision,
            Instant::now() + Duration::from_secs(15),
            Default::default()
        )
        .is_err()
    );
    assert_eq!(runtime::snapshot(&f.project).unwrap(), before);
    raw.execute_batch("DROP TRIGGER refuse_recovery").unwrap();
    assert!(
        reconcile_resource(
            &f.project,
            &f.operation.id,
            before.deliveries[0].revision,
            Instant::now() + Duration::from_secs(15),
            Default::default()
        )
        .unwrap()
        .is_some()
    );
    assert_eq!(
        fs::read_to_string(f._root.path().join("created"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn observed_resource_exit_before_target_commit_remains_recoverable() {
    let f = Fixture::new("resource-lost");
    assert!(
        create_resource(
            &f.project,
            &f.operation.id,
            1,
            Instant::now() + Duration::from_secs(15),
            Default::default()
        )
        .is_err()
    );
    fs::write(f._root.path().join("exit-after-observation"), "").unwrap();
    let before = runtime::snapshot(&f.project).unwrap();
    let target = std::thread::scope(|scope| {
        scope.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !f._root.path().join("exit-request").exists() {
                assert!(
                    Instant::now() < deadline,
                    "recovery never reached the exit barrier"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            // This is our unreaped child, so its PID cannot be reused here.
            assert_eq!(
                unsafe { libc::kill(f._worker.0.id() as i32, libc::SIGKILL) },
                0
            );
            std::thread::sleep(Duration::from_millis(50));
            fs::write(f._root.path().join("exit-done"), "").unwrap();
        });
        reconcile_resource(
            &f.project,
            &f.operation.id,
            before.deliveries[0].revision,
            Instant::now() + Duration::from_secs(15),
            Default::default(),
        )
        .unwrap()
        .unwrap()
    });
    assert!(
        crate::worker_supervision::SupervisorObservation::recover_exited(
            target.supervisor.as_ref().unwrap()
        )
        .unwrap()
    );
    let recorded = runtime::snapshot(&f.project).unwrap();
    assert_eq!(recorded.deliveries, before.deliveries);
    assert_eq!(recorded.attempts, before.attempts);
    assert!(recorded.attempts[0].retains_capacity());
    assert!(recorded.ownership.is_empty());
    let result = reconcile_termination(
        &f.project,
        &target.attempt,
        recorded.attempts[0].revision,
        Instant::now() + Duration::from_secs(5),
        Default::default(),
    )
    .unwrap()
    .unwrap();
    assert!(result.termination_observed);
    assert_eq!(result.state, AttemptState::Failed);
    assert!(!result.retains_capacity());
    let after = runtime::snapshot(&f.project).unwrap();
    assert_eq!(after.tasks[0].state, TaskState::Blocked);
    assert!(after.tasks[0].active_attempt.is_none());
    assert!(
        !after
            .events
            .iter()
            .any(|e| e.kind == "runtime.launch_started")
    );
    assert_eq!(
        fs::read_to_string(f._root.path().join("created"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(f.sent().is_empty());
}

#[test]
fn creation_and_recovery_refuse_terminal_replacement_during_process_observation() {
    for mode in ["resource", "resource-lost"] {
        let f = Fixture::new(mode);
        fs::write(f._root.path().join("replace-terminal"), "").unwrap();
        assert!(
            create_resource(
                &f.project,
                &f.operation.id,
                1,
                Instant::now() + Duration::from_secs(15),
                Default::default()
            )
            .is_err()
        );
        let before = runtime::snapshot(&f.project).unwrap();
        assert!(
            !before
                .events
                .iter()
                .any(|e| e.kind == "runtime.launch_target")
        );
        assert!(before.attempts[0].retains_capacity());
        if mode == "resource-lost" {
            assert!(
                reconcile_resource(
                    &f.project,
                    &f.operation.id,
                    before.deliveries[0].revision,
                    Instant::now() + Duration::from_secs(15),
                    Default::default()
                )
                .is_err()
            );
            assert_eq!(runtime::snapshot(&f.project).unwrap(), before);
        }
        assert_eq!(
            fs::read_to_string(f._root.path().join("created"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        assert!(f.sent().is_empty());
    }
}

#[test]
fn exit_before_any_identity_observation_keeps_uncertain_creation_reserved() {
    for mode in ["resource-lost","resource-release-workspace"] {
    let mut f = Fixture::new(mode);
    if mode.contains("workspace"){fs::write(f._root.path().join("lose-workspace-reply"),b"lost acknowledgment").unwrap();}
    assert!(
        create_resource(
            &f.project,
            &f.operation.id,
            1,
            Instant::now() + Duration::from_secs(15),
            Default::default()
        )
        .is_err()
    );
    f._worker.0.kill().unwrap();
    f._worker.0.wait().unwrap();
    fs::write(f._root.path().join("no-processes"), "").unwrap();
    let before = runtime::snapshot(&f.project).unwrap();
    assert!(
        reconcile_resource(
            &f.project,
            &f.operation.id,
            before.deliveries[0].revision,
            Instant::now() + Duration::from_secs(15),
            Default::default()
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(runtime::snapshot(&f.project).unwrap(), before);
    assert!(before.attempts[0].retains_capacity());
    assert!(
        !before
            .events
            .iter()
            .any(|e| e.kind == "runtime.launch_target")
    );
    let batch=crate::reconcile::ObservationBatch {
        expected_head:before.head,observations:before.runtime_bindings.iter().map(|binding|crate::reconcile::RuntimeObservation {
            binding:binding.id.clone(),binding_revision:binding.revision,
            task_revision:binding.task.as_ref().and_then(|id|before.tasks.iter().find(|t|&t.id==id).map(|t|t.revision)),
            observed_unix_ms:now(),collector:"herdr-git-v2".into(),config_digest:before.control.as_ref().unwrap().config_digest.clone(),
            pane:crate::reconcile::ResourceState::Absent,worktree:crate::reconcile::ResourceState::Absent,..Default::default()
        }).collect(),dispatch_allowed:false,recorded_head:None,
    };
    let config=before.control.as_ref().unwrap().config_digest.as_deref();
    let live=crate::reconcile::plan::build(&before,&batch,now(),config).unwrap();
    let launch=live.items.iter().find(|i|i.entity_kind=="operation"&&i.entity==f.operation.id.as_str()).unwrap();
    assert_eq!(launch.action,crate::reconcile::plan::RepairAction::Wait);
    let expired=crate::reconcile::plan::build(&before,&batch,before.deliveries[0].lease_until_ms.unwrap()+1,config).unwrap();
    for kind in ["attempt","operation"] {
        let item=expired.items.iter().find(|i|i.entity_kind==kind).unwrap();
        assert_eq!(item.action,crate::reconcile::plan::RepairAction::InspectLaunchIdentity);
        assert!(item.reason.contains("no exact process identity"));
    }
    let runtime=live.items.iter().find(|i|i.entity_kind=="runtime").unwrap();
    assert_eq!(runtime.action,crate::reconcile::plan::RepairAction::InspectLaunchIdentity);
    assert_eq!(expired.retained_attempts,1);assert!(!expired.dispatch_allowed);
    assert_eq!(runtime::snapshot(&f.project).unwrap(),before);

    assert_eq!(
        fs::read_to_string(f._root.path().join(if mode.contains("workspace"){"workspace-requests"}else{"created"}))
            .unwrap()
            .lines()
            .count(),
        1
    );
    }
}

#[test]
fn resource_preparation_enforces_profile_budget_before_claim_or_external_effect() {
    for (mode, expected) in [
        ("resource-small-input", "profile input budget"),
        (
            "resource-block-usage",
            "verified usage telemetry is required",
        ),
    ] {
        let f = Fixture::new(mode);
        let before = runtime::snapshot(&f.project).unwrap();
        let error = create_resource(
            &f.project,
            &f.operation.id,
            1,
            Instant::now() + Duration::from_secs(15),
            Default::default(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
        assert_eq!(runtime::snapshot(&f.project).unwrap(), before);
        assert!(before.approvals.iter().all(|a| a.consumed.is_none()));
        assert_eq!(before.deliveries[0].attempts, 0);
        assert!(!f._root.path().join("created").exists());
        assert!(f.sent().is_empty());
    }
}

fn retained_launch_claim(
    state: &crate::domain::Snapshot,
    operation: &OperationId,
) -> crate::operations::Claim {
    let d = state
        .deliveries
        .iter()
        .find(|d| &d.operation == operation)
        .unwrap();
    crate::operations::Claim {
        operation: operation.clone(),
        revision: d.revision,
        owner: d.owner.clone().unwrap(),
        epoch: d.epoch,
        lease_until_ms: d.lease_until_ms.unwrap(),
    }
}

#[test]
fn gate_release_boundary_is_one_use_and_does_not_confirm_a_start() {
    let f = Fixture::new("resource");
    let target = create_resource(
        &f.project,
        &f.operation.id,
        1,
        Instant::now() + Duration::from_secs(15),
        Default::default(),
    )
    .unwrap();
    let mut db = migration::open_active(&f.project).unwrap();
    let before = db.read_snapshot(None).unwrap();
    let claim = retained_launch_claim(&before, &f.operation.id);
    let receipt = PreparedLaunchStarted {
        receipt: LaunchStartedReceipt {
            version: 2,
            attempt: target.attempt.clone(),
            operation: target.operation.clone(),
            route: target.route.clone(),
            terminal: target.terminal.clone(),
            session: target.session.clone(),
            agent: AgentIdentity {
                kind: "claude".into(),
                name: worker_agent_name(&target.attempt),
            },
            supervisor: target.supervisor.clone(),
            observed_unix_ms: now(),
        },
    };
    assert!(
        db.record_launch_started(&claim, &receipt, now())
            .unwrap_err()
            .to_string()
            .contains("no durable release intent")
    );
    assert_eq!(db.read_snapshot(None).unwrap(), before);
    let release = PreparedLaunchRelease {
        intent: LaunchReleaseIntent {
            version: 1,
            target: target.clone(),
            observed_unix_ms: now(),
        },
    };
    db.record_launch_release(&claim, &release, now()).unwrap();
    let after = db.read_snapshot(None).unwrap();
    assert_eq!(after.deliveries, before.deliveries);
    assert_eq!(after.attempts, before.attempts);
    assert_eq!(after.approvals, before.approvals);
    assert!(after.ownership.is_empty());
    assert!(db.record_launch_release(&claim, &release, now()).is_err());
    assert_eq!(db.read_snapshot(None).unwrap(), after);
    assert!(
        !after
            .events
            .iter()
            .any(|e| e.kind == "runtime.launch_started")
    );
    db.cancel_attempt(
        &target.attempt,
        after.attempts[0].revision,
        after.head,
        "cancel after uncertain gate input",
        now(),
    )
    .unwrap();
    let cancelled = db.read_snapshot(None).unwrap();
    assert!(
        !reconcile_termination(
            &f.project,
            &target.attempt,
            cancelled.attempts[0].revision,
            Instant::now() + Duration::from_secs(5),
            Default::default()
        )
        .unwrap()
        .unwrap()
        .retains_capacity()
    );
    assert!(f.sent().is_empty());
}

#[test]
fn gate_release_refuses_changed_target_authority_and_failed_commit_is_atomic() {
    for case in ["target", "revoked", "expired", "cancelled", "commit"] {
        let f = Fixture::new("resource");
        let mut target = create_resource(
            &f.project,
            &f.operation.id,
            1,
            Instant::now() + Duration::from_secs(15),
            Default::default(),
        )
        .unwrap();
        let mut db = migration::open_active(&f.project).unwrap();
        let state = db.read_snapshot(None).unwrap();
        let claim = retained_launch_claim(&state, &f.operation.id);
        let raw = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
        match case {
            "target" => target.terminal = "foreign".into(),
            "revoked" => {db.revoke_approval(&state.approvals[0].reference.id, state.head, now(), "withdraw launch").unwrap();},
            "cancelled" => {db.cancel_attempt(&target.attempt, state.attempts[0].revision, state.head, "cancel", now()).unwrap();},
            "commit" => raw.execute_batch("CREATE TRIGGER refuse_release BEFORE INSERT ON events WHEN NEW.kind='runtime.launch_release' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap(),
            _ => (),
        }
        let before = db.read_snapshot(None).unwrap();
        let release = PreparedLaunchRelease {
            intent: LaunchReleaseIntent {
                version: 1,
                target,
                observed_unix_ms: now(),
            },
        };
        assert!(
            db.record_launch_release(
                &claim,
                &release,
                if case == "expired" {
                    claim.lease_until_ms
                } else {
                    now()
                }
            )
            .is_err()
        );
        assert_eq!(db.read_snapshot(None).unwrap(), before);
        if case == "commit" {
            raw.execute_batch("DROP TRIGGER refuse_release").unwrap();
            db.record_launch_release(&claim, &release, now()).unwrap();
        }
    }
}

#[test]
fn live_gate_proof_refuses_the_same_child_after_exec() {
    use std::io::Write;
    let mut f = Fixture::new("resource");
    let stage: Value =
        serde_json::from_slice(&fs::read(f._root.path().join("stage.json")).unwrap()).unwrap();
    let argv: Vec<String> = serde_json::from_value(stage["argv"].clone()).unwrap();
    let supervisor =
        crate::worker_supervision::SupervisorObservation::observe(f._worker.0.id(), &argv).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let gate = loop {
        match supervisor.waiting_gate(&argv) {
            Ok(gate) => break gate,
            Err(error) => {
                assert!(
                    Instant::now() < deadline && !supervisor.exited().unwrap(),
                    "gate did not become observable: {error:#}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };
    gate.check().unwrap();
    writeln!(
        f._worker.0.stdin.as_mut().unwrap(),
        "release-{}",
        f.operation.id.as_str()
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while gate.check().is_ok() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!supervisor.exited().unwrap());
    assert!(supervisor.waiting_gate(&argv).is_err());
}

#[test]
fn native_start_confirmation_requires_real_exec_and_exact_agent_then_replays_read_only() {
    use std::io::Write;
    for expire in [false, true] {
        let mut f = Fixture::new("resource");
        let target = create_resource(
            &f.project,
            &f.operation.id,
            1,
            Instant::now() + Duration::from_secs(15),
            Default::default(),
        )
        .unwrap();
        let mut db = migration::open_active(&f.project).unwrap();
        let state = db.read_snapshot(None).unwrap();
        let claim = retained_launch_claim(&state, &f.operation.id);
        db.record_launch_release(
            &claim,
            &PreparedLaunchRelease {
                intent: LaunchReleaseIntent {
                    version: 1,
                    target: target.clone(),
                    observed_unix_ms: now(),
                },
            },
            now(),
        )
        .unwrap();
        let agent = json!({"pane_id":target.route.pane_id,"tab_id":target.route.tab_id,
        "workspace_id":target.route.workspace_id,"cwd":target.route.cwd,"terminal_id":target.terminal,
        "agent":"claude","name":worker_agent_name(&target.attempt)});
        fs::write(
            f._root.path().join("agent.json"),
            serde_json::to_vec(&agent).unwrap(),
        )
        .unwrap();
        let before = db.read_snapshot(None).unwrap();
        // Herdr metadata alone cannot convert a still-waiting gate into a start.
        assert!(
            reconcile_start(
                &f.project,
                &f.operation.id,
                claim.revision,
                Instant::now() + Duration::from_secs(15),
                Default::default()
            )
            .is_err()
        );
        assert_eq!(db.read_snapshot(None).unwrap(), before);
        writeln!(
            f._worker.0.stdin.as_mut().unwrap(),
            "release-{}",
            f.operation.id.as_str()
        )
        .unwrap();
        let supervisor = crate::worker_supervision::SupervisorObservation::reconnect(
            target.supervisor.as_ref().unwrap(),
        )
        .unwrap();
        let profile = before.attempt_inputs[0]
            .inputs
            .effective_profile
            .as_ref()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while supervisor
            .agent_process(Path::new(&profile.agent.path), &profile.arguments_digest)
            .is_err()
        {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut foreign = agent.clone();
        foreign["terminal_id"] = json!("foreign");
        fs::write(
            f._root.path().join("agent.json"),
            serde_json::to_vec(&foreign).unwrap(),
        )
        .unwrap();
        assert!(
            reconcile_start(
                &f.project,
                &f.operation.id,
                claim.revision,
                Instant::now() + Duration::from_secs(15),
                Default::default()
            )
            .is_err()
        );
        assert_eq!(db.read_snapshot(None).unwrap(), before);
        fs::write(
            f._root.path().join("agent.json"),
            serde_json::to_vec(&agent).unwrap(),
        )
        .unwrap();
        for pending in [json!(true), json!(null), json!("false")] {
            let mut invalid = agent.clone();
            invalid["launch_pending"] = pending;
            fs::write(
                f._root.path().join("agent.json"),
                serde_json::to_vec(&invalid).unwrap(),
            )
            .unwrap();
            assert!(
                reconcile_start(
                    &f.project,
                    &f.operation.id,
                    claim.revision,
                    Instant::now() + Duration::from_secs(15),
                    Default::default()
                )
                .is_err()
            );
            assert_eq!(db.read_snapshot(None).unwrap(), before);
        }
        fs::write(
            f._root.path().join("agent.json"),
            serde_json::to_vec(&agent).unwrap(),
        )
        .unwrap();
        if expire {
            db.expire_claims(claim.lease_until_ms).unwrap();
        }
        let state = db.read_snapshot(None).unwrap();
        db.revoke_approval(
            &state.approvals[0].reference.id,
            state.head,
            now(),
            "no new effects",
        )
        .unwrap();
        fs::write(&profile.config.path, "# changed since the original launch").unwrap();
        let state = db.read_snapshot(None).unwrap();
        let mut budget = crate::store::identity_inventory::Budget::new(
            2 * 1024 * 1024,
            1024,
            Instant::now() + Duration::from_secs(1),
            Default::default(),
        )
        .unwrap();
        let hints = (0..2)
            .map(|turn| {
                migration::read_controller_effect_hint(&f.project, &mut budget, turn, now())
                    .unwrap()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(hints.iter().any(|h| h.operation.id == f.operation.id
            && h.mode == crate::store::controller_hint::EffectMode::Observe));
        let raw = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
        raw.execute_batch("CREATE TRIGGER refuse_start BEFORE INSERT ON events WHEN NEW.kind='runtime.launch_started' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(
            reconcile_launch(
                &f.project,
                &f.operation.id,
                state.deliveries[0].revision,
                Instant::now() + Duration::from_secs(15),
                Default::default()
            )
            .is_err()
        );
        assert_eq!(db.read_snapshot(None).unwrap(), state);
        raw.execute_batch("DROP TRIGGER refuse_start").unwrap();
        assert!(
            reconcile_launch(
                &f.project,
                &f.operation.id,
                state.deliveries[0].revision,
                Instant::now() + Duration::from_secs(15),
                Default::default()
            )
            .unwrap()
        );
        let observed = db.read_snapshot(None).unwrap();
        let receipt = reconcile_start(
            &f.project,
            &f.operation.id,
            observed.deliveries[0].revision,
            Instant::now() + Duration::from_secs(15),
            Default::default(),
        )
        .unwrap();
        assert_eq!(receipt.supervisor, target.supervisor);
        let after = db.read_snapshot(None).unwrap();
        assert_eq!(after.attempts[0].state, AttemptState::Launching);
        assert!(after.attempts[0].retains_capacity());
        assert_eq!(after.ownership.len(), 1);
        assert_eq!(after.approvals, state.approvals);
        assert_eq!(
            reconcile_start(
                &f.project,
                &f.operation.id,
                after.deliveries[0].revision,
                Instant::now() + Duration::from_secs(15),
                Default::default()
            )
            .unwrap(),
            receipt
        );
        assert_eq!(db.read_snapshot(None).unwrap(), after);
        assert!(f.sent().is_empty());
    }
}

#[test]
fn native_gate_submission_is_once_even_when_the_reply_is_lost() {
    use std::io::Write;
    for mode in ["resource-release", "resource-release-lost"] {
        let mut f = Fixture::new(mode);
        let target = create_resource(
            &f.project,
            &f.operation.id,
            1,
            Instant::now() + Duration::from_secs(15),
            Default::default(),
        )
        .unwrap();
        let state = runtime::snapshot(&f.project).unwrap();
        let revision = state.deliveries[0].revision;
        let mut input = f._worker.0.stdin.take().unwrap();
        let result = std::thread::scope(|scope| {
            scope.spawn(|| {
                let deadline = Instant::now() + Duration::from_secs(5);
                let path = f._root.path().join("gate-requests");
                while !path.exists() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                let request: Value = serde_json::from_str(
                    fs::read_to_string(&path).unwrap().lines().next().unwrap(),
                )
                .unwrap();
                assert_eq!(request["params"]["pane_id"], target.route.pane_id);
                assert_eq!(
                    request["params"]["text"],
                    format!("release-{}\n", f.operation.id.as_str())
                );
                input
                    .write_all(request["params"]["text"].as_str().unwrap().as_bytes())
                    .unwrap();
                fs::write(f._root.path().join("gate-sent"), "").unwrap();
            });
            release_gate(
                &f.project,
                &f.operation.id,
                revision,
                Instant::now() + Duration::from_secs(15),
                Default::default(),
            )
        });
        assert_eq!(result.is_ok(), mode == "resource-release", "{result:?}");
        let after = runtime::snapshot(&f.project).unwrap();
        assert!(
            after
                .events
                .iter()
                .any(|e| e.kind == "runtime.launch_release")
        );
        assert!(
            !after
                .events
                .iter()
                .any(|e| e.kind == "runtime.launch_started")
        );
        assert_eq!(after.attempts, state.attempts);
        assert!(after.ownership.is_empty());
        assert!(
            release_gate(
                &f.project,
                &f.operation.id,
                revision,
                Instant::now() + Duration::from_secs(15),
                Default::default()
            )
            .is_err()
        );
        assert_eq!(runtime::snapshot(&f.project).unwrap(), after);
        assert_eq!(
            fs::read_to_string(f._root.path().join("gate-requests"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        fs::write(f._root.path().join("agent.json"),serde_json::to_vec(&json!({
            "pane_id":target.route.pane_id,"tab_id":target.route.tab_id,"workspace_id":target.route.workspace_id,
            "cwd":target.route.cwd,"terminal_id":target.terminal,"agent":"claude","name":worker_agent_name(&target.attempt)
        })).unwrap()).unwrap();
        let agent_path = f._root.path().join("agent.json");
        let mut agent: Value = serde_json::from_slice(&fs::read(&agent_path).unwrap()).unwrap();
        agent["name"] = Value::Null;
        fs::write(&agent_path, serde_json::to_vec(&agent).unwrap()).unwrap();
        let unnamed = runtime::snapshot(&f.project).unwrap();
        assert!(
            reconcile_start(
                &f.project,
                &f.operation.id,
                revision,
                Instant::now() + Duration::from_secs(15),
                Default::default()
            )
            .is_err()
        );
        assert_eq!(runtime::snapshot(&f.project).unwrap(), unnamed);
        // Foreign names are never overwritten, even with valid launch authority.
        agent["name"] = json!("someone-else");
        fs::write(&agent_path, serde_json::to_vec(&agent).unwrap()).unwrap();
        assert!(
            name_started_agent(
                &f.project,
                &f.operation.id,
                revision,
                Instant::now() + Duration::from_secs(15),
                Default::default()
            )
            .is_err()
        );
        assert!(!f._root.path().join("name-requests").exists());
        agent["name"] = Value::Null;
        fs::write(&agent_path, serde_json::to_vec(&agent).unwrap()).unwrap();
        let mut db = migration::open_active(&f.project).unwrap();
        rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap().execute_batch("CREATE TRIGGER refuse_name BEFORE INSERT ON events WHEN NEW.kind='runtime.launch_name' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        let before_name = db.read_snapshot(None).unwrap();
        assert!(
            name_started_agent(
                &f.project,
                &f.operation.id,
                revision,
                Instant::now() + Duration::from_secs(15),
                Default::default()
            )
            .is_err()
        );
        assert_eq!(db.read_snapshot(None).unwrap(), before_name);
        assert!(!f._root.path().join("name-requests").exists());
        rusqlite::Connection::open(f.project.join(".state/state.db"))
            .unwrap()
            .execute_batch("DROP TRIGGER refuse_name;")
            .unwrap();
        if mode == "resource-release-lost" {
            fs::write(f._root.path().join("lose-name-reply"), "").unwrap();
        }
        let named = name_started_agent(
            &f.project,
            &f.operation.id,
            revision,
            Instant::now() + Duration::from_secs(15),
            Default::default(),
        );
        assert_eq!(named.is_ok(), mode == "resource-release", "{named:?}");
        assert_eq!(
            fs::read_to_string(f._root.path().join("name-requests"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        if mode == "resource-release-lost" {
            // An uncertain rename is not retried even if the name disappears.
            let named_agent = fs::read(&agent_path).unwrap();
            fs::write(&agent_path, serde_json::to_vec(&agent).unwrap()).unwrap();
            let before = runtime::snapshot(&f.project).unwrap();
            assert!(
                name_started_agent(
                    &f.project,
                    &f.operation.id,
                    revision,
                    Instant::now() + Duration::from_secs(15),
                    Default::default()
                )
                .is_err()
            );
            assert_eq!(runtime::snapshot(&f.project).unwrap(), before);
            assert_eq!(
                fs::read_to_string(f._root.path().join("name-requests"))
                    .unwrap()
                    .lines()
                    .count(),
                1
            );
            fs::write(&agent_path, named_agent).unwrap();
        }
        // Recovery observes the assigned name after a lost reply without replay.
        let revision = runtime::snapshot(&f.project).unwrap().deliveries[0].revision;
        assert!(
            reconcile_launch(
                &f.project,
                &f.operation.id,
                revision,
                Instant::now() + Duration::from_secs(15),
                Default::default()
            )
            .unwrap()
        );
        assert_eq!(
            runtime::snapshot(&f.project).unwrap().attempts[0].state,
            AttemptState::Launching
        );
        assert!(f.sent().is_empty());
    }
}

#[test]
fn native_gate_refusal_does_not_consume_the_release_opportunity_or_send_input() {
    for case in [
        "home",
        "home-mode",
        "config",
        "revoked",
        "terminal",
        "commit",
    ] {
        let f = Fixture::new(if case == "home" {
            "resource"
        } else {
            "resource-release"
        });
        create_resource(
            &f.project,
            &f.operation.id,
            1,
            Instant::now() + Duration::from_secs(15),
            Default::default(),
        )
        .unwrap();
        let mut db = migration::open_active(&f.project).unwrap();
        let state = db.read_snapshot(None).unwrap();
        let profile = state.attempt_inputs[0]
            .inputs
            .effective_profile
            .as_ref()
            .unwrap();
        match case {
            "home-mode" => fs::set_permissions(profile.execution_home.as_ref().unwrap(),fs::Permissions::from_mode(0o777)).unwrap(),
            "config" => fs::write(&profile.config.path,"# changed").unwrap(),
            "revoked" => {db.revoke_approval(&state.approvals[0].reference.id,state.head,now(),"withdraw gate authority").unwrap();},
            "terminal" => fs::write(f._root.path().join("replace-terminal"),"").unwrap(),
            "commit" => rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap()
                .execute_batch("CREATE TRIGGER refuse_native_release BEFORE INSERT ON events WHEN NEW.kind='runtime.launch_release' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap(),
            _ => (),
        }
        let before = db.read_snapshot(None).unwrap();
        assert!(
            release_gate(
                &f.project,
                &f.operation.id,
                before.deliveries[0].revision,
                Instant::now() + Duration::from_secs(15),
                Default::default()
            )
            .is_err(),
            "{case}"
        );
        assert_eq!(db.read_snapshot(None).unwrap(), before, "{case}");
        assert!(!f._root.path().join("gate-requests").exists(), "{case}");
        assert!(
            !before
                .events
                .iter()
                .any(|e| e.kind == "runtime.launch_release")
        );
    }
}

#[test]
fn launch_advancement_recovers_each_boundary_then_delivers_brief_and_stops() {
    use std::io::Write;
    for lost in ["none", "creation", "release", "name", "workspace", "repository", "repository_creation", "historical_workspace"] {
        let mut f = Fixture::new(if lost.starts_with("repository") {
            "resource-release-workspace-repository"
        } else if lost == "release" {
            "resource-release-lost"
        } else if matches!(lost,"workspace"|"historical_workspace") {
            "resource-release-workspace"
        } else {
            "resource-release"
        });
        if lost == "creation" {
            fs::write(f._root.path().join("lose-creation-reply"), "").unwrap();
        }
        if lost == "name" {
            fs::write(f._root.path().join("lose-name-reply"), "").unwrap();
        }
        if lost == "repository_creation" {fs::write(f._root.path().join("lose-workspace-reply"), "").unwrap();}
        let mut input = f._worker.0.stdin.take().unwrap();
        let initial = runtime::snapshot(&f.project).unwrap();
        let attempt = initial.attempts[0].id.clone();
        let deadline = Instant::now() + Duration::from_secs(15);
        if lost=="historical_workspace" {
            resources::create_legacy_resource(&f.project,&f.operation.id,1,deadline,Default::default()).unwrap();
        }
        let receipt = std::thread::scope(|scope| {
            scope.spawn(|| {
                let request = f._root.path().join("gate-requests");
                while !request.exists() {
                    assert!(Instant::now() < deadline, "gate request missing: {lost}");
                    std::thread::sleep(Duration::from_millis(10));
                }
                let request: Value = serde_json::from_str(
                    fs::read_to_string(request).unwrap().lines().next().unwrap(),
                )
                .unwrap();
                input
                    .write_all(request["params"]["text"].as_str().unwrap().as_bytes())
                    .unwrap();
                let stage: Value =
                    serde_json::from_slice(&fs::read(f._root.path().join("stage.json")).unwrap())
                        .unwrap();
                let argv: Vec<String> = serde_json::from_value(stage["argv"].clone()).unwrap();
                let supervisor = crate::worker_supervision::SupervisorObservation::observe(
                    f._worker.0.id(),
                    &argv,
                )
                .unwrap();
                let profile = initial.attempt_inputs[0]
                    .inputs
                    .effective_profile
                    .as_ref()
                    .unwrap();
                while supervisor
                    .agent_process(Path::new(&profile.agent.path), &profile.arguments_digest)
                    .is_err()
                {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                fs::write(
                    f._root.path().join("agent.json"),
                    serde_json::to_vec(&json!({
                        "pane_id":"w1:p1", "tab_id":"w1:t1", "workspace_id":"w1",
                        "cwd":if lost.starts_with("repository") {worktree_plans(&initial.attempt_inputs[0].inputs,&attempt).unwrap()[0].path.clone()}else{f.project.display().to_string()}, "terminal_id":"term1", "agent":"claude", "name":null,
                        "interactive_ready":true, "agent_status":"idle"
                    }))
                    .unwrap(),
                )
                .unwrap();
                fs::write(f._root.path().join("gate-sent"), "").unwrap();
            });
            let mut budget=crate::store::identity_inventory::Budget::new(2*1024*1024,1024,deadline,Default::default()).unwrap();
            let hint=migration::read_controller_dispatch_hint(&f.project,&mut budget,0,now(),true).unwrap().unwrap();
            assert_eq!(hint.mode,crate::store::controller_hint::EffectMode::Deliver);
            assert_eq!(hint.operation.id,f.operation.id);
            let first = advance_launch(&f.project, &hint.operation.id, hint.delivery_revision, deadline, Default::default());
            if matches!(lost, "none" | "workspace" | "repository" | "historical_workspace") {
                return first.unwrap_or_else(|e|panic!("{lost}: {e:#}")).unwrap();
            }
            assert!(first.is_err(), "expected lost {lost} acknowledgment");
            let state = runtime::snapshot(&f.project).unwrap();
            let delivery = state
                .deliveries
                .iter()
                .find(|d| d.operation == f.operation.id)
                .unwrap();
            let hint=(0..4).find_map(|turn| {
                let mut budget=crate::store::identity_inventory::Budget::new(2*1024*1024,1024,deadline,Default::default()).unwrap();
                migration::read_controller_dispatch_hint(&f.project,&mut budget,turn,now(),true).unwrap().filter(|h|h.operation.id==f.operation.id)
            }).expect("controller must resume the original live launch");
            assert_eq!(hint.delivery_revision,delivery.revision);
            assert_eq!(hint.mode,crate::store::controller_hint::EffectMode::Deliver);
            advance_launch(
                &f.project,
                &hint.operation.id,
                hint.delivery_revision,
                deadline,
                Default::default(),
            )
            .unwrap()
            .unwrap()
        });
        if lost.starts_with("repository") {
            let state=runtime::snapshot(&f.project).unwrap();let tree=worktree_plans(&initial.attempt_inputs[0].inputs,&attempt).unwrap().remove(0);
            assert_eq!(receipt.route.cwd,tree.path);assert_eq!(state.runtime_bindings[0].identity.worktree_path,tree.path);
            assert_eq!(state.runtime_bindings[0].identity.branch,tree.branch);assert!(state.ownership[0].worktree.is_some());
            assert_eq!(state.deliveries[0].attempts,1);assert_eq!(state.approvals.iter().filter(|a|a.consumed.is_some()).count(),1);
            assert_eq!(fs::read_to_string(Path::new(&tree.path).join("source.txt")).unwrap(),"approved base\n");
            assert!(crate::memory::render_attempt_brief(&f.project,attempt.as_str()).unwrap().text.contains(&tree.path));
        }
        assert_eq!(receipt.attempt, attempt);
        let state = runtime::snapshot(&f.project).unwrap();
        let revision = state
            .deliveries
            .iter()
            .find(|d| d.operation == f.operation.id)
            .unwrap()
            .revision;
        assert_eq!(
            advance_launch(
                &f.project,
                &f.operation.id,
                revision,
                deadline,
                Default::default()
            )
            .unwrap(),
            Some(receipt)
        );
        assert_eq!(runtime::snapshot(&f.project).unwrap(), state);
        for kind in [
            "runtime.launch_creation",
            "runtime.launch_release",
            "runtime.launch_name",
            "runtime.launch_started",
        ] {
            assert_eq!(
                state.events.iter().filter(|e| e.kind == kind).count(),
                1,
                "{lost}: {kind}"
            );
        }
        if lost == "workspace" || lost.starts_with("repository") {
            assert!(f._root.path().join("direct-created").exists());
            assert!(!f._root.path().join("created").exists());
        } else {
            assert_eq!(fs::read_to_string(f._root.path().join("created")).unwrap(), "created\\n");
        }
        for file in ["gate-requests", "name-requests"] {
            assert_eq!(
                fs::read_to_string(f._root.path().join(file))
                    .unwrap()
                    .lines()
                    .count(),
                1,
                "{lost}: {file}"
            );
        }
        let brief = prepare_brief(
            &f.project,
            &attempt,
            state.attempts[0].revision,
            deadline,
            Default::default(),
        )
        .unwrap();
        deliver_brief(&f.project, &brief.id, 1, deadline, Default::default()).unwrap();
        assert_eq!(f.sent().len(), 1);
        let output=worker_output_path(&initial.attempt_inputs[0].inputs,&attempt).unwrap();
        assert_eq!(state.runtime_bindings[0].identity.thread_dir,output);
        let rendered=crate::memory::render_attempt_brief(&f.project,attempt.as_str()).unwrap();
        assert_eq!(rendered.output_directory,output);assert!(rendered.text.contains("report.md"));
        assert!(rendered.text.contains(&serde_json::to_string(&output).unwrap()));
        fs::create_dir_all(&output).unwrap();
        let artifact=Path::new(&output).join("report.md");
        fs::write(&artifact, "keep this result").unwrap();
        fs::create_dir(Path::new(&output).join("library")).unwrap();
        fs::write(Path::new(&output).join("library/partial.bin"),[0,255,17]).unwrap();
        fs::write(Path::new(&output).join(".git"),b"ordinary output data").unwrap();
        let mut db = migration::open_active(&f.project).unwrap();
        let state = db.read_snapshot(None).unwrap();
        if lost.starts_with("repository") {
            assert!(crate::worktree_preservation::capture_stopped_repository(&f.project,&attempt,state.head,Instant::now()+Duration::from_secs(10),Default::default()).is_err());
            assert_eq!(db.read_snapshot(None).unwrap(),state);
            assert!(!f.project.join(".state/worktree-file-snapshots").exists());
            let tree=Path::new(&state.runtime_bindings[0].identity.worktree_path);
            fs::write(tree.join("partial.bin"),[0,255,1,2]).unwrap();
            fs::set_permissions(tree.join("partial.bin"),fs::Permissions::from_mode(0o755)).unwrap();
            fs::create_dir(tree.join("empty-output")).unwrap();
        }
        db.cancel_attempt(
            &attempt,
            state.attempts[0].revision,
            state.head,
            "workflow complete",
            now(),
        )
        .unwrap();
        let state = db.read_snapshot(None).unwrap();
        if matches!(lost,"none"|"name"|"release") {
            let destination=f.project.join(".state/worker-output-snapshots");
            let moved=Path::new(&output).with_extension("retained-fixture");
            let raw=rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
            if lost=="none" {fs::rename(&output,&moved).unwrap();std::os::unix::fs::symlink(&moved,&output).unwrap();}
            else if lost=="name" {std::os::unix::fs::symlink(f._root.path(),&destination).unwrap();}
            else {raw.execute_batch("CREATE TRIGGER reject_output_stop BEFORE INSERT ON events WHEN NEW.kind='runtime.worker_terminated' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();}
            assert!(reconcile_termination(&f.project,&attempt,state.attempts[0].revision,deadline,Default::default()).is_err());
            assert_eq!(db.read_snapshot(None).unwrap(),state);assert!(state.attempts[0].retains_capacity());
            if lost=="none" {fs::remove_file(&output).unwrap();fs::rename(&moved,&output).unwrap();}
            else if lost=="name" {fs::remove_file(&destination).unwrap();}
            else {assert_eq!(fs::read_dir(destination.join(attempt.as_str())).unwrap().count(),1);raw.execute_batch("DROP TRIGGER reject_output_stop").unwrap();}
        }
        if lost.starts_with("repository") {
            let destination=f.project.join(".state/worktree-file-snapshots");
            let raw=rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
            if lost=="repository" {
                std::os::unix::fs::symlink(&f._root.path(),&destination).unwrap();
            }else{
                raw.execute_batch("CREATE TRIGGER reject_preserved_stop BEFORE INSERT ON events WHEN NEW.kind='runtime.worker_terminated' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
            }
            assert!(reconcile_termination(&f.project,&attempt,state.attempts[0].revision,deadline,Default::default()).is_err());
            assert_eq!(db.read_snapshot(None).unwrap(),state);
            assert!(state.attempts[0].retains_capacity());
            assert_eq!(fs::read_to_string(&artifact).unwrap(),"keep this result");
            if lost=="repository" {fs::remove_file(&destination).unwrap();}
            else {
                assert_eq!(fs::read_dir(destination.join(attempt.as_str())).unwrap().count(),1);
                raw.execute_batch("DROP TRIGGER reject_preserved_stop").unwrap();
            }
        }
        let mut done = reconcile_termination(
            &f.project,
            &attempt,
            state.attempts[0].revision,
            deadline,
            Default::default(),
        );
        if lost=="historical_workspace" {
            assert!(format!("{:#}",done.unwrap_err()).contains("bootstrap quiescence"));
            assert_eq!(db.read_snapshot(None).unwrap(),state);
            assert!(state.attempts[0].retains_capacity());
            assert!(!state.attempts[0].termination_observed);
            assert_eq!(fs::read_to_string(&artifact).unwrap(),"keep this result");
            assert!(reconcile_termination(&f.project,&attempt,state.attempts[0].revision,deadline,Default::default()).is_err());
            assert_eq!(db.read_snapshot(None).unwrap(),state);
            let previous=previous_boot_fixture(&f,true);
            let before=db.read_snapshot(None).unwrap();
            let raw=rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
            raw.execute_batch("CREATE TRIGGER reject_reboot_receipt BEFORE INSERT ON events WHEN NEW.kind='runtime.worker_terminated' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
            assert!(reconcile_termination(&f.project,&attempt,before.attempts[0].revision,deadline,Default::default()).is_err());
            assert_eq!(db.read_snapshot(None).unwrap(),before);
            raw.execute_batch("DROP TRIGGER reject_reboot_receipt").unwrap();
            done=reconcile_termination(&f.project,&attempt,before.attempts[0].revision,deadline,Default::default());
            let after=db.read_snapshot(None).unwrap();
            let event=after.events.iter().find(|e|e.kind=="runtime.worker_terminated").unwrap();
            assert_eq!(event.payload["host_reboot"]["previous_boot_id"],previous);
            assert_ne!(event.payload["host_reboot"]["current_boot_id"],previous);
            assert_eq!(after.ownership,before.ownership);
            reconcile_termination(&f.project,&attempt,after.attempts[0].revision,deadline,Default::default()).unwrap().unwrap();
            assert_eq!(db.read_snapshot(None).unwrap(),after);
        }
        let done=done.unwrap().unwrap();
        assert!(done.termination_observed);
        assert!(!done.retains_capacity());
        assert_eq!(fs::read_to_string(artifact).unwrap(), "keep this result");
        let preserved=runtime::snapshot(&f.project).unwrap();
        let stop:WorkerTerminationReceipt=serde_json::from_value(preserved.events.iter().find(|e|e.kind=="runtime.worker_terminated").unwrap().payload.clone()).unwrap();
        let outputs=stop.output_snapshot.unwrap();assert_eq!(outputs.source,output);
        let directory=f.project.join(".state/worker-output-snapshots").join(attempt.as_str()).join(outputs.digest.unwrap());
        let manifest:crate::worktree_preservation::OutputManifest=serde_json::from_slice(&fs::read(directory.join("manifest.json")).unwrap()).unwrap();
        for (name,bytes) in [("report.md",b"keep this result".as_slice()),("library/partial.bin",&[0,255,17]),(".git",b"ordinary output data".as_slice())] {
            let entry=manifest.entries.iter().find(|e|e.path==name).unwrap();
            assert_eq!(fs::read(directory.join(&entry.sha256)).unwrap(),bytes);
        }
        assert_eq!(fs::read_dir(f.project.join(".state/worker-output-snapshots").join(attempt.as_str())).unwrap().count(),1);
        let binding=&preserved.runtime_bindings[0];
        let finalization=crate::operations::finalization::Finalization {
            authority:"operator.artifact_finalization".into(),binding:binding.id.clone(),binding_revision:binding.revision,
            control_epoch:preserved.control.as_ref().unwrap().epoch,config:initial.attempt_inputs[0].inputs.config.clone(),
            source:binding.identity.thread_dir.clone(),report_hash:crate::operations::finalization::digest(b"keep this result"),reason:"preserve cancelled worker output".into(),
        };
        finalization.clone().operation(&preserved,now()).unwrap();
        let control=crate::source_tree::Control::default();
        fs::remove_dir_all(&output).unwrap();
        let loaded=crate::worktree_preservation::load_binding_outputs(&f.project,&preserved,binding,&control).unwrap().unwrap();
        assert_eq!(loaded.report_hash(),Some(finalization.report_hash.as_str()));
        let entry=loaded.manifest().entries.iter().find(|e|e.path=="library/partial.bin").unwrap();
        assert_eq!(loaded.bytes(entry).unwrap(),[0,255,17]);
        let mut changed=preserved.clone();changed.attempts[0].termination_observed=false;
        assert!(crate::worktree_preservation::load_binding_outputs(&f.project,&changed,binding,&control).is_err());
        let event=changed.events.iter_mut().find(|e|e.kind=="runtime.worker_terminated").unwrap();event.payload["output_snapshot"]["source"]=serde_json::json!("/unrelated");
        changed.attempts[0].termination_observed=true;
        assert!(crate::worktree_preservation::load_binding_outputs(&f.project,&changed,binding,&control).is_err());
        fs::write(directory.join(&entry.sha256),b"corrupt").unwrap();
        assert!(crate::worktree_preservation::load_binding_outputs(&f.project,&preserved,binding,&control).is_err());

        if lost.starts_with("repository") {
            let tree=Path::new(&binding.identity.worktree_path);
            let captures=crate::worktree_preservation::capture_stopped_repository(&f.project,&attempt,preserved.head,Instant::now()+Duration::from_secs(10),Default::default()).unwrap();
            assert_eq!(captures.len(),1);
            let capture=&captures[0];
            assert_eq!(capture.manifest.scope,"repository_state");
            assert!(capture.manifest.git.is_some());
            let stopped:WorkerTerminationReceipt=serde_json::from_value(preserved.events.iter().find(|e|e.kind=="runtime.worker_terminated").unwrap().payload.clone()).unwrap();
            assert_eq!(stopped.repository_snapshots,vec![WorktreeSnapshotReference{plan:capture.manifest.worktree.plan.clone(),digest:capture.digest.clone()}]);
            assert_eq!(fs::read_dir(f.project.join(".state/worktree-file-snapshots").join(attempt.as_str())).unwrap().count(),1);
            let entry=capture.manifest.entries.iter().find(|e|e.path=="partial.bin").unwrap();
            assert!(entry.executable);
            assert_eq!(fs::read(capture.directory.join(&entry.sha256)).unwrap(),[0,255,1,2]);
            assert!(capture.manifest.entries.iter().any(|e|e.path=="empty-output" && e.directory));
            assert!(!capture.manifest.entries.iter().any(|e|e.path==".git"));
            let repeated=crate::worktree_preservation::capture_stopped_repository(&f.project,&attempt,preserved.head,Instant::now()+Duration::from_secs(10),Default::default()).unwrap();
            assert_eq!(repeated[0].digest,capture.digest);
            assert_eq!(runtime::snapshot(&f.project).unwrap(),preserved);
            fs::write(capture.directory.join(&entry.sha256),b"bad!").unwrap();
            assert!(crate::worktree_preservation::capture_stopped_repository(&f.project,&attempt,preserved.head,Instant::now()+Duration::from_secs(10),Default::default()).is_err());
            assert_eq!(fs::read(tree.join("partial.bin")).unwrap(),[0,255,1,2]);
            assert_eq!(runtime::snapshot(&f.project).unwrap(),preserved);
        }
        if lost == "workspace" {
            let mut budget = crate::store::identity_inventory::Budget::new(
                2 * 1024 * 1024,
                1024,
                Instant::now() + Duration::from_secs(2),
                Default::default(),
            )
            .unwrap();
            let resources =
                migration::read_launch_target_inventory(&f.project, &mut budget).unwrap();
            assert!(
                resources.iter().any(
                    |(_, target)| target.route.pane_id == "w1:p1" && target.terminal == "term1"
                ),
                "created workspace ownership must survive worker termination"
            );
            assert_eq!(
                fs::read_to_string(f._root.path().join("workspace-requests"))
                    .unwrap()
                    .lines()
                    .count(),
                1
            );
        }
    }
}

#[test]
fn staged_repository_stop_requires_and_records_preserved_partial_files() {
    let f=Fixture::new("resource-release-workspace-repository");
    let deadline=Instant::now()+Duration::from_secs(15);
    let target=create_resource(&f.project,&f.operation.id,1,deadline,Default::default()).unwrap();
    let mut db=migration::open_active(&f.project).unwrap();let state=db.read_snapshot(None).unwrap();
    let plan=worktree_plans(&state.attempt_inputs[0].inputs,&target.attempt).unwrap().remove(0);
    fs::write(Path::new(&plan.path).join("partial.txt"),b"pre-start partial result").unwrap();
    db.cancel_attempt(&target.attempt,state.attempts[0].revision,state.head,"stop staged repository",now()).unwrap();
    let before=db.read_snapshot(None).unwrap();
    let incomplete=PreparedLaunchStopped{receipt:LaunchStoppedReceipt {
        version:1,target:target.clone(),host_reboot:None,repository_snapshots:vec![],output_snapshot:None,observed_unix_ms:now(),
    }};
    let error=db.record_launch_stopped(&incomplete,before.attempts[0].revision,before.head,now()).unwrap_err();
    assert!(error.to_string().contains("preservation"));assert_eq!(db.read_snapshot(None).unwrap(),before);
    let stopped=reconcile_termination(&f.project,&target.attempt,before.attempts[0].revision,deadline,Default::default()).unwrap().unwrap();
    assert!(stopped.termination_observed && !stopped.retains_capacity());
    let state=db.read_snapshot(None).unwrap();
    let receipt:LaunchStoppedReceipt=serde_json::from_value(state.events.iter().find(|e|e.kind=="runtime.launch_stopped").unwrap().payload.clone()).unwrap();
    let outputs=receipt.output_snapshot.as_ref().unwrap();assert!(outputs.digest.is_none());
    assert_eq!(outputs.source,worker_output_path(&state.attempt_inputs[0].inputs,&target.attempt).unwrap());
    assert_eq!(receipt.repository_snapshots.len(),1);assert_eq!(receipt.repository_snapshots[0].plan,plan);
    let directory=f.project.join(".state/worktree-file-snapshots").join(target.attempt.as_str()).join(&receipt.repository_snapshots[0].digest);
    let manifest:crate::worktree_preservation::Manifest=serde_json::from_slice(&fs::read(directory.join("manifest.json")).unwrap()).unwrap();
    let entry=manifest.entries.iter().find(|e|e.path=="partial.txt").unwrap();
    assert_eq!(fs::read(directory.join(&entry.sha256)).unwrap(),b"pre-start partial result");
    assert_eq!(fs::read(Path::new(&plan.path).join("partial.txt")).unwrap(),b"pre-start partial result");
    reconcile_termination(&f.project,&target.attempt,stopped.revision,deadline,Default::default()).unwrap().unwrap();
    assert_eq!(db.read_snapshot(None).unwrap(),state);
}

#[test]
fn staged_stop_requires_output_evidence_and_preserves_an_empty_directory() {
    let f=Fixture::new("resource-release");let deadline=Instant::now()+Duration::from_secs(10);
    let target=create_resource(&f.project,&f.operation.id,1,deadline,Default::default()).unwrap();
    let mut db=migration::open_active(&f.project).unwrap();let state=db.read_snapshot(None).unwrap();
    let source=worker_output_path(&state.attempt_inputs[0].inputs,&target.attempt).unwrap();fs::create_dir_all(&source).unwrap();
    db.cancel_attempt(&target.attempt,state.attempts[0].revision,state.head,"stop empty outputs",now()).unwrap();
    let before=db.read_snapshot(None).unwrap();
    for output in [None,Some(AttemptOutputReference{source:"/unrelated".into(),digest:None}),Some(AttemptOutputReference{source:source.clone(),digest:Some("invalid".into())})] {
        let incomplete=PreparedLaunchStopped{receipt:LaunchStoppedReceipt{version:1,target:target.clone(),host_reboot:None,repository_snapshots:vec![],output_snapshot:output,observed_unix_ms:now()}};
        let error=db.record_launch_stopped(&incomplete,before.attempts[0].revision,before.head,now()).unwrap_err();
        assert!(error.to_string().contains("output preservation"));assert_eq!(db.read_snapshot(None).unwrap(),before);
    }
    reconcile_termination(&f.project,&target.attempt,before.attempts[0].revision,deadline,Default::default()).unwrap().unwrap();
    let state=db.read_snapshot(None).unwrap();
    let receipt:LaunchStoppedReceipt=serde_json::from_value(state.events.iter().find(|e|e.kind=="runtime.launch_stopped").unwrap().payload.clone()).unwrap();
    let evidence=receipt.output_snapshot.unwrap();assert_eq!(evidence.source,source);
    let manifest=f.project.join(".state/worker-output-snapshots").join(target.attempt.as_str()).join(evidence.digest.unwrap()).join("manifest.json");
    let manifest:crate::worktree_preservation::OutputManifest=serde_json::from_slice(&fs::read(manifest).unwrap()).unwrap();
    assert!(manifest.entries.is_empty());assert!(Path::new(&source).is_dir());
}

#[test]
fn launch_advancement_refuses_unusable_environment_before_creation() {
    for mode in ["resource", "resource-release"] {
        let f = Fixture::new(mode);
        if mode == "resource-release" {
            fs::set_permissions(
                f._root.path().join("agent-home"),
                fs::Permissions::from_mode(0o777),
            )
            .unwrap();
        }
        let before = runtime::snapshot(&f.project).unwrap();
        assert!(
            advance_launch(
                &f.project,
                &f.operation.id,
                1,
                Instant::now() + Duration::from_secs(5),
                Default::default()
            )
            .is_err()
        );
        assert_eq!(runtime::snapshot(&f.project).unwrap(), before);
        for file in ["created", "gate-requests", "name-requests"] {
            assert!(!f._root.path().join(file).exists());
        }
    }
}

#[test]
fn direct_workers_require_positive_visible_readiness_not_managed_launch_flag() {
    let f = Fixture::new("ok");
    let path = f._root.path().join("agent.json");
    let mut agent: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    agent.as_object_mut().unwrap().remove("interactive_ready");
    fs::write(&path, serde_json::to_vec(&agent).unwrap()).unwrap();
    let before = runtime::snapshot(&f.project).unwrap();
    for rejected in [
        json!({"visible_idle":false}),
        json!({"visible_blocker":true}),
        json!({"visible_working":true}),
        json!({"state":"blocked"}),
        json!({"matched_rule":null}),
        json!({"screen_detection_skipped":true}),
        json!({"manifest_source":"remote:unbound"}),
        json!({"skip_state_update":true}),
        json!({"agent":"foreign"}),
        json!({"visible_idle":"true"}),
        json!({"fallback_reason":"unknown screen"}),
        json!({"warning":"unverified detection"}),
    ] {
        fs::write(
            f._root.path().join("readiness.json"),
            serde_json::to_vec(&rejected).unwrap(),
        )
        .unwrap();
        assert!(f.send().is_err(), "{rejected}");
        assert_eq!(runtime::snapshot(&f.project).unwrap(), before);
        assert!(f.sent().is_empty());
    }
    fs::remove_file(f._root.path().join("readiness.json")).unwrap();
    f.send().unwrap();
    assert_eq!(f.sent().len(), 1);
}

#[test]
fn readiness_lost_after_claim_prevents_prompt_and_retains_one_use_claim() {
    let f = Fixture::new("ok");
    fs::write(f._root.path().join("lose-readiness"), "").unwrap();
    assert!(f.send().is_err());
    assert!(f.sent().is_empty());
    let before = runtime::snapshot(&f.project).unwrap();
    let delivery = before
        .deliveries
        .iter()
        .find(|d| d.operation == f.operation.id)
        .unwrap();
    assert_eq!(delivery.state, crate::operations::DeliveryState::Claimed);
    assert_eq!(delivery.attempts, 1);
    fs::remove_file(f._root.path().join("lose-readiness")).unwrap();
    assert!(f.send().is_err());
    assert_eq!(runtime::snapshot(&f.project).unwrap(), before);
    assert!(f.sent().is_empty());
}

#[test]
fn new_workspace_creation_and_lost_layout_reply_are_one_use() {
    for lost in [false, true] {
        let f = Fixture::new("resource-release-workspace");
        if lost {
            fs::write(f._root.path().join("lose-creation-reply"), "").unwrap();
        }
        let created = resources::create_legacy_resource(
            &f.project,
            &f.operation.id,
            1,
            Instant::now() + Duration::from_secs(10),
            Default::default(),
        );
        let state = runtime::snapshot(&f.project).unwrap();
        let revision = state.deliveries[0].revision;
        let target = if lost {
            assert!(created.is_err());
            reconcile_resource(
                &f.project,
                &f.operation.id,
                revision,
                Instant::now() + Duration::from_secs(10),
                Default::default(),
            )
            .unwrap()
            .unwrap()
        } else {
            created.unwrap()
        };
        assert_eq!(target.route.workspace_id, "w1");
        assert_eq!(target.route.pane_id, "w1:p1");
        let state = runtime::snapshot(&f.project).unwrap();
        assert!(
            resources::create_legacy_resource(
                &f.project,
                &f.operation.id,
                revision,
                Instant::now() + Duration::from_secs(10),
                Default::default()
            )
            .is_err()
        );
        assert!(
            continue_workspace_layout(
                &f.project,
                &f.operation.id,
                revision,
                Instant::now() + Duration::from_secs(10),
                Default::default()
            )
            .is_err()
        );
        assert_eq!(runtime::snapshot(&f.project).unwrap(), state);
        for kind in [
            "runtime.launch_workspace",
            "runtime.launch_layout",
            "runtime.launch_target",
        ] {
            assert_eq!(state.events.iter().filter(|e| e.kind == kind).count(), 1);
        }
        assert_eq!(
            fs::read_to_string(f._root.path().join("workspace-requests"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        assert_eq!(
            fs::read_to_string(f._root.path().join("created")).unwrap(),
            "created\\n"
        );
        let mut db=migration::open_active(&f.project).unwrap();
        db.cancel_attempt(&target.attempt,state.attempts[0].revision,state.head,"stop historical staged worker",now()).unwrap();
        let cancelled=db.read_snapshot(None).unwrap();
        let error=reconcile_termination(&f.project,&target.attempt,cancelled.attempts[0].revision,Instant::now()+Duration::from_secs(10),Default::default()).unwrap_err();
        assert!(format!("{error:#}").contains("bootstrap quiescence"));
        assert_eq!(db.read_snapshot(None).unwrap(),cancelled);
        assert!(cancelled.attempts[0].retains_capacity());
        assert!(!cancelled.attempts[0].termination_observed);
        let previous=previous_boot_fixture(&f,false);
        let before=db.read_snapshot(None).unwrap();
        let retained:LaunchTarget=serde_json::from_value(before.events.iter().find(|e|e.kind=="runtime.launch_target").unwrap().payload.clone()).unwrap();
        let reboot=crate::worker_supervision::SupervisorObservation::observe_reboot(retained.supervisor.as_ref().unwrap()).unwrap().unwrap();
        for field in ["host","previous","current","missing"] {
            let mut evidence=reboot.clone();
            match field {"host"=>evidence.host_id="0".repeat(64),"previous"=>evidence.previous_boot_id=evidence.current_boot_id.clone(),"current"=>evidence.current_boot_id=evidence.previous_boot_id.clone(),_=>{}};
            let prepared=PreparedLaunchStopped{receipt:LaunchStoppedReceipt {
                version:1,target:retained.clone(),host_reboot:(field!="missing").then_some(evidence),repository_snapshots:vec![],output_snapshot:None,observed_unix_ms:now(),
            }};
            assert!(db.record_launch_stopped(&prepared,before.attempts[0].revision,before.head,now()).is_err(),"{field}");
            assert_eq!(db.read_snapshot(None).unwrap(),before);
        }
        let stopped=reconcile_termination(&f.project,&target.attempt,before.attempts[0].revision,Instant::now()+Duration::from_secs(10),Default::default()).unwrap().unwrap();
        assert!(stopped.termination_observed && !stopped.retains_capacity());
        let after=db.read_snapshot(None).unwrap();
        let receipt=after.events.iter().find(|e|e.kind=="runtime.launch_stopped").unwrap();
        assert_eq!(receipt.payload["host_reboot"]["previous_boot_id"],previous);
        assert_ne!(receipt.payload["host_reboot"]["current_boot_id"],previous);
        reconcile_termination(&f.project,&target.attempt,stopped.revision,Instant::now()+Duration::from_secs(10),Default::default()).unwrap().unwrap();
        assert_eq!(db.read_snapshot(None).unwrap(),after);
    }
}

#[test]
fn workspace_acknowledgment_loss_retains_uncertainty_without_layout_or_recreation() {
    let f = Fixture::new("resource-release-workspace");
    fs::write(f._root.path().join("lose-workspace-reply"), "").unwrap();
    let error = resources::create_legacy_resource(
        &f.project,
        &f.operation.id,
        1,
        Instant::now() + Duration::from_secs(10),
        Default::default(),
    )
    .unwrap_err();
    assert!(!error.to_string().contains("cleanup budget"), "{error:#}");
    let before = runtime::snapshot(&f.project).unwrap();
    assert!(
        reconcile_resource(
            &f.project,
            &f.operation.id,
            before.deliveries[0].revision,
            Instant::now() + Duration::from_secs(10),
            Default::default()
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(runtime::snapshot(&f.project).unwrap(), before);
    assert!(before.attempts[0].retains_capacity());
    assert!(!f._root.path().join("created").exists());
    assert_eq!(
        fs::read_to_string(f._root.path().join("workspace-requests"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn workspace_layout_commit_failure_resumes_without_recreating_workspace() {
    let f = Fixture::new("resource-release-workspace");
    let raw = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
    raw.execute_batch("CREATE TRIGGER refuse_layout BEFORE INSERT ON events WHEN NEW.kind='runtime.launch_layout' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    let error = resources::create_legacy_resource(
        &f.project,
        &f.operation.id,
        1,
        Instant::now() + Duration::from_secs(10),
        Default::default(),
    )
    .unwrap_err();
    assert!(!error.to_string().contains("cleanup budget"), "{error:#}");
    let before = runtime::snapshot(&f.project).unwrap();
    assert!(!f._root.path().join("created").exists());
    assert!(
        before
            .events
            .iter()
            .any(|e| e.kind == "runtime.launch_workspace")
    );
    raw.execute_batch("DROP TRIGGER refuse_layout;").unwrap();
    let target = continue_workspace_layout(
        &f.project,
        &f.operation.id,
        before.deliveries[0].revision,
        Instant::now() + Duration::from_secs(10),
        Default::default(),
    )
    .unwrap();
    assert_eq!(target.route.workspace_id, "w1");
    assert_eq!(
        fs::read_to_string(f._root.path().join("workspace-requests"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn workspace_receipt_commit_failure_never_submits_layout_or_recreates() {
    let f = Fixture::new("resource-release-workspace");
    let raw = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
    raw.execute_batch("CREATE TRIGGER refuse_workspace BEFORE INSERT ON events WHEN NEW.kind='runtime.launch_workspace' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    assert!(
        resources::create_legacy_resource(
            &f.project,
            &f.operation.id,
            1,
            Instant::now() + Duration::from_secs(10),
            Default::default()
        )
        .is_err()
    );
    let before = runtime::snapshot(&f.project).unwrap();
    assert!(
        before
            .events
            .iter()
            .any(|e| e.kind == "runtime.launch_creation")
    );
    assert!(!before.events.iter().any(|e| matches!(
        e.kind.as_str(),
        "runtime.launch_workspace" | "runtime.launch_layout" | "runtime.launch_target"
    )));
    assert!(!f._root.path().join("created").exists());
    raw.execute_batch("DROP TRIGGER refuse_workspace;").unwrap();
    assert!(
        advance_launch(
            &f.project,
            &f.operation.id,
            before.deliveries[0].revision,
            Instant::now() + Duration::from_secs(10),
            Default::default()
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(runtime::snapshot(&f.project).unwrap(), before);
    assert_eq!(
        fs::read_to_string(f._root.path().join("workspace-requests"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn lost_workspace_reply_recovers_exact_live_marker_even_after_revocation_and_expiry() {
    for expired in [false, true] {
        let f = Fixture::new("resource-release-workspace");
        fs::write(f._root.path().join("lose-workspace-reply"), "").unwrap();
        assert!(
            resources::create_legacy_resource(
                &f.project,
                &f.operation.id,
                1,
                Instant::now() + Duration::from_secs(10),
                Default::default()
            )
            .is_err()
        );
        let request: Value = serde_json::from_str(
            fs::read_to_string(f._root.path().join("workspace-requests"))
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        let token = request["params"]["env"]["HP_WORKSPACE_CREATION"]
            .as_str()
            .unwrap();
        let mut bootstrap = Worker(
            std::process::Command::new("/usr/bin/sleep")
                .arg("30")
                .env_clear()
                .env("HP_WORKSPACE_CREATION", token)
                .current_dir(&f.project)
                .spawn()
                .unwrap(),
        );
        fs::write(
            f._root.path().join("bootstrap.json"),
            serde_json::to_vec(&json!({"pid":bootstrap.0.id()})).unwrap(),
        )
        .unwrap();
        let mut db = migration::open_active(&f.project).unwrap();
        if expired {
            let state = db.read_snapshot(None).unwrap();
            let grant = state
                .approvals
                .iter()
                .find(|g| g.consumed.is_some())
                .unwrap();
            db.revoke_approval(&grant.reference.id, state.head, now(), "fixture revoke")
                .unwrap();
            db.expire_claims(now() + 30001).unwrap();
        }
        let before = db.read_snapshot(None).unwrap();
        let revision = before.deliveries[0].revision;
        // A matching label is insufficient when the live marker is missing.
        let original = fs::read(f._root.path().join("bootstrap.json")).unwrap();
        fs::remove_file(f._root.path().join("bootstrap.json")).unwrap();
        assert!(
            reconcile_resource(
                &f.project,
                &f.operation.id,
                revision,
                Instant::now() + Duration::from_secs(10),
                Default::default()
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(db.read_snapshot(None).unwrap(), before);
        fs::write(f._root.path().join("bootstrap.json"), original).unwrap();
        fs::write(f._root.path().join("duplicate-workspaces"), "").unwrap();
        assert!(
            reconcile_resource(
                &f.project,
                &f.operation.id,
                revision,
                Instant::now() + Duration::from_secs(10),
                Default::default()
            )
            .is_err()
        );
        assert_eq!(db.read_snapshot(None).unwrap(), before);
        fs::remove_file(f._root.path().join("duplicate-workspaces")).unwrap();
        let raw = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
        raw.execute_batch("CREATE TRIGGER refuse_recovered_workspace BEFORE INSERT ON events WHEN NEW.kind='runtime.launch_workspace' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(
            reconcile_resource(
                &f.project,
                &f.operation.id,
                revision,
                Instant::now() + Duration::from_secs(10),
                Default::default()
            )
            .is_err()
        );
        assert_eq!(db.read_snapshot(None).unwrap(), before);
        raw.execute_batch("DROP TRIGGER refuse_recovered_workspace;")
            .unwrap();
        assert!(
            reconcile_resource(
                &f.project,
                &f.operation.id,
                revision,
                Instant::now() + Duration::from_secs(10),
                Default::default()
            )
            .unwrap()
            .is_none()
        );
        let after = db.read_snapshot(None).unwrap();
        assert_eq!(after.deliveries, before.deliveries);
        assert_eq!(after.attempts, before.attempts);
        assert_eq!(after.approvals, before.approvals);
        let workspace = after
            .events
            .iter()
            .find(|e| e.kind == "runtime.launch_workspace")
            .unwrap();
        assert_eq!(workspace.payload["route"]["pane_id"], "w1:p0");
        assert!(!f._root.path().join("created").exists());
        if !expired {
            continue_workspace_layout(
                &f.project,
                &f.operation.id,
                revision,
                Instant::now() + Duration::from_secs(10),
                Default::default(),
            )
            .unwrap();
        } else {
            assert!(
                continue_workspace_layout(
                    &f.project,
                    &f.operation.id,
                    revision,
                    Instant::now() + Duration::from_secs(10),
                    Default::default()
                )
                .is_err()
            );
            assert_eq!(db.read_snapshot(None).unwrap(), after);
        }
        assert_eq!(
            fs::read_to_string(f._root.path().join("workspace-requests"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        bootstrap.0.kill().unwrap();
        bootstrap.0.wait().unwrap();
    }
}

#[test]
fn creation_claim_and_recovery_intent_roll_back_together_before_native_effects() {
    for boundary in ["approval", "claim", "creation"] {
        let f = Fixture::new("resource-release-workspace");
        let before = runtime::snapshot(&f.project).unwrap();
        let raw = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
        let trigger = match boundary {
            "approval" => "CREATE TRIGGER refuse_atomic_creation AFTER INSERT ON approval_uses BEGIN SELECT RAISE(ABORT,'fixture'); END;",
            "claim" => "CREATE TRIGGER refuse_atomic_creation AFTER INSERT ON events WHEN NEW.kind='operation.claimed' BEGIN SELECT RAISE(ABORT,'fixture'); END;",
            _ => "CREATE TRIGGER refuse_atomic_creation AFTER INSERT ON events WHEN NEW.kind='runtime.launch_creation' BEGIN SELECT RAISE(ABORT,'fixture'); END;",
        };
        raw.execute_batch(trigger).unwrap();
        assert!(create_resource(&f.project, &f.operation.id, 1, Instant::now()+Duration::from_secs(15), Default::default()).is_err(), "{boundary}");
        // Reopen through the production reader: no consumed approval, budget
        // admission, claim, creation event or delivery revision survives failure.
        assert_eq!(runtime::snapshot(&f.project).unwrap(), before, "{boundary}");
        assert!(!f._root.path().join("workspace-requests").exists());
        assert!(!f._root.path().join("created").exists());
        raw.execute_batch("DROP TRIGGER refuse_atomic_creation").unwrap();
        create_resource(&f.project, &f.operation.id, 1, Instant::now()+Duration::from_secs(15), Default::default()).unwrap();
        let after = runtime::snapshot(&f.project).unwrap();
        for kind in ["operation.claimed", "runtime.launch_creation"] {
            assert_eq!(after.events.iter().filter(|e| e.kind==kind && e.entity==f.operation.id.as_str()).count(), 1);
        }
        assert_eq!(after.deliveries[0].attempts, 1);
        assert_eq!(fs::read_to_string(f._root.path().join("workspace-requests")).unwrap().lines().count(), 1);
    }
}

#[test]
fn supervised_root_creation_and_commit_losses_recover_without_bootstrap_or_replay() {
    for failure in ["none", "reply", "workspace_commit", "target_commit", "unsupported"] {
        let f = Fixture::new("resource-release-workspace");
        let raw = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
        match failure {
            "reply" => fs::write(f._root.path().join("lose-workspace-reply"), "").unwrap(),
            "unsupported" => fs::write(f._root.path().join("reject-workspace-command"), "").unwrap(),
            "workspace_commit" | "target_commit" => {
                let kind = if failure == "workspace_commit" { "runtime.launch_workspace" } else { "runtime.launch_target" };
                raw.execute_batch(&format!("CREATE TRIGGER refuse_root BEFORE INSERT ON events WHEN NEW.kind='{kind}' BEGIN SELECT RAISE(ABORT,'fixture'); END;")).unwrap();
            }
            _ => {}
        }
        let result = create_resource(&f.project, &f.operation.id, 1, Instant::now()+Duration::from_secs(10), Default::default());
        assert_eq!(result.is_ok(), failure == "none", "{failure}: {result:?}");
        let before = runtime::snapshot(&f.project).unwrap();
        let revision = before.deliveries[0].revision;
        let creation = before.events.iter().find(|e| e.kind == "runtime.launch_creation").unwrap();
        assert_eq!(creation.payload["version"], 2);
        assert!(creation.payload.get("workspace_token").is_none());
        if failure != "none" {
            assert!(!before.events.iter().any(|e| matches!(e.kind.as_str(), "runtime.launch_workspace"|"runtime.launch_target")));
        }
        if failure.ends_with("commit") { raw.execute_batch("DROP TRIGGER refuse_root;").unwrap(); }
        assert!(create_resource(&f.project, &f.operation.id, revision, Instant::now()+Duration::from_secs(10), Default::default()).is_err());
        let recovered = reconcile_resource(&f.project, &f.operation.id, revision, Instant::now()+Duration::from_secs(10), Default::default()).unwrap();
        if failure == "unsupported" {
            assert!(recovered.is_none());
            assert_eq!(runtime::snapshot(&f.project).unwrap(), before);
        } else {
            let target = recovered.unwrap();
            assert_eq!(target.route.pane_id, "w1:p1");
            assert!(target.supervisor.is_some());
            let after = runtime::snapshot(&f.project).unwrap();
            let workspace = after.events.iter().find(|e| e.kind == "runtime.launch_workspace").unwrap();
            let staged = after.events.iter().find(|e| e.kind == "runtime.launch_target").unwrap();
            assert_eq!(workspace.payload, staged.payload);
            assert_eq!(workspace.payload, serde_json::to_value(target).unwrap());
            assert_eq!(after.deliveries, before.deliveries);
            assert_eq!(after.approvals, before.approvals);
            assert!(!after.events.iter().any(|e| e.kind == "runtime.launch_layout"));
            assert!(continue_workspace_layout(&f.project, &f.operation.id, revision, Instant::now()+Duration::from_secs(10), Default::default()).is_err());
        }
        let requests = fs::read_to_string(f._root.path().join("workspace-requests")).unwrap();
        assert_eq!(requests.lines().count(), 1);
        let request: Value = serde_json::from_str(requests.trim()).unwrap();
        assert_eq!(request["method"], "workspace.create_command");
        assert!(request["params"]["command"].as_array().is_some());
        assert!(!f._root.path().join("created").exists(), "layout.apply must not run");
        assert!(!f._root.path().join("gate-requests").exists());
    }
}

#[test]
fn supervised_root_recovery_needs_exact_process_and_preserves_expired_authority() {
    let f = Fixture::new("resource-release-workspace");
    fs::write(f._root.path().join("lose-workspace-reply"), "").unwrap();
    assert!(create_resource(&f.project, &f.operation.id, 1, Instant::now()+Duration::from_secs(10), Default::default()).is_err());
    let mut db = migration::open_active(&f.project).unwrap();
    let state = db.read_snapshot(None).unwrap();
    let grant = state.approvals.iter().find(|g| g.consumed.is_some()).unwrap();
    db.revoke_approval(&grant.reference.id, state.head, now(), "fixture revoke").unwrap();
    db.expire_claims(now()+30001).unwrap();
    let before = db.read_snapshot(None).unwrap();
    let revision = before.deliveries[0].revision;
    for flag in ["no-processes", "duplicate-workspaces", "other-pane", "replace-terminal"] {
        fs::write(f._root.path().join(flag), "").unwrap();
        // Reset the observation counter so replacement happens across this read.
        fs::remove_file(f._root.path().join("pane-reads")).ok();
        let result = reconcile_resource(&f.project, &f.operation.id, revision, Instant::now()+Duration::from_secs(10), Default::default());
        if flag == "no-processes" { assert!(result.unwrap().is_none()); } else { assert!(result.is_err(), "{flag}"); }
        assert_eq!(db.read_snapshot(None).unwrap(), before);
        fs::remove_file(f._root.path().join(flag)).unwrap();
    }
    let target = reconcile_resource(&f.project, &f.operation.id, revision, Instant::now()+Duration::from_secs(10), Default::default()).unwrap().unwrap();
    let after = db.read_snapshot(None).unwrap();
    assert_eq!(after.deliveries, before.deliveries);
    assert_eq!(after.attempts, before.attempts);
    assert_eq!(after.approvals, before.approvals);
    assert_eq!(target.route.pane_id, "w1:p1");
    assert!(release_gate(&f.project, &f.operation.id, revision, Instant::now()+Duration::from_secs(10), Default::default()).is_err());
    assert!(!f._root.path().join("gate-requests").exists());
    assert_eq!(db.read_snapshot(None).unwrap(), after);
}

#[test]
#[ignore = "requires HP_LIVE_HERDR with workspace.create_command; disposable native server"]
fn live_canonical_supervised_root_creation_release_and_termination() {
    live_supervised_root(false);
}

#[test]
#[ignore = "requires HP_LIVE_HERDR with workspace.create_command; disposable native server"]
fn live_canonical_repository_creation_release_and_termination() {
    live_supervised_root(true);
}

#[test]
#[ignore = "requires HP_LIVE_HERDR; real native effects with a reply-dropping proxy"]
fn live_canonical_lost_creation_and_gate_replies_preserve_one_use() {
    for method in ["workspace.create_command","pane.send_input"] {
        live_supervised_root_failure(true,Some(method),false,LiveLifecycle::Normal);
    }
}

#[test]
#[ignore = "requires HP_LIVE_HERDR; SIGKILL after real native effects"]
fn live_canonical_caller_crash_after_creation_and_gate_input() {
    for method in ["workspace.create_command","pane.send_input"] {
        live_supervised_root_failure(true,Some(method),true,LiveLifecycle::Normal);
    }
}

#[test]
fn live_native_crash_driver() {
    let Some(request)=std::env::var_os("HP_NATIVE_CRASH_REQUEST") else{return;};
    let (project,operation,revision,action):(PathBuf,OperationId,u64,String)=serde_json::from_str(request.to_str().unwrap()).unwrap();
    let deadline=Instant::now()+Duration::from_secs(45);
    match action.as_str() {
        "workspace.create_command"=>{create_resource(&project,&operation,revision,deadline,Default::default()).unwrap();}
        "pane.send_input"=>release_gate(&project,&operation,revision,deadline,Default::default()).unwrap(),
        _=>panic!("invalid crash fixture action"),
    }
    panic!("crash fixture returned before SIGKILL");
}

fn crash_native_caller(project:&Path,operation:&OperationId,revision:u64,method:&str,lab:&Path) {
    use std::os::unix::process::ExitStatusExt;
    let mut child=Worker(std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact","canonical_worker::tests::live_native_crash_driver","--nocapture"])
        .env("HP_NATIVE_CRASH_REQUEST",serde_json::to_string(&(project,operation,revision,method)).unwrap())
        .stdin(std::process::Stdio::null()).spawn().unwrap());
    let deadline=Instant::now()+Duration::from_secs(15);
    while !lab.join("dropped").exists() {
        assert!(child.0.try_wait().unwrap().is_none(),"native crash caller ended before the effect");
        assert!(Instant::now()<deadline,"native effect did not reach the crash boundary");
        std::thread::sleep(Duration::from_millis(10));
    }
    child.0.kill().unwrap();assert_eq!(child.0.wait().unwrap().signal(),Some(libc::SIGKILL));
    // Only unblock the reply proxy after observing the caller's actual death.
    fs::write(lab.join("continue-reply"),b"caller reaped").unwrap();
    let deadline=Instant::now()+Duration::from_secs(10);
    loop {
        match crate::execution_guard::RootGuard::exclusive(project.parent().unwrap()) {
            Ok(guard)=>{drop(guard);break;}
            Err(error)=>{assert!(Instant::now()<deadline,"crashed helper retained root lease: {error:#}");std::thread::sleep(Duration::from_millis(10));}
        }
    }
}

#[test]
#[ignore = "requires HP_LIVE_HERDR; actual server restart and replacement worker"]
fn live_canonical_server_restart_preserves_old_outputs_and_new_worker() {
    live_supervised_root_failure(true,None,false,LiveLifecycle::ServerRestart);
}

fn live_supervised_root(repository: bool) {live_supervised_root_failure(repository,None,false,LiveLifecycle::Normal)}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LiveLifecycle { Normal, ServerRestart, UnobservedExit, HistoricalBootstrap, VendorWorkflow }

#[test]
#[ignore = "requires HP_LIVE_HERDR; real native exit before durable identity capture"]
fn live_canonical_unobserved_exit_keeps_capacity_and_refuses_replay() {
    live_supervised_root_failure(true,Some("workspace.create_command"),false,LiveLifecycle::UnobservedExit);
}

#[test]
#[ignore = "requires HP_LIVE_HERDR; real historical bootstrap quiescence refusal"]
fn live_canonical_historical_bootstrap_blocks_release_after_worker_stop() {
    live_supervised_root_failure(false,None,false,LiveLifecycle::HistoricalBootstrap);
}

fn live_supervised_root_failure(repository:bool, lost_reply:Option<&str>, crash:bool, lifecycle:LiveLifecycle) {
    use std::{os::unix::fs::FileTypeExt, process::{Command, Stdio}};
    let binary = PathBuf::from(std::env::var_os("HP_LIVE_HERDR").expect("set patched Herdr binary"));
    assert!(binary.is_absolute());
    let lab = tempfile::tempdir().unwrap();
    let home = lab.path().join("home"); fs::create_dir(&home).unwrap();
    let runtime_dir = lab.path().join("runtime"); fs::create_dir(&runtime_dir).unwrap();
    fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let config = lab.path().join("config.toml");
    fs::write(&config, "onboarding = false\n[terminal]\ndefault_shell = '/bin/sh'\nshell_mode = 'non_login'\n[update]\nversion_check = false\nmanifest_check = false\n").unwrap();
    let socket = lab.path().join("native.sock");
    let spawn_server=|| {
        let log=fs::OpenOptions::new().create(true).append(true).open(lab.path().join("server.log")).unwrap();
        Worker(Command::new(&binary).arg("server").env_clear()
            .env("HOME", &home).env("PATH", "/usr/bin:/bin").env("SHELL", "/bin/sh")
            .env("TERM", "xterm-256color").env("LANG", "C.UTF-8")
            .env("HERDR_CONFIG_PATH", &config).env("HERDR_SOCKET_PATH", &socket)
            .env("XDG_RUNTIME_DIR", &runtime_dir).current_dir(lab.path())
            .stdin(Stdio::null()).stdout(log.try_clone().unwrap()).stderr(log).spawn().unwrap())
    };
    let wait_server=|server:&mut Worker| {
        let deadline=Instant::now()+Duration::from_secs(15);
        while !fs::symlink_metadata(&socket).is_ok_and(|m|m.file_type().is_socket()) {
            assert!(Instant::now()<deadline&&server.0.try_wait().unwrap().is_none(),"native server failed: {}",fs::read_to_string(lab.path().join("server.log")).unwrap());
            std::thread::sleep(Duration::from_millis(25));
        }
    };
    let mut server=spawn_server();wait_server(&mut server);
    let proxy=lab.path().join("reply-proxy");
    if let Some(method)=lost_reply {
        fs::write(&proxy,format!(r#"#!/usr/bin/python3
import json,os,pathlib,subprocess,sys,time
binary={binary:?}
root=pathlib.Path({root:?})
if sys.argv[1:]==['--version']:os.execv(binary,[binary,'--version'])
request=sys.stdin.readline()
method=json.loads(request)['method']
with open(root/'requests','a') as log:log.write(method+'\n')
result=subprocess.run([binary,*sys.argv[1:]],input=request.encode(),stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=12)
if method=={method:?} and not (root/'dropped').exists() and result.returncode==0:
 response=json.loads(result.stdout)
 if 'result' in response and 'error' not in response:
  (root/'dropped').write_text(method)
  if {unobserved_exit}:
   (root/'creation-effect.json').write_text(json.dumps({{'request':json.loads(request),'response':response}}))
  if {crash}:
   until=time.monotonic()+12
   while not (root/'continue-reply').exists() and time.monotonic()<until:time.sleep(0.01)
  sys.exit(1)
sys.stdout.buffer.write(result.stdout)
sys.stderr.buffer.write(result.stderr)
sys.exit(result.returncode)
"#,binary=binary.display().to_string(),root=lab.path().display().to_string(),crash=if crash {"True"}else{"False"},unobserved_exit=if lifecycle==LiveLifecycle::UnobservedExit {"True"}else{"False"})).unwrap();
        fs::set_permissions(&proxy,fs::Permissions::from_mode(0o700)).unwrap();
    }
    let signer=lab.path().join("workflow-owner");
    if lifecycle==LiveLifecycle::VendorWorkflow {
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-q","-t","ed25519","-N","","-f"]).arg(&signer).status().unwrap().success());
    }
    let mut f = Fixture::with_project(if repository {"resource-release-workspace-repository"} else {"resource-release-workspace"}, |_,_| {}, Some((if lost_reply.is_some(){&proxy}else{&binary}, &socket)),None,(lifecycle==LiveLifecycle::VendorWorkflow).then_some(signer.as_path()));
    if lifecycle==LiveLifecycle::VendorWorkflow {
        let driver=PathBuf::from(std::env::var_os("HP_CONTROLLER_TEST_BINARY").expect("compiled controller test binary required"));
        fs::write(f.project.join("PROJECT.md"),"+++\nname='Worker fixture'\n+++\nNew instructions must not replace the retained attempt input.").unwrap();
        let status=Command::new(driver).args(["--exact","canonical_controller::launch_tests::live_dispatch_workflow_driver","--nocapture"])
            .env("HP_LIVE_DISPATCH_PROJECT",&f.project).status().unwrap();
        // Even a failed driver must stop an identity already recorded by the
        // production launch path before its credential-bearing home is removed.
        let state=runtime::snapshot(&f.project).unwrap();
        if let Some(event)=state.events.iter().find(|e|e.kind=="runtime.launch_target") {
            let target:LaunchTarget=serde_json::from_value(event.payload.clone()).unwrap();
            if let Some(identity)=target.supervisor {
                crate::worker_supervision::SupervisorObservation::stop_recorded(&identity,Instant::now()+Duration::from_secs(10),&Default::default()).unwrap();
            }
        }
        assert!(status.success(),"live dispatch workflow driver failed");
        let root=f._root.path().to_owned();drop(f);assert!(!root.exists(),"temporary worker login must be removed");
        return;
    }
    // This fixture's simulated process is not part of the live native launch.
    f._worker.0.kill().unwrap(); f._worker.0.wait().unwrap();
    let created = if lifecycle==LiveLifecycle::HistoricalBootstrap {
        resources::create_legacy_resource(&f.project,&f.operation.id,1,Instant::now()+Duration::from_secs(45),Default::default())
    } else if crash&&lost_reply==Some("workspace.create_command") {
        crash_native_caller(&f.project,&f.operation.id,1,"workspace.create_command",lab.path());
        Err(anyhow::anyhow!("fixture caller was killed"))
    }else{create_resource(&f.project, &f.operation.id, 1, Instant::now()+Duration::from_secs(45), Default::default())};
    let target=if lost_reply==Some("workspace.create_command") {
        assert!(created.is_err());
        let before=runtime::snapshot(&f.project).unwrap();assert!(!before.events.iter().any(|e|e.kind=="runtime.launch_target"));
        assert_eq!(before.approvals.iter().filter(|a|a.consumed.is_some()).count(),1);
        assert!(create_resource(&f.project,&f.operation.id,before.deliveries[0].revision,Instant::now()+Duration::from_secs(45),Default::default()).is_err());
        assert_eq!(runtime::snapshot(&f.project).unwrap(),before);
        if lifecycle==LiveLifecycle::UnobservedExit {
            live_unobserved_exit(&f,lab.path(),&binary,&socket);
            return;
        }
        reconcile_resource(&f.project,&f.operation.id,before.deliveries[0].revision,Instant::now()+Duration::from_secs(45),Default::default()).unwrap().unwrap()
    }else{created.unwrap()};
    let state = runtime::snapshot(&f.project).unwrap();
    let workspace = state.events.iter().find(|e| e.kind=="runtime.launch_workspace").unwrap();
    if lifecycle==LiveLifecycle::HistoricalBootstrap {
        assert_eq!(workspace.payload["version"],1);
        assert_ne!(workspace.payload["route"]["pane_id"],target.route.pane_id);
        assert!(state.events.iter().any(|e|e.kind=="runtime.launch_layout"));
    } else {
        assert_eq!(workspace.payload, serde_json::to_value(&target).unwrap());
        assert!(!state.events.iter().any(|e| e.kind=="runtime.launch_layout"));
    }
    let released=if crash&&lost_reply==Some("pane.send_input") {
        crash_native_caller(&f.project,&f.operation.id,state.deliveries[0].revision,"pane.send_input",lab.path());
        Err(anyhow::anyhow!("fixture caller was killed"))
    }else{release_gate(&f.project, &f.operation.id, state.deliveries[0].revision, Instant::now()+Duration::from_secs(45), Default::default())};
    if lost_reply==Some("pane.send_input") {
        assert!(released.is_err());let before=runtime::snapshot(&f.project).unwrap();
        assert_eq!(before.events.iter().filter(|e|e.kind=="runtime.launch_release").count(),1);
        assert!(release_gate(&f.project,&f.operation.id,before.deliveries[0].revision,Instant::now()+Duration::from_secs(45),Default::default()).is_err());
        assert_eq!(runtime::snapshot(&f.project).unwrap(),before);
    }else{released.unwrap();}
    let artifact = if repository {
        let state=runtime::snapshot(&f.project).unwrap();
        let plan=worktree_plans(&state.attempt_inputs[0].inputs,&target.attempt).unwrap().remove(0);
        assert_eq!(target.route.cwd,plan.path);
        assert_eq!(fs::read_to_string(Path::new(&plan.path).join("source.txt")).unwrap(),"approved base\n");
        let artifact=Path::new(&plan.path).join("REPORT.md");
        fs::write(&artifact,"preserve native worker result").unwrap();
        Some(artifact)
    } else {None};
    let identity = target.supervisor.as_ref().unwrap();
    let observation = crate::worker_supervision::SupervisorObservation::reconnect(identity).unwrap();
    let deadline = Instant::now()+Duration::from_secs(5);
    loop {
        let state = runtime::snapshot(&f.project).unwrap();
        let profile = state.attempt_inputs[0].inputs.effective_profile.as_ref().unwrap();
        if observation.agent_process(Path::new(&profile.agent.path), &profile.arguments_digest).is_ok() { break; }
        assert!(Instant::now()<deadline,"native gate did not start exact executable");
        std::thread::sleep(Duration::from_millis(25));
    }
    let state=runtime::snapshot(&f.project).unwrap();
    let output=PathBuf::from(worker_output_path(&state.attempt_inputs[0].inputs,&target.attempt).unwrap());
    fs::create_dir_all(output.join("library")).unwrap();
    fs::write(output.join("report.md"),b"durable native report").unwrap();
    fs::write(output.join("library/result.bin"),[0,255,1,254]).unwrap();
    let replacement=if lifecycle==LiveLifecycle::ServerRestart {
        use crate::runner::{RealRunner,Runner};
        server.0.kill().unwrap();server.0.wait().unwrap();
        if socket.exists(){fs::remove_file(&socket).unwrap();}
        server=spawn_server();wait_server(&mut server);
        let replacement_session=session_identity(&socket).unwrap();assert_ne!(replacement_session,target.session);
        let call=|method:&str,params:Value| {
            let mut command=Cmd::new(binary.to_str().unwrap(),Duration::from_secs(10)).arg("remote-api-bridge")
                .env("HERDR_SOCKET_PATH",socket.to_str().unwrap()).env("PATH","/usr/bin:/bin")
                .stdin(format!("{}\n",json!({"id":"replacement-fixture","method":method,"params":params})));
            command.env_clear=true;
            let output=RealRunner.run(&command).unwrap();assert!(output.success());
            let response:Value=serde_json::from_str(&output.stdout).unwrap();assert!(response.get("error").is_none(),"{response}");response["result"].clone()
        };
        let argv=crate::worker_supervision::command(Path::new("/usr/bin/sleep"),&["30".into()],60).unwrap();
        let created=call("workspace.create_command",json!({"cwd":home,"label":"replacement-worker","focus":false,"command":argv,"env":{}}));
        let pane=created["root_pane"]["pane_id"].as_str().unwrap();
        let deadline=Instant::now()+Duration::from_secs(10);
        let identity=loop {
            let result=call("pane.process_info",json!({"pane_id":pane}));
            let found=result["process_info"]["foreground_processes"].as_array().into_iter().flatten().find_map(|p| {
                if p["argv"]!=json!(argv){return None;}
                crate::worker_supervision::SupervisorObservation::observe(u32::try_from(p["pid"].as_u64().unwrap()).unwrap(),&argv).ok().map(|s|s.identity().clone())
            });
            if let Some(identity)=found{break identity;}
            assert!(Instant::now()<deadline,"replacement supervisor missing");std::thread::sleep(Duration::from_millis(20));
        };
        // Herdr can recycle its pane IDs; the new session must still be untouched.
        assert_eq!(pane,target.route.pane_id);
        assert_ne!(&identity,target.supervisor.as_ref().unwrap());
        Some((replacement_session,identity))
    }else{None};
    let mut db = migration::open_active(&f.project).unwrap();
    let state = db.read_snapshot(None).unwrap();
    db.cancel_attempt(&target.attempt, state.attempts[0].revision, state.head, "live contract done", now()).unwrap();
    let state = db.read_snapshot(None).unwrap();
    if lifecycle==LiveLifecycle::HistoricalBootstrap {
        for _ in 0..2 {
            let error=reconcile_termination(&f.project,&target.attempt,state.attempts[0].revision,Instant::now()+Duration::from_secs(45),Default::default()).unwrap_err();
            assert!(format!("{error:#}").contains("bootstrap quiescence"),"{error:#}");
            assert!(crate::worker_supervision::SupervisorObservation::recover_exited(identity).unwrap());
            assert_eq!(runtime::snapshot(&f.project).unwrap(),state);
        }
        assert!(state.attempts[0].retains_capacity());assert!(!state.attempts[0].termination_observed);
        assert!(server.0.try_wait().unwrap().is_none());
        // A real live bootstrap remains outside the stopped worker namespace.
        use crate::runner::{RealRunner,Runner};
        let workspace=&state.events.iter().find(|e|e.kind=="runtime.launch_workspace").unwrap().payload;
        let creation=&state.events.iter().find(|e|e.kind=="runtime.launch_creation").unwrap().payload;
        let mut cmd=Cmd::new(binary.to_str().unwrap(),Duration::from_secs(5)).arg("remote-api-bridge")
            .env("HERDR_SOCKET_PATH",socket.to_str().unwrap()).env("PATH","/usr/bin:/bin")
            .stdin(format!("{}\n",json!({"id":"historical-bootstrap-fixture","method":"pane.process_info","params":{"pane_id":workspace["route"]["pane_id"]}})));
        cmd.env_clear=true;
        let result=RealRunner.run(&cmd).unwrap();assert!(result.success());
        let response:Value=serde_json::from_str(&result.stdout).unwrap();assert!(response.get("error").is_none(),"{response}");
        let bootstrap=response["result"]["process_info"]["foreground_processes"].as_array().unwrap().iter().find_map(|p| {
            crate::worker_supervision::ProcessMarkerObservation::observe(u32::try_from(p["pid"].as_u64().unwrap()).unwrap(),
                creation["workspace_token"].as_str().unwrap(),Path::new(workspace["route"]["cwd"].as_str().unwrap())).ok().flatten()
        }).expect("real historical bootstrap must still be alive after worker stop");
        bootstrap.check().unwrap();
        assert_eq!(fs::read(output.join("report.md")).unwrap(),b"durable native report");
        assert_eq!(fs::read(output.join("library/result.bin")).unwrap(),[0,255,1,254]);
        assert!(artifact.is_none());
        return;
    }
    let stopped = reconcile_termination(&f.project, &target.attempt, state.attempts[0].revision, Instant::now()+Duration::from_secs(45), Default::default()).unwrap().unwrap();
    assert!(stopped.termination_observed && !stopped.retains_capacity());
    assert!(crate::worker_supervision::SupervisorObservation::recover_exited(identity).unwrap());
    if let Some((session,new_identity))=&replacement {
        let settled=runtime::snapshot(&f.project).unwrap();
        assert_eq!(reconcile_termination(&f.project,&target.attempt,stopped.revision,Instant::now()+Duration::from_secs(45),Default::default()).unwrap(),Some(stopped.clone()));
        assert_eq!(runtime::snapshot(&f.project).unwrap(),settled);
        assert!(server.0.try_wait().unwrap().is_none());assert_eq!(session_identity(&socket).unwrap(),*session);
        assert!(!crate::worker_supervision::SupervisorObservation::recover_exited(new_identity).unwrap());
        crate::worker_supervision::SupervisorObservation::stop_recorded(new_identity,Instant::now()+Duration::from_secs(10),&Default::default()).unwrap();
        assert!(crate::worker_supervision::SupervisorObservation::recover_exited(new_identity).unwrap());
    }
    let state=runtime::snapshot(&f.project).unwrap();
    let receipt:LaunchStoppedReceipt=serde_json::from_value(state.events.iter().find(|e|e.kind=="runtime.launch_stopped"&&e.entity==f.operation.id.as_str()).unwrap().payload.clone()).unwrap();
    assert_eq!(receipt.target,target);
    let saved=receipt.output_snapshot.as_ref().unwrap();assert!(saved.digest.is_some());
    assert!(output.starts_with(f._root.path()));fs::remove_dir_all(&output).unwrap();
    let preserved=crate::worktree_preservation::load_outputs(&f.project,&target.attempt,saved,&crate::source_tree::Control{deadline:Instant::now()+Duration::from_secs(45),cancellation:Default::default()}).unwrap();
    for (name,expected) in [("report.md",b"durable native report".as_slice()),("library/result.bin",[0,255,1,254].as_slice())] {
        let entry=preserved.manifest().entries.iter().find(|e|e.path==name).unwrap();assert_eq!(preserved.bytes(entry).unwrap(),expected);
    }
    assert!(!output.exists());
    assert_eq!(receipt.repository_snapshots.len(),usize::from(repository));
    if let Some(artifact)=artifact {
        assert_eq!(fs::read_to_string(&artifact).unwrap(),"preserve native worker result");
        let reference=&receipt.repository_snapshots[0];
        let directory=f.project.join(".state/worktree-file-snapshots").join(target.attempt.as_str()).join(&reference.digest);
        let bytes=fs::read(directory.join("manifest.json")).unwrap();assert_eq!(format!("{:x}",Sha256::digest(&bytes)),reference.digest);
        let manifest:crate::worktree_preservation::Manifest=serde_json::from_slice(&bytes).unwrap();
        assert_eq!(manifest.attempt,target.attempt);assert_eq!(manifest.worktree.plan,reference.plan);assert_eq!(manifest.scope,"repository_state");
        let entry=manifest.entries.iter().find(|e|e.path=="REPORT.md").unwrap();
        assert!(artifact.starts_with(f._root.path()));fs::remove_file(&artifact).unwrap();
        let retained=fs::read(directory.join(&entry.sha256)).unwrap();assert_eq!(retained,b"preserve native worker result");assert_eq!(format!("{:x}",Sha256::digest(&retained)),entry.sha256);
        let archive=manifest.git.as_ref().unwrap();let pack=fs::read(directory.join(&archive.pack.sha256)).unwrap();
        assert_eq!(pack.len() as u64,archive.pack.bytes);assert_eq!(format!("{:x}",Sha256::digest(&pack)),archive.pack.sha256);
        assert_eq!(fs::read_to_string(f.project.join("source.txt")).unwrap(),"approved base\n");
        assert!(!f.project.join("REPORT.md").exists());
    }
    if let Some(method)=lost_reply {
        assert_eq!(fs::read_to_string(lab.path().join("dropped")).unwrap(),method);
        let requests=fs::read_to_string(lab.path().join("requests")).unwrap();
        assert_eq!(requests.lines().filter(|m|*m=="workspace.create_command").count(),1);
        assert_eq!(requests.lines().filter(|m|*m=="pane.send_input").count(),1);
        assert_eq!(state.events.iter().filter(|e|e.kind=="runtime.launch_creation").count(),1);
        assert_eq!(state.events.iter().filter(|e|e.kind=="runtime.launch_release").count(),1);
    }
    eprintln!("Real native canonical root creation, durable ownership, gate release, termination and source-loss preservation passed; vendor workflow certification remains untested");
}


// The harness knows which disposable process to stop, but deliberately never
// gives that identity to the store. Production recovery must not invent it.
fn live_unobserved_exit(f:&Fixture,lab:&Path,binary:&Path,socket:&Path) {
    use crate::runner::{RealRunner,Runner};
    use crate::worker_supervision::SupervisorObservation;
    let effect:Value=serde_json::from_slice(&fs::read(lab.join("creation-effect.json")).unwrap()).unwrap();
    let argv:Vec<String>=serde_json::from_value(effect["request"]["params"]["command"].clone()).unwrap();
    let pane=effect["response"]["result"]["root_pane"]["pane_id"].as_str().unwrap();
    let deadline=Instant::now()+Duration::from_secs(10);
    let identity=loop {
        let mut cmd=Cmd::new(binary.to_str().unwrap(),Duration::from_secs(5)).arg("remote-api-bridge")
            .env("HERDR_SOCKET_PATH",socket.to_str().unwrap()).env("PATH","/usr/bin:/bin")
            .stdin(format!("{}\n",json!({"id":"unobserved-exit-fixture","method":"pane.process_info","params":{"pane_id":pane}})));
        cmd.env_clear=true;
        let output=RealRunner.run(&cmd).unwrap();assert!(output.success());
        let response:Value=serde_json::from_str(&output.stdout).unwrap();assert!(response.get("error").is_none(),"{response}");
        let found=response["result"]["process_info"]["foreground_processes"].as_array().into_iter().flatten().find_map(|p| {
            if p["argv"]!=json!(argv){return None;}
            SupervisorObservation::observe(u32::try_from(p["pid"].as_u64().unwrap()).unwrap(),&argv).ok().map(|s|s.identity().clone())
        });
        if let Some(identity)=found{break identity;}
        assert!(Instant::now()<deadline,"unobserved native supervisor missing");std::thread::sleep(Duration::from_millis(20));
    };
    let original=runtime::snapshot(&f.project).unwrap();
    assert!(!original.events.iter().any(|e|matches!(e.kind.as_str(),"runtime.launch_target"|"runtime.launch_started"|"runtime.launch_release")));
    SupervisorObservation::stop_recorded(&identity,Instant::now()+Duration::from_secs(10),&Default::default()).unwrap();
    assert!(SupervisorObservation::recover_exited(&identity).unwrap());
    assert_eq!(runtime::snapshot(&f.project).unwrap(),original);
    for cancelled in [false,true] {
        if cancelled {
            let mut db=migration::open_active(&f.project).unwrap();let state=db.read_snapshot(None).unwrap();
            db.cancel_attempt(&state.attempts[0].id,state.attempts[0].revision,state.head,"cancel unobserved live exit",now()).unwrap();
        }
        let before=runtime::snapshot(&f.project).unwrap();
        for _ in 0..2 {
            assert!(reconcile_resource(&f.project,&f.operation.id,before.deliveries[0].revision,Instant::now()+Duration::from_secs(45),Default::default()).unwrap().is_none());
            assert!(create_resource(&f.project,&f.operation.id,before.deliveries[0].revision,Instant::now()+Duration::from_secs(45),Default::default()).is_err());
            let error=reconcile_termination(&f.project,&before.attempts[0].id,before.attempts[0].revision,Instant::now()+Duration::from_secs(45),Default::default()).unwrap_err();
            assert!(error.to_string().contains("staged worker target missing"),"{error:#}");
            assert_eq!(runtime::snapshot(&f.project).unwrap(),before);
        }
        assert!(before.attempts[0].retains_capacity());assert!(!before.attempts[0].termination_observed);
        assert_eq!(before.approvals.iter().filter(|a|a.consumed.is_some()).count(),1);
        assert_eq!(before.events.iter().filter(|e|e.kind=="runtime.launch_creation").count(),1);
        let plans=worktree_plans(&before.attempt_inputs[0].inputs,&before.attempts[0].id).unwrap();
        assert_eq!(plans.len(),1);
        assert_eq!(fs::read(Path::new(&plans[0].path).join("source.txt")).unwrap(),b"approved base\n");
    }
    let requests=fs::read_to_string(lab.join("requests")).unwrap();
    assert_eq!(requests.lines().filter(|m|*m=="workspace.create_command").count(),1);
    assert_eq!(requests.lines().filter(|m|*m=="pane.send_input").count(),0);
}

#[test]
fn unsupported_connected_server_refuses_creation_before_approval_or_worktrees() {
    for mode in ["resource-release-workspace", "resource-release-workspace-repository"] {
        for capability in [
            json!({"type":"pong","version":"0.9.1"}),
            json!({"type":"pong","version":"0.9.1","capabilities":{"workspace_create_command":false}}),
            json!({"type":"pong","version":"0.9.1","capabilities":{"workspace_create_command":"true"}}),
            json!({"type":"pong","version":"0.9.2","capabilities":{"workspace_create_command":true}}),
            json!({"type":"unknown","version":"0.9.1","capabilities":{"workspace_create_command":true}}),
        ] {
            let f=Fixture::new(mode);
            fs::write(f._root.path().join("server-capability.json"),serde_json::to_vec(&capability).unwrap()).unwrap();
            let before=runtime::snapshot(&f.project).unwrap();
            let error=create_resource(&f.project,&f.operation.id,1,Instant::now()+Duration::from_secs(15),Default::default()).unwrap_err();
            assert!(error.to_string().contains("does not advertise"),"{mode}: {error:#}");
            assert_eq!(runtime::snapshot(&f.project).unwrap(),before);
            assert!(!f._root.path().join("workspace-requests").exists());
            assert!(!f.project.join(".state/worktrees").exists());
        }
    }
}

#[test]
fn prepared_launch_selection_rotates_stages_and_excludes_cancelled_or_expired_effects() {
    for expired in [false,true] {
        let f=Fixture::new("resource-release-workspace");
        let read=|turn,enabled,time| {
            let mut budget=crate::store::identity_inventory::Budget::new(2*1024*1024,1024,Instant::now()+Duration::from_secs(2),Default::default()).unwrap();
            migration::read_controller_dispatch_hint(&f.project,&mut budget,turn,time,enabled).unwrap()
        };
        let initial=runtime::snapshot(&f.project).unwrap();
        assert!(read(0,false,now()).is_none());
        assert!(read(0,true,now()-86_400_000).is_none());
        let hint=read(0,true,now()).unwrap();
        assert_eq!(hint.operation.id,f.operation.id);assert_eq!(hint.delivery_revision,1);
        assert_eq!(hint.mode,crate::store::controller_hint::EffectMode::Deliver);
        assert_eq!(runtime::snapshot(&f.project).unwrap(),initial);
        let raw=rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
        raw.execute("UPDATE project_control SET state='paused'",[]).unwrap();
        assert!(read(0,true,now()).is_none());
        raw.execute("UPDATE project_control SET state='active',reconciliation_required=1",[]).unwrap();
        // A mismatched reconciliation publication refuses the read entirely.
        let mut inconsistent=crate::store::identity_inventory::Budget::new(2*1024*1024,1024,Instant::now()+Duration::from_secs(2),Default::default()).unwrap();
        assert!(migration::read_controller_dispatch_hint(&f.project,&mut inconsistent,0,now(),true).is_err());
        let marker=f.project.join(".state/format.json");let original=fs::read(&marker).unwrap();
        let mut published:serde_json::Value=serde_json::from_slice(&original).unwrap();
        published["reconciliation_required"]=true.into();fs::write(&marker,serde_json::to_vec(&published).unwrap()).unwrap();
        assert!(read(0,true,now()).is_none());
        raw.execute("UPDATE project_control SET reconciliation_required=0",[]).unwrap();
        fs::write(&marker,original).unwrap();
        assert_eq!(runtime::snapshot(&f.project).unwrap(),initial);
        let mut empty=crate::store::identity_inventory::Budget::new(0,0,Instant::now()+Duration::from_secs(1),Default::default()).unwrap();
        assert!(migration::read_controller_dispatch_hint(&f.project,&mut empty,0,now(),true).is_err());
        create_resource(&f.project,&hint.operation.id,hint.delivery_revision,Instant::now()+Duration::from_secs(15),Default::default()).unwrap();
        let staged=runtime::snapshot(&f.project).unwrap();
        let first=read(0,true,now()).unwrap();let next=read(1,true,now()).unwrap();
        assert_ne!(first.operation.id,next.operation.id);
        let launch=[&first,&next].into_iter().find(|h|h.operation.kind=="runtime.launch").unwrap();
        assert_eq!(launch.mode,crate::store::controller_hint::EffectMode::Deliver);
        assert_eq!(launch.delivery_revision,staged.deliveries[0].revision);
        assert_eq!(read(2,true,now()).unwrap().operation.id,first.operation.id);
        assert_eq!(runtime::snapshot(&f.project).unwrap(),staged);
        let mut db=migration::open_active(&f.project).unwrap();
        if expired {db.expire_claims(staged.deliveries[0].lease_until_ms.unwrap()).unwrap();}
        else {db.cancel_attempt(&staged.attempts[0].id,staged.attempts[0].revision,staged.head,"cancel selected launch",now()).unwrap();}
        for turn in 0..4 {
            let hint=read(turn,true,now()).unwrap();
            assert_eq!(hint.operation.kind,"runtime.worker_termination");
            assert_eq!(hint.mode,crate::store::controller_hint::EffectMode::Observe);
        }
    }
}

#[test]
fn creation_retains_observed_supervisor_after_authority_changes() {
    for mode in ["resource-release-workspace","resource-release"] {
    for changed in ["expired","revoked","cancelled"] {
    let f=Fixture::new(mode);
    fs::write(f._root.path().join("hold-creation-reply"),"").unwrap();
    let deadline=Instant::now()+Duration::from_secs(20);
    let target=std::thread::scope(|scope| {
        scope.spawn(|| {
            while !f._root.path().join("direct-created").exists() && !f._root.path().join("created").exists() {
                assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(10));
            }
            let mut db=migration::open_active(&f.project).unwrap();let state=db.read_snapshot(None).unwrap();
            let delivery=state.deliveries.iter().find(|d|d.operation==f.operation.id).unwrap();
            match changed {
                "expired"=>assert_eq!(db.expire_claims(delivery.lease_until_ms.unwrap()).unwrap(),1),
                "revoked"=>{db.revoke_approval(&state.approvals[0].reference.id,state.head,now(),"withdraw creation authority").unwrap();},
                _=>{db.cancel_attempt(&state.attempts[0].id,state.attempts[0].revision,state.head,"stop concurrent creation",now()).unwrap();},
            }
            fs::write(f._root.path().join("continue-creation-reply"),"").unwrap();
        });
        create_resource(&f.project,&f.operation.id,1,deadline,Default::default()).unwrap()
    });
    let state=runtime::snapshot(&f.project).unwrap();
    assert_eq!(state.deliveries[0].state,if changed=="expired" {crate::operations::DeliveryState::Ambiguous} else {crate::operations::DeliveryState::Claimed});
    assert_eq!(state.deliveries[0].attempts,1);
    assert!(state.events.iter().any(|e|e.kind=="runtime.launch_target" && e.payload==serde_json::to_value(&target).unwrap()));
    assert!(release_gate(&f.project,&f.operation.id,state.deliveries[0].revision,deadline,Default::default()).is_err());
    assert!(!f._root.path().join("gate-requests").exists());
    assert_eq!(runtime::snapshot(&f.project).unwrap(),state);
    let mut db=migration::open_active(&f.project).unwrap();
    if changed!="cancelled" {db.cancel_attempt(&target.attempt,state.attempts[0].revision,state.head,"stop concurrent creation",now()).unwrap();}
    let before=db.read_snapshot(None).unwrap();
    let stopped=reconcile_termination(&f.project,&target.attempt,before.attempts[0].revision,deadline,Default::default()).unwrap().unwrap();
    assert!(stopped.termination_observed);assert!(!stopped.retains_capacity());
    if mode.contains("workspace") {assert_eq!(fs::read_to_string(f._root.path().join("workspace-requests")).unwrap().lines().count(),1);}
    else {assert_eq!(fs::read_to_string(f._root.path().join("created")).unwrap(),"created\\n");}
    assert_eq!(runtime::snapshot(&f.project).unwrap().approvals,state.approvals);
}

}

}

#[test]
#[ignore = "requires HP_CONTROLLER_TEST_BINARY pointing to the compiled main test executable"]
fn enabled_controller_queue_launches_cancels_and_preserves_across_projects() {controller_fixture(false)}

#[test]
#[ignore = "requires HP_CONTROLLER_TEST_BINARY pointing to the compiled main test executable"]
fn enabled_ticker_launches_and_preserves_with_shared_root_maintenance() {controller_fixture(true)}

fn controller_fixture(whole_ticker:bool) {
    use std::{io::Write,process::{Command,Stdio},sync::{Arc,atomic::{AtomicBool,Ordering}}};
    let binary=std::env::var_os("HP_CONTROLLER_TEST_BINARY").expect("build lib and bin tests, then set HP_CONTROLLER_TEST_BINARY");
    let common=tempfile::tempdir().unwrap();
    let source=common.path().join("fixture-agent.c");
    fs::write(&source,r#"#include <stdio.h>
#include <string.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc == 2 && !strcmp(argv[1], "--version")) { puts("2.1.0 (Claude Code)"); return 0; }
    if (argc != 1) return 2;
    sleep(30); return 0;
}
"#).unwrap();
    assert!(Command::new("/usr/bin/cc").arg(&source).arg("-o").arg(common.path().join("fixture-agent")).status().unwrap().success());
    let signer=common.path().join("owner-key");
    assert!(Command::new("/usr/bin/ssh-keygen").args(["-q","-t","ed25519","-N","","-f"]).arg(&signer).status().unwrap().success());
    let mut fixtures=(0..3).map(|n|Fixture::with_project("resource-release-workspace-repository",|_,_|{},None,whole_ticker.then(||common.path().join(format!("project-{n}"))).as_deref(),Some(&signer))).collect::<Vec<_>>();
    fixtures.sort_by(|a,b|a.project.cmp(&b.project));
    fs::write(fixtures[0]._root.path().join("lose-name-reply"),b"lost acknowledgment").unwrap();
    let projects=fixtures.iter().map(|f|f.project.clone()).collect::<Vec<_>>();
    let done=Arc::new(AtomicBool::new(false));let deadline=Instant::now()+Duration::from_secs(30);
    let status=std::thread::scope(|scope| {
        for f in fixtures.iter_mut().take(2) {
            let mut input=f._worker.0.stdin.take().unwrap();let done=done.clone();
            scope.spawn(move|| {
                let request=f._root.path().join("gate-requests");
                while !request.exists() {
                    if done.load(Ordering::Acquire){return;}
                    assert!(Instant::now()<deadline,"queued gate request missing");std::thread::sleep(Duration::from_millis(5));
                }
                let request:Value=serde_json::from_str(fs::read_to_string(request).unwrap().lines().next().unwrap()).unwrap();
                input.write_all(request["params"]["text"].as_str().unwrap().as_bytes()).unwrap();
                let stage:Value=serde_json::from_slice(&fs::read(f._root.path().join("stage.json")).unwrap()).unwrap();let argv:Vec<String>=serde_json::from_value(stage["argv"].clone()).unwrap();
                let supervisor=crate::worker_supervision::SupervisorObservation::observe(f._worker.0.id(),&argv).unwrap();
                let state=runtime::snapshot(&f.project).unwrap();let record=&state.attempt_inputs[0];let profile=record.inputs.effective_profile.as_ref().unwrap();
                while supervisor.agent_process(Path::new(&profile.agent.path),&profile.arguments_digest).is_err() {assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(5));}
                let cwd=worktree_plans(&record.inputs,&record.attempt).unwrap().first().map(|p|p.path.clone()).unwrap_or_else(||f.project.display().to_string());
                fs::write(f._root.path().join("agent.json"),serde_json::to_vec(&json!({"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1","cwd":cwd,"terminal_id":"term1","agent":"claude","name":null,"interactive_ready":true,"agent_status":"idle"})).unwrap()).unwrap();
                fs::write(f._root.path().join("gate-sent"),b"").unwrap();
            });
        }
        let mut child=Command::new(binary).args(["--exact",if whole_ticker {"canonical_controller::launch_tests::enabled_ticker_fixture_driver"}else{"canonical_controller::launch_tests::enabled_dispatch_fixture_driver"},"--nocapture"])
            .env("HP_CONTROLLER_FIXTURE_PROJECTS",serde_json::to_string(&projects).unwrap()).stdin(Stdio::null()).stdout(Stdio::inherit()).stderr(Stdio::inherit()).spawn().unwrap();
        let status=loop {
            if let Some(status)=child.try_wait().unwrap(){break status;}
            if Instant::now()>=deadline{child.kill().unwrap();break child.wait().unwrap();}
            std::thread::sleep(Duration::from_millis(10));
        };
        done.store(true,Ordering::Release);status
    });
    assert!(status.success(),"enabled controller driver failed: {status}");
    for f in &fixtures[..2] {
        let sent=f.sent();assert_eq!(sent.len(),1);
        let state=runtime::snapshot(&f.project).unwrap();
        let brief=state.operations.iter().find(|o|o.kind=="runtime.worker_brief").unwrap();
        let brief:WorkerBriefIntent=serde_json::from_value(brief.payload.clone()).unwrap();
        assert_eq!(format!("{:x}",Sha256::digest(sent[0]["params"]["text"].as_str().unwrap().as_bytes())),brief.prompt_digest);
        assert_eq!(fs::read_to_string(f._root.path().join("workspace-requests")).unwrap().lines().count(),1);
        for name in ["gate-requests","name-requests"] {assert_eq!(fs::read_to_string(f._root.path().join(name)).unwrap().lines().count(),1,"{name}");}
        let state=runtime::snapshot(&f.project).unwrap();assert!(state.attempts[0].termination_observed&&!state.attempts[0].retains_capacity());
        assert!(state.approvals.iter().any(|a|a.consumed.is_some()));
    }
    for name in ["created","workspace-requests","gate-requests","name-requests","sent"] {assert!(!fixtures[2]._root.path().join(name).exists(),"cancelled queued launch performed {name}");}
}


#[test]
#[ignore = "requires explicit live workflow authorization, HP_LIVE_HERDR, HP_LIVE_AGENT, HP_LIVE_AUTH_FILE and HP_CONTROLLER_TEST_BINARY"]
fn live_authenticated_controller_workflow() {
    live_supervised_root_failure(false,None,false,LiveLifecycle::VendorWorkflow);
}
