//! Memory identities. Markdown is authoritative until T05.3 cutover to sqlite-v1.
use serde::{Deserialize, Serialize};
use super::{MemoryRecordId, SnapshotId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectId(String);
impl ObjectId {
    pub fn from_hex(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.len() != 64 || !value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err("object hash must be 64 lowercase hex characters".into());
        }
        Ok(Self(value))
    }
    pub fn parse(value: &str) -> Result<Self, String> {
        Self::from_hex(value.strip_prefix("sha256:").unwrap_or(value))
    }
    pub fn as_str(&self) -> &str { &self.0 }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum MemoryKind { Constraint, HardMemory, Contract, Observation, Assumption, TaskLocal }
impl MemoryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Constraint=>"constraint", Self::HardMemory=>"hard_memory", Self::Contract=>"contract",
            Self::Observation=>"observation", Self::Assumption=>"assumption", Self::TaskLocal=>"task_local",
        }
    }
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "constraint"=>Ok(Self::Constraint), "hard_memory"=>Ok(Self::HardMemory), "contract"=>Ok(Self::Contract),
            "observation"=>Ok(Self::Observation), "assumption"=>Ok(Self::Assumption), "task_local"=>Ok(Self::TaskLocal),
            _=>Err("unknown memory kind".into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Applicability { pub domains: Vec<String>, pub paths: Vec<String> }
impl Applicability {
    pub fn validate(&self) -> Result<(), String> {
        for (label, values) in [("domains", &self.domains), ("paths", &self.paths)] {
            if values.len() > 256 { return Err(format!("{label} exceed bounds")); }
            for value in values {
                if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
                    return Err(format!("invalid applicability {label} entry"));
                }
            }
        }
        for path in &self.paths {
            if std::path::Path::new(path).is_absolute()
                || !std::path::Path::new(path).components().all(|c| matches!(c, std::path::Component::Normal(_)))
                || path.contains('\\') {
                return Err("applicability paths must be relative and contained".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: MemoryRecordId,
    pub record_key: String,
    pub scope_id: String,
    pub kind: MemoryKind,
    pub is_hard: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRevision {
    pub record_id: MemoryRecordId,
    pub revision: u64,
    pub body_hash: ObjectId,
    pub provenance_hash: ObjectId,
    pub promoted_seq: u64,
    pub applicability: Applicability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryHead {
    pub record_id: MemoryRecordId,
    pub revision: u64,
    pub status: String,
    pub row_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryValidity {
    pub record_id: MemoryRecordId,
    pub revision: u64,
    pub state: String,
    pub reason: String,
    pub expiry_unix_ms: Option<i64>,
    pub evaluated_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryDependency {
    pub derived_record: MemoryRecordId,
    pub derived_revision: u64,
    pub source_record: MemoryRecordId,
    pub source_revision: u64,
    pub kind: String,
}

#[derive(Debug, Clone)]
pub struct ControlContext { pub now_unix_ms: i64 }

#[derive(Debug, Clone)]
pub struct NewRevision {
    pub id: MemoryRecordId,
    pub record_key: String,
    pub scope_id: String,
    pub kind: MemoryKind,
    pub body_hash: ObjectId,
    pub provenance_hash: ObjectId,
    pub applicability: Applicability,
    pub dependencies: Vec<(MemoryRecordId, u64, String)>,
    pub expected: Option<u64>,
    pub expiry_unix_ms: Option<i64>,
    /// Empty defaults to `valid`. Import uses `stale`.
    pub validity_state: String,
    /// Empty defaults to `control_insert`. Import uses `unverified_import`.
    pub validity_reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveFact {
    pub record: MemoryRecord,
    pub revision: MemoryRevision,
    pub validity: MemoryValidity,
}

pub(crate) fn parse_kind(value: &str) -> Result<MemoryKind, String> { MemoryKind::parse(value) }

pub const SELECTION_POLICY_ID: &str = "memory-selection";
pub const SELECTION_POLICY_VERSION: u32 = 1;
pub const SELECTION_ESTIMATOR: &str = "char-count-v1";

pub struct SelectionWeights {
    pub domain_match: i64,
    pub path_overlap: i64,
    pub symbol_overlap: i64,
    pub dependency_base: i64,
    pub contract: i64,
    pub observation: i64,
    pub assumption: i64,
    pub lexical_cap: i64,
}
pub const WEIGHTS: SelectionWeights = SelectionWeights {
    domain_match: 100, path_overlap: 40, symbol_overlap: 0, dependency_base: 15,
    contract: 10, observation: 6, assumption: 2, lexical_cap: 10,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRequest {
    pub schema_version: u32,
    pub task_id: String,
    pub profile: String,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub pinned_keys: Vec<String>,
    #[serde(default)]
    pub sensitivity: String,
}

#[derive(Debug, Clone)]
pub struct SnapshotPlan {
    pub coordinator: bool,
    pub session_id: Option<String>,
    pub request: SnapshotRequest,
    pub profile_name: String,
    pub profile_digest: String,
    pub config_digest: Option<String>,
    pub budget_chars: u64,
    pub estimator: String,
    pub instructions: String,
    pub now_unix_ms: i64,
    pub expected_heads_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySnapshot {
    pub id: SnapshotId,
    pub task_id: String,
    pub task_revision: u64,
    pub profile_name: String,
    pub profile_digest: String,
    pub config_digest: Option<String>,
    pub selection_policy_version: u32,
    pub estimator: String,
    pub sequence: u64,
    pub required_bytes: u64,
    pub optional_bytes: u64,
    pub budget_bytes: u64,
    pub omitted_optional_count: u64,
    pub manifest_hash: String,
    pub scope_digest: String,
    pub entries: Vec<SnapshotEntry>,
    pub subscriber: String,
    pub since_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotEntry {
    pub ordinal: u64,
    pub record_id: MemoryRecordId,
    pub revision: u64,
    pub role: String,
    pub reason: String,
}

pub fn glob_match(pattern: &str, path: &str) -> bool {
    if pattern == path { return true; }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return path == prefix || path.starts_with(&format!("{prefix}/"));
    }
    if let Some(prefix) = pattern.strip_suffix("/*") {
        return path.strip_prefix(&format!("{prefix}/")).is_some_and(|rest| !rest.is_empty() && !rest.contains('/'));
    }
    false
}

fn tokens(text: &str) -> std::collections::BTreeSet<String> {
    let mut set = std::collections::BTreeSet::new();
    let mut cur = String::new();
    for c in text.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() { cur.push(c); }
        else if !cur.is_empty() { set.insert(std::mem::take(&mut cur)); }
    }
    if !cur.is_empty() { set.insert(cur); }
    set
}

pub fn selection_score(request: &SnapshotRequest, fact: &ActiveFact, hops: Option<u32>) -> i64 {
    let domains = request.domains.iter().any(|d| fact.revision.applicability.domains.iter().any(|x| x == d));
    let paths = request.paths.iter().any(|p| fact.revision.applicability.paths.iter().any(|x| glob_match(p, x) || glob_match(x, p) || p == x));
    let kind = match fact.record.kind {
        MemoryKind::Contract => WEIGHTS.contract,
        MemoryKind::Observation => WEIGHTS.observation,
        MemoryKind::Assumption => WEIGHTS.assumption,
        _ => 0,
    };
    let mut source = request.task_id.clone();
    for key in &request.pinned_keys { source.push(' '); source.push_str(key); }
    let shared = tokens(&source).intersection(&tokens(&fact.record.record_key)).count() as i64;
    let distance = hops.filter(|h| *h <= 4).map(|h| WEIGHTS.dependency_base / (1 + h as i64)).unwrap_or(0);
    (if domains { WEIGHTS.domain_match } else { 0 })
        + (if paths { WEIGHTS.path_overlap } else { 0 })
        + WEIGHTS.symbol_overlap + distance + kind + shared.min(WEIGHTS.lexical_cap)
}
