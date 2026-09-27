use super::*;

const SHA1: &[u8]=include_bytes!("../../../contracts/factory/task-contract-sha1-v1.json");
const SHA256: &[u8]=include_bytes!("../../../contracts/factory/task-contract-sha256-v1.json");

#[test]
fn task_contract_v2_requires_an_exact_versioned_barrier_reference() {
    let raw = include_bytes!("../../../contracts/factory/task-contract-barrier-v2.json");
    let parsed = PreparedContract::parse_verified(raw).unwrap();
    assert_eq!(parsed.raw, raw);
    assert_eq!(parsed.digest, include_str!("../../../contracts/factory/task-contract-barrier-v2.sha256").trim());
    assert_eq!(parsed.required_barrier.as_ref().unwrap().release_sequence, 37);
    let original: serde_json::Value = serde_json::from_slice(raw).unwrap();
    for (pointer, value) in [
        ("/version", 1.into()),
        ("/required_barrier", serde_json::Value::Null),
        ("/required_barrier/schema_version", 2.into()),
        ("/required_barrier/release_sequence", 0.into()),
        ("/required_barrier/release_sequence", u64::MAX.into()),
        ("/required_barrier/barrier_id", "not-a-digest".into()),
        ("/required_barrier/authorization_digest", "AB".repeat(32).into()),
    ] {
        let mut invalid = original.clone();
        *invalid.pointer_mut(pointer).unwrap() = value;
        assert!(PreparedContract::parse_verified(&serde_json::to_vec(&invalid).unwrap()).is_err(), "{pointer}");
    }
    let mut missing = original.clone();
    missing.as_object_mut().unwrap().remove("required_barrier");
    assert!(PreparedContract::parse_verified(&serde_json::to_vec(&missing).unwrap()).is_err());
    let mut unknown = original;
    unknown["required_barrier"]["untrusted_override"] = true.into();
    assert!(PreparedContract::parse_verified(&serde_json::to_vec(&unknown).unwrap()).is_err());
    let duplicated = std::str::from_utf8(raw).unwrap().replace("\"release_sequence\": 37", "\"release_sequence\": 37, \"release_sequence\": 37");
    assert!(PreparedContract::parse_verified(duplicated.as_bytes()).is_err());
}

#[test]
fn task_contract_fixed_vectors_preserve_signed_bytes_and_explicit_oid_formats() {
    for (raw,digest,format) in [
        (SHA1,include_str!("../../../contracts/factory/task-contract-sha1-v1.sha256"),ObjectFormat::Sha1),
        (SHA256,include_str!("../../../contracts/factory/task-contract-sha256-v1.sha256"),ObjectFormat::Sha256),
    ] {
        let contract=PreparedContract::parse_verified(raw).unwrap();
        assert_eq!(contract.raw,raw);
        assert_eq!(contract.digest,digest.trim());
        assert_eq!(contract.object_format,format);
        assert_eq!(contract.base_oid.len(),format.oid_len());
        assert_eq!(contract.dependencies[0].policy_digest.is_some(),format==ObjectFormat::Sha256);
        let mut changed=raw.to_vec();changed.push(b' ');
        let changed=PreparedContract::parse_verified(&changed).unwrap();
        assert_eq!(changed.dependencies,contract.dependencies);
        assert_ne!(changed.digest,contract.digest);
    }
}

#[test]
fn task_contract_vectors_reject_duplicate_and_unknown_critical_fields() {
    let raw=std::str::from_utf8(SHA1).unwrap();
    for needle in ["\"version\": 1", "\"revision\": 1", "\"policy_id\": \"tests\"", "\"access\": \"write\""] {
        let duplicated=raw.replacen(needle,&format!("{needle}, {needle}"),1);
        assert_ne!(duplicated,raw);
        assert!(PreparedContract::parse_verified(duplicated.as_bytes()).is_err(),"{needle}");
    }
    for pointer in ["", "/authority", "/dependencies/0", "/acceptance_policies/0", "/scope/paths/0"] {
        let mut value:serde_json::Value=serde_json::from_slice(SHA1).unwrap();
        value.pointer_mut(pointer).unwrap().as_object_mut().unwrap().insert("unknown_authority".into(),true.into());
        assert!(PreparedContract::parse_verified(&serde_json::to_vec(&value).unwrap()).is_err(),"{pointer}");
    }
}

#[test]
fn task_contract_vectors_reject_ambiguous_dependencies_and_mismatched_git_formats() {
    for raw in [SHA1,SHA256] {
        let original:serde_json::Value=serde_json::from_slice(raw).unwrap();
        let mut duplicate=original.clone();
        let dependency=duplicate["dependencies"][0].clone();
        duplicate["dependencies"].as_array_mut().unwrap().push(dependency);
        assert!(PreparedContract::parse_verified(&serde_json::to_vec(&duplicate).unwrap()).is_err());
        let mut wrong_format=original.clone();
        wrong_format["object_format"]=if original["object_format"]=="sha1" {"sha256".into()}else{"sha1".into()};
        assert!(PreparedContract::parse_verified(&serde_json::to_vec(&wrong_format).unwrap()).is_err());
        let mut wrong_revision=original;
        wrong_revision["contract_revision"]=0.into();
        assert!(PreparedContract::parse_verified(&serde_json::to_vec(&wrong_revision).unwrap()).is_err());
    }
}
