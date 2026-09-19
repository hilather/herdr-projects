use super::*;

fn json_string(value:&serde_json::Value,key:&str)->Result<String> {
    match value.get(key) {
        None=>Ok(String::new()),
        Some(v)=>v.as_str().map(String::from).ok_or_else(||StoreError::Corrupt("invalid runtime string field".into())),
    }
}
fn identity(value:&serde_json::Value,is_thread:bool)->Result<RuntimeIdentity> {
    let thread_field=|key| if is_thread {json_string(value,key)}else{Ok(String::new())};
    Ok(RuntimeIdentity{
        machine:thread_field("machine")?,socket:if is_thread{String::new()}else{json_string(value,"socket")?},
        workspace_id:json_string(value,"workspace_id")?,tab_id:json_string(value,"tab_id")?,pane_id:json_string(value,"pane_id")?,
        cwd:json_string(value,"cwd")?,repo:thread_field("repo")?,branch:thread_field("branch")?,worktree_path:thread_field("worktree_path")?,
        thread_dir:thread_field("thread_dir")?,agent:thread_field("agent")?,agent_name:json_string(value,"agent_name")?,
        legacy_status:thread_field("status")?,execution_fingerprint:None,
    })
}
pub(super) fn import_sources(db:&Connection)->Result<()> {
    let sources=super::import::read_sources(db)?;
    let coordinator=sources.iter().find(|s|s.path==".state/coordinator.json"&&s.kind=="runtime");
    let session_digest=coordinator.map(|s|s.digest.clone());
    let coordinator=coordinator.map(|s|serde_json::from_slice::<serde_json::Value>(&s.bytes).map_err(|_|StoreError::Corrupt("invalid coordinator provenance".into()))).transpose()?;
    let socket=coordinator.as_ref().map(|v|json_string(v,"socket")).transpose()?.unwrap_or_default();
    for source in &sources {
        let binding=if source.kind=="thread" {
            let thread:toml::Value=toml::from_str(std::str::from_utf8(&source.bytes).map_err(|_|StoreError::Corrupt("invalid thread encoding".into()))?).map_err(|_|StoreError::Corrupt("invalid thread provenance".into()))?;
            let id=thread.get("id").and_then(|v|v.as_str()).ok_or_else(||StoreError::Corrupt("thread identity missing".into()))?;
            if source.path!=format!("threads/{id}.toml") {return Err(StoreError::Corrupt("thread source identity mismatch".into()));}
            let task=TaskId::new(format!("legacy-{id}")).map_err(StoreError::Corrupt)?;
            let value=serde_json::to_value(&thread).map_err(|_|StoreError::Corrupt("invalid thread identity".into()))?;
            let mut identity=identity(&value,true)?;identity.socket=socket.clone();
            identity.execution_fingerprint=Some(crate::operations::receipts::legacy_execution_fingerprint(&thread).ok_or_else(||StoreError::Corrupt("invalid execution identity".into()))?);
            RuntimeBinding{id:format!("thread:{id}"),task:Some(task),revision:1,source_path:source.path.clone(),source_digest:source.digest.clone(),session_source_digest:session_digest.clone(),verification:RuntimeVerification::Unverified,identity}
        } else if source.path==".state/coordinator.json"&&source.kind=="runtime" {
            RuntimeBinding{id:"coordinator".into(),task:None,revision:1,source_path:source.path.clone(),source_digest:source.digest.clone(),session_source_digest:None,verification:RuntimeVerification::Unverified,identity:identity(coordinator.as_ref().ok_or_else(||StoreError::Corrupt("coordinator missing".into()))?,false)?}
        } else {continue;};
        let payload=serde_json::to_string(&binding).map_err(|e|StoreError::Invalid(e.to_string()))?;
        db.execute("INSERT INTO runtime_bindings VALUES(?1,?2,?3,?4,?5,?6)",params![binding.id,binding.task.as_ref().map(TaskId::as_str),integer(binding.revision)?,binding.source_path,payload,format!("{:x}",Sha256::digest(payload.as_bytes()))])?;
    }
    Ok(())
}

pub(super) fn read_all(db:&Connection)->Result<Vec<RuntimeBinding>> {
    let mut stmt=db.prepare("SELECT b.id,b.task_id,b.revision,b.source_path,b.payload,b.payload_hash,s.digest,s.bytes FROM runtime_bindings b LEFT JOIN legacy_sources s ON s.path=b.source_path ORDER BY b.id")?;
    let rows=stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,u64>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,Option<String>>(6)?,r.get::<_,Option<Vec<u8>>>(7)?)))?;
    let session:Option<(String,Vec<u8>)>=db.query_row("SELECT digest,bytes FROM legacy_sources WHERE path='.state/coordinator.json' AND kind='runtime'",[],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((digest,bytes))=&session {if format!("{:x}",Sha256::digest(bytes))!=*digest {return Err(StoreError::Corrupt("runtime session source hash mismatch".into()));}}
    let session_digest=session.map(|s|s.0);
    let bindings=rows.map(|row|{
        let(id,task,revision,source,payload,hash,digest,bytes)=row?;
        let digest=digest.ok_or_else(||StoreError::Corrupt("runtime source missing".into()))?;
        let bytes=bytes.ok_or_else(||StoreError::Corrupt("runtime source bytes missing".into()))?;
        if format!("{:x}",Sha256::digest(&bytes))!=digest {return Err(StoreError::Corrupt("runtime source hash mismatch".into()));}
        if format!("{:x}",Sha256::digest(payload.as_bytes()))!=hash {return Err(StoreError::Corrupt("runtime binding payload hash mismatch".into()));}
        let binding:RuntimeBinding=serde_json::from_str(&payload).map_err(|_|StoreError::Corrupt("invalid runtime binding payload".into()))?;
        if binding.id!=id || binding.task.as_ref().map(TaskId::as_str)!=task.as_deref() || binding.revision!=revision || binding.source_path!=source || binding.source_digest!=digest {return Err(StoreError::Corrupt("runtime binding row identity mismatch".into()));}
        if binding.session_source_digest!=if binding.task.is_some(){session_digest.clone()}else{None} {return Err(StoreError::Corrupt("runtime session provenance mismatch".into()));}
        Ok(binding)
    }).collect::<Result<Vec<_>>>()?;
    let expected:u64=db.query_row("SELECT count(*) FROM legacy_sources WHERE kind='thread' OR (path='.state/coordinator.json' AND kind='runtime')",[],|r|r.get(0))?;
    if bindings.len() as u64!=expected {return Err(StoreError::Corrupt("runtime binding inventory mismatch".into()));}
    Ok(bindings)
}
