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
