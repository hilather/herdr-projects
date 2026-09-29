//! Session graph and model segments (docs/telemetry/contracts-accounting.md §3):
//! derived with the ledger from the Codex tables, linking rollouts only on
//! native evidence so an inclusive parent is never added to what it covers,
//! and child sessions to a parent only on its A4 parent id, kept apart.
use super::ledger::Entry;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Codex guardian (auto-review) rollouts (contracts §5 binding rule).
const GUARDIAN_MODEL: &str = "codex-auto-review";

/// `[input, output, reasoning, total]` of a counted, normalized entry.
fn counters(entry: &Entry) -> Option<[i64; 4]> {
    entry.normalized.filter(|_| entry.counted()).map(|n| [n[0], n[4], n[5], n[6]])
}

/// A4 metadata of a rollout (ingest 0004 `rollout_metadata`, certified `fixture`).
struct Metadata {
    forked_from: Option<String>,
    subagent_kind: Option<String>,
    parent_thread: Option<String>,
}

/// Rebuild `session_nodes` and `model_segments` from `entries` (the ledger just
/// derived in the same transaction); returns `(sessions, model_segments)` counts.
pub fn store(tx: &Connection, entries: &[Entry]) -> Result<(usize, usize)> {
    tx.execute_batch("DELETE FROM session_nodes; DELETE FROM model_segments;")?;
    let guardian: BTreeSet<String> = tx.prepare("SELECT DISTINCT session_id FROM codex_usage WHERE model=?1")?
        .query_map([GUARDIAN_MODEL], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    // A turn is mixed when its records (or its completion) carry more than one model.
    let mixed: BTreeSet<(String, i64)> = tx.prepare("SELECT u.session_id,u.ordinal FROM codex_usage u WHERE u.turn_id IS NOT NULL AND
        (SELECT count(DISTINCT m) FROM (SELECT v.model m FROM codex_usage v WHERE v.session_id=u.session_id AND v.turn_id=u.turn_id
         UNION SELECT t.model FROM codex_turns t WHERE t.session_id=u.session_id AND t.turn_id=u.turn_id)) > 1")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let metadata: BTreeMap<String, Metadata> = tx.prepare("SELECT path_digest,forked_from_id,subagent_kind,subagent_parent_thread_id FROM rollout_metadata")?
        .query_map([], |r| Ok((r.get(0)?, Metadata { forked_from: r.get(1)?, subagent_kind: r.get(2)?, parent_thread: r.get(3)? })))?
        .collect::<rusqlite::Result<_>>()?;
    let delta: Vec<&Entry> = entries.iter().filter(|e| e.basis == "delta").collect();

    // Rollouts of one session, the one with the most records first.
    type Source = (String, String, Option<String>, Option<String>);
    let sources: Vec<Source> = tx.prepare("SELECT path_digest,session_id,source,attempt_id FROM rollout_sources ORDER BY session_id,records DESC,path_digest")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut sessions = BTreeMap::<&str, Vec<&Source>>::new();
    for source in &sources { sessions.entry(source.1.as_str()).or_default().push(source); }
    for (session, rollouts) in &sessions {
        // Per rollout: the entries it observed, and its inclusive total
        // (`None` when it observed an entry that is not counted and normalized).
        let mut observed = BTreeMap::<&str, (BTreeSet<&str>, Option<i64>)>::new();
        let mut conflict = false;
        for entry in delta.iter().filter(|e| e.session == *session) {
            for (path, disposition, _) in &entry.provenance {
                conflict |= *disposition == "conflict";
                let node = observed.entry(path.as_str()).or_insert((BTreeSet::new(), Some(0)));
                node.0.insert(entry.id.as_str());
                node.1 = node.1.zip(counters(entry)).map(|(sum, c)| sum + c[3]);
            }
        }
        let empty = (BTreeSet::new(), Some(0));
        let root = rollouts[0];
        // Native parent evidence of the session (its root rollout's first
        // `session_meta`): a spawned subagent's parent thread, else the fork origin.
        let meta = metadata.get(root.0.as_str());
        let other = |id: &Option<String>| id.clone().filter(|id| id != session);
        let forked = meta.and_then(|m| other(&m.forked_from));
        let link = meta.and_then(|m| other(&m.parent_thread)).map(|id| (id, "parent_thread_id"))
            .or_else(|| forked.clone().map(|id| (id, "forked_from_id")));
        let role = match meta.and_then(|m| m.subagent_kind.as_deref()) {
            Some("review") => "guardian",
            _ if guardian.contains(*session) => "guardian",
            Some(_) => "subagent",
            None if root.2.as_deref() == Some("subagent") => "subagent",
            None if forked.is_some() => "fork",
            None => "primary",
        };
        for rollout in rollouts {
            let (seen, total) = observed.get(rollout.0.as_str()).unwrap_or(&empty);
            // Resume evidence: the same native session id, and the root observed every
            // entry this rollout did with the same payload (else a conflict).
            let covered = !conflict && seen.is_subset(&observed.get(root.0.as_str()).unwrap_or(&empty).0);
            let (mut parent, mut linked, mut reason, mut claimed) = (None, None, None, None);
            let linkage = match (std::ptr::eq(*rollout, root), covered) {
                (_, false) => "unresolved",
                (false, true) => { parent = Some(root.0.as_str()); "included" }
                (true, true) => match &link {
                    Some((id, basis)) if sessions.contains_key(id.as_str()) => { linked = Some((id.as_str(), *basis)); "linked_child" }
                    Some((id, _)) => { (reason, claimed) = (Some("parent_not_collected"), Some(id.as_str())); "unlinked_child" }
                    None if role == "primary" => "root",
                    None => { reason = Some("no_native_parent_evidence"); "unlinked_child" }
                },
            };
            tx.execute("INSERT INTO session_nodes(path_digest,session_id,role,linkage,parent_path_digest,evidence,attempt_id,inclusive_total,
                parent_session_id,link_basis,parent_reason,claimed_parent_session_id,forked) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                params![rollout.0, session, role, linkage, parent, parent.map(|_| "same_session_prefix"), rollout.3, total,
                    linked.map(|l| l.0), linked.map(|l| l.1), reason, claimed, forked.is_some()])?;
        }
    }

    // Model segments over counted entries by position; buckets do not break a segment.
    let mut segments = 0;
    let mut buckets = BTreeMap::<(&str, &str, i64), (Option<&str>, i64, i64, i64, [i64; 4])>::new();
    let mut last: Option<(&str, &str, i64)> = None;
    for entry in &delta {
        let Some(c) = counters(entry) else { continue };
        let key = if mixed.contains(&(entry.session.clone(), entry.position)) { (entry.session.as_str(), "mixed", 0) }
            else if let Some(model) = entry.model.as_deref() {
                match last.filter(|l| l.0 == entry.session && buckets[l].0 == Some(model)) {
                    Some(l) => l,
                    None => { let next = last.filter(|l| l.0 == entry.session).map_or(1, |l| l.2 + 1); (entry.session.as_str(), "model", next) }
                }
            } else { (entry.session.as_str(), "unallocated", 0) };
        if key.1 == "model" { last = Some(key); }
        let bucket = buckets.entry(key).or_insert((entry.model.as_deref().filter(|_| key.1 == "model"), entry.position, entry.position, 0, [0; 4]));
        bucket.2 = entry.position;
        bucket.3 += 1;
        for (sum, v) in bucket.4.iter_mut().zip(c) { *sum += v; }
    }
    for ((session, bucket, segment), (model, first, last, count, c)) in &buckets {
        tx.execute("INSERT INTO model_segments(session_id,bucket,segment,model,first_position,last_position,entries,input_tokens,output_tokens,reasoning_tokens,total_tokens)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)", params![session, bucket, segment, model, first, last, count, c[0], c[1], c[2], c[3]])?;
        segments += 1;
    }
    Ok((sessions.len(), segments))
}

fn bucket(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    Ok(json!({"first_position": r.get::<_, i64>(0)?, "last_position": r.get::<_, i64>(1)?, "entries": r.get::<_, i64>(2)?,
        "input_tokens": r.get::<_, i64>(3)?, "output_tokens": r.get::<_, i64>(4)?, "reasoning_tokens": r.get::<_, i64>(5)?, "total_tokens": r.get::<_, i64>(6)?}))
}

/// The synced graph and segments per session, with the rollup of root totals.
/// A session with an `unresolved` rollout has no total (`inclusion_unknown`).
/// A linked child is listed under its parent (`children`) and never added to
/// the parent's total; a fork's inclusion is `fork_replay_not_certified`.
/// Read-only; `ledger_not_synced` before the first sync of this stream version.
pub fn read(db: &Connection) -> Result<Value> {
    let synced = db.query_row("SELECT count(*)=2 FROM sqlite_master WHERE type='table' AND name IN ('session_nodes','usage_ledger')", [], |r| r.get::<_, bool>(0))?
        && db.query_row("SELECT 1 FROM usage_ledger", [], |_| Ok(())).optional()?.is_some();
    if !synced { return Ok(super::unavailable("ledger_not_synced")); }
    const COLUMNS: &str = "first_position,last_position,entries,input_tokens,output_tokens,reasoning_tokens,total_tokens";
    let mut model = db.prepare(&format!("SELECT {COLUMNS},segment,model FROM model_segments WHERE session_id=?1 AND bucket='model' ORDER BY segment"))?;
    let mut other = db.prepare(&format!("SELECT {COLUMNS} FROM model_segments WHERE session_id=?1 AND bucket=?2"))?;
    let mut nodes = db.prepare("SELECT path_digest,linkage,parent_path_digest,evidence,inclusive_total,attempt_id FROM session_nodes WHERE session_id=?1 ORDER BY linkage='included',path_digest")?;
    let zero = json!({"first_position": null, "last_position": null, "entries": 0, "input_tokens": 0, "output_tokens": 0, "reasoning_tokens": 0, "total_tokens": 0});
    let (mut rooted, mut linked, mut children, mut incomplete, mut linked_fork) = (0, 0, 0, 0, false);
    let (mut out, mut under) = (Vec::new(), BTreeMap::<String, Vec<Value>>::new());
    let sessions: Vec<(String, String)> = db.prepare("SELECT DISTINCT session_id,role FROM session_nodes ORDER BY session_id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    for (session, role) in sessions {
        type Head = (String, Option<i64>, Option<String>, Option<String>, Option<String>, Option<String>, bool);
        let (linkage, total, parent_session, basis, reason, claimed, forked): Head = db.query_row("SELECT linkage,inclusive_total,parent_session_id,link_basis,
            parent_reason,claimed_parent_session_id,forked FROM session_nodes WHERE session_id=?1 AND linkage<>'included'
            ORDER BY linkage<>'unresolved',path_digest LIMIT 1", [&session], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?;
        let total = match (linkage.as_str(), total) {
            ("unresolved", _) => { incomplete += 1; super::unavailable("inclusion_unknown") }
            (_, None) => { incomplete += 1; super::unavailable("incomplete") }
            ("root", Some(t)) => { rooted += t; json!(t) }
            ("linked_child", Some(t)) => { linked += t; linked_fork |= forked; json!(t) }
            (_, Some(t)) => { children += t; json!(t) }
        };
        let parent = match (linkage.as_str(), &parent_session) {
            ("root", _) => Value::Null,
            ("linked_child", Some(id)) => json!({"session_id": id, "link_basis": basis, "certified": "fixture"}),
            ("unlinked_child", _) => {
                let mut value = super::unavailable(reason.as_deref().unwrap_or("no_native_parent_evidence"));
                if let Some(id) = claimed { value["session_id"] = json!(id); }
                value
            }
            _ => super::unavailable("inclusion_unknown"),
        };
        if let Some(id) = &parent_session {
            let inclusion = if forked { super::unavailable("fork_replay_not_certified") } else { json!("separate") };
            under.entry(id.clone()).or_default().push(json!({"session_id": session, "role": role, "link_basis": basis, "certified": "fixture",
                "total_tokens": total, "inclusion": inclusion}));
        }
        let segments = model.query_map([&session], |r| { let mut b = bucket(r)?; b["segment"] = json!(r.get::<_, i64>(7)?); b["model"] = json!(r.get::<_, String>(8)?); Ok(b) })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut kind = |name: &str| other.query_row(params![session, name], bucket).optional().map(|b| b.unwrap_or_else(|| zero.clone()));
        let (mixed, unallocated) = (kind("mixed")?, kind("unallocated")?);
        let rollouts = nodes.query_map([&session], |r| Ok(json!({"path_digest": r.get::<_, String>(0)?, "linkage": r.get::<_, String>(1)?,
            "parent": r.get::<_, Option<String>>(2)?, "evidence": r.get::<_, Option<String>>(3)?, "inclusive_total": r.get::<_, Option<i64>>(4)?,
            "attempt_id": r.get::<_, Option<String>>(5)?})))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        out.push(json!({"session_id": session, "role": role, "linkage": linkage, "parent": parent, "total_tokens": total,
            "rollouts": rollouts, "segments": segments, "mixed": mixed, "unallocated": unallocated}));
    }
    // Linked children under their parent, apart from its total: their sum is
    // shown only when every child total is known and none may replay the parent.
    for session in &mut out {
        let Some(list) = session["session_id"].as_str().and_then(|id| under.remove(id)) else { continue };
        let total = if list.iter().any(|c| !c["total_tokens"].is_i64()) { super::unavailable("incomplete") }
            else if list.iter().any(|c| c["inclusion"] != "separate") { super::unavailable("fork_replay_not_certified") }
            else { json!(list.iter().filter_map(|c| c["total_tokens"].as_i64()).sum::<i64>()) };
        session["children"] = json!({"sessions": list, "total_tokens": total});
    }
    // A partial sum is never shown as the total.
    let sum = |value: i64| if incomplete > 0 { super::unavailable("incomplete_sessions") } else { json!(value) };
    let linked = if incomplete == 0 && linked_fork { super::unavailable("fork_replay_not_certified") } else { sum(linked) };
    Ok(json!({"sessions": out, "rollup": {"sessions": sum(rooted), "linked_children": linked, "unlinked_children": sum(children), "incomplete_sessions": incomplete}}))
}
