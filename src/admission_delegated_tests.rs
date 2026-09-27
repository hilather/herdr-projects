use crate::domain::{BudgetPolicy, BudgetLimits, PreparedBudget, UnknownUsagePolicy, PreparedDelegation, PreparedDelegatedReservation, DelegatedReservationRequest};

fn setup() -> (tempfile::TempDir, std::path::PathBuf, SqliteStore, String, Vec<LaunchInputs>) {
    setup_with_limits(1, false)
}

fn setup_with_limits(concurrent: u32, overlap: bool) -> (tempfile::TempDir, std::path::PathBuf, SqliteStore, String, Vec<LaunchInputs>) {
    let paths: &[(&str, &str)] = if overlap { &[("shared.txt", "write")] } else { &[] };
    let (root, project) = world(4, &[
        Spec { id: "a", priority: 0, age_ms: 0, paths, named: &[] },
        Spec { id: "b", priority: 0, age_ms: 0, paths, named: &[] },
        Spec { id: "c", priority: 0, age_ms: 0, paths, named: &[] },
    ]);
    let path = store_file(&project).unwrap();
    let raw = rusqlite::Connection::open(&path).unwrap();
    let mut db = SqliteStore::open(&path).unwrap();
    let initial = prepared_admission_inputs(&project).unwrap().unwrap();
    let authority = initial.effective_profile.as_ref().unwrap().permission_policy.clone();
    for task in ["a", "b", "c"] {
        let bytes: Vec<u8> = raw.query_row("SELECT raw_bytes FROM task_contracts WHERE task_id=?1", [task], |r| r.get(0)).unwrap();
        let mut document: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        document["contract_revision"] = 2.into();
        document["expected_head"] = db.current_head().unwrap().into();
        document["authority"] = serde_json::to_value(&authority).unwrap();
        db.install_contract(&PreparedContract::parse_verified(&serde_json::to_vec(&document).unwrap()).unwrap()).unwrap();
    }
    let head = db.current_head().unwrap();
    let budget_ref = db.install_budget(&PreparedBudget { policy: BudgetPolicy {
        version: 1, project_store: initial.project_store.clone(), revision: 1, authority: authority.clone(),
        limits: BudgetLimits { max_attempts: Some(20), max_provider_tokens: None, unknown_usage: UnknownUsagePolicy::Refuse },
    } }, head).unwrap();
    let header = db.admission_header().unwrap();
    let profiles = db.admission_profiles_with_budget(header.control.config_digest.as_deref(),None).unwrap();
    let mut inputs: Vec<_> = readiness_now(&project).unwrap().iter().map(|candidate|
        seal(&initial.project_store, &header, candidate, &profiles[0], placeholder_approval()).unwrap()).collect();
    inputs.sort_by(|a,b| a.task.cmp(&b.task));
    let mut document: serde_json::Value = serde_json::from_slice(include_bytes!("../contracts/factory/delegation-v2.json")).unwrap();
    document["project_store"] = initial.project_store.into();
    document["authority"] = serde_json::to_value(&authority).unwrap();
    document["policy_revision"] = authority.revision.into();
    document["profile_kinds"] = serde_json::json!([profiles[0].kind]);
    document["max_concurrent_attempts"] = concurrent.into();
    document["reservation_scope"]["max_total_attempts"] = 2.into();
    document["reservation_scope"]["profiles"] = serde_json::json!([inputs[0].profile]);
    document["reservation_scope"]["task_contracts"] = serde_json::json!(inputs.iter().map(|i| i.task_contract.clone().unwrap()).collect::<Vec<_>>());
    document["reservation_scope"]["budget"] = serde_json::to_value(budget_ref).unwrap();
    document["repositories"] = serde_json::json!([{"repository": inputs[0].repositories[0].repository, "ref":"refs/heads/factory"}]);
    document["reservation_scope"]["repository_bases"] = serde_json::json!([{"repository":inputs[0].repositories[0].repository,"ref":"refs/heads/factory","commit_oid":inputs[0].repositories[0].commit,"object_format":"sha1"}]);
    document["expires_unix_ms"] = 9_000_000_000_000i64.into();
    let grant = PreparedDelegation::parse_verified(&serde_json::to_vec(&document).unwrap()).unwrap();
    let id = db.install_delegation(&grant, jiff::Timestamp::now().as_millisecond()).unwrap();
    (root, project, db, id, inputs)
}

fn request(db: &SqliteStore, grant: &str, inputs: &LaunchInputs, key: &str) -> PreparedDelegatedReservation {
    let grant = db.delegation_for_reservation_signature(grant).unwrap();
    let raw = rusqlite::Connection::open(&inputs.project_store).unwrap();
    let request = DelegatedReservationRequest {
        schema_version: 1, grant_id: grant.digest, subject: grant.subject,
        store_incarnation: raw.query_row("SELECT incarnation FROM active_work_meta", [], |r| r.get(0)).unwrap(),
        idempotency_key: key.into(), expected_head: db.current_head().unwrap(),
        issued_unix_ms: jiff::Timestamp::now().as_millisecond(), inputs: inputs.clone(),
    };
    PreparedDelegatedReservation::parse_verified(&serde_json::to_vec(&request).unwrap()).unwrap()
}

fn now() -> i64 { jiff::Timestamp::now().as_millisecond() }

#[test]
fn delegated_reservation_is_real_replayable_and_counts_lifetime_and_retained_capacity() {
    let (_root, project, mut db, grant, inputs) = setup();
    let before = db.read_snapshot(None).unwrap();
    let draft = db.draft_delegated_reservation(&grant, "unsigned", inputs[0].clone(), now()).unwrap();
    assert_eq!(draft.inputs, inputs[0]);
    assert_eq!(draft.expected_head, before.head);
    assert_eq!(db.read_snapshot(None).unwrap(), before);
    let first = request(&db, &grant, &inputs[0], "first");
    let reserved = db.reserve_delegated(&first, now()).unwrap();
    assert!(db.read_snapshot(None).unwrap().attempts[0].retains_capacity());
    let other = request(&db, &grant, &inputs[1], "second");
    let before = db.read_snapshot(None).unwrap();
    let error = db.reserve_delegated(&other, now()).unwrap_err();
    assert!(error.to_string().contains("concurrent"), "{error}");
    assert_eq!(db.read_snapshot(None).unwrap(), before);
    db.cancel_attempt(&reserved.record.attempt, 1, before.head, "no launch", now()).unwrap();
    drop(db);
    let mut db = SqliteStore::open(&store_file(&project).unwrap()).unwrap();
    let replay = db.reserve_delegated(&first, now()).unwrap();
    assert_eq!(serde_json::to_value(&replay).unwrap(), serde_json::to_value(&reserved).unwrap());
    let mut changed = first.request.clone(); changed.issued_unix_ms += 1;
    let changed = PreparedDelegatedReservation::parse_verified(&serde_json::to_vec(&changed).unwrap()).unwrap();
    assert!(matches!(db.reserve_delegated(&changed, now()), Err(StoreError::Conflict)));
    let second = request(&db, &grant, &inputs[1], "second");
    let second = db.reserve_delegated(&second, now()).unwrap();
    db.cancel_attempt(&second.record.attempt, 1, second.head, "no launch", now()).unwrap();
    let third = request(&db, &grant, &inputs[2], "third");
    let error = db.reserve_delegated(&third, now()).unwrap_err();
    assert!(error.to_string().contains("lifetime"), "{error}");
    assert_eq!(db.read_snapshot(None).unwrap().attempts.len(), 2);
}

#[test]
fn cancelled_claim_with_expired_lease_still_consumes_delegated_capacity() {
    let (_root, _project, mut db, grant, inputs) = setup();
    let first = request(&db, &grant, &inputs[0], "first");
    let reserved = db.reserve_delegated(&first, now()).unwrap();
    let claimed_at = now();
    db.claim_operation(&reserved.record.operation, 1, "worker", claimed_at, 1_000).unwrap();
    let cancelled = db.cancel_attempt(&reserved.record.attempt, 1, db.current_head().unwrap(), "stop uncertain worker", claimed_at+1).unwrap();
    assert!(!cancelled.released);
    let second = request(&db, &grant, &inputs[1], "second");
    let error = db.reserve_delegated(&second, claimed_at+10_000).unwrap_err();
    assert!(error.to_string().contains("concurrent"), "{error}");
    assert!(db.read_snapshot(None).unwrap().attempts[0].retains_capacity());
}

#[test]
fn concurrent_clients_cannot_double_spend_delegated_capacity() {
    let (_root, project, mut db, grant, inputs) = setup();
    let requests = [request(&db, &grant, &inputs[0], "first"), request(&db, &grant, &inputs[1], "second")];
    assert_eq!(requests[0].request.expected_head, requests[1].request.expected_head);
    let path = store_file(&project).unwrap();
    let barrier = std::sync::Barrier::new(3);
    let results = std::thread::scope(|scope| {
        let clients: Vec<_> = requests.iter().map(|request| {
            let path = &path; let barrier = &barrier;
            scope.spawn(move || {
                let mut client = SqliteStore::open(path).unwrap();
                barrier.wait();
                client.reserve_delegated(request, now())
            })
        }).collect();
        barrier.wait();
        clients.into_iter().map(|client| client.join().unwrap()).collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let loser = results.iter().position(|result| result.is_err()).unwrap();
    // A fresh head cannot turn the loser's stale/conflicting transaction into
    // an extra admission while the winner's termination remains unobserved.
    let retry = request(&db, &grant, &inputs[loser], "retry");
    let error = db.reserve_delegated(&retry, now()).unwrap_err();
    assert!(error.to_string().contains("concurrent"), "{error}");
    let raw = rusqlite::Connection::open(path).unwrap();
    assert_eq!(raw.query_row("SELECT count(*) FROM delegated_reservations", [], |row| row.get::<_, u64>(0)).unwrap(), 1);
    assert_eq!(db.read_snapshot(None).unwrap().attempts.len(), 1);
}

#[test]
fn delegated_reservation_respects_resource_claims_when_automatic_admission_is_off() {
    let (_root, project, mut db, grant, inputs) = setup_with_limits(2, true);
    let raw = rusqlite::Connection::open(&inputs[0].project_store).unwrap();
    let column = ["factory", "_admission"].concat();
    raw.execute(&format!("UPDATE project_control SET {column}=?1"), ["off"]).unwrap();
    let first = request(&db, &grant, &inputs[0], "first");
    db.reserve_delegated(&first, now()).unwrap();
    assert!(prepared_admission_inputs(&project).unwrap().is_none());
    let second = request(&db, &grant, &inputs[1], "second");
    let before = db.read_snapshot(None).unwrap();
    let draft_error = db.draft_delegated_reservation(&grant, "second-draft", inputs[1].clone(), now()).unwrap_err();
    assert!(draft_error.to_string().contains("resource_conflict"), "{draft_error}");
    let error = db.reserve_delegated(&second, now()).expect_err("a grant cannot authorize overlapping retained resource claims");
    assert!(error.to_string().contains("resource_conflict"), "{error}");
    assert_eq!(db.read_snapshot(None).unwrap(), before);
}

#[test]
fn owner_reservation_and_draft_also_preserve_resources_with_automatic_admission_off() {
    let (_root, _project, mut db, _grant, mut inputs) = setup_with_limits(2, true);
    let raw = rusqlite::Connection::open(&inputs[0].project_store).unwrap();
    let column = ["factory", "_admission"].concat();
    raw.execute(&format!("UPDATE project_control SET {column}=?1"), ["off"]).unwrap();
    for input in &mut inputs {
        let approval = ApprovalGrant {
            version: 1, scope: ApprovalScope::for_launch(input).unwrap(),
            policy: input.effective_profile.as_ref().unwrap().permission_policy.clone(),
            issued_unix_ms: now(), expires_unix_ms: 9_000_000_000_000,
        };
        input.approval = db.install_approval(&PreparedApproval { grant: approval }, db.current_head().unwrap(), now()).unwrap();
    }
    db.reserve_prepared(&[PreparedLaunch { inputs: inputs[0].clone() }], db.current_head().unwrap(), now()).unwrap();
    let before = db.read_snapshot(None).unwrap();
    let draft_error = db.validate_launch_draft(&inputs[1], before.head, now()).unwrap_err();
    assert!(draft_error.to_string().contains("resource_conflict"), "{draft_error}");
    let error = db.reserve_prepared(&[PreparedLaunch { inputs: inputs[1].clone() }], before.head, now()).unwrap_err();
    assert!(error.to_string().contains("resource_conflict"), "{error}");
    assert_eq!(db.read_snapshot(None).unwrap(), before);
}

#[test]
fn delegated_ledger_failure_rolls_back_approval_attempt_task_and_operation() {
    let (_root, project, mut db, grant, inputs) = setup();
    let request = request(&db, &grant, &inputs[0], "first");
    let raw = rusqlite::Connection::open(store_file(&project).unwrap()).unwrap();
    raw.execute_batch("CREATE TRIGGER refuse_delegated BEFORE INSERT ON delegated_reservations BEGIN SELECT RAISE(ABORT,'fixture ledger failure'); END;").unwrap();
    let before = db.read_snapshot(None).unwrap();
    assert!(db.reserve_delegated(&request, now()).is_err());
    assert_eq!(db.read_snapshot(None).unwrap(), before);
    assert_eq!(raw.query_row("SELECT count(*) FROM delegated_reservations", [], |r| r.get::<_, u64>(0)).unwrap(), 0);
    raw.execute_batch("DROP TRIGGER refuse_delegated").unwrap();
    assert!(db.reserve_delegated(&request, now()).is_ok());
}

#[test]
fn delegation_revocation_is_rechecked_before_claim_and_after_consumption() {
    for claimed in [false, true] {
        let (_root, _project, mut db, grant, inputs) = setup();
        let request = request(&db, &grant, &inputs[0], "first");
        let reserved = db.reserve_delegated(&request, now()).unwrap();
        let claim = claimed.then(|| db.claim_operation(&reserved.record.operation, 1, "worker", now(), 10_000).unwrap());
        db.revoke_delegation(&grant, db.current_head().unwrap(), now(), "stop future work").unwrap();
        if let Some(claim) = claim { assert!(db.validate_claim(&claim, now()).is_err()); }
        else { assert!(db.claim_operation(&reserved.record.operation, 1, "worker", now(), 10_000).is_err()); }
        assert!(db.read_snapshot(None).unwrap().attempts[0].retains_capacity());
    }
}

#[test]
fn delegated_scope_and_incarnation_cannot_be_widened_by_a_subject() {
    let (_root, _project, mut db, grant, inputs) = setup();
    for field in 0..7 {
        let mut draft = request(&db, &grant, &inputs[0], "first").request;
        match field {
            0 => draft.inputs.task_contract.as_mut().unwrap().revision += 1,
            1 => draft.inputs.profile.digest = "f".repeat(64),
            2 => draft.inputs.budget = None,
            3 => draft.inputs.repositories[0].commit = "e".repeat(40),
            4 => draft.store_incarnation = "d".repeat(64),
            5 => draft.subject = "another-subject".into(),
            _ => draft.issued_unix_ms = 9_000_000_000_001,
        }
        let prepared = PreparedDelegatedReservation::parse_verified(&serde_json::to_vec(&draft).unwrap()).unwrap();
        let before = db.read_snapshot(None).unwrap();
        assert!(db.reserve_delegated(&prepared, now()).is_err(), "field {field}");
        assert_eq!(db.read_snapshot(None).unwrap(), before);
    }
}
