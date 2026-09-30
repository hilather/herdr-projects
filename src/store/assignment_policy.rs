//! Assignment-policy settings, the owner-signed `randomized_assignment`
//! authority (factory plan F2.5) and the policy record of an assigned
//! dispatch decision (TM4.7, docs/telemetry/contracts-evaluation.md §9,
//! migration 0065). The settings are the operator's switch (no row: `off`);
//! `assign` additionally names an installed, unexpired owner grant that
//! permits the first policy. Signatures are verified by `crate::authority`
//! before any value here is constructed; the store keeps the exact signed
//! bytes and re-derives every field from them.
use super::*;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

pub const GRANT_SCHEMA: &str = "randomized_assignment_authority.v1";
pub const SCOPE: &str = "randomized_assignment";
/// Effects a grant never confers; the owner signs the full list.
pub const PROHIBITED_EFFECTS: [&str; 5] = ["alter_review_policy", "alter_verification_policy", "choose_outside_eligible_set", "exceed_budget", "increase_permissions"];
pub const MODES: [&str; 4] = ["off", "shadow", "suggest", "assign"];
const MAX_VALIDITY_MS: i64 = 366 * 86_400_000;
const MAX_BYTES: usize = 65_536;

fn invalid(message: impl Into<String>) -> StoreError { StoreError::Invalid(message.into()) }
fn digest(bytes: &[u8]) -> String { format!("sha256:{:x}", Sha256::digest(bytes)) }
fn hex64(value: &str) -> bool { value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) }
fn sha_ref(value: &str) -> bool { value.strip_prefix("sha256:").is_some_and(hex64) }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantDocument {
    schema: String,
    scope: String,
    issuer: String,
    project_store: String,
    policies: Vec<String>,
    max_exploration_ppm: u32,
    arm_caps: BTreeMap<String, u32>,
    valid_from_unix_ms: i64,
    expires_unix_ms: i64,
    prohibited_effects: Vec<String>,
    authority: VersionedReference,
}

/// An owner-signed `randomized_assignment` grant. Constructed only from bytes
/// whose owner signature `crate::authority` has verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedAssignmentAuthority {
    pub(crate) raw: Vec<u8>,
    pub(crate) grant_id: String,
    pub(crate) project_store: String,
    pub(crate) policies: Vec<String>,
    pub(crate) max_exploration_ppm: u32,
    pub(crate) arm_caps: BTreeMap<String, u32>,
    pub(crate) valid_from_unix_ms: i64,
    pub(crate) expires_unix_ms: i64,
    pub(crate) authority: VersionedReference,
}

impl PreparedAssignmentAuthority {
    /// Parse bytes whose owner signature was already verified. Checks only shape.
    pub(crate) fn parse_verified(raw: &[u8]) -> std::result::Result<Self, String> {
        if raw.len() > MAX_BYTES { return Err("assignment authority document exceeds 65536 bytes".into()); }
        let d: GrantDocument = serde_json::from_slice(raw).map_err(|e| format!("invalid assignment authority document: {e}"))?;
        if d.schema != GRANT_SCHEMA || d.scope != SCOPE { return Err(format!("an assignment authority grant is schema {GRANT_SCHEMA}, scope {SCOPE}")); }
        if d.issuer != "owner" { return Err("an assignment authority grant is issued by the owner".into()); }
        if !d.project_store.starts_with('/') || d.project_store.len() > 4096 { return Err("project_store must be an absolute path".into()); }
        if d.policies.is_empty() || d.policies.len() > 4 || !d.policies.windows(2).all(|w| w[0] < w[1]) || !d.policies.iter().all(|p| POLICIES.contains(&p.as_str())) {
            return Err(format!("policies are 1 to 4 sorted, distinct names of {}", POLICIES.join(", ")));
        }
        if d.max_exploration_ppm > PPM { return Err("max_exploration_ppm is at most 1000000".into()); }
        if d.arm_caps.is_empty() || d.arm_caps.len() > 16 || !d.arm_caps.keys().all(|k| sha_ref(k)) || d.arm_caps.values().any(|c| !(1..=100_000).contains(c)) {
            return Err("arm_caps maps 1 to 16 configuration ids to caps of 1 to 100000 decisions".into());
        }
        if d.prohibited_effects != PROHIBITED_EFFECTS { return Err(format!("prohibited_effects must list exactly {}", PROHIBITED_EFFECTS.join(", "))); }
        if d.valid_from_unix_ms < 0 || d.expires_unix_ms <= d.valid_from_unix_ms || d.expires_unix_ms - d.valid_from_unix_ms > MAX_VALIDITY_MS {
            return Err("a grant is valid for a positive interval of at most 366 days".into());
        }
        if d.authority.id.is_empty() || d.authority.revision == 0 || !hex64(&d.authority.digest) { return Err("invalid authority policy reference".into()); }
        Ok(Self { grant_id: digest(raw), raw: raw.to_vec(), project_store: d.project_store, policies: d.policies, max_exploration_ppm: d.max_exploration_ppm,
            arm_caps: d.arm_caps, valid_from_unix_ms: d.valid_from_unix_ms, expires_unix_ms: d.expires_unix_ms, authority: d.authority })
    }
    pub fn grant_id(&self) -> &str { &self.grant_id }
    pub(crate) fn authority(&self) -> &VersionedReference { &self.authority }
    pub(crate) fn raw(&self) -> &[u8] { &self.raw }
    /// Whether this grant lets `spec` assign at `now`: a permitted policy
    /// within the exploration share, inside the validity interval.
    pub(crate) fn permits(&self, spec: &PolicySpec, now: i64) -> std::result::Result<(), String> {
        if now < self.valid_from_unix_ms || now >= self.expires_unix_ms { return Err("the randomized_assignment grant is not valid now".into()); }
        if !self.policies.contains(&spec.policy) { return Err(format!("the randomized_assignment grant does not permit {}", spec.policy)); }
        if spec.epsilon_ppm.is_some_and(|e| e > self.max_exploration_ppm) { return Err("epsilon_ppm exceeds the grant's max_exploration_ppm".into()); }
        Ok(())
    }
    /// The effective cap of an arm under `spec`: the smaller of the spec's and
    /// the grant's; an arm the grant does not list has cap 0.
    pub fn effective_caps(&self, spec: &PolicySpec) -> BTreeMap<String, u32> {
        let mut caps = spec.arm_caps.clone();
        for (arm, cap) in &self.arm_caps {
            let entry = caps.entry(arm.clone()).or_insert(*cap);
            *entry = (*entry).min(*cap);
        }
        caps.retain(|arm, _| self.arm_caps.contains_key(arm));
        caps
    }
}

/// The current operator switch and policies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignmentSettings {
    pub revision: u64,
    pub mode: String,
    pub policies: Vec<PolicySpec>,
    pub grant_id: Option<String>,
    pub set_unix_ms: i64,
}

/// Whether this store has the assignment-policy tables (migration 0065).
pub(crate) fn present(db: &Connection) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='assignment_policy_settings')", [], |r| r.get(0))?)
}

fn require(db: &Connection) -> Result<()> {
    check_schema(db)?;
    if !present(db)? { return Err(StoreError::UnsupportedSchema(db.query_row("PRAGMA user_version", [], |r| r.get(0))?)); }
    Ok(())
}

fn project_store(db: &Connection) -> Result<String> {
    let path = db.path().ok_or_else(|| invalid("assignment authority requires a file-backed store"))?;
    std::fs::canonicalize(path).map(|p| p.to_string_lossy().into_owned()).map_err(|_| invalid("store path unavailable"))
}

/// The latest settings row, `None` without one (or before 0065): `off`.
pub(crate) fn current(db: &Connection) -> Result<Option<AssignmentSettings>> {
    if !present(db)? { return Ok(None); }
    let row: Option<(i64, String, String, Option<String>, i64)> = db.query_row(
        "SELECT revision,mode,policies,grant_id,set_unix_ms FROM assignment_policy_settings ORDER BY revision DESC LIMIT 1", [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()?;
    let Some((revision, mode, policies, grant_id, set_unix_ms)) = row else { return Ok(None) };
    let specs: Vec<PolicySpec> = serde_json::from_str(&policies).map_err(|_| StoreError::Corrupt("assignment policy settings".into()))?;
    let specs = specs.into_iter().map(|s| s.normalized().map_err(|e| StoreError::Corrupt(format!("assignment policy settings: {e}")))).collect::<Result<_>>()?;
    Ok(Some(AssignmentSettings { revision: u64::try_from(revision).map_err(|_| StoreError::Corrupt("settings revision".into()))?, mode, policies: specs, grant_id, set_unix_ms }))
}

fn load_grant(db: &Connection, grant_id: &str) -> Result<(PreparedAssignmentAuthority, Vec<u8>)> {
    let row: Option<(Vec<u8>, Vec<u8>)> = db.query_row("SELECT raw_bytes,signature FROM assignment_authority_grants WHERE grant_id=?1", [grant_id],
        |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    let (raw, signature) = row.ok_or_else(|| invalid(format!("no randomized_assignment grant {grant_id}")))?;
    if digest(&raw) != grant_id { return Err(StoreError::Corrupt("assignment authority grant digest mismatch".into())); }
    let grant = PreparedAssignmentAuthority::parse_verified(&raw).map_err(|e| StoreError::Corrupt(format!("stored assignment authority grant: {e}")))?;
    Ok((grant, signature))
}

/// Decisions that chose `configuration` at or after `since` (per-arm caps count these).
pub(crate) fn chosen_since(db: &Connection, configuration: &str, since: i64) -> Result<u64> {
    Ok(db.query_row("SELECT count(*) FROM dispatch_decisions WHERE chosen_configuration_id=?1 AND decided_unix_ms>=?2", params![configuration, since], |r| r.get(0))?)
}

/// Write the policy record of an assigned decision, after its
/// `dispatch_decisions` row and in the same transaction. Re-checks the
/// switch, the grant and the chosen arm's cap here, so neither a disabled
/// switch nor a concurrent assignment can carry a stale evaluation past them.
pub(super) fn record(tx: &Connection, attempt: &AttemptId, chosen: &str, assignment: &PolicyAssignment, now: i64) -> Result<()> {
    require(tx)?;
    let settings = current(tx)?.ok_or_else(|| invalid("randomized assignment is off"))?;
    if settings.revision != assignment.settings_revision || settings.mode != "assign" || settings.grant_id.as_deref() != Some(assignment.grant_id.as_str())
        || settings.policies.first() != Some(&assignment.spec) {
        return Err(invalid("assignment policy settings changed since the policy was evaluated"));
    }
    let (grant, _) = load_grant(tx, &assignment.grant_id)?;
    if grant.project_store != project_store(tx)? { return Err(invalid("randomized_assignment grant belongs to another project")); }
    grant.permits(&assignment.spec, now).map_err(invalid)?;
    let cap = grant.effective_caps(&assignment.spec).get(chosen).copied().unwrap_or(0);
    // This decision's row is already written: at most `cap` decisions for the arm.
    if chosen_since(tx, chosen, settings.set_unix_ms)? > u64::from(cap) { return Err(invalid("the chosen arm's budget cap is reached")); }
    let constraints = serde_json::Value::Array(assignment.constraints.iter().map(|(c, r)| serde_json::json!({"configuration_id": c, "reason": r})).collect()).to_string();
    tx.execute("INSERT INTO dispatch_policy_assignments(attempt_id,settings_revision,policy,policy_digest,spec,seed,draw_ppm,grant_id,constraints,assigned_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![attempt.as_str(), integer(assignment.settings_revision)?, assignment.spec.policy, assignment.spec.digest(), assignment.spec.canonical_json(),
            format!("{:016x}", assignment.seed), assignment.draw_ppm, assignment.grant_id, constraints, now])?;
    Ok(())
}

/// Settings history, grants and assignment counts, as JSON. `None` before 0065.
pub fn assignment_state(db: &Connection, now: i64) -> Result<Option<serde_json::Value>> {
    if !present(db)? { return Ok(None); }
    let settings = current(db)?;
    let mut grants = Vec::new();
    let ids: Vec<String> = db.prepare("SELECT grant_id FROM assignment_authority_grants ORDER BY installed_unix_ms,grant_id")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    for id in ids {
        let (g, _) = load_grant(db, &id)?;
        let status = if now < g.valid_from_unix_ms { "not_yet_valid" } else if now >= g.expires_unix_ms { "expired" } else { "active" };
        grants.push(serde_json::json!({"grant_id": g.grant_id, "status": status, "policies": g.policies, "max_exploration_ppm": g.max_exploration_ppm,
            "arm_caps": g.arm_caps, "valid_from_unix_ms": g.valid_from_unix_ms, "expires_unix_ms": g.expires_unix_ms, "prohibited_effects": PROHIBITED_EFFECTS}));
    }
    let history: Vec<serde_json::Value> = db.prepare("SELECT revision,mode,grant_id,set_unix_ms FROM assignment_policy_settings ORDER BY revision")?
        .query_map([], |r| Ok(serde_json::json!({"revision": r.get::<_, i64>(0)?, "mode": r.get::<_, String>(1)?, "grant_id": r.get::<_, Option<String>>(2)?,
            "set_unix_ms": r.get::<_, i64>(3)?})))?.collect::<rusqlite::Result<_>>()?;
    let assigned: i64 = db.query_row("SELECT count(*) FROM dispatch_policy_assignments", [], |r| r.get(0))?;
    Ok(Some(serde_json::json!({
        "mode": settings.as_ref().map_or("off", |s| s.mode.as_str()),
        "default": settings.is_none(),
        "revision": settings.as_ref().map(|s| s.revision),
        "policies": settings.as_ref().map(|s| s.policies.iter().map(|p| serde_json::json!({"policy": p.policy, "policy_digest": p.digest(),
            "spec": serde_json::from_str::<serde_json::Value>(&p.canonical_json()).unwrap_or_default()})).collect::<Vec<_>>()).unwrap_or_default(),
        "grant_id": settings.as_ref().and_then(|s| s.grant_id.clone()),
        "set_unix_ms": settings.as_ref().map(|s| s.set_unix_ms),
        "history": history, "grants": grants, "assigned_decisions": assigned,
    })))
}

impl SqliteStore {
    /// The current switch and policies; `None` (off) without a row or before 0065.
    pub fn assignment_settings(&self) -> Result<Option<AssignmentSettings>> { current(&self.connection) }

    /// Decisions that chose `configuration` at or after `since`.
    pub(crate) fn arm_decisions_since(&self, configuration: &str, since: i64) -> Result<u64> { chosen_since(&self.connection, configuration, since) }

    /// Install one verified grant. The same bytes replay; an expired grant or
    /// one for another project is refused. A grant never changes.
    pub fn install_assignment_authority(&mut self, grant: &PreparedAssignmentAuthority, signature: &[u8], now: i64) -> Result<serde_json::Value> {
        let reparsed = PreparedAssignmentAuthority::parse_verified(&grant.raw).map_err(invalid)?;
        if reparsed != *grant { return Err(invalid("changed assignment authority bytes")); }
        if signature.is_empty() || signature.len() > 8192 { return Err(invalid("invalid assignment authority signature")); }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(&tx)?;
        if grant.project_store != project_store(&tx)? { return Err(invalid("randomized_assignment grant belongs to another project")); }
        if now >= grant.expires_unix_ms { return Err(invalid("randomized_assignment grant is expired")); }
        let existing: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM assignment_authority_grants WHERE grant_id=?1)", [&grant.grant_id], |r| r.get(0))?;
        if !existing {
            tx.execute("INSERT INTO assignment_authority_grants(grant_id,raw_bytes,signature,scope,issuer,project_store,policies,max_exploration_ppm,arm_caps,valid_from_unix_ms,expires_unix_ms,authority_revision,authority_digest,installed_unix_ms)
                VALUES(?1,?2,?3,?4,'owner',?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                params![grant.grant_id, grant.raw, signature, SCOPE, grant.project_store, serde_json::to_string(&grant.policies).unwrap_or_default(),
                    grant.max_exploration_ppm, serde_json::to_string(&grant.arm_caps).unwrap_or_default(), grant.valid_from_unix_ms, grant.expires_unix_ms,
                    integer(grant.authority.revision)?, grant.authority.digest, now])?;
            tx.commit()?;
        }
        Ok(serde_json::json!({"grant_id": grant.grant_id, "installed": !existing, "expires_unix_ms": grant.expires_unix_ms}))
    }

    /// The stored grant, re-derived from its bytes: `(grant, owner signature)`.
    pub fn assignment_authority_grant(&self, grant_id: &str) -> Result<(PreparedAssignmentAuthority, Vec<u8>)> { load_grant(&self.connection, grant_id) }

    /// Append a settings revision (the operator switch). `assign` requires
    /// `grant`, re-verified by the caller, that permits the first policy now.
    pub fn set_assignment_settings(&mut self, mode: &str, policies: &[PolicySpec], grant: Option<&PreparedAssignmentAuthority>, now: i64) -> Result<AssignmentSettings> {
        if !MODES.contains(&mode) { return Err(invalid(format!("mode is one of {}", MODES.join(", ")))); }
        if mode != "off" && policies.is_empty() { return Err(invalid("shadow, suggest and assign need at least one --policy")); }
        if policies.len() > 8 { return Err(invalid("at most 8 policies")); }
        let mut digests = BTreeSet::new();
        for spec in policies {
            if spec.clone().normalized().as_ref() != Ok(spec) { return Err(invalid("policy spec is not normalized")); }
            if !digests.insert(spec.digest()) { return Err(invalid("duplicate policy spec")); }
        }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(&tx)?;
        let grant_id = match (mode, grant) {
            ("assign", None) => return Err(invalid("assign requires an owner-signed randomized_assignment grant (--grant)")),
            ("assign", Some(grant)) => {
                let (stored, _) = load_grant(&tx, &grant.grant_id)?;
                if stored != *grant { return Err(invalid("changed assignment authority bytes")); }
                if grant.project_store != project_store(&tx)? { return Err(invalid("randomized_assignment grant belongs to another project")); }
                grant.permits(&policies[0], now).map_err(invalid)?;
                Some(grant.grant_id.clone())
            }
            (_, Some(_)) => return Err(invalid("--grant applies to assign only")),
            (_, None) => None,
        };
        let revision: i64 = tx.query_row("SELECT coalesce(max(revision),0)+1 FROM assignment_policy_settings", [], |r| r.get(0))?;
        let text = serde_json::to_string(policies).map_err(|e| invalid(e.to_string()))?;
        tx.execute("INSERT INTO assignment_policy_settings(revision,mode,policies,grant_id,set_unix_ms) VALUES(?1,?2,?3,?4,?5)", params![revision, mode, text, grant_id, now])?;
        tx.commit()?;
        Ok(AssignmentSettings { revision: revision as u64, mode: mode.to_owned(), policies: policies.to_vec(), grant_id, set_unix_ms: now })
    }
}
