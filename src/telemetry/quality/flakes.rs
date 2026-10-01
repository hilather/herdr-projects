//! DG6: passive, derived verification evidence. Never changes a verdict.
use anyhow::Result;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

/// Scan at most `limit` unseen canonical runs, using a canonical read-only
/// connection and a separate sidecar transaction. Replay is idempotent by run ID.
pub fn collect(project: &Path, create: bool, limit: usize) -> Result<Value> {
    let Some(mut sidecar) = super::super::sidecar::open(project, create)? else {
        return Ok(json!({"observed": 0}));
    };
    let canonical = super::super::read_only(&project.join(".state/state.db"))?;
    let metadata = has_column(&canonical, "verification_runs", "metadata")?;
    let cursor: i64 = sidecar.query_row(
        "SELECT source_rowid FROM quality_verification_cursor WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    let mut stmt = canonical.prepare(&format!("SELECT run_id,tree_oid,object_format,policy_id,policy_digest,state,created_unix_ms,{},rowid,reason FROM verification_runs WHERE rowid>?1 ORDER BY rowid LIMIT ?2", if metadata { "metadata" } else { "NULL" }))?;
    let mut rows = stmt.query(params![cursor, i64::try_from(limit).unwrap_or(i64::MAX)])?;
    let tx = sidecar.transaction()?;
    let mut observed = 0;
    let mut scanned_rowid = cursor;
    while let Some(row) = rows.next()? {
        scanned_rowid = row.get(8)?;
        let tree: Option<String> = row.get(1)?;
        let state: String = row.get(5)?;
        let reason: Option<String> = row.get(9)?;
        if tree.is_none()
            || !(state == "accepted"
                || (state == "rejected" && reason.as_deref() == Some("checks_failed")))
        {
            continue;
        }
        let id: String = row.get(0)?;
        let raw: Option<String> = row.get(7)?;
        let evidence: Value = raw
            .filter(|s| s.len() <= 2 * 1024 * 1024)
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null);
        let load = evidence["load"]["host_load_1m"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|n| n.is_finite() && *n >= 0.0);
        let tests = validated_tests(&evidence["tests"]);
        // Make the first sidecar operation a write: a deferred read followed
        // by an INSERT can fail with BUSY_SNAPSHOT when another lane commits.
        // The insert count also preserves the idempotent observed-run total.
        observed += tx.execute(
            "INSERT OR IGNORE INTO quality_verification_runs VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                id,
                row.get::<_, i64>(8)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                load,
                if tests.is_some() {
                    "available"
                } else {
                    "unavailable"
                }
            ],
        )?;
        if let Some(tests) = tests {
            for (name, outcome) in tests {
                tx.execute(
                    "INSERT OR IGNORE INTO quality_test_results VALUES(?1,?2,?3)",
                    params![id, name, outcome],
                )?;
            }
        }
        if evidence["version"] == "verification-metadata.v2"
            && let Some(rows) = evidence["observations"].as_array().filter(|rows| rows.len() <= 700) {
            for observation in rows {
                if observation["kind"] != "flake" { continue; }
                let Some(sequence) = observation["sequence"].as_u64().filter(|n| *n < 700) else { continue; };
                let verdict = match observation["outcome"].as_str() {
                    Some("pass") => "accepted", Some("fail") => "rejected", _ => continue,
                };
                let observation_id = format!("{id}:flake:{sequence}");
                let load = observation["load"]["host_load_1m"].as_str()
                    .and_then(|s| s.parse::<f64>().ok()).filter(|n| n.is_finite() && *n >= 0.0);
                tx.execute("INSERT OR IGNORE INTO quality_verification_observations VALUES(?1,?2,?3,?4,?5)",
                    params![observation_id,id,sequence,verdict,load])?;
                if let Some(tests) = validated_tests(&observation["tests"]) {
                    for (name, outcome) in tests {
                        tx.execute("INSERT OR IGNORE INTO quality_observation_tests VALUES(?1,?2,?3)", params![observation_id,name,outcome])?;
                    }
                }
            }
        }
    }
    tx.execute(
        "UPDATE quality_verification_cursor SET source_rowid=max(source_rowid,?1),collected_unix_ms=?2 WHERE singleton=1",
        params![scanned_rowid, jiff::Timestamp::now().as_millisecond()],
    )?;
    tx.commit()?;
    Ok(json!({"observed": observed}))
}
fn validated_tests(value: &Value) -> Option<BTreeMap<String, String>> {
    if value["status"] != "available" {
        return None;
    }
    let tests = value["results"].as_array().filter(|t| t.len() <= 5000)?;
    let mut out = BTreeMap::new();
    for test in tests {
        let name = test["name"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))?;
        let name = super::super::sanitize::excerpt(name);
        let outcome = test["outcome"]
            .as_str()
            .filter(|s| matches!(*s, "pass" | "fail" | "ignored"))?;
        if out.insert(name, outcome.into()).is_some() {
            return None;
        }
    }
    Some(out)
}
fn has_column(db: &Connection, table: &str, column: &str) -> Result<bool> {
    Ok(db
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|c| c == column))
}
fn available(db: &Connection) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='quality_verification_runs' AND type='table')", [], |r| r.get(0))?)
}
fn ratio(n: i64, d: i64) -> Value {
    if d == 0 {
        json!({"status": "unavailable", "reason": "empty_denominator"})
    } else {
        json!(format!("{n}/{d}"))
    }
}

/// `[since, to)` includes only completed checks recorded in the window. Each
/// (object format, tree, policy id, digest) pair counts once, regardless of reruns.
pub fn report(project: &Path, since: Option<i64>, to: Option<i64>) -> Result<Value> {
    let Some(db) = super::super::sidecar::read(project)? else {
        return Ok(json!({"status": "unavailable", "reason": "verification_not_collected"}));
    };
    if !available(&db)? {
        return Ok(json!({"status": "unavailable", "reason": "verification_not_collected"}));
    }
    let collected: Option<i64> = db.query_row(
        "SELECT collected_unix_ms FROM quality_verification_cursor WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    if collected.is_none() {
        return Ok(json!({"status": "unavailable", "reason": "verification_not_collected"}));
    }
    // Old read-only sidecars remain reportable until the next explicit collection.
    let observations: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='quality_completed_verifications')", [], |r| r.get(0))?;
    let sql = |query: &str| if observations { query.to_owned() } else {
        query.replace("quality_completed_verifications", "quality_verification_runs")
            .replace("quality_completed_tests", "quality_test_results")
    };
    let mut stmt = db.prepare(&sql("SELECT tree_oid,object_format,policy_id,policy_digest,count(*),count(DISTINCT verdict) FROM quality_completed_verifications
        WHERE (?1 IS NULL OR created_unix_ms>=?1) AND (?2 IS NULL OR created_unix_ms<?2)
        GROUP BY tree_oid,object_format,policy_id,policy_digest HAVING count(*)>=2 ORDER BY policy_id,policy_digest,object_format,tree_oid"))?;
    let mut rows = stmt.query(params![since, to])?;
    let mut policies = BTreeMap::<(String, String), (i64, i64)>::new();
    let mut flips = Vec::new();
    let (mut denominator, mut numerator) = (0, 0);
    while let Some(row) = rows.next()? {
        let tree: String = row.get(0)?;
        let format: String = row.get(1)?;
        let policy: String = row.get(2)?;
        let digest: String = row.get(3)?;
        let flipped = row.get::<_, i64>(5)? > 1;
        let cell = policies
            .entry((policy.clone(), digest.clone()))
            .or_default();
        cell.1 += 1;
        denominator += 1;
        if !flipped {
            continue;
        }
        cell.0 += 1;
        numerator += 1;
        let runs = db.prepare(&sql("SELECT run_id,verdict FROM quality_completed_verifications WHERE tree_oid=?1 AND object_format=?2 AND policy_id=?3 AND policy_digest=?4
            AND (?5 IS NULL OR created_unix_ms>=?5) AND (?6 IS NULL OR created_unix_ms<?6) GROUP BY verdict HAVING run_id=min(run_id) ORDER BY verdict LIMIT 2"))?
            .query_map(params![tree,format,policy,digest,since,to], |r| Ok(json!({"run_id": r.get::<_, String>(0)?, "verdict": r.get::<_, String>(1)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        if flips.len() < 100 {
            flips.push(json!({"tree": tree, "object_format": format, "policy_id": policy, "policy_digest": digest, "runs": runs, "completed_runs": row.get::<_, i64>(4)?}));
        }
    }
    let by_policy: Vec<_> = policies.into_iter().map(|((id,digest),(n,d))| json!({"policy_id": id,"policy_digest": digest,"numerator": n,"denominator": d,"value": ratio(n,d)})).collect();
    let load = db.prepare(&sql("SELECT CASE WHEN load_1m IS NULL THEN 'unknown' WHEN load_1m<2 THEN '<2' WHEN load_1m<=8 THEN '2-8' ELSE '>8' END AS bucket,
        sum(verdict='rejected'),count(*) FROM quality_completed_verifications WHERE (?1 IS NULL OR created_unix_ms>=?1) AND (?2 IS NULL OR created_unix_ms<?2) GROUP BY bucket ORDER BY bucket"))?
        .query_map(params![since,to], |r| { let n:i64=r.get(1)?;let d:i64=r.get(2)?;Ok(json!({"bucket":r.get::<_, String>(0)?,"failures":n,"runs":d,"value":ratio(n,d)})) })?.collect::<rusqlite::Result<Vec<_>>>()?;
    let tests = db.prepare(&sql("SELECT v.tree_oid,v.object_format,v.policy_id,v.policy_digest,t.name,count(DISTINCT t.outcome)
        FROM quality_completed_verifications v JOIN quality_completed_tests t USING(run_id) WHERE t.outcome IN ('pass','fail')
        AND (?1 IS NULL OR v.created_unix_ms>=?1) AND (?2 IS NULL OR v.created_unix_ms<?2)
        GROUP BY v.tree_oid,v.object_format,v.policy_id,v.policy_digest,t.name HAVING count(DISTINCT t.outcome)>1 ORDER BY v.tree_oid,v.policy_id,v.policy_digest,t.name LIMIT 100"))?
        .query_map(params![since,to], |r| Ok(json!({"tree":r.get::<_, String>(0)?,"object_format":r.get::<_, String>(1)?,"policy_id":r.get::<_, String>(2)?,"policy_digest":r.get::<_, String>(3)?,"name":r.get::<_, String>(4)?,"outcomes":["pass","fail"]})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(
        json!({"status":"available","numerator":numerator,"denominator":denominator,"value":ratio(numerator,denominator),"by_policy":by_policy,
        "flips":flips,"evidence_limit":100,"failure_rate_by_load":load,"tests":tests,"tests_evidence_limit":100,"source_trust":"verifier_observed"}),
    )
}

pub fn metric(project: &Path, since: Option<i64>) -> Result<Value> {
    let report = report(project, since, None)?;
    let value = if report["status"] == "unavailable" {
        json!({"status":"unavailable","reason":report["reason"]})
    } else {
        report["value"].clone()
    };
    Ok(
        json!({"definition":"verification_flip_rate.v1","name":"verification_flip_rate","proxy":true,"source_trust":"verifier_observed",
        "value":value,"numerator":report["numerator"],"denominator":report["denominator"],"by_policy":report["by_policy"],"failure_rate_by_load":report["failure_rate_by_load"],"detail":report}),
    )
}
