//! DG4d synthetic native storage through public CLI/store workflows.
#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)]
mod support;
use support::telemetry::*;
use serde_json::{Value,json};
use rusqlite::{Connection,params};
use std::fs;

fn fixture() -> Fixture {
    let mut f = Fixture::new();
    f.home = f.tmp.path().join("opencode-execution-home");
    // Synthetic capability/attempt evidence only; admission never launches an agent.
    let mut profile = codex_profile(&f.config,"opencode","opencode",Some(&f.home));
    profile.agent.version = "1.18.34".into();
    let path = f.project.join(".state/state.db");
    plant_profile(&path,profile);
    f.readmit("opencode");
    let db = Connection::open(path).unwrap();
    (f.attempt,f.decided) = db.query_row("SELECT a.id,d.decided_unix_ms FROM attempts a JOIN dispatch_decisions d ON d.attempt_id=a.id WHERE a.state='reserved'",
        [],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    db.execute("INSERT INTO collector_bindings(attempt_id,revision,state,collector,execution_home,unix_ms,source)
        VALUES(?1,1,'active','opencode',?2,?3,'apply_launch_started')",params![f.attempt,f.home.display().to_string(),unix_ms()]).unwrap();
    f
}
fn native(f: &Fixture) -> Connection {
    let path = f.home.join(".local/share/opencode/opencode.db");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let db = Connection::open(path).unwrap();
    db.execute_batch("CREATE TABLE session(id TEXT PRIMARY KEY,directory TEXT,version TEXT,time_created INTEGER);
        CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,time_created INTEGER,time_updated INTEGER,data TEXT);
        CREATE TABLE part(id TEXT PRIMARY KEY,message_id TEXT,session_id TEXT,time_created INTEGER,time_updated INTEGER,data TEXT);
        CREATE TABLE session_message(id TEXT PRIMARY KEY,session_id TEXT,type TEXT,seq INTEGER,time_created INTEGER,time_updated INTEGER,data TEXT);").unwrap();
    db
}
fn session(db: &Connection, id: &str, cwd: &str, version: &str, time: i64) {
    db.execute("INSERT INTO session VALUES(?1,?2,?3,?4)",params![id,cwd,version,time]).unwrap();
}
fn message(db: &Connection, sid: &str, id: &str, v2: bool, completed: bool, at: i64) {
    let mut raw: Value = serde_json::from_str(include_str!("fixtures/telemetry/opencode/assistant.json")).unwrap();
    raw["time"]["created"] = json!(at);
    raw["content"][2]["id"] = json!(format!("{id}-tool"));
    raw["content"][2]["time"] = json!({"created":at,"completed":at+1});
    if completed { raw["time"]["completed"] = json!(at+10); }
    if v2 { db.execute("INSERT INTO session_message VALUES(?1,?2,'assistant',1,?3,?3,?4)",params![id,sid,at,raw.to_string()]).unwrap(); }
    else {
        db.execute("INSERT INTO message VALUES(?1,?2,?3,?3,?4)",params![id,sid,at,raw.to_string()]).unwrap();
        for (part,status) in [("ok","completed"),("fail","error")] {
            let data = json!({"type":"tool","tool":"fixture-tool","state":{"status":status,"input":"OPENCODE_SECRET_INPUT",
                "output":"OPENCODE_SECRET_OUTPUT","error":"OPENCODE_SECRET_ERROR","time":{"start":at,"end":at+1}}});
            db.execute("INSERT INTO part VALUES(?1,?2,?3,?4,?4,?5)",params![format!("{id}-{part}"),id,sid,at,data.to_string()]).unwrap();
        }
    }
}
fn usage(f: &Fixture) -> Value {
    f.cli_args(&["usage","--json"]).0["attempts"].as_array().unwrap().iter().find(|a|a["attempt_id"]==f.attempt).unwrap()["usage"].clone()
}
fn privacy(f: &Fixture) {
    for name in ["telemetry.db","telemetry.db-wal","telemetry.db-shm"] {
        if let Ok(bytes) = fs::read(f.project.join(".state").join(name)) {
            assert!(!bytes.windows(b"OPENCODE_SECRET_".len()).any(|w|w==b"OPENCODE_SECRET_"));
        }
    }
}
#[test]
fn native_opencode_usage_cost_tools_privacy_and_replay() {
    let f = fixture(); let db = native(&f);
    session(&db,SID,&f.worktree(),"1.18.34",f.decided+1);
    message(&db,SID,"msg-one",false,true,f.decided+2);
    message(&db,SID,"msg-two",true,true,f.decided+3);
    message(&db,SID,"msg-pending",false,false,f.decided+4);
    assert_eq!(f.cli("collect").0["collected"]["records"],2);
    assert_eq!(usage(&f),json!({"input_tokens":280,"cached_input_tokens":60,"cache_write_input_tokens":20,"output_tokens":50,
        "reasoning_output_tokens":10,"total_tokens":330,"records":2}));
    let sidecar = f.sidecar();
    assert_eq!(sidecar.query_row("SELECT sum(cost) FROM opencode_messages",[],|r|r.get::<_,f64>(0)).unwrap(),0.25);
    assert_eq!(f.count("opencode_tools"),3);
    f.cli_args(&["accounting","sync"]);
    let entries = f.cli_args(&["accounting","entries"]).0;
    assert_eq!(entries["entries"].as_array().unwrap().len(),2);
    for entry in entries["entries"].as_array().unwrap() {
        assert_eq!(f.sidecar().query_row("SELECT count(*) FROM usage_entries WHERE source='opencode'",[],|r|r.get::<_,i64>(0)).unwrap(),2);
        assert_eq!(entry["normalization_version"],"opencode-v1");
        assert_eq!(entry["normalized"],json!({"input_tokens":140,"cache_read_tokens":30,"new_input_tokens":100,"cache_write_tokens":10,
            "output_tokens":25,"reasoning_tokens":5,"total_tokens":165}));
    }
    let tools = f.cli_args(&["accounting","tools","--json"]).0;
    assert_eq!(tools["metrics"]["M16"]["value"]["issued"],3);
    assert_eq!(tools["metrics"]["M16"]["executed"]["by_scope"]["opencode"],3);
    assert_eq!(tools["metrics"]["M17"]["value"],"1/3");
    assert_eq!(f.cli("collect").0["collected"]["records"],0);
    assert_eq!(usage(&f)["total_tokens"],330);
    db.execute("UPDATE message SET data=json_set(data,'$.time.completed',?1) WHERE id='msg-pending'",[f.decided+20]).unwrap();
    f.cli("collect"); f.cli_args(&["accounting","sync"]);
    assert_eq!(usage(&f)["total_tokens"],495);
    assert_eq!(f.count("usage_entries"),3);
    let caps = f.cli_args(&["collectors","capabilities","--json"]).0;
    let cap = caps["adapters"].as_array().unwrap().iter().find(|a|a["adapter"]=="opencode").unwrap();
    assert_eq!(cap["fixture_versions"],json!(["1.18.34"]));
    assert!(cap["fields"].as_array().unwrap().iter().all(|v|v["certified"]=="fixture"));
    assert_eq!(sidecar.query_row("SELECT count(*) FROM source_observations WHERE json_extract(provenance,'$.adapter')='opencode'",[],|r|r.get::<_,i64>(0)).unwrap(),3);
    // A changed observation of the same native message is a conflict, never another charge.
    db.execute("UPDATE message SET data=json_set(data,'$.tokens.input',200),time_updated=time_updated+100 WHERE id='msg-one'",[]).unwrap();
    f.cli("collect"); f.cli_args(&["accounting","sync"]);
    assert_eq!(f.count("codex_quarantine"),1);
    assert_eq!(f.count("usage_entries"),3);
    privacy(&f);
}
#[test]
fn native_opencode_exact_binding_and_versions() {
    let f = fixture(); let db = native(&f);
    for (sid,cwd,version,at) in [("unbound","/tmp/synthetic-other","1.18.34",f.decided+1),
        ("early",f.worktree().as_str(),"1.18.34",f.decided-1),("future",f.worktree().as_str(),"9.9.9",f.decided+1)] {
        session(&db,sid,cwd,version,at);message(&db,sid,&format!("msg-{sid}"),false,true,at+1);
    }
    f.cli("collect");
    assert_eq!(usage(&f)["reason"],"cli_version_uncertified");
    assert_eq!(f.sidecar().query_row("SELECT count(*) FROM rollout_sources WHERE binding='unbound'",[],|r|r.get::<_,i64>(0)).unwrap(),2);
    assert_eq!(f.sidecar().query_row("SELECT count(*) FROM codex_usage WHERE session_id='opencode:future' AND accepted=0 AND total_tokens IS NULL",[],|r|r.get::<_,i64>(0)).unwrap(),1);
    db.execute("UPDATE session SET version='1.18.34' WHERE id='future'",[]).unwrap();
    assert_eq!(f.cli("collect").0["collected"]["reevaluated"],1);
    assert_eq!(usage(&f)["total_tokens"],165);
    privacy(&f);
}
#[test]
fn native_opencode_backup_retention_and_tombstones() {
    let f = fixture();let db = native(&f);
    session(&db,SID,&f.worktree(),"1.18.34",f.decided+1);message(&db,SID,"msg-one",false,true,f.decided+2);
    f.cli("collect");f.cli_args(&["accounting","sync"]);
    let backup = f.tmp.path().join("native-backup");
    f.cli_args(&["backup","create","--out",backup.to_str().unwrap()]);
    for name in ["telemetry.db","telemetry.db-wal","telemetry.db-shm"] { let _=fs::remove_file(f.project.join(".state").join(name)); }
    let restored = f.cli_args(&["backup","restore","--from",backup.to_str().unwrap()]).0;
    assert_eq!(restored["rows"]["opencode_messages"],1);assert_eq!(restored["rows"]["opencode_tools"],2);
    f.cancel_reserved();f.sidecar().execute("UPDATE rollout_sources SET observed_unix_ms=observed_unix_ms-?1",[91_i64*86400000]).unwrap();
    let plan = f.cli_args(&["maintenance","plan","--json"]).0;
    f.cli_args(&["maintenance","apply","--confirm",plan["plan_digest"].as_str().unwrap(),"--json"]);
    assert_eq!(f.count("opencode_messages"),0);assert_eq!(f.count("opencode_tools"),0);
    for table in ["accounting_usage_totals", "accounting_native_totals", "accounting_source_summary", "accounting_tool_summary"] { assert_eq!(f.count(table), 0, "retention purges {table}"); }
    f.cli("collect");assert_eq!(f.count("opencode_messages"),0);
    f.cli_args(&["backup","restore","--from",backup.to_str().unwrap(),"--force"]);
    assert_eq!(f.count("opencode_messages"),0);privacy(&f);
}

#[test]
fn native_opencode_and_codex_keep_independent_usage() {
    let f = fixture(); let db = native(&f);
    session(&db,SID,&f.worktree(),"1.18.34",f.decided+1);
    message(&db,SID,"msg-one",false,true,f.decided+2);
    let canonical = Connection::open(f.project.join(".state/state.db")).unwrap();
    let (old,decided): (String,i64) = canonical.query_row("SELECT attempt_id,decided_unix_ms FROM dispatch_decisions WHERE attempt_id<>?1",[&f.attempt],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    let cwd = format!("{}/.state/worktrees/{old}/repo-00",f.project.display());
    f.rollout(&f.tmp.path().join("codex-home"),SID,&["head.jsonl","tail.jsonl"],&cwd,decided+1000,"0.154.0");
    f.cli("collect");
    let full = aggregate_read_snapshot(&f);
    f.cli_args(&["accounting","sync"]);
    assert_eq!(aggregate_read_snapshot(&f), full, "mixed Codex/OpenCode maintained reads");
    let all = f.cli_args(&["usage","--json"]).0;
    let old_usage = &all["attempts"].as_array().unwrap().iter().find(|a|a["attempt_id"]==old).unwrap()["usage"];
    assert_eq!(old_usage["input_tokens"],1500);
    assert_eq!(usage(&f)["input_tokens"],140);
    assert_eq!(f.count("usage_entries"),4);
    privacy(&f);
}

#[test]
fn opencode_maintained_reads_match_full_derivation() {
    let f = fixture();
    let terminated = plant_aggregate_termination(&f);
    let db = native(&f);
    session(&db, SID, &f.worktree(), "1.18.34", terminated + 1);
    message(&db, SID, "msg-one", false, true, terminated + 2);
    f.cli("collect");
    let replay = aggregate_read_snapshot(&f);
    assert_eq!(replay["metrics"][0]["value"], 140);
    assert_eq!(replay["metrics"][1]["value"], 25);
    assert_eq!(replay["tools"]["metrics"]["M16"]["executed"]["by_scope"]["opencode"], 2);
    assert_eq!(replay["tools"]["metrics"]["M17"]["value"], "1/2");
    assert_eq!(replay["after_termination"].as_array().unwrap().len(), 1);
    let card = f.tmp.path().join("opencode-rates.json");
    fs::write(&card, json!({"card_id":"synthetic-opencode", "version":1,
        "provider":"fixture-provider", "product":"opencode", "models":["fixture-model"],
        "currency":"USD", "rate_unit":1_000_000, "effective_from_unix_ms":0,
        "includes":{"discounts":false,"taxes":false,"fees":false},
        "source":"INVENTED synthetic rates; not provider prices",
        "rates":[{"category":"input","rate":"2"},{"category":"cache_read","rate":"0.5"},
            {"category":"cache_write","rate":"3"},{"category":"output","rate":"6"}]}).to_string()).unwrap();
    f.cli_args(&["accounting", "import-rate-card", card.to_str().unwrap()]);
    verify_aggregate_replay(&f, &replay);
    for table in ["accounting_usage_totals", "accounting_native_totals", "accounting_source_summary", "accounting_tool_summary"] {
        assert_eq!(f.count(table), 1, "OpenCode populates {table}");
    }
    assert_eq!(f.report()["metrics"]["M14"]["value"], "1/1");
    let native_cost: f64 = f.sidecar().query_row("SELECT sum(cost) FROM opencode_messages", [], |r| r.get(0)).unwrap();
    assert_eq!(native_cost, 0.125);
    // Correct a tool outcome with no new usage; cached reads must invalidate.
    f.sidecar().execute("UPDATE opencode_tools SET status='completed',is_error=0 WHERE part_id='msg-one-fail'", []).unwrap();
    let corrected = aggregate_read_snapshot(&f);
    assert_eq!(corrected["tools"]["metrics"]["M17"]["value"], "2/2");
    f.cli_args(&["analytics", "refresh"]);
    assert_eq!(f.cli_args(&["analytics", "rebuild", "--verify"]).0["identical"], true);
    verify_aggregate_replay(&f, &corrected);
    // A second invocation without cache writes; the card has a cache-write
    // rate (DG5), so both entries are priced.
    message(&db, SID, "msg-priced", false, true, terminated + 3);
    db.execute("UPDATE message SET data=json_set(data,'$.tokens.cache.write',0) WHERE id='msg-priced'", []).unwrap();
    f.cli("collect");
    let appended = aggregate_read_snapshot(&f);
    assert_eq!(appended["metrics"][0]["value"], 270);
    assert_eq!(appended["metrics"][1]["value"], 50);
    verify_aggregate_replay(&f, &appended);
    let metrics = f.report()["metrics"].clone();
    assert_eq!(metrics["M12"]["estimate"], json!({"status": "complete", "currency": "USD", "amount": "0.00076"}));
    assert_eq!(metrics["M14"]["value"], "2/2");
    privacy(&f);
}
