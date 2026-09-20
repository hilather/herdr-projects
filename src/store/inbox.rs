use super::*;
use crate::operations::{DeliveryState,Outcome};
use std::collections::BTreeSet;

pub(super) fn read_all(db:&Connection)->Result<Vec<InboxItem>> {
    let mut stmt=db.prepare("SELECT revision,payload,payload_hash,seen,done,id FROM inbox_items ORDER BY id")?;
    let rows=stmt.query_map([],|r|Ok((r.get::<_,u64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,bool>(3)?,r.get::<_,bool>(4)?,r.get::<_,String>(5)?)))?;
    rows.map(|row|{let(revision,payload,hash,seen,done,id)=row?;
        if format!("{:x}",Sha256::digest(payload.as_bytes()))!=hash{return Err(StoreError::Corrupt("inbox payload hash mismatch".into()));}
        let content:InboxContent=serde_json::from_str(&payload).map_err(|e|StoreError::Corrupt(e.to_string()))?;
        if content.id!=id{return Err(StoreError::Corrupt("inbox row identity mismatch".into()));}
        content.validate().map_err(StoreError::Corrupt)?;Ok(InboxItem{revision,content,seen,done})
    }).collect()
}
fn insert(db:&Connection,item:&InboxItem)->Result<()> {
    item.content.validate().map_err(StoreError::Invalid)?;
    let payload=serde_json::to_string(&item.content).map_err(|e|StoreError::Invalid(e.to_string()))?;
    if payload.len()>16*MAX_RECORD_BYTES{return Err(StoreError::Invalid("inbox record too large".into()));}
    db.execute("INSERT INTO inbox_items VALUES(?1,?2,?3,?4,?5,?6)",params![item.content.id,integer(item.revision)?,payload,format!("{:x}",Sha256::digest(payload.as_bytes())),item.seen,item.done])?;Ok(())
}
/// Imports only immutable provenance; upgrades never read stale legacy files.
pub(super) fn import_sources(db:&Connection)->Result<()> {
    let seen:Option<(Vec<u8>,String)>=db.query_row("SELECT bytes,digest FROM legacy_sources WHERE path='.state/inbox-seen.json'",[],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((bytes,hash))=&seen {if format!("{:x}",Sha256::digest(bytes))!=*hash{return Err(StoreError::Corrupt("seen source hash mismatch".into()));}}
    let seen:BTreeSet<String>=seen.map(|(b,_)|serde_json::from_slice(&b)).transpose().map_err(|e|StoreError::Corrupt(e.to_string()))?.unwrap_or_default();
    let records={let mut stmt=db.prepare("SELECT path,bytes,digest FROM legacy_sources WHERE kind='inbox' ORDER BY path")?;stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Vec<u8>>(1)?,r.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?};
    for(path,bytes,digest)in records {
        if !path.ends_with(".md"){continue;}
        if format!("{:x}",Sha256::digest(&bytes))!=digest{return Err(StoreError::Corrupt("inbox source digest mismatch".into()));}
        let text=std::str::from_utf8(&bytes).map_err(|e|StoreError::Corrupt(e.to_string()))?;
        let rest=text.strip_prefix("+++\n").ok_or_else(||StoreError::Corrupt("inbox header missing".into()))?;
        let(header,body)=rest.split_once("\n+++\n").or_else(||rest.strip_suffix("\n+++").map(|h|(h,""))).ok_or_else(||StoreError::Corrupt("inbox header unclosed".into()))?;
        let mut content:InboxContent=toml::from_str(header).map_err(|e|StoreError::Corrupt(e.to_string()))?;
        content.body=body.trim_matches('\n').into();
        let expected=if path.starts_with("inbox/done/"){format!("inbox/done/{}.md",content.id)}else{format!("inbox/{}.md",content.id)};
        if expected!=path{return Err(StoreError::Corrupt("inbox source path/identity mismatch".into()));}
        let item=InboxItem{revision:1,seen:seen.contains(&content.id),done:path.starts_with("inbox/done/"),content};insert(db,&item)?;
    }
    Ok(())
}
impl SqliteStore {
    /// Safe internal adapter: inspect, insert/deduplicate, and receipt commit in
    /// one SQLite transaction. It never retries an external/terminal effect.
    pub fn drain_inbox(&mut self,expected_head:u64,now:i64)->Result<usize> {
        super::delivery::now_check(now)?;
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;check_schema(&tx)?;
        if head(&tx)?!=expected_head{return Err(StoreError::Conflict);}
        let operations=read_operations(&tx)?;let mut count=0;
        let mut items=read_all(&tx)?.into_iter().map(|i|(i.content.id.clone(),i)).collect::<std::collections::BTreeMap<_,_>>();
        for op in operations.into_iter().filter(|o|o.kind=="legacy.inbox") {
            let old=super::delivery::delivery(&tx,&op.id)?;
            if !matches!(old.state,DeliveryState::Pending|DeliveryState::Ambiguous){continue;}
            // Explicit internal reconciliation can inspect ambiguous records now;
            // ordinary pending records still respect their backoff.
            if old.state==DeliveryState::Pending && old.next_due_ms>now {continue;}
            let revision:i64=tx.query_row("SELECT revision FROM tasks WHERE id=?1",[op.task.as_ref().ok_or(StoreError::Conflict)?.as_str()],|r|r.get(0))?;
            if integer(op.expected_revision)?!=revision{return Err(StoreError::Conflict);}
            if op.payload_version!=1{return Err(StoreError::Invalid("unsupported inbox intent version".into()));}
            if ["id","kind","subject","summary","body"].iter().any(|key|op.payload.get(key).is_none_or(|v|!v.is_string())) {return Err(StoreError::Corrupt("inbox intent requires all content fields".into()));}
            let mut content:InboxContent=serde_json::from_value(op.payload.clone()).map_err(|e|StoreError::Corrupt(e.to_string()))?;
            content.validate().map_err(StoreError::Corrupt)?;
            if content.id!=op.target{return Err(StoreError::Corrupt("inbox intent target mismatch".into()));}
            content.summary=content.summary.chars().map(|c|if c.is_control(){' '}else{c}).collect();content.body=content.body.trim_matches('\n').trim_end().into();
            let existing=items.get(&content.id);
            if let Some(existing)=existing {
                if !existing.content.same_delivery(&content){return Err(StoreError::Conflict);}
            }else{
                content.created=jiff::Timestamp::from_millisecond(now).map_err(|e|StoreError::Invalid(e.to_string()))?.to_string();
                let item=InboxItem{revision:1,content:content.clone(),seen:false,done:false};
                insert(&tx,&item)?;items.insert(content.id.clone(),item);
                tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('inbox.delivered',?1,1,1,?2)",params![content.id,serde_json::to_string(&content).map_err(|e|StoreError::Invalid(e.to_string()))?])?;
            }
            super::delivery::update_outcome(&tx,&old,&Outcome::Confirmed{observed_identity:format!("inbox:{}",content.id)},now,"atomic-inbox-adapter")?;
            count+=1;
        }
        tx.commit()?;Ok(count)
    }
    /// Update only items actually shown to the caller at this event head.
    pub fn update_inbox(&mut self,expected_head:u64,ids:&[String],done:bool)->Result<usize> {
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;check_schema(&tx)?;
        if head(&tx)?!=expected_head{return Err(StoreError::Conflict);}
        let items=read_all(&tx)?;let mut unique=BTreeSet::new();let mut count=0;
        for id in ids {
            if !unique.insert(id){continue;}
            let item=items.iter().find(|i|&i.content.id==id).ok_or(StoreError::Conflict)?;
            if (done&&item.done)||(!done&&item.seen){continue;}
            let revision=item.revision.checked_add(1).ok_or_else(||StoreError::Invalid("inbox revision exhausted".into()))?;
            tx.execute("UPDATE inbox_items SET revision=?2,seen=1,done=?3 WHERE id=?1",params![id,integer(revision)?,done||item.done])?;
            tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES(?1,?2,?3,1,?4)",params![if done{"inbox.done"}else{"inbox.seen"},id,integer(revision)?,serde_json::json!({"seen":true,"done":done||item.done}).to_string()])?;
            count+=1;
        }
        tx.commit()?;Ok(count)
    }
}
