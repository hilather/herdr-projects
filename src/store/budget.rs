//! Durable admission budgets, evaluated under the reservation/claim transaction.
use super::*;

fn invalid(s:&str)->StoreError {StoreError::Invalid(s.into())}
pub(super) fn read_all(db:&Connection)->Result<Vec<BudgetPolicy>> {read_all_with_budget(db,None)}
pub(super) fn read_all_with_budget(db:&Connection,budget:Option<&read_budget::ReadBudget>)->Result<Vec<BudgetPolicy>> {
    let mut statement=db.prepare("SELECT revision,payload,payload_hash FROM budget_policies ORDER BY revision")?;
    let mut rows=statement.query([])?;
    let mut policies=Vec::new();
    while let Some(row)=rows.next()? {
        if let Some(budget)=budget {budget.row(row,&[(1,1)])?;}
        let revision:u64=row.get(0)?;
        let payload:String=row.get(1)?;
        let digest:String=row.get(2)?;
        if policies.len()>=10_000 || payload.len()>MAX_RECORD_BYTES || format!("{:x}",Sha256::digest(payload.as_bytes()))!=digest {
            return Err(StoreError::Corrupt("budget history exceeds bounds or hash mismatch".into()));
        }
        let policy:BudgetPolicy=serde_json::from_str(&payload).map_err(|_|StoreError::Corrupt("invalid budget record".into()))?;
        let reference=policy.reference().map_err(StoreError::Corrupt)?;
        if revision!=policies.len() as u64+1 || policy.revision!=revision || reference.digest!=digest {
            return Err(StoreError::Corrupt("budget revision or identity mismatch".into()));
        }
        policies.push(policy);
    }
    Ok(policies)
}
fn current(db:&Connection)->Result<Option<BudgetPolicy>> {
    let version:u32=db.query_row("PRAGMA user_version",[],|r|r.get(0))?;
    if version<14 {return Ok(None);}
    Ok(read_all(db)?.pop())
}
pub(super) fn report(db:&Connection,reserved:bool)->Result<BudgetReport> {
    let policy=current(db)?;
    let count:u64=db.query_row("SELECT count(*) FROM attempts",[],|r|r.get(0))?;
    let mut blockers=Vec::new();
    let mut incomplete=false;
    if let Some(policy)=&policy {
        if policy.limits.max_attempts.is_some_and(|cap|if reserved {count>cap}else{count>=cap}) {
            blockers.push("attempt_budget_exhausted".into());
        }
        if policy.limits.max_provider_tokens==Some(0) {
            blockers.push("provider_token_budget_exhausted".into());
        } else if policy.limits.max_provider_tokens.is_some() {
            match policy.limits.unknown_usage {
                UnknownUsagePolicy::Refuse=>blockers.push("provider_usage_unavailable".into()),
                UnknownUsagePolicy::AllowIncomplete=>incomplete=true,
            }
        }
    }
    Ok(BudgetReport{policy,admitted_attempts:count,provider_tokens:UsageAvailability::Unknown,incomplete,blockers})
}
pub(super) fn check(db:&Connection,reference:Option<&VersionedReference>,reserved:bool)->Result<()> {
    let report=report(db,reserved)?;
    let current=report.policy.as_ref().map(BudgetPolicy::reference).transpose().map_err(|s|invalid(&s))?;
    if current.as_ref()!=reference {return Err(invalid("budget policy changed or is not pinned"));}
    if !report.blockers.is_empty() {return Err(invalid("budget admission refused"));}
    Ok(())
}
impl SqliteStore {
    pub fn budget_report(&mut self)->Result<BudgetReport> {
        let tx=self.connection.transaction()?;check_schema(&tx)?;
        let result=report(&tx,false)?;tx.commit()?;Ok(result)
    }
    pub fn install_budget(&mut self,prepared:&PreparedBudget,expected_head:u64)->Result<VersionedReference> {
        let policy=&prepared.policy;
        let reference=policy.reference().map_err(|s|invalid(&s))?;
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;check_schema(&tx)?;
        let version:u32=tx.query_row("PRAGMA user_version",[],|r|r.get(0))?;
        if version<14 {return Err(StoreError::UnsupportedSchema(version));}
        if head(&tx)?!=expected_head {return Err(StoreError::Conflict);}
        let path=tx.path().ok_or_else(||invalid("budget requires file-backed store"))?;
        let path=std::fs::canonicalize(path).map_err(|_|invalid("budget store unavailable"))?;
        if path.to_str()!=Some(policy.project_store.as_str()) {return Err(invalid("budget belongs to a different project"));}
        let history=read_all(&tx)?;
        if history.len()>=10_000 {return Err(invalid("budget policy history limit reached"));}
        if policy.revision!=history.len() as u64+1 {return Err(StoreError::Conflict);}
        let payload=serde_json::to_string(policy).map_err(|_|invalid("budget encoding failed"))?;
        tx.execute("INSERT INTO budget_policies VALUES(?1,?2,?3)",params![integer(policy.revision)?,payload,reference.digest])?;
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('budget.policy_changed','project-budget',?1,1,?2)",params![integer(policy.revision)?,payload])?;
        tx.commit()?;Ok(reference)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy_installation_is_atomic_project_bound_and_revision_checked() {
        let temp=tempfile::tempdir().unwrap();let path=temp.path().join("state.db");
        let mut db=SqliteStore::create(&path).unwrap();
        let mut policy=BudgetPolicy{version:1,revision:1,project_store:path.canonicalize().unwrap().display().to_string(),
            authority:VersionedReference{id:"owner".into(),revision:1,digest:"a".repeat(64)},
            limits:BudgetLimits{max_attempts:Some(0),max_provider_tokens:None,unknown_usage:UnknownUsagePolicy::Refuse}};
        let before=db.read_snapshot(None).unwrap();
        let mut other=policy.clone();other.project_store=temp.path().join("other.db").display().to_string();
        assert!(db.install_budget(&PreparedBudget{policy:other},before.head).is_err());
        assert!(db.install_budget(&PreparedBudget{policy:policy.clone()},before.head+1).is_err());
        assert_eq!(db.read_snapshot(None).unwrap(),before);
        db.connection.execute_batch("CREATE TRIGGER fail_budget BEFORE INSERT ON events WHEN NEW.kind='budget.policy_changed' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(db.install_budget(&PreparedBudget{policy:policy.clone()},before.head).is_err());
        assert_eq!(db.read_snapshot(None).unwrap(),before);
        db.connection.execute_batch("DROP TRIGGER fail_budget;").unwrap();
        db.install_budget(&PreparedBudget{policy:policy.clone()},before.head).unwrap();
        let before=db.read_snapshot(None).unwrap();
        assert!(db.install_budget(&PreparedBudget{policy:policy.clone()},before.head).is_err());
        assert_eq!(db.read_snapshot(None).unwrap(),before);
        assert_eq!(db.budget_report().unwrap().blockers,vec!["attempt_budget_exhausted"]);
        policy.revision=2;policy.limits.max_attempts=None;
        db.install_budget(&PreparedBudget{policy},before.head).unwrap();
        drop(db);let mut db=SqliteStore::open(&path).unwrap();
        assert_eq!(db.read_snapshot(None).unwrap().budget_policies.len(),2);
        assert!(db.budget_report().unwrap().blockers.is_empty());
        assert!(db.connection.execute("DELETE FROM budget_policies",[]).is_err());
        db.connection.execute_batch("DROP TRIGGER budget_policies_no_update; UPDATE budget_policies SET payload_hash=printf('%064d',0) WHERE revision=1;").unwrap();
        assert!(db.read_snapshot(None).is_err());
        assert!(db.budget_report().is_err());
    }
}
