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

/// Trusted factory-admission capability. JSON cannot construct it. The store
/// keeps the original signed bytes and does not reserialize them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedAdmission {
    pub(crate) raw: Vec<u8>,
    pub(crate) digest: String,
    pub(crate) enabled: bool,
    pub(crate) project_store: String,
    pub(crate) evidence_digest: String,
    pub(crate) authority: VersionedReference,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdmissionInstall {
    pub enabled: bool,
    pub factory_admission: String,
    pub policy_digest: String,
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
struct UntrustedAdmissionDocument {
    version: u32,
    enabled: bool,
    project_store: String,
    evidence_digest: String,
    authority: VersionedReference,
}

impl PreparedAdmission {
    /// Parse bytes that have already been signature-checked. Does not verify a signature.
    pub(crate) fn parse_verified(raw: &[u8]) -> Result<Self, String> {
        if raw.is_empty() || raw.len() > 65_536 {
            return Err("admission document exceeds 65536 bytes".into());
        }
        let document: UntrustedAdmissionDocument =
            serde_json::from_slice(raw).map_err(|_| "invalid admission document".to_string())?;
        if document.version != 1 {
            return Err("invalid admission version".into());
        }
        if !std::path::Path::new(&document.project_store).is_absolute()
            || !plain(&document.project_store, 4_096)
        {
            return Err("admission project_store must be an absolute path".into());
        }
        if !hex_oid(&document.evidence_digest, ObjectFormat::Sha256) {
            return Err("admission evidence digest must be lowercase sha256".into());
        }
        if document.authority.revision == 0
            || document.authority.revision > i64::MAX as u64
            || !plain(&document.authority.id, 512)
            || !hex_oid(&document.authority.digest, ObjectFormat::Sha256)
        {
            return Err("invalid admission authority reference".into());
        }
        Ok(Self {
            digest: format!("{:x}", Sha256::digest(raw)),
            raw: raw.to_vec(),
            enabled: document.enabled,
            project_store: document.project_store,
            evidence_digest: document.evidence_digest,
            authority: document.authority,
        })
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DelegationAction {
    ReserveAttempt,
    ReviewMemory,
}
impl DelegationAction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ReserveAttempt => "reserve_attempt",
            Self::ReviewMemory => "review_memory",
        }
    }
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "reserve_attempt" => Ok(Self::ReserveAttempt),
            "review_memory" => Ok(Self::ReviewMemory),
            "runtime_launch" | "launch" => Err("delegation is not a launch approval".into()),
            _ => Err("invalid delegation action".into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DelegationRepoScope {
    pub(crate) repository: String,
    pub(crate) git_ref: String,
}

/// Trusted ingress capability. JSON cannot construct it. Callers keep the
/// original signed bytes; this type is not a launch grant.
/// ```compile_fail
/// use herdr_projects::domain::PreparedDelegation;
/// let _: PreparedDelegation = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedDelegation {
    pub(crate) raw: Vec<u8>,
    pub(crate) digest: String,
    pub(crate) issuer: String,
    pub(crate) subject: String,
    pub(crate) subject_public_key: String,
    pub(crate) actions: Vec<DelegationAction>,
    pub(crate) repositories: Vec<DelegationRepoScope>,
    pub(crate) profile_kinds: Vec<String>,
    pub(crate) max_concurrent_attempts: u32,
    pub(crate) expires_unix_ms: i64,
    pub(crate) revocation_epoch: u64,
    pub(crate) child_delegation: String,
    pub(crate) policy_revision: u64,
    pub(crate) project_store: String,
    pub(crate) authority: VersionedReference,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UntrustedDelegationRepo {
    repository: String,
    #[serde(rename = "ref")]
    git_ref: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UntrustedDelegationDocument {
    version: u32,
    issuer: String,
    subject: String,
    subject_public_key: String,
    action_classes: Vec<String>,
    repositories: Vec<UntrustedDelegationRepo>,
    profile_kinds: Vec<String>,
    max_concurrent_attempts: u32,
    expires_unix_ms: i64,
    revocation_epoch: u64,
    child_delegation: String,
    policy_revision: u64,
    project_store: String,
    authority: VersionedReference,
}

fn ssh_ed25519_key(value: &str) -> bool {
    let mut parts = value.split(' ');
    let kind = parts.next();
    let body = parts.next();
    parts.next().is_none()
        && kind == Some("ssh-ed25519")
        && body.is_some_and(|body| {
            (32..=256).contains(&body.len())
                && body
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"+/=".contains(&byte))
        })
}

impl PreparedDelegation {
    /// Parse bytes that have already been signature-checked. Does not verify a signature.
    pub(crate) fn parse_verified(raw: &[u8]) -> Result<Self, String> {
        if raw.len() > 65_536 {
            return Err("delegation document exceeds 65536 bytes".into());
        }
        let document: UntrustedDelegationDocument =
            serde_json::from_slice(raw).map_err(|_| "invalid delegation document".to_string())?;
        document.into_prepared(raw)
    }

    /// A delegation is not an approval grant and cannot bind a launch.
    pub(crate) fn matches_launch(&self) -> Result<(), String> {
        if serde_json::from_slice::<super::ApprovalGrant>(&self.raw).is_ok() {
            return Err("delegation document must not decode as a launch grant".into());
        }
        Err("delegation is not a launch approval".into())
    }
}

impl UntrustedDelegationDocument {
    fn into_prepared(self, raw: &[u8]) -> Result<PreparedDelegation, String> {
        if self.version != 1 {
            return Err("invalid delegation version".into());
        }
        if self.child_delegation != "forbidden" {
            return Err("child delegation is forbidden".into());
        }
        if !identifier(&self.issuer) || !identifier(&self.subject) {
            return Err("invalid delegation issuer or subject".into());
        }
        // The subject cannot be the issuer. A delegate does not sign its own grant.
        if self.issuer == self.subject {
            return Err("delegation self-signature is forbidden".into());
        }
        if !ssh_ed25519_key(&self.subject_public_key) {
            return Err("invalid delegation subject key".into());
        }
        if self.action_classes.is_empty() || self.action_classes.len() > 8 {
            return Err("delegation actions exceed bounds".into());
        }
        let mut seen_actions = std::collections::BTreeSet::new();
        let mut actions = Vec::new();
        for action in &self.action_classes {
            let parsed = DelegationAction::parse(action)?;
            if !seen_actions.insert(parsed) {
                return Err("invalid delegation action".into());
            }
            actions.push(parsed);
        }
        if self.repositories.is_empty() || self.repositories.len() > 32 {
            return Err("delegation repositories exceed bounds".into());
        }
        let mut seen_repos = std::collections::BTreeSet::new();
        let mut repositories = Vec::new();
        for repo in self.repositories {
            if !std::path::Path::new(&repo.repository).is_absolute()
                || !plain(&repo.repository, 4_096)
                || !repo.git_ref.starts_with("refs/")
                || !plain(&repo.git_ref, 256)
                || repo.git_ref.contains(' ')
                || repo
                    .git_ref
                    .split('/')
                    .any(|part| part.is_empty() || part == "." || part == "..")
            {
                return Err("delegation repository scope must be absolute with a refs name".into());
            }
            if !seen_repos.insert((repo.repository.clone(), repo.git_ref.clone())) {
                return Err("delegation repository scope is duplicated".into());
            }
            repositories.push(DelegationRepoScope {
                repository: repo.repository,
                git_ref: repo.git_ref,
            });
        }
        if self.profile_kinds.is_empty() || self.profile_kinds.len() > 8 {
            return Err("delegation profile kinds exceed bounds".into());
        }
        let mut seen_kinds = std::collections::BTreeSet::new();
        for kind in &self.profile_kinds {
            if !plain(kind, 64) || !seen_kinds.insert(kind.clone()) {
                return Err("invalid delegation profile kind".into());
            }
        }
        if !(1..=64).contains(&self.max_concurrent_attempts) {
            return Err("invalid delegation attempt cap".into());
        }
        if self.expires_unix_ms <= 0
            || self.revocation_epoch == 0
            || self.revocation_epoch > i64::MAX as u64
        {
            return Err("invalid delegation expiry or revocation epoch".into());
        }
        if self.policy_revision == 0
            || self.policy_revision > i64::MAX as u64
            || self.policy_revision != self.authority.revision
        {
            return Err("delegation policy revision does not match its authority".into());
        }
        if !std::path::Path::new(&self.project_store).is_absolute()
            || !plain(&self.project_store, 4_096)
        {
            return Err("delegation project_store must be an absolute path".into());
        }
        if self.authority.revision == 0
            || self.authority.revision > i64::MAX as u64
            || !plain(&self.authority.id, 512)
            || !hex_oid(&self.authority.digest, ObjectFormat::Sha256)
        {
            return Err("invalid delegation authority reference".into());
        }
        Ok(PreparedDelegation {
            digest: format!("{:x}", Sha256::digest(raw)),
            raw: raw.to_vec(),
            issuer: self.issuer,
            subject: self.subject,
            subject_public_key: self.subject_public_key,
            actions,
            repositories,
            profile_kinds: self.profile_kinds,
            max_concurrent_attempts: self.max_concurrent_attempts,
            expires_unix_ms: self.expires_unix_ms,
            revocation_epoch: self.revocation_epoch,
            child_delegation: self.child_delegation,
            policy_revision: self.policy_revision,
            project_store: self.project_store,
            authority: self.authority,
        })
    }
}
