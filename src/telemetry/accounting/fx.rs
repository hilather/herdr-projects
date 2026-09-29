//! Dated currency conversion (docs/telemetry/contracts-accounting.md §13):
//! exchange-rate tables imported from synthetic fixture files, applied at read
//! time as a separate dated valuation of stored estimates. Currencies are
//! never added without a recorded rate, and the stored estimates never change.
use super::charges::{self, FIXTURE_ONLY};
use super::cost::{self, Dec};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FxFile {
    synthetic: bool,
    table_id: String,
    version: i64,
    source: String,
    rates: Vec<FxRate>,
}

#[derive(Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct FxRate {
    from: String,
    to: String,
    effective_from_unix_ms: i64,
    effective_to_unix_ms: Option<i64>,
    rate: String,
}

/// `accounting import-fx <file>`: append an exchange-rate table version. The
/// same content again is a no-op; different content under an existing
/// version is refused.
pub fn import(db: &mut Connection, file: &Path) -> Result<Value> {
    let mut table: FxFile = charges::read_file(file)?;
    let context = || format!("exchange-rate file {}", file.display());
    charges::synthetic(table.synthetic, &table.source).with_context(context)?;
    ensure!(!table.table_id.is_empty() && table.table_id.len() <= 128 && table.table_id.bytes().all(|b| b.is_ascii_graphic()) && table.version > 0,
        "table_id must be printable ASCII and version positive");
    ensure!(!table.rates.is_empty(), "rates must not be empty");
    for rate in &mut table.rates {
        charges::currency(&rate.from).and_then(|_| charges::currency(&rate.to)).with_context(context)?;
        ensure!(rate.from != rate.to, "a rate converts between two different currencies");
        ensure!(rate.effective_to_unix_ms.is_none_or(|to| to > rate.effective_from_unix_ms), "effective_to_unix_ms must be after effective_from_unix_ms");
        rate.rate = Dec::rate(&rate.rate).with_context(|| format!("rate {} -> {}", rate.from, rate.to))?.to_string();
        ensure!(rate.rate != "0", "a rate must be positive");
    }
    table.rates.sort();
    for pair in table.rates.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        ensure!((&a.from, &a.to) != (&b.from, &b.to) || a.effective_to_unix_ms.is_some_and(|to| to <= b.effective_from_unix_ms),
            "rates for {} -> {} overlap within one table version", a.from, a.to);
    }
    let digest = cost::digest(serde_json::to_string(&table)?.as_bytes());
    let tx = db.transaction()?;
    let existing: Option<String> = tx.query_row("SELECT digest FROM fx_tables WHERE table_id=?1 AND version=?2", params![table.table_id, table.version], |r| r.get(0)).optional()?;
    let imported = match existing {
        Some(existing) if existing == digest => false,
        Some(_) => bail!("exchange-rate table {} version {} exists with different content; tables are append-only, import a new version", table.table_id, table.version),
        None => {
            tx.execute("INSERT INTO fx_tables(table_id,version,digest,source,synthetic,imported_unix_ms) VALUES(?1,?2,?3,?4,1,?5)",
                params![table.table_id, table.version, digest, table.source, jiff::Timestamp::now().as_millisecond()])?;
            for r in &table.rates {
                tx.execute("INSERT INTO fx_rates(table_id,version,from_currency,to_currency,rate,effective_from_unix_ms,effective_to_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![table.table_id, table.version, r.from, r.to, r.rate, r.effective_from_unix_ms, r.effective_to_unix_ms])?;
            }
            true
        }
    };
    tx.commit()?;
    Ok(json!({"table_id": table.table_id, "version": table.version, "digest": digest, "imported": imported}))
}

/// One stored rate.
struct Rate {
    table: String,
    version: i64,
    from: i64,
    to: Option<i64>,
    rate: Dec,
    text: String,
}

/// The rate from `currency` to `target` effective over the whole usage
/// interval: the highest version of one table among the rates overlapping it.
fn find<'a>(rates: &'a [(String, String, Rate)], currency: &str, target: &str, (from, to): (i64, i64)) -> std::result::Result<&'a Rate, &'static str> {
    let overlapping: Vec<&Rate> = rates.iter().filter(|(f, t, r)| f == currency && t == target && r.from <= to && r.to.is_none_or(|e| e > from)).map(|r| &r.2).collect();
    let Some(best) = overlapping.iter().max_by_key(|r| r.version) else { return Err("no_fx_rate") };
    if overlapping.iter().any(|r| r.table != best.table) { return Err("ambiguous_fx_tables"); }
    let current: Vec<&&Rate> = overlapping.iter().filter(|r| r.version == best.version).collect();
    match current.as_slice() {
        [one] if one.from <= from && one.to.is_none_or(|e| e > to) => Ok(one),
        _ => Err("fx_rate_change_within_usage_interval"),
    }
}

/// `accounting fx --to <currency>`: the stored estimates of the valuation
/// revision (the latest, `--revision N`, or the latest computed by `as_of`)
/// converted to `target` with the rate effective over each entry's usage
/// interval, from tables imported by `as_of`. Each conversion records its
/// rate id; a total is complete only when every entry is priced and
/// converted, else partial or unavailable. Derived at read time; read-only.
pub fn convert(db: &Connection, target: &str, revision: Option<i64>, as_of: Option<i64>) -> Result<Value> {
    charges::currency(target)?;
    let Some((revision, basis, ..)) = cost::header(db, revision, as_of)? else { return Ok(super::unavailable("not_priced")) };
    let rows = cost::stored_at(db, revision)?;
    let rates: Vec<(String, String, Rate)> = if cost::table(db, "fx_rates")? {
        let mut stmt = db.prepare("SELECT r.from_currency,r.to_currency,r.table_id,r.version,r.effective_from_unix_ms,r.effective_to_unix_ms,r.rate FROM fx_rates r
            JOIN fx_tables t ON t.table_id=r.table_id AND t.version=r.version WHERE ?1 IS NULL OR t.imported_unix_ms<=?1")?;
        let rows = stmt.query_map([as_of], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get::<_, String>(6)?)))?;
        rows.map(|row| { let (f, t, table, version, from, to, text) = row?; Ok((f, t, Rate { table, version, from, to, rate: Dec::parse(&text)?, text })) })
            .collect::<Result<_>>()?
    } else { Vec::new() };
    let mut attempts = BTreeMap::<Option<String>, Vec<Value>>::new();
    let mut entries = Vec::new();
    for s in rows.values() {
        let original = cost::valuation(s, &basis)?;
        let converted = match (s.status.as_str(), s.currency.as_deref(), s.amount.as_deref(), s.from.zip(s.to)) {
            ("priced", Some(c), Some(amount), _) if c == target => json!({"status": "priced", "currency": target, "amount": amount, "conversion": "same_currency"}),
            ("priced", Some(c), Some(amount), Some(interval)) => match find(&rates, c, target, interval) {
                Ok(rate) => json!({"status": "priced", "currency": target, "amount": Dec::parse(amount)?.mul(rate.rate)?.to_string(),
                    "conversion": {"from_currency": c, "rate": rate.text, "rate_id": format!("{}@{}:{c}->{target}@{}", rate.table, rate.version, rate.from),
                        "table_id": rate.table, "version": rate.version, "effective_from_unix_ms": rate.from, "effective_to_unix_ms": rate.to,
                        "dated_by": "usage_interval"}}),
                Err(reason) => super::unavailable(reason),
            },
            ("priced", ..) => super::unavailable("usage_time_unknown"),
            _ => original.clone(),
        };
        let entry = json!({"entry_id": s.entry_id, "attempt_id": s.attempt_id, "estimate": original, "converted": converted});
        attempts.entry(s.attempt_id.clone()).or_default().push(json!({"valuation": converted}));
        entries.push(entry);
    }
    let mut rollups = Vec::new();
    for (attempt, values) in &attempts {
        let (estimate, coverage) = cost::summarize(&values.iter().collect::<Vec<_>>())?;
        rollups.push(json!({"attempt_id": attempt, "estimate": estimate, "coverage": coverage}));
    }
    Ok(json!({"basis": basis, "valuation": "dated_fx_conversion", "fixture_only": FIXTURE_ONLY, "target_currency": target, "revision": revision,
        "as_of_unix_ms": as_of, "attempts": rollups, "entries": entries,
        "note": "a separate dated valuation of the stored estimates, which stay unchanged in their own currency; no rate, no conversion"}))
}
