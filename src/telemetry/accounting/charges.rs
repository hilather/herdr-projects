//! Provider charges, invoices and invoice allocation
//! (docs/telemetry/contracts-accounting.md §13): imported from local files
//! marked synthetic (fixture-only), kept as their own cost bases and never
//! added to published-rate estimates; a reconciliation view compares a charge
//! with the estimate of the same usage. Amounts are exact decimals.
use super::cost::{self, Dec, Stored};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const CHARGE_BASIS: &str = "provider_billed";
pub const ALLOCATION_BASIS: &str = "invoice_allocation";
pub(super) const FIXTURE_ONLY: &str = "fixture-only by owner decision: every imported charge, invoice and exchange-rate file is marked synthetic \
    (invented test values, never a provider export), so these amounts are only as real as the files imported";
/// Allocation shares are computed in units of 10^-12 of the invoice currency.
const ALLOCATION_PLACES: u32 = 12;
/// Named, versioned allocation rules (§13).
pub const RULES: [(&str, &str); 1] = [("by_total_tokens.v1", "share = the attempt's Codex total_tokens (input incl. cache reads + output incl. reasoning) \
    of counted entries whose usage interval lies inside the invoice period / all such tokens; amounts in units of 10^-12, the remaining units to the \
    largest remainders (ties by key), so the allocations sum to the invoice exactly")];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChargeFile {
    synthetic: bool,
    source: String,
    provider: String,
    product: String,
    #[serde(default)]
    charges: Vec<ChargeRecord>,
    #[serde(default)]
    invoices: Vec<InvoiceRecord>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ChargeRecord {
    charge_id: String,
    revision: i64,
    currency: String,
    amount: String,
    #[serde(default)]
    response_id: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    charged_unix_ms: Option<i64>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InvoiceRecord {
    invoice_id: String,
    revision: i64,
    kind: String,
    currency: String,
    amount: String,
    period_from_unix_ms: i64,
    period_to_unix_ms: i64,
}

fn name(s: &str) -> bool { !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_graphic()) }

pub(super) fn currency(code: &str) -> Result<()> {
    ensure!(code.len() == 3 && code.bytes().all(|b| b.is_ascii_uppercase()), "currency must be an ISO 4217 code such as USD");
    Ok(())
}

/// A fixture file must say it is synthetic, and name its source.
pub(super) fn synthetic(flag: bool, source: &str) -> Result<()> {
    ensure!(flag, "only synthetic fixture files can be imported (fixture-only by owner decision): set `synthetic = true`");
    ensure!(!source.trim().is_empty() && source.len() <= 512, "source must be 1-512 characters");
    Ok(())
}

pub(super) fn read_file<T: for<'de> Deserialize<'de>>(file: &Path) -> Result<T> {
    let text = std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    Ok(if file.extension().is_some_and(|e| e == "toml") { toml::from_str(&text)? } else { serde_json::from_str(&text)? })
}

/// Insert revision `revision` of `id` in `table` (keyed by `key`) when it is
/// the next one; the same digest again is a no-op, a different one refused.
fn append(tx: &rusqlite::Transaction, table: &str, key: &str, id: &str, revision: i64, digest: &str, insert: impl FnOnce() -> Result<()>) -> Result<bool> {
    let existing: Option<String> = tx.query_row(&format!("SELECT digest FROM {table} WHERE {key}=?1 AND revision=?2"), params![id, revision], |r| r.get(0)).optional()?;
    match existing {
        Some(existing) if existing == digest => Ok(false),
        Some(_) => bail!("{id} revision {revision} exists with different content; {table} are append-only, import the next revision"),
        None => {
            let latest: i64 = tx.query_row(&format!("SELECT coalesce(max(revision),0) FROM {table} WHERE {key}=?1"), [id], |r| r.get(0))?;
            ensure!(revision == latest + 1, "{id} revision {revision} is not the next revision (latest is {latest})");
            insert()?;
            Ok(true)
        }
    }
}

/// `accounting import-charges <file>`: append charge and invoice revisions.
pub fn import(db: &mut Connection, file: &Path) -> Result<Value> {
    let data: ChargeFile = read_file(file)?;
    let context = || format!("charges file {}", file.display());
    synthetic(data.synthetic, &data.source).with_context(context)?;
    ensure!(name(&data.provider) && name(&data.product), "provider and product must be 1-128 printable ASCII characters without spaces");
    let now = jiff::Timestamp::now().as_millisecond();
    let tx = db.transaction()?;
    let (mut charges, mut invoices) = (Vec::new(), Vec::new());
    for mut c in data.charges {
        ensure!(name(&c.charge_id) && c.revision > 0, "charge_id must be printable ASCII and revision positive");
        currency(&c.currency).with_context(context)?;
        c.amount = Dec::rate(&c.amount).with_context(|| format!("charge {} amount", c.charge_id))?.to_string();
        ensure!(c.response_id.as_deref().is_none_or(name) && c.session_id.as_deref().is_none_or(name), "response_id and session_id must be printable ASCII");
        let digest = cost::digest(serde_json::to_string(&(&data.provider, &data.product, &data.source, &c))?.as_bytes());
        let imported = append(&tx, "provider_charges", "charge_id", &c.charge_id, c.revision, &digest, || {
            tx.execute("INSERT INTO provider_charges(charge_id,revision,digest,provider,product,currency,amount,response_id,session_id,charged_unix_ms,source,synthetic,imported_unix_ms)
                VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,1,?12)", params![c.charge_id, c.revision, digest, data.provider, data.product, c.currency, c.amount,
                    c.response_id, c.session_id, c.charged_unix_ms, data.source, now])?;
            Ok(())
        })?;
        charges.push(json!({"charge_id": c.charge_id, "revision": c.revision, "digest": digest, "imported": imported}));
    }
    for mut i in data.invoices {
        ensure!(name(&i.invoice_id) && i.revision > 0, "invoice_id must be printable ASCII and revision positive");
        ensure!(["usage", "subscription"].contains(&i.kind.as_str()), "invoice kind must be usage or subscription");
        ensure!(i.period_to_unix_ms > i.period_from_unix_ms, "period_to_unix_ms must be after period_from_unix_ms");
        currency(&i.currency).with_context(context)?;
        i.amount = Dec::rate(&i.amount).with_context(|| format!("invoice {} amount", i.invoice_id))?.to_string();
        let digest = cost::digest(serde_json::to_string(&(&data.provider, &data.product, &data.source, &i))?.as_bytes());
        let imported = append(&tx, "provider_invoices", "invoice_id", &i.invoice_id, i.revision, &digest, || {
            tx.execute("INSERT INTO provider_invoices(invoice_id,revision,digest,provider,product,kind,currency,amount,period_from_unix_ms,period_to_unix_ms,source,synthetic,imported_unix_ms)
                VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,1,?12)", params![i.invoice_id, i.revision, digest, data.provider, data.product, i.kind, i.currency, i.amount,
                    i.period_from_unix_ms, i.period_to_unix_ms, data.source, now])?;
            Ok(())
        })?;
        invoices.push(json!({"invoice_id": i.invoice_id, "revision": i.revision, "digest": digest, "imported": imported}));
    }
    tx.commit()?;
    Ok(json!({"charges": charges, "invoices": invoices}))
}

/// One stored charge revision.
struct Charge {
    id: String,
    revision: i64,
    currency: String,
    amount: String,
    response_id: Option<String>,
    session_id: Option<String>,
    charged: Option<i64>,
    imported: i64,
}

/// Every charge's revisions imported by `as_of`, oldest first, by charge id.
fn load_charges(db: &Connection, as_of: Option<i64>) -> Result<BTreeMap<String, Vec<Charge>>> {
    let mut out = BTreeMap::<String, Vec<Charge>>::new();
    if !cost::table(db, "provider_charges")? { return Ok(out); }
    let mut stmt = db.prepare("SELECT charge_id,revision,currency,amount,response_id,session_id,charged_unix_ms,imported_unix_ms FROM provider_charges
        WHERE ?1 IS NULL OR imported_unix_ms<=?1 ORDER BY charge_id,revision")?;
    for c in stmt.query_map([as_of], |r| Ok(Charge { id: r.get(0)?, revision: r.get(1)?, currency: r.get(2)?, amount: r.get(3)?, response_id: r.get(4)?,
        session_id: r.get(5)?, charged: r.get(6)?, imported: r.get(7)? }))? {
        let c = c?;
        out.entry(c.id.clone()).or_default().push(c);
    }
    Ok(out)
}

/// Revision history with the signed adjustment each correction appended.
fn history(revisions: &[Charge]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut previous: Option<Dec> = None;
    for c in revisions {
        let amount = Dec::parse(&c.amount)?;
        let adjustment = previous.map(|p| amount.sub(p)).transpose()?.map(|d| d.to_string());
        out.push(json!({"revision": c.revision, "amount": c.amount, "imported_unix_ms": c.imported, "adjustment": adjustment}));
        previous = Some(amount);
    }
    Ok(out)
}

/// One invoice revision.
pub(super) struct Invoice {
    pub(super) id: String,
    pub(super) revision: i64,
    kind: String,
    pub(super) currency: String,
    pub(super) amount: String,
    pub(super) from: i64,
    pub(super) to: i64,
    imported: i64,
    history: Vec<(i64, String, i64)>,
}

impl Invoice {
    fn json(&self) -> Value {
        json!({"invoice_id": self.id, "revision": self.revision, "kind": self.kind, "currency": self.currency, "amount": self.amount,
            "period": {"from_unix_ms": self.from, "to_unix_ms": self.to}, "imported_unix_ms": self.imported,
            "history": self.history.iter().map(|(r, a, at)| json!({"revision": r, "amount": a, "imported_unix_ms": at})).collect::<Vec<_>>()})
    }
}

/// The latest revision imported by `as_of` of every invoice.
fn load_invoices(db: &Connection, as_of: Option<i64>) -> Result<Vec<Invoice>> {
    let mut out: Vec<Invoice> = Vec::new();
    if !cost::table(db, "provider_invoices")? { return Ok(out); }
    let mut stmt = db.prepare("SELECT invoice_id,revision,kind,currency,amount,period_from_unix_ms,period_to_unix_ms,imported_unix_ms FROM provider_invoices
        WHERE ?1 IS NULL OR imported_unix_ms<=?1 ORDER BY invoice_id,revision")?;
    let mut rows = stmt.query([as_of])?;
    while let Some(r) = rows.next()? {
        let i = Invoice { id: r.get(0)?, revision: r.get(1)?, kind: r.get(2)?, currency: r.get(3)?, amount: r.get(4)?, from: r.get(5)?, to: r.get(6)?,
            imported: r.get(7)?, history: Vec::new() };
        let mut history = if out.last().is_some_and(|l| l.id == i.id) { out.pop().map(|l| l.history).unwrap_or_default() } else { Vec::new() };
        history.push((i.revision, i.amount.clone(), i.imported));
        out.push(Invoice { history, ..i });
    }
    Ok(out)
}

/// The valuation revision a read uses (the latest, or the latest computed by
/// `as_of`) and its rows; `None` before any reprice.
pub(super) fn valuations(db: &Connection, as_of: Option<i64>) -> Result<Option<(cost::Header, BTreeMap<String, Stored>)>> {
    let Some(header) = cost::header(db, None, as_of)? else { return Ok(None) };
    let rows = cost::stored_at(db, header.0)?;
    Ok(Some((header, rows)))
}

/// `(discounts, taxes, fees)` included, per rate card version.
fn includes(db: &Connection) -> Result<BTreeMap<(String, i64), [bool; 3]>> {
    if !cost::table(db, "rate_cards")? { return Ok(BTreeMap::new()); }
    Ok(db.prepare("SELECT card_id,version,includes_discounts,includes_taxes,includes_fees FROM rate_cards")?
        .query_map([], |r| Ok(((r.get(0)?, r.get(1)?), [r.get(2)?, r.get(3)?, r.get(4)?])))?.collect::<rusqlite::Result<_>>()?)
}

/// `accounting charges`: every charge (latest revision imported by `as_of`,
/// with its correction history), matched to the ledger entries of the same
/// usage and reconciled against their estimate from the valuation revision of
/// that time; unmatched charges and uncharged estimates are shown apart and
/// never added to each other. Read-only.
pub fn charges(db: &Connection, as_of: Option<i64>) -> Result<Value> {
    let charges = load_charges(db, as_of)?;
    let invoices = load_invoices(db, as_of)?;
    let valued = valuations(db, as_of)?;
    let rows: Vec<&Stored> = valued.as_ref().map(|(_, rows)| rows.values().collect()).unwrap_or_default();
    // Native response ids of ledger entries (immutable per entry id).
    let responses: BTreeMap<String, Option<String>> = if cost::table(db, "usage_entries")? {
        db.prepare("SELECT entry_id,response_id FROM usage_entries WHERE basis='delta'")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    } else { BTreeMap::new() };
    let includes = includes(db)?;
    let basis = valued.as_ref().map_or(cost::BASIS.to_owned(), |(h, _)| h.1.clone());

    // Match each charge's latest revision.
    let mut matched = BTreeMap::<&str, (Result<(&'static str, Vec<&Stored>), &'static str>, &Charge)>::new();
    for (id, revisions) in &charges {
        let Some(current) = revisions.last() else { continue };
        let result = if valued.is_none() { Err("not_priced") } else {
            match (&current.response_id, &current.session_id) {
                (None, None) => Err("no_request_identity"),
                (Some(response), session) => {
                    let found: Vec<&Stored> = rows.iter().copied().filter(|s| responses.get(&s.entry_id).and_then(Option::as_deref) == Some(response.as_str())
                        && session.as_ref().is_none_or(|sid| *sid == s.session_id)).collect();
                    match found.len() { 0 => Err("no_matching_usage"), 1 => Ok(("response_id", found)), _ => Err("ambiguous_match") }
                }
                (None, Some(session)) => {
                    let found: Vec<&Stored> = rows.iter().copied().filter(|s| s.session_id == *session).collect();
                    if found.is_empty() { Err("no_matching_usage") } else { Ok(("session_id", found)) }
                }
            }
        };
        matched.insert(id.as_str(), (result, current));
    }
    // An entry claimed by two charges is a conflict for both, never charged twice.
    let mut claims = BTreeMap::<&str, usize>::new();
    for (result, _) in matched.values() {
        if let Ok((_, entries)) = result { for e in entries { *claims.entry(e.entry_id.as_str()).or_default() += 1; } }
    }
    let mut charged = BTreeSet::<&str>::new();
    let (mut out, mut totals) = (Vec::new(), BTreeMap::<String, Dec>::new());
    let (mut n_matched, mut n_unmatched) = (0, BTreeMap::<&str, usize>::new());
    for (id, (result, current)) in &matched {
        let total = totals.entry(current.currency.clone()).or_insert(Dec::ZERO);
        *total = total.add(Dec::parse(&current.amount)?)?;
        let result = match result {
            Ok((_, entries)) if entries.iter().any(|e| claims[e.entry_id.as_str()] > 1) => Err("overlapping_charges"),
            other => other.clone(),
        };
        let reconciliation = match result {
            Err(reason) => {
                *n_unmatched.entry(reason).or_default() += 1;
                json!({"status": "unmatched", "reason": reason})
            }
            Ok((by, entries)) => {
                n_matched += 1;
                charged.extend(entries.iter().map(|e| e.entry_id.as_str()));
                let values = entries.iter().map(|s| Ok(json!({"valuation": cost::valuation(s, &basis)?}))).collect::<Result<Vec<_>>>()?;
                let (estimate, coverage) = cost::summarize(&values.iter().collect::<Vec<_>>())?;
                let difference = match estimate["status"].as_str() {
                    Some("complete") if estimate["currency"] == current.currency.as_str() => {
                        let d = Dec::parse(&current.amount)?.sub(Dec::parse(estimate["amount"].as_str().context("estimate amount")?)?)?;
                        json!({"currency": current.currency, "amount": d.to_string()})
                    }
                    Some("complete") => json!({"status": "unavailable", "reason": "currency_differs", "detail": "no implicit conversion; see `accounting fx`"}),
                    Some("partial") => super::unavailable("estimate_partial"),
                    _ => super::unavailable("estimate_unavailable"),
                };
                // Evidence that can explain a difference: what the cards used exclude.
                let mut explanations = BTreeSet::new();
                if difference["amount"].as_str().is_some_and(|a| a != "0") {
                    explanations.insert("rate_cards_fixture_only".to_owned());
                    for e in &entries {
                        let Some(flags) = e.card_id.clone().zip(e.card_version).and_then(|k| includes.get(&k)) else { continue };
                        for (included, what) in flags.iter().zip(["discounts", "taxes", "fees"]) {
                            if !included { explanations.insert(format!("estimate_excludes_{what}")); }
                        }
                    }
                }
                json!({"status": "matched", "matched_by": by, "entries": entries.iter().map(|e| e.entry_id.clone()).collect::<Vec<_>>(),
                    "attempt_ids": entries.iter().map(|e| e.attempt_id.clone()).collect::<BTreeSet<_>>(),
                    "estimate": estimate, "coverage": coverage, "difference": difference, "explanations": explanations})
            }
        };
        out.push(json!({"charge_id": id, "revision": current.revision, "basis": CHARGE_BASIS, "currency": current.currency, "amount": current.amount,
            "response_id": current.response_id, "session_id": current.session_id, "charged_unix_ms": current.charged,
            "history": history(&charges[*id])?, "reconciliation": reconciliation}));
    }
    let uncharged: Vec<Value> = rows.iter().filter(|s| !charged.contains(s.entry_id.as_str()))
        .map(|s| Ok(json!({"valuation": cost::valuation(s, &basis)?}))).collect::<Result<_>>()?;
    let (estimate, coverage) = cost::summarize(&uncharged.iter().collect::<Vec<_>>())?;
    Ok(json!({"basis": CHARGE_BASIS, "fixture_only": FIXTURE_ONLY, "as_of_unix_ms": as_of,
        "valuation_revision": valued.as_ref().map(|(h, _)| h.0), "charges": out,
        "provider_billed": totals.iter().map(|(c, a)| (c.clone(), a.to_string())).collect::<BTreeMap<_, _>>(),
        "reconciliation": {"matched": n_matched, "unmatched": n_unmatched,
            "uncharged_estimates": {"basis": basis, "estimate": estimate, "coverage": coverage, "note": "estimates of usage no charge matched; never added to provider_billed"}},
        "invoices": invoices.iter().map(Invoice::json).collect::<Vec<_>>()}))
}

/// `accounting allocate <invoice>`: the invoice's latest revision (by
/// `as_of`) allocated to attempts by a named, versioned rule over the token
/// quantities of the valuation revision of that time. Derived at read time
/// from append-only inputs, so `as_of` reproduces an earlier allocation.
/// Usage whose tokens or time is unknown leaves the allocation partial (the
/// known shares are upper bounds; the unknown attempts `unavailable`), never 0.
pub fn allocate(db: &Connection, invoice: &str, rule: &str, as_of: Option<i64>) -> Result<Value> {
    let Some((rule, description)) = RULES.iter().find(|(r, _)| *r == rule) else {
        bail!("unknown allocation rule {rule:?}; rules: {}", RULES.map(|r| r.0).join(", "));
    };
    let Some(inv) = load_invoices(db, as_of)?.into_iter().find(|i| i.id == invoice) else {
        bail!("no invoice {invoice:?} imported{}", as_of.map(|t| format!(" by {t}")).unwrap_or_default());
    };
    let rule_json = json!({"id": rule, "description": description, "places": ALLOCATION_PLACES});
    let head = json!({"basis": ALLOCATION_BASIS, "fixture_only": FIXTURE_ONLY, "as_of_unix_ms": as_of, "invoice": inv.json(), "rule": rule_json});
    let with = |mut v: Value, extra: Value| { if let (Value::Object(v), Value::Object(e)) = (&mut v, extra) { v.extend(e); } v };
    let Some(((revision, ..), rows)) = valuations(db, as_of)? else {
        return Ok(with(head, json!({"status": "unavailable", "reason": "not_priced", "valuation_revision": null})));
    };
    // Tokens per attempt (None: unattributed) of known usage inside the period.
    let (mut known, mut unknown) = (BTreeMap::<Option<String>, i64>::new(), BTreeMap::<(Option<String>, &str), usize>::new());
    let (mut in_period, mut outside) = (0, 0);
    for s in rows.values() {
        let reason = match (s.from.zip(s.to), s.tokens()) {
            (Some((from, to)), _) if to < inv.from || from >= inv.to => { outside += 1; continue }
            (None, _) => "usage_time_unknown",
            (Some((from, to)), _) if from < inv.from || to >= inv.to => "usage_interval_straddles_period",
            (_, None) => "usage_not_counted",
            (_, Some(tokens)) => { in_period += 1; *known.entry(s.attempt_id.clone()).or_default() += tokens; continue }
        };
        *unknown.entry((s.attempt_id.clone(), reason)).or_default() += 1;
    }
    let total: i64 = known.values().sum();
    let coverage = json!({"entries_in_period": in_period, "unknown": unknown.values().sum::<usize>(), "outside_period": outside});
    let unknown_json: Vec<Value> = unknown.iter().map(|((a, reason), n)| json!({"attempt_id": a, "entries": n, "allocation": super::unavailable(reason)})).collect();
    if total == 0 {
        let reason = if unknown.is_empty() { "no_usage_in_period" } else { "usage_unknown_in_period" };
        return Ok(with(head, json!({"status": "unavailable", "reason": reason, "valuation_revision": revision, "coverage": coverage, "unknown": unknown_json})));
    }
    let units = Dec::parse(&inv.amount)?.units(ALLOCATION_PLACES).context("invoice amount has more than 12 decimal places")?;
    // Largest remainder: floor shares, then one unit each to the largest remainders (ties by key).
    let mut shares: Vec<(Option<String>, i64, i128, i128)> = known.iter().map(|(k, t)| {
        let product = units * i128::from(*t);
        (k.clone(), *t, product / i128::from(total), product % i128::from(total))
    }).collect();
    let mut left = units - shares.iter().map(|s| s.2).sum::<i128>();
    let mut order: Vec<usize> = (0..shares.len()).collect();
    order.sort_by(|a, b| shares[*b].3.cmp(&shares[*a].3).then_with(|| shares[*a].0.cmp(&shares[*b].0)));
    for i in order {
        if left == 0 { break; }
        shares[i].2 += 1;
        left -= 1;
    }
    let partial = !unknown.is_empty();
    let entry = |(key, tokens, amount, _): &(Option<String>, i64, i128, i128)| json!({"attempt_id": key, "tokens": tokens, "share": format!("{tokens}/{total}"),
        "currency": inv.currency, "amount": Dec::from_units(*amount, ALLOCATION_PLACES).to_string(), "bound": if partial { json!("upper") } else { Value::Null }});
    let attempts: Vec<Value> = shares.iter().filter(|s| s.0.is_some()).map(entry).collect();
    let unattributed = shares.iter().find(|s| s.0.is_none()).map(entry).unwrap_or(Value::Null);
    Ok(with(head, json!({"status": if partial { "partial" } else { "complete" }, "reason": if partial { json!("usage_unknown_in_period") } else { Value::Null },
        "valuation_revision": revision, "tokens": total, "allocations": attempts, "unattributed": unattributed, "unknown": unknown_json, "coverage": coverage,
        "note": "derived from the invoice; never added to published-rate estimates or to provider charges"})))
}

/// M11 `reported_spend_subtotal` (doc 07): Σ the latest revision of every
/// provider-reported charge (corrections as adjustments), per currency;
/// invoices and subscriptions are separate. Fixture-only (§13).
pub fn metrics(project: &Path, since: Option<i64>) -> Result<BTreeMap<String, Value>> {
    let body = |value: Value, extra: Value| {
        let mut m = json!({"definition": "M11.charges-v1", "name": "reported_spend_subtotal", "basis": CHARGE_BASIS, "charges": "fixture_only",
            "caveat": FIXTURE_ONLY, "never_added_to": "M12", "value": value});
        if let (Value::Object(m), Value::Object(e)) = (&mut m, extra) { m.extend(e); }
        BTreeMap::from([("M11".to_owned(), m)])
    };
    let Some(db) = crate::telemetry::sidecar::read(project)? else { return Ok(body(super::unavailable("collection_not_run"), json!({}))) };
    let charges = load_charges(&db, None)?;
    let invoices = load_invoices(&db, None)?.len();
    if charges.is_empty() { return Ok(body(super::unavailable("no_provider_charges"), json!({"invoices_separate": invoices}))); }
    let (mut by_currency, mut untimed, mut outside) = (BTreeMap::<String, Dec>::new(), 0, 0);
    for current in charges.values().filter_map(|r| r.last()) {
        if let Some(since) = since {
            match current.charged { None => { untimed += 1; continue } Some(at) if at < since => { outside += 1; continue } _ => {} }
        }
        let sum = by_currency.entry(current.currency.clone()).or_insert(Dec::ZERO);
        *sum = sum.add(Dec::parse(&current.amount)?)?;
    }
    let extra = json!({"charges_counted": charges.len() - untimed - outside, "invoices_separate": invoices,
        "excluded": {"outside_window": outside, "charge_time_unknown": untimed}});
    Ok(match by_currency.len() {
        _ if untimed > 0 => body(super::unavailable("charge_time_unknown"), extra),
        0 => body(super::unavailable("no_provider_charges"), extra),
        1 => {
            let (currency, amount) = by_currency.iter().next().map(|(c, a)| (c.clone(), a.to_string())).context("currency")?;
            let mut m = body(json!(amount), extra);
            if let Some(m) = m.get_mut("M11") { m["currency"] = json!(currency); }
            m
        }
        _ => body(json!({"status": "unavailable", "reason": "mixed_currency",
            "by_currency": by_currency.iter().map(|(c, a)| (c.clone(), a.to_string())).collect::<BTreeMap<_, _>>()}), extra),
    })
}
