//! Factory contracts and untrusted result submissions.
//! `PreparedContract` is built only after the raw file bytes verify. It is not
//! `Deserialize`: a signed document is parsed into an untrusted struct, then copied.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{TaskId, VersionedReference};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}
impl ObjectFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
        }
    }
    pub fn oid_len(self) -> usize {
        match self {
            Self::Sha1 => 40,
            Self::Sha256 => 64,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractRoute {
    VerifyThenIntegrate,
    VerifyOnly,
}
impl ContractRoute {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VerifyThenIntegrate => "verify_then_integrate",
            Self::VerifyOnly => "verify_only",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptancePolicy {
    pub(crate) id: String,
    pub(crate) text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScopeAccess {
    Read,
    Write,
}
impl ScopeAccess {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

/// Exact file paths can be compared literally. Globs and directory prefixes
/// stay uncertain so a later schema can conflict without this install locking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScopeCertainty {
    Exact,
    Uncertain,
}
impl ScopeCertainty {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Uncertain => "uncertain",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NamedResource {
    Schema,
    Lockfile,
    Generated,
}
impl NamedResource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Schema => "schema",
            Self::Lockfile => "lockfile",
            Self::Generated => "generated",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractScopePath {
    pub(crate) path: String,
    pub(crate) access: ScopeAccess,
    pub(crate) certainty: ScopeCertainty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractNamedResource {
    pub(crate) name: NamedResource,
    pub(crate) access: ScopeAccess,
}

/// Trusted install capability. JSON cannot construct it; callers pass the
/// original signed bytes, which the store keeps without reserializing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedContract {
    pub(crate) raw: Vec<u8>,
    pub(crate) digest: String,
    pub(crate) task_id: TaskId,
    pub(crate) contract_revision: u64,
    pub(crate) plan_revision: Option<u64>,
    pub(crate) project_store: String,
    pub(crate) expected_head: u64,
    pub(crate) repository: String,
    pub(crate) base_oid: String,
    pub(crate) object_format: ObjectFormat,
    pub(crate) memory_snapshot_id: Option<String>,
    pub(crate) route: ContractRoute,
    pub(crate) authority: VersionedReference,
    pub(crate) acceptance_policies: Vec<AcceptancePolicy>,
    pub(crate) scope_paths: Vec<ContractScopePath>,
    pub(crate) named_resources: Vec<ContractNamedResource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContractInstall {
    pub task_id: String,
    pub contract_revision: u64,
    pub digest: String,
    pub replayed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResultReceipt {
    pub submission_id: String,
    pub idempotency_key: String,
    pub payload_digest: String,
    pub replayed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResultObjectView {
    pub oid: String,
    pub relative_path: String,
    pub byte_sha256: String,
    pub size: u64,
}

/// Display copy of an untrusted submission. `claimed_checks` are stored claims,
/// not verifier evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResultView {
    pub submission_id: String,
    pub task_id: String,
    pub contract_revision: u64,
    pub contract_digest: String,
    pub attempt_id: String,
    pub repository: String,
    pub base_oid: String,
    pub candidate_oid: String,
    pub object_format: String,
    pub memory_snapshot_id: Option<String>,
    pub artifact_manifest: serde_json::Value,
    pub claimed_checks: serde_json::Value,
    pub objects: Vec<ResultObjectView>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UntrustedAcceptancePolicy {
    id: String,
    text: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UntrustedDependency {
    predecessor: TaskId,
    edge: String,
    policy_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UntrustedScopePath {
    path: String,
    access: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UntrustedNamedResource {
    name: String,
    access: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct UntrustedScope {
    #[serde(default)]
    paths: Vec<UntrustedScopePath>,
    #[serde(default)]
    named_resources: Vec<UntrustedNamedResource>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UntrustedContractDocument {
    version: u32,
    project_store: String,
    expected_head: u64,
    task_id: TaskId,
    contract_revision: u64,
    #[serde(default)]
    plan_revision: Option<u64>,
    deliverable: String,
    non_goals: String,
    acceptance_policies: Vec<UntrustedAcceptancePolicy>,
    repository: String,
    base_oid: String,
    object_format: ObjectFormat,
    #[serde(default)]
    memory_snapshot_id: Option<String>,
    dependencies: Vec<UntrustedDependency>,
    #[serde(default)]
    scope: UntrustedScope,
    capability_flags: Vec<String>,
    profile_kind: String,
    retry_class: String,
    result_schema_id: String,
    route: ContractRoute,
    authority: VersionedReference,
}

fn plain(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
fn hex_oid(value: &str, format: ObjectFormat) -> bool {
    value.len() == format.oid_len()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn identifier(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
}

fn scope_access(value: &str) -> Result<ScopeAccess, String> {
    match value {
        "read" => Ok(ScopeAccess::Read),
        "write" => Ok(ScopeAccess::Write),
        _ => Err("invalid scope access".into()),
    }
}

fn named_resource(value: &str) -> Result<NamedResource, String> {
    match value {
        "schema" => Ok(NamedResource::Schema),
        "lockfile" => Ok(NamedResource::Lockfile),
        "generated" => Ok(NamedResource::Generated),
        _ => Err("invalid named resource".into()),
    }
}

/// Collapse `.` and duplicate slashes. `..` is not a repo-relative path.
/// A glob or trailing slash is uncertain overlap data, not a sandbox.
fn normalize_scope_path(raw: &str) -> Result<(String, ScopeCertainty), String> {
    if raw.is_empty()
        || raw.len() > 512
        || raw.starts_with('/')
        || raw.contains('\\')
        || raw.chars().any(char::is_control)
    {
        return Err("scope path must be repo-relative".into());
    }
    let directory = raw.ends_with('/');
    let mut glob = false;
    let mut parts = Vec::new();
    for part in raw.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return Err("scope path must be repo-relative".into());
        }
        if part.contains('*') || part.contains('?') || part.contains('[') {
            glob = true;
        }
        parts.push(part);
    }
    if parts.is_empty() {
        return Err("scope path must be repo-relative".into());
    }
    let mut path = parts.join("/");
    if directory {
        path.push('/');
    }
    if path.len() > 512 {
        return Err("scope path must be repo-relative".into());
    }
    let certainty = if directory || glob {
        ScopeCertainty::Uncertain
    } else {
        ScopeCertainty::Exact
    };
    Ok((path, certainty))
}

impl PreparedContract {
    /// Parse bytes that have already been signature-checked. Does not verify a signature.
    pub(crate) fn parse_verified(raw: &[u8]) -> Result<Self, String> {
        if raw.len() > 65_536 {
            return Err("contract document exceeds 65536 bytes".into());
        }
        let document: UntrustedContractDocument =
            serde_json::from_slice(raw).map_err(|_| "invalid contract document".to_string())?;
        document.into_prepared(raw)
    }
}

impl UntrustedContractDocument {
    fn into_prepared(self, raw: &[u8]) -> Result<PreparedContract, String> {
        if self.version != 1
            || self.contract_revision == 0
            || self.contract_revision > i64::MAX as u64
            || self.expected_head > i64::MAX as u64
        {
            return Err("invalid contract version or revision".into());
        }
        if self
            .plan_revision
            .is_some_and(|revision| revision == 0 || revision > i64::MAX as u64)
        {
            return Err("invalid plan revision".into());
        }
        if !plain(&self.deliverable, 8_000)
            || !plain(&self.non_goals, 8_000)
            || !plain(&self.profile_kind, 128)
            || !plain(&self.retry_class, 128)
            || !plain(&self.result_schema_id, 128)
        {
            return Err("invalid contract text".into());
        }
        if !std::path::Path::new(&self.project_store).is_absolute()
            || !plain(&self.project_store, 4_096)
            || !std::path::Path::new(&self.repository).is_absolute()
            || !plain(&self.repository, 4_096)
        {
            return Err("contract project_store and repository must be absolute paths".into());
        }
        if !hex_oid(&self.base_oid, self.object_format) {
            return Err("contract base oid does not match object format".into());
        }
        if let Some(snapshot) = &self.memory_snapshot_id {
            if !identifier(snapshot) {
                return Err("invalid memory snapshot id".into());
            }
        }
        if self.authority.revision == 0
            || self.authority.revision > i64::MAX as u64
            || !plain(&self.authority.id, 512)
            || !hex_oid(&self.authority.digest, ObjectFormat::Sha256)
        {
            return Err("invalid contract authority reference".into());
        }
        if self.acceptance_policies.is_empty() || self.acceptance_policies.len() > 32 {
            return Err("contract acceptance policies exceed bounds".into());
        }
        let mut seen_policies = std::collections::BTreeSet::new();
        let mut acceptance_policies = Vec::new();
        for policy in self.acceptance_policies {
            if !identifier(&policy.id)
                || !plain(&policy.text, 4_000)
                || !seen_policies.insert(policy.id.clone())
            {
                return Err("invalid acceptance policy".into());
            }
            acceptance_policies.push(AcceptancePolicy {
                id: policy.id,
                text: policy.text,
            });
        }
        if self.dependencies.len() > 64 || self.capability_flags.len() > 32 {
            return Err("contract dependencies or capabilities exceed bounds".into());
        }
        for flag in &self.capability_flags {
            if !plain(flag, 128) {
                return Err("invalid capability flag".into());
            }
        }
        for dependency in &self.dependencies {
            // The predecessor stays inside the signed bytes. This schema does not satisfy it.
            if dependency.predecessor == self.task_id
                || !matches!(
                    dependency.edge.as_str(),
                    "verified_result"
                        | "integrated_commit"
                        | "integration_candidate"
                        | "landed_commit"
                )
                || !seen_policies.contains(&dependency.policy_id)
            {
                return Err("invalid contract dependency".into());
            }
        }
        if self.scope.paths.len() > 64 || self.scope.named_resources.len() > 8 {
            return Err("contract scope exceeds bounds".into());
        }
        let mut seen_paths = std::collections::BTreeSet::new();
        let mut scope_paths = Vec::new();
        for path in self.scope.paths {
            let access = scope_access(&path.access)?;
            let (normalized, certainty) = normalize_scope_path(&path.path)?;
            if !seen_paths.insert(normalized.clone()) {
                return Err("invalid scope path".into());
            }
            scope_paths.push(ContractScopePath {
                path: normalized,
                access,
                certainty,
            });
        }
        let mut seen_resources = std::collections::BTreeSet::new();
        let mut named_resources = Vec::new();
        for resource in self.scope.named_resources {
            let name = named_resource(&resource.name)?;
            let access = scope_access(&resource.access)?;
            if !seen_resources.insert(name) {
                return Err("invalid named resource".into());
            }
            named_resources.push(ContractNamedResource { name, access });
        }
        Ok(PreparedContract {
            digest: format!("{:x}", Sha256::digest(raw)),
            raw: raw.to_vec(),
            task_id: self.task_id,
            contract_revision: self.contract_revision,
            plan_revision: self.plan_revision,
            project_store: self.project_store,
            expected_head: self.expected_head,
            repository: self.repository,
            base_oid: self.base_oid,
            object_format: self.object_format,
            memory_snapshot_id: self.memory_snapshot_id,
            route: self.route,
            authority: self.authority,
            acceptance_policies,
            scope_paths,
            named_resources,
        })
    }
}
