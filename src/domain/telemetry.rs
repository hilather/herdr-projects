//! Telemetry records derived from canonical state (docs/telemetry/contracts.md).
//! Analytics only: nothing here grants launch or reads outcomes.
use sha2::{Digest, Sha256};

pub const TASK_TAXONOMY: &str = "task-taxonomy.v1";
pub const TASK_CLASSIFIER: &str = "rule:task-taxonomy.v1";

/// Contract properties of one signed task contract revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractScope {
    pub revision: u64,
    pub route: String,
    /// `(path, certainty is uncertain)` for each `write` scope path.
    pub write_paths: Vec<(String, bool)>,
    /// Names of `write` named resources.
    pub write_named_resources: Vec<String>,
}

/// Revision 1 of a contracts §1 classification. JSON keys are written in
/// sorted order, so the bytes are canonical with or without `preserve_order`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskClassification {
    pub id: String,
    pub task: String,
    pub contract_revision: Option<u64>,
    pub class: &'static str,
    pub band: &'static str,
    pub features: String,
    pub created_unix_ms: i64,
}

/// Contracts §1 rubric. `None` contract: class `unscoped`, band `unknown`,
/// contract-derived features unavailable rather than zero.
pub fn classify_task(task: &str, contract: Option<&ContractScope>, dependencies: usize, repositories: usize, now: i64) -> TaskClassification {
    let (class, band, features) = match contract {
        None => {
            let missing = serde_json::json!({"reason": "no_contract", "status": "unavailable"});
            ("unscoped", "unknown", serde_json::json!({"dependencies": dependencies, "repositories": repositories, "route": missing,
                "uncertain_write_paths": missing, "write_named_resources": missing, "write_paths": missing}))
        }
        Some(scope) => {
            let writes = scope.write_paths.len();
            let uncertain = scope.write_paths.iter().filter(|(_, uncertain)| *uncertain).count();
            let named = |name: &str| scope.write_named_resources.iter().any(|n| n == name);
            let every = |rule: &dyn Fn(&str) -> bool| writes > 0 && scope.write_paths.iter().all(|(path, _)| rule(path));
            let under = |dir: &'static str| move |path: &str| path == dir || path.starts_with(&format!("{dir}/"));
            let class = if named("schema") { "schema_change" }
                else if named("lockfile") { "dependency_change" }
                else if writes == 0 && scope.write_named_resources.is_empty() { "read_only" }
                else if every(&|path| under("docs")(path) || path.ends_with(".md")) { "docs" }
                else if every(&under("tests")) { "tests" }
                else { "code" };
            let points = writes.min(8) + 2 * uncertain + 3 * scope.write_named_resources.len() + dependencies
                + 2 * repositories.saturating_sub(1) + usize::from(scope.route == "verify_then_integrate");
            let band = match points { 0..=3 => "small", 4..=8 => "medium", _ => "large" };
            (class, band, serde_json::json!({"dependencies": dependencies, "repositories": repositories, "route": scope.route,
                "uncertain_write_paths": uncertain, "write_named_resources": scope.write_named_resources.len(), "write_paths": writes}))
        }
    };
    let contract_revision = contract.map(|scope| scope.revision);
    let record = serde_json::json!({"band": band, "class": class, "classifier": TASK_CLASSIFIER, "contract_revision": contract_revision,
        "created_unix_ms": now, "features": features, "reason": null, "revision": 1, "task_id": task, "taxonomy": TASK_TAXONOMY});
    TaskClassification {
        id: format!("sha256:{:x}", Sha256::digest(record.to_string().as_bytes())),
        task: task.into(), contract_revision, class, band, features: features.to_string(), created_unix_ms: now,
    }
}

pub const AGENT_CONFIGURATION_SCHEMA: &str = "agent_configuration.v1";
/// Reason codes an operator may give in `LaunchSelection.reason` (contracts §3).
pub const OPERATOR_REASONS: [&str; 8] = ["operator_selected", "recommended", "operator_preference", "availability", "exploration", "replay", "continuation", "unspecified"];

/// Contracts §2 comparison arm: canonical bytes and their `sha256:` identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfiguration {
    pub id: String,
    pub canonical_json: String,
}

/// Derived from the frozen profile without host paths, names or evidence churn.
/// Keys are written in sorted order. Model and effort stay unmapped until certified.
pub fn agent_configuration(profile: &super::FrozenProfile) -> AgentConfiguration {
    let reference = |r: &super::VersionedReference| serde_json::json!({"digest": r.digest, "id": r.id, "revision": r.revision});
    let mut environment = profile.environment_names.clone();
    environment.sort();
    let canonical_json = serde_json::json!({"adapter": reference(&profile.adapter), "agent_digest": profile.agent.digest,
        "agent_version": profile.agent.version, "arguments_digest": profile.arguments_digest, "definition_digest": profile.definition_digest,
        "environment_names": environment, "kind": profile.kind, "permission_policy": reference(&profile.permission_policy),
        "reasoning_effort": null, "reasoning_effort_reason": "mapping_unverified", "requested_model": null,
        "requested_model_reason": "mapping_unverified", "schema": AGENT_CONFIGURATION_SCHEMA}).to_string();
    AgentConfiguration { id: format!("sha256:{:x}", Sha256::digest(canonical_json.as_bytes())), canonical_json }
}

/// One profile weighed for a dispatch, in evaluation order. `status` is one of
/// `chosen`, `no_knowledge`, `no_approval`, `not_evaluated`, and (policy
/// assignment only) `eligible`: approved, weighed by the policy, not chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EligibleProfile {
    pub configuration: AgentConfiguration,
    pub profile_digest: String,
    pub status: &'static str,
}

/// How a new attempt's profile was chosen. Descriptive only: it is never part of
/// `LaunchInputs`, the attempt identity or approval matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchContext {
    /// An owner-approved reservation. `reason` must be one of `OPERATOR_REASONS`.
    Operator { reason: Option<String>, note: Option<String> },
    /// Every profile automatic admission matched, with the status each reached.
    Automatic { eligible: Vec<EligibleProfile> },
    /// A reservation under a delegated grant; the grant is read from the request.
    Delegated,
    /// Automatic admission whose profile an enabled assignment policy chose
    /// among the approved profiles (TM4.7), with its per-entry probabilities.
    Assigned { eligible: Vec<EligibleProfile>, assignment: super::PolicyAssignment },
}

impl DispatchContext {
    pub const OPERATOR: Self = Self::Operator { reason: None, note: None };
}

/// Contracts §7 excerpt: first line, home prefixes as `~`, URL queries and
/// fragments stripped, token-like strings masked, at most 160 scalar values.
pub fn excerpt(text: &str, home: Option<&str>) -> Option<String> {
    let mut line: String = text.lines().next().unwrap_or("").chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    if let Some(home) = home.map(|h| h.trim_end_matches('/')).filter(|h| h.len() > 1) { line = tilde(&line, home, false); }
    for root in ["/home/", "/Users/"] { line = tilde(&line, root, true); }
    let mut bearer = false;
    let words = line.split(' ').map(|word| {
        let masked = if bearer && !word.is_empty() { "[redacted]".to_owned() } else { mask(word) };
        if !word.is_empty() { bearer = word.eq_ignore_ascii_case("bearer"); }
        masked
    }).collect::<Vec<_>>().join(" ");
    let text = words.trim();
    if text.is_empty() { return None; }
    Some(if text.chars().count() > 160 { text.chars().take(159).chain(['…']).collect() } else { text.to_owned() })
}

/// Replace `prefix` (plus one user segment when `user`) at a path boundary with `~`.
fn tilde(line: &str, prefix: &str, user: bool) -> String {
    let (mut out, mut rest) = (String::new(), line);
    while let Some(i) = rest.find(prefix) {
        let after = &rest[i + prefix.len()..];
        let end = if user { after.find(|c: char| c == '/' || c.is_whitespace()).unwrap_or(after.len()) } else { 0 };
        let boundary = (!user || end > 0) && after[end..].chars().next().is_none_or(|c| c == '/' || c.is_whitespace());
        out.push_str(&rest[..i]);
        if boundary { out.push('~'); } else { out.push_str(&rest[i..i + prefix.len() + end]); }
        rest = &after[end..];
    }
    out + rest
}

fn mask(word: &str) -> String {
    let word = if word.contains("://") || word.starts_with(['/', '~']) { &word[..word.find(['?', '#']).unwrap_or(word.len())] } else { word };
    let word = word.split('&').map(|part| {
        let lower = part.to_ascii_lowercase();
        match ["key=", "token=", "secret=", "password="].iter().filter_map(|k| lower.find(k).map(|i| i + k.len())).min() {
            Some(end) if end < part.len() => format!("{}[redacted]", &part[..end]),
            _ => part.to_owned(),
        }
    }).collect::<Vec<_>>().join("&");
    let flush = |run: &mut String, out: &mut String| {
        let secret = (run.len() >= 20 && run.chars().any(|c| c.is_ascii_alphabetic()) && run.chars().any(|c| c.is_ascii_digit()))
            || ["sk-", "ghp_", "github_pat_", "AKIA"].iter().any(|p| run.len() > p.len() && run.starts_with(p))
            || (run.len() > 5 && run.starts_with("xox") && run.as_bytes()[4] == b'-');
        out.push_str(if secret { "[redacted]" } else { run });
        run.clear();
    };
    let (mut out, mut run) = (String::new(), String::new());
    for c in word.chars() {
        if c.is_ascii_alphanumeric() || "_-+/=".contains(c) { run.push(c); } else { flush(&mut run, &mut out); out.push(c); }
    }
    flush(&mut run, &mut out);
    out
}
