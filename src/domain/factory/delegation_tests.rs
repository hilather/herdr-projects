use super::*;

const VECTOR:&[u8]=include_bytes!("../../../contracts/factory/delegation-v2.json");
fn vector()->serde_json::Value {serde_json::from_slice(VECTOR).unwrap()}
fn parse(value:&serde_json::Value)->Result<PreparedDelegation,String> {
    PreparedDelegation::parse_verified(&serde_json::to_vec(value).unwrap())
}

#[test]
fn delegation_v2_vector_preserves_signed_bytes_and_binds_finite_scope() {
    let parsed=PreparedDelegation::parse_verified(VECTOR).unwrap();
    assert_eq!(parsed.raw,VECTOR);
    assert_eq!(parsed.digest,include_str!("../../../contracts/factory/delegation-v2.sha256").trim());
    let scope=parsed.reservation_scope.as_ref().unwrap();
    assert_eq!(scope.task_contracts.len(),2);
    assert_eq!(scope.profiles.len(),1);
    assert_eq!(scope.max_total_attempts,4);
    assert_eq!(parsed.max_concurrent_attempts,2);
    assert!(parsed.matches_launch().is_err());
    let mut whitespace=VECTOR.to_vec();whitespace.push(b' ');
    let changed=PreparedDelegation::parse_verified(&whitespace).unwrap();
    assert_eq!(changed.reservation_scope,parsed.reservation_scope);
    assert_ne!(changed.digest,parsed.digest,"signature identity is raw bytes, never reserialized JSON");
}

#[test]
fn delegation_scope_cannot_be_removed_or_smuggled_into_version_one() {
    let mut value=vector();value["version"]=1.into();
    assert!(parse(&value).is_err());
    value.as_object_mut().unwrap().remove("reservation_scope");
    let legacy=parse(&value).unwrap();assert!(legacy.reservation_scope.is_none());
    value["version"]=2.into();assert!(parse(&value).is_err());
    let mut value=vector();value["action_classes"]=serde_json::json!(["review_memory"]);
    assert!(parse(&value).is_err());
}

#[test]
fn delegation_v2_rejects_unbounded_ambiguous_or_unpinned_scope() {
    for (pointer,replacement) in [
        ("/reservation_scope/max_total_attempts",serde_json::json!(0)),
        ("/reservation_scope/max_total_attempts",serde_json::json!(1)),
        ("/reservation_scope/max_total_attempts",serde_json::json!(1025)),
        ("/reservation_scope/task_contracts",serde_json::json!([])),
        ("/reservation_scope/profiles",serde_json::json!([])),
        ("/reservation_scope/budget",serde_json::Value::Null),
        ("/reservation_scope/budget/revision",serde_json::json!(0)),
        ("/reservation_scope/budget/digest",serde_json::json!("A".repeat(64))),
        ("/reservation_scope/repository_bases",serde_json::json!([])),
        ("/reservation_scope/repository_bases/0/repository",serde_json::json!("/outside")),
        ("/reservation_scope/repository_bases/0/ref",serde_json::json!("refs/heads/outside")),
        ("/reservation_scope/repository_bases/0/commit_oid",serde_json::json!("f".repeat(64))),
    ] {
        let mut value=vector();*value.pointer_mut(pointer).unwrap()=replacement;
        assert!(parse(&value).is_err(),"accepted altered bound at {pointer}");
    }
    for field in ["task_contracts","profiles","repository_bases"] {
        let mut value=vector();let list=value["reservation_scope"][field].as_array_mut().unwrap();
        list.push(list[0].clone());assert!(parse(&value).is_err(),"duplicate {field}");
    }
    let mut value=vector();value["reservation_scope"]["task_contracts"].as_array_mut().unwrap().reverse();
    assert!(parse(&value).is_err());
    let mut value=vector();value["reservation_scope"]["prohibited_effects_override"]=true.into();
    assert!(parse(&value).is_err());
    let mut value=vector();value["reservation_scope"]["repository_bases"][0]["object_format"]="sha256".into();
    value["reservation_scope"]["repository_bases"][0]["commit_oid"]="f".repeat(64).into();
    assert!(parse(&value).is_ok());
}
