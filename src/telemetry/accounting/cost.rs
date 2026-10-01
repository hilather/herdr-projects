//! Versioned rate cards and published-rate estimates
//! (docs/telemetry/contracts-accounting.md §4): cards imported from local
//! files, valuations appended as calculation revisions over the synced ledger.
//! Amounts are exact decimals; rounding happens only in the text view.
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const BASIS: &str = "published_rate_estimate";
/// How the usage time of an entry is bounded (its record time when collected,
/// else session start to first observation), that a rate change inside it is
/// never split, and that a reported model provider must match the card's.
pub const POLICY: &str = "usage_interval=record_time|session_start..first_observed;split=none;provider=checked_when_reported";
/// The policy of revisions appended before A4 metadata was consumed.
const LEGACY_POLICY: &str = "usage_interval=session_start..first_observed;split=none";
const RECORD_TIME: &str = "record_time";
const FALLBACK: &str = "session_start..first_observed";
const CATEGORIES: [&str; 4] = ["input", "cache_read", "cache_write", "output"];
/// Text view: amounts rounded half-up to this many decimal places.
const TEXT_PLACES: u32 = 6;

/// An exact decimal: `mantissa / 10^scale`. Parsed values are non-negative;
/// only a difference (`sub`) can be negative.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Dec {
    mantissa: i128,
    scale: u32,
}

impl Dec {
    pub(super) const ZERO: Dec = Dec {
        mantissa: 0,
        scale: 0,
    };

    /// Plain non-negative decimal notation (a stored amount), digits only.
    pub(super) fn parse(text: &str) -> Result<Dec> {
        let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
        ensure!(
            !whole.is_empty()
                && whole
                    .bytes()
                    .chain(fraction.bytes())
                    .all(|b| b.is_ascii_digit())
                && (!text.contains('.') || !fraction.is_empty())
                && whole.len() + fraction.len() <= 36,
            "{text:?} is not a plain non-negative decimal"
        );
        Ok(Dec {
            mantissa: format!("{whole}{fraction}").parse()?,
            scale: fraction.len() as u32,
        }
        .trim())
    }

    /// A rate: at most 18 digits and 12 after the point, so any amount fits exactly.
    pub(super) fn rate(text: &str) -> Result<Dec> {
        let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
        ensure!(
            fraction.len() <= 12 && whole.len() + fraction.len() <= 18,
            "rate {text:?} has more than 18 digits or 12 decimal places"
        );
        Dec::parse(text).with_context(|| format!("rate {text:?}"))
    }

    fn trim(mut self) -> Dec {
        while self.scale > 0 && self.mantissa % 10 == 0 {
            self.mantissa /= 10;
            self.scale -= 1;
        }
        self
    }

    fn at(self, scale: u32) -> i128 {
        self.mantissa * 10i128.pow(scale - self.scale)
    }

    /// Exact sum; an overflow is an error, never a wrapped amount.
    pub(super) fn add(self, other: Dec) -> Result<Dec> {
        let scale = self.scale.max(other.scale);
        let widen = |d: Dec| {
            10i128
                .checked_pow(scale - d.scale)
                .and_then(|p| d.mantissa.checked_mul(p))
        };
        let mantissa = widen(self)
            .zip(widen(other))
            .and_then(|(a, b)| a.checked_add(b))
            .context("amount overflows exact arithmetic")?;
        Ok(Dec { mantissa, scale }.trim())
    }

    /// `self × tokens / 10^places`, exact (a rate below 10^18 times tokens below 2^53 fits in i128).
    fn times(self, tokens: i64, places: u32) -> Dec {
        Dec {
            mantissa: self.mantissa * i128::from(tokens),
            scale: self.scale + places,
        }
        .trim()
    }

    /// Exact difference `self − other` (negative when `other` is larger).
    pub(super) fn sub(self, other: Dec) -> Result<Dec> {
        self.add(Dec { mantissa: -other.mantissa, scale: other.scale })
    }

    /// Exact product; an overflow is an error, never a wrapped amount.
    pub(super) fn mul(self, other: Dec) -> Result<Dec> {
        let mantissa = self.mantissa.checked_mul(other.mantissa).context("amount overflows exact arithmetic")?;
        let scale = self.scale + other.scale;
        ensure!(scale <= 36, "amount overflows exact arithmetic");
        Ok(Dec { mantissa, scale }.trim())
    }

    pub(super) fn is_negative(self) -> bool { self.mantissa < 0 }

    /// The value in units of `10^-places`, when it has at most `places` decimals.
    pub(super) fn units(self, places: u32) -> Option<i128> {
        (self.scale <= places).then(|| 10i128.checked_pow(places - self.scale).and_then(|p| self.mantissa.checked_mul(p))).flatten()
    }

    /// `units × 10^-places`.
    pub(super) fn from_units(units: i128, places: u32) -> Dec { Dec { mantissa: units, scale: places }.trim() }

    /// Rounded half-up (away from zero) to `places`, for presentation only.
    pub(super) fn rounded(self, places: u32) -> String {
        if self.mantissa < 0 {
            return format!("-{}", Dec { mantissa: -self.mantissa, scale: self.scale }.rounded(places));
        }
        let value = if self.scale <= places {
            self.at(places)
        } else {
            let divisor = 10i128.pow(self.scale - places);
            (self.mantissa + divisor / 2) / divisor
        };
        let digits = format!("{value:0>width$}", width = places as usize + 1);
        let (whole, fraction) = digits.split_at(digits.len() - places as usize);
        if places == 0 {
            whole.to_owned()
        } else {
            format!("{whole}.{fraction}")
        }
    }
}

impl std::fmt::Display for Dec {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.rounded(self.scale))
    }
}

/// The file `accounting import-rate-card` reads (TOML by extension, else JSON).
/// Rates are decimal strings; a number is refused so no float touches money.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CardFile {
    card_id: String,
    version: i64,
    provider: String,
    product: String,
    models: Vec<String>,
    currency: String,
    rate_unit: i64,
    effective_from_unix_ms: i64,
    effective_to_unix_ms: Option<i64>,
    includes: Includes,
    source: String,
    rates: Vec<RateFile>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Includes {
    discounts: bool,
    taxes: bool,
    fees: bool,
}
#[derive(Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct RateFile {
    category: String,
    #[serde(default)]
    cache_tier: String,
    rate: String,
}

/// Validate and canonicalize (sorted models and rates, canonical decimals).
fn canonical(mut card: CardFile) -> Result<CardFile> {
    let name = |s: &str| !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_graphic());
    ensure!(
        name(&card.card_id),
        "card_id must be 1-128 printable ASCII characters without spaces"
    );
    ensure!(card.version > 0, "version must be positive");
    ensure!(
        name(&card.provider) && name(&card.product),
        "provider and product must be 1-128 printable ASCII characters without spaces"
    );
    ensure!(
        !card.source.trim().is_empty() && card.source.len() <= 512,
        "source must be 1-512 characters"
    );
    ensure!(
        card.currency.len() == 3 && card.currency.bytes().all(|b| b.is_ascii_uppercase()),
        "currency must be an ISO 4217 code such as USD"
    );
    ensure!(
        (0..=9).any(|k| 10i64.pow(k) == card.rate_unit),
        "rate_unit must be a power of ten from 1 to 1000000000"
    );
    ensure!(
        card.effective_to_unix_ms
            .is_none_or(|to| to > card.effective_from_unix_ms),
        "effective_to_unix_ms must be after effective_from_unix_ms"
    );
    card.models.sort();
    card.models.dedup();
    ensure!(
        !card.models.is_empty() && card.models.iter().all(|m| name(m)),
        "models must list at least one model name"
    );
    ensure!(!card.rates.is_empty(), "rates must not be empty");
    for rate in &mut card.rates {
        ensure!(
            CATEGORIES.contains(&rate.category.as_str()),
            "rate category {:?} is not one of {CATEGORIES:?}",
            rate.category
        );
        ensure!(
            rate.cache_tier.is_empty() || name(&rate.cache_tier),
            "cache_tier must be printable ASCII without spaces"
        );
        rate.rate = Dec::rate(&rate.rate)?.to_string();
    }
    card.rates.sort();
    let before = card.rates.len();
    card.rates
        .dedup_by(|a, b| (&a.category, &a.cache_tier) == (&b.category, &b.cache_tier));
    ensure!(
        card.rates.len() == before,
        "a category and cache tier may have only one rate"
    );
    Ok(card)
}

pub(super) fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// Append a card version. The same content again is a no-op; different
/// content under an existing `(card_id, version)` is refused (append-only).
pub fn import(db: &mut Connection, file: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(file)
        .with_context(|| format!("read rate card {}", file.display()))?;
    let card: CardFile = if file.extension().is_some_and(|e| e == "toml") {
        toml::from_str(&text)?
    } else {
        serde_json::from_str(&text)?
    };
    let card = canonical(card).with_context(|| format!("rate card {}", file.display()))?;
    let digest = digest(serde_json::to_string(&card)?.as_bytes());
    let tx = db.transaction()?;
    let existing: Option<String> = tx
        .query_row(
            "SELECT digest FROM rate_cards WHERE card_id=?1 AND version=?2",
            params![card.card_id, card.version],
            |r| r.get(0),
        )
        .optional()?;
    let imported = match existing {
        Some(existing) if existing == digest => false,
        Some(_) => bail!(
            "rate card {} version {} exists with different content; rate cards are append-only, import a new version",
            card.card_id,
            card.version
        ),
        None => {
            tx.execute("INSERT INTO rate_cards(card_id,version,digest,provider,product,currency,rate_unit,effective_from_unix_ms,effective_to_unix_ms,
                includes_discounts,includes_taxes,includes_fees,source,imported_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params![card.card_id, card.version, digest, card.provider, card.product, card.currency, card.rate_unit, card.effective_from_unix_ms,
                    card.effective_to_unix_ms, card.includes.discounts, card.includes.taxes, card.includes.fees, card.source, jiff::Timestamp::now().as_millisecond()])?;
            for model in &card.models {
                tx.execute(
                    "INSERT INTO rate_card_models(card_id,version,model) VALUES(?1,?2,?3)",
                    params![card.card_id, card.version, model],
                )?;
            }
            for rate in &card.rates {
                tx.execute("INSERT INTO rate_card_rates(card_id,version,category,cache_tier,rate) VALUES(?1,?2,?3,?4,?5)",
                    params![card.card_id, card.version, rate.category, rate.cache_tier, rate.rate])?;
            }
            true
        }
    };
    tx.commit()?;
    Ok(
        json!({"card_id": card.card_id, "version": card.version, "digest": digest, "imported": imported}),
    )
}

pub(super) fn table(db: &Connection, name: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |r| r.get(0),
    )?)
}

/// Every imported card version. Read-only.
pub fn list(db: &Connection) -> Result<Value> {
    if !table(db, "rate_cards")? {
        return Ok(json!({"rate_cards": []}));
    }
    let mut models = db.prepare(
        "SELECT model FROM rate_card_models WHERE card_id=?1 AND version=?2 ORDER BY model",
    )?;
    let mut rates = db.prepare("SELECT category,cache_tier,rate FROM rate_card_rates WHERE card_id=?1 AND version=?2 ORDER BY category,cache_tier")?;
    let mut stmt = db.prepare("SELECT card_id,version,digest,provider,product,currency,rate_unit,effective_from_unix_ms,effective_to_unix_ms,
        includes_discounts,includes_taxes,includes_fees,source,imported_unix_ms FROM rate_cards ORDER BY card_id,version")?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let (id, version): (String, i64) = (r.get(0)?, r.get(1)?);
        let models: Vec<String> = models
            .query_map(params![id, version], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let rates: Vec<Value> = rates
            .query_map(params![id, version], |r| {
                Ok(json!({"category": r.get::<_, String>(0)?,
            "cache_tier": r.get::<_, String>(1)?, "rate": r.get::<_, String>(2)?}))
            })?
            .collect::<rusqlite::Result<_>>()?;
        out.push(json!({"card_id": id, "version": version, "digest": r.get::<_, String>(2)?, "provider": r.get::<_, String>(3)?,
            "product": r.get::<_, String>(4)?, "models": models, "currency": r.get::<_, String>(5)?, "rate_unit": r.get::<_, i64>(6)?,
            "effective_from_unix_ms": r.get::<_, i64>(7)?, "effective_to_unix_ms": r.get::<_, Option<i64>>(8)?,
            "includes": {"discounts": r.get::<_, bool>(9)?, "taxes": r.get::<_, bool>(10)?, "fees": r.get::<_, bool>(11)?},
            "source": r.get::<_, String>(12)?, "imported_unix_ms": r.get::<_, i64>(13)?, "rates": rates}));
    }
    Ok(json!({"rate_cards": out}))
}

/// A card version applicable to a product and model.
struct Card {
    id: String,
    version: i64,
    provider: String,
    currency: String,
    places: u32,
    from: i64,
    to: Option<i64>,
    /// `category → [(cache_tier, rate)]`.
    rates: BTreeMap<String, Vec<(String, Dec)>>,
}

/// One valuation row (the `valuations` columns after `revision`).
#[derive(Clone, Serialize)]
struct Row {
    entry_id: String,
    session_id: String,
    role: String,
    attempt_id: Option<String>,
    model: Option<String>,
    usage: Option<(i64, i64)>,
    /// How `usage` was bounded (`record_time` or the fallback); absent in a legacy revision.
    #[serde(skip_serializing_if = "Option::is_none")]
    usage_basis: Option<&'static str>,
    /// `[new_input, cache_read, cache_write, output]`.
    quantities: Option<[i64; 4]>,
    reason: Option<String>,
    card: Option<(String, i64)>,
    currency: Option<String>,
    amount: Option<String>,
    components: Option<BTreeMap<String, String>>,
    /// A priced entry's provider check (`matched` or `provider_unverified`).
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_check: Option<&'static str>,
}

impl Row {
    /// The same result as a revision before A4 would give: the fallback
    /// interval (or none) and no reported provider.
    fn legacy(&self) -> bool {
        self.usage_basis.is_none_or(|b| b == FALLBACK) && self.provider_check.is_none_or(|c| c == "provider_unverified")
            && self.reason.as_deref() != Some("provider_mismatch")
    }
}

fn cards(db: &Connection) -> Result<Vec<(String, BTreeSet<String>, Card)>> {
    let mut models =
        db.prepare("SELECT model FROM rate_card_models WHERE card_id=?1 AND version=?2")?;
    let mut rates = db.prepare(
        "SELECT category,cache_tier,rate FROM rate_card_rates WHERE card_id=?1 AND version=?2",
    )?;
    let mut stmt = db.prepare("SELECT card_id,version,product,currency,rate_unit,effective_from_unix_ms,effective_to_unix_ms,provider FROM rate_cards")?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let (id, version, product, unit): (String, i64, String, i64) =
            (r.get(0)?, r.get(1)?, r.get(2)?, r.get(4)?);
        let mut card = Card {
            id: id.clone(),
            version,
            provider: r.get(7)?,
            currency: r.get(3)?,
            places: unit.ilog10(),
            from: r.get(5)?,
            to: r.get(6)?,
            rates: BTreeMap::new(),
        };
        for rate in rates.query_map(params![id, version], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })? {
            let (category, tier, rate) = rate?;
            card.rates
                .entry(category)
                .or_default()
                .push((tier, Dec::rate(&rate)?));
        }
        let models = models
            .query_map(params![id, version], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        out.push((product, models, card));
    }
    Ok(out)
}

/// Price one entry: the one card version effective over the whole usage
/// interval, every non-zero category rated, amounts exact. A reported model
/// provider keeps only the cards of that provider (none: `provider_mismatch`).
fn price<'a>(
    cards: &'a [(String, BTreeSet<String>, Card)],
    product: &str,
    model: &str,
    provider: Option<&str>,
    (from, to): (i64, i64),
    q: [i64; 4],
) -> std::result::Result<(&'a Card, Dec, BTreeMap<String, String>), &'static str> {
    let mut overlapping: Vec<&Card> = cards
        .iter()
        .filter(|(p, models, c)| {
            p == product && models.contains(model) && c.from <= to && c.to.is_none_or(|t| t > from)
        })
        .map(|c| &c.2)
        .collect();
    if let Some(provider) = provider.filter(|_| !overlapping.is_empty()) {
        overlapping.retain(|c| c.provider == provider);
        if overlapping.is_empty() {
            return Err("provider_mismatch");
        }
    }
    let Some(card) = overlapping.iter().max_by_key(|c| c.version) else {
        return Err("no_rate_card");
    };
    if overlapping.iter().any(|c| c.id != card.id) {
        return Err("ambiguous_rate_cards");
    }
    if card.from > from || card.to.is_some_and(|t| t <= to) {
        return Err("rate_change_within_usage_interval");
    }
    let (mut total, mut components) = (Dec::ZERO, BTreeMap::new());
    for (category, tokens) in [("input", q[0]), ("cache_read", q[1]), ("cache_write", q[2]), ("output", q[3])] {
        if tokens == 0 {
            continue;
        }
        let rate = match card.rates.get(category).map(Vec::as_slice) {
            None | Some([]) => {
                return Err(match category {
                    "input" => "input_rate_missing",
                    "cache_read" => "cache_read_rate_missing",
                    "cache_write" => "cache_write_convention_unknown",
                    _ => "output_rate_missing",
                });
            }
            Some([(_, rate)]) => *rate,
            Some(_) => return Err("cache_tier_unknown"),
        };
        let amount = rate.times(tokens, card.places);
        total = total.add(amount).map_err(|_| "amount_overflow")?;
        components.insert(category.to_owned(), amount.to_string());
    }
    Ok((card, total, components))
}

/// What a reprice reads for one delta entry of the synced ledger, with the
/// rollout that stored it: its attempt, its A4 record time (else the session
/// start and the first observation) and its A4 model provider. Its digest,
/// with the rate card versions, is the input fingerprint (§12).
#[derive(Serialize)]
struct Input {
    entry_id: String,
    session_id: String,
    source: String,
    model: Option<String>,
    /// `[new_input, cache_read, cache_write, output]` as normalized.
    q: [Option<i64>; 4],
    counted: bool,
    role: String,
    attempt_id: Option<String>,
    start: Option<i64>,
    observed: Option<i64>,
    record: Option<i64>,
    provider: Option<String>,
}

fn inputs(db: &Connection) -> Result<Vec<Input>> {
    let mut stmt = db.prepare("SELECT e.entry_id,e.session_id,e.source,e.model,e.new_input_tokens,e.cache_read_tokens,e.cache_write_tokens,e.output_tokens,
        EXISTS(SELECT 1 FROM usage_dispositions d WHERE d.entry_id=e.entry_id AND d.disposition='accepted'),
        coalesce((SELECT g.role FROM session_graph_nodes g WHERE g.session_id=e.session_id LIMIT 1),'primary'),
        s.attempt_id,s.session_unix_ms,u.observed_unix_ms,t.record_unix_ms,m.model_provider
        FROM usage_entries e LEFT JOIN codex_usage u ON u.session_id=e.session_id AND u.ordinal=e.position
        LEFT JOIN rollout_sources s ON s.path_digest=u.path_digest
        LEFT JOIN codex_usage_times t ON t.session_id=e.session_id AND t.ordinal=e.position
        LEFT JOIN rollout_metadata m ON m.path_digest=u.path_digest WHERE e.basis='delta'
        AND NOT EXISTS(SELECT 1 FROM usage_dispositions d WHERE d.entry_id=e.entry_id AND (d.reason=?1 OR d.reason='native_surface_precedence')) ORDER BY e.entry_id")?;
    // A repeated response (ledger `REPEATED`) is not usage: it is never valued.
    let rows = stmt.query_map([super::ledger::REPEATED], |r| Ok(Input { entry_id: r.get(0)?, session_id: r.get(1)?, source: r.get(2)?, model: r.get(3)?,
        q: [r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?], counted: r.get(8)?, role: r.get(9)?, attempt_id: r.get(10)?, start: r.get(11)?,
        observed: r.get(12)?, record: r.get(13)?, provider: r.get(14)? }))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Value one entry with the cards effective over its usage interval.
fn value(cards: &[(String, BTreeSet<String>, Card)], input: &Input) -> Row {
    let quantities = input.q.iter().all(Option::is_some).then(|| input.q.map(Option::unwrap_or_default));
    let (usage, usage_basis) = match input.record {
        Some(at) => (Some((at, at)), Some(RECORD_TIME)),
        None => {
            let usage = input.start.zip(input.observed).map(|(a, b)| (a.min(b), a.max(b)));
            (usage, usage.map(|_| FALLBACK))
        }
    };
    let mut row = Row {
        entry_id: input.entry_id.clone(),
        session_id: input.session_id.clone(),
        role: input.role.clone(),
        attempt_id: input.attempt_id.clone(),
        model: input.model.clone(),
        usage,
        usage_basis,
        quantities,
        reason: None,
        card: None,
        currency: None,
        amount: None,
        components: None,
        provider_check: None,
    };
    let result = match (input.counted, quantities, input.model.as_deref(), usage) {
        (false, ..) | (_, None, ..) => Err("usage_not_counted"),
        (_, _, None, _) => Err("model_unknown"),
        (_, _, _, None) => Err("usage_time_unknown"),
        (true, Some(q), Some(model), Some(usage)) => price(cards, &input.source, model, input.provider.as_deref(), usage, q),
    };
    match result {
        Ok((card, amount, components)) => {
            (row.card, row.currency, row.amount, row.components) =
                (Some((card.id.clone(), card.version)), Some(card.currency.clone()), Some(amount.to_string()), Some(components));
            row.provider_check = Some(if input.provider.is_some() { "matched" } else { "provider_unverified" });
        }
        Err(reason) => row.reason = Some(reason.to_owned()),
    }
    row
}

/// Value every delta entry of the synced ledger; append a revision when the
/// result differs from the latest one. Measured tokens are only read.
pub fn reprice(db: &mut Connection) -> Result<Value> { run(db, None) }

/// The ticker's reprice (§12): only when a rate card has been imported and the
/// input fingerprint (card versions and ledger rows) changed since the last
/// reprice, and only when those inputs fit in the tick's byte budget.
pub fn tick(db: &mut Connection, budget: crate::telemetry::codex::Budget) -> Result<Value> { run(db, Some(budget)) }

fn run(db: &mut Connection, gate: Option<crate::telemetry::codex::Budget>) -> Result<Value> {
    // Immediate: racing reprices serialize, so the second sees the first's revision.
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let Some(synced) = tx.query_row("SELECT synced_unix_ms FROM usage_ledger", [], |r| r.get::<_, i64>(0)).optional()? else {
        return Ok(super::unavailable("ledger_not_synced"));
    };
    let versions: Vec<(String, i64, String)> = tx.prepare("SELECT card_id,version,digest FROM rate_cards ORDER BY card_id,version")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    if gate.is_some() && versions.is_empty() {
        return Ok(json!({"skipped": "no_rate_cards"}));
    }
    let inputs = inputs(&tx)?;
    let read = serde_json::to_string(&(POLICY, &versions, &inputs))?;
    let fingerprint = digest(read.as_bytes());
    if let Some(budget) = gate {
        let last: Option<String> = tx.query_row("SELECT digest FROM valuation_inputs", [], |r| r.get(0)).optional()?;
        if last.as_deref() == Some(fingerprint.as_str()) { return Ok(json!({"skipped": "unchanged"})); }
        if read.len() as u64 > budget.bytes { return Ok(json!({"skipped": "budget_exhausted", "input_bytes": read.len()})); }
    }
    let cards = cards(&tx)?;
    let out: Vec<Row> = inputs.iter().map(|input| value(&cards, input)).collect();
    let digest = digest(serde_json::to_string(&(POLICY, &out))?.as_bytes());
    let latest: Option<(i64, String, String)> = tx
        .query_row(
            "SELECT revision,digest,policy FROM valuation_revisions ORDER BY revision DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    // A revision appended before A4 is compared as it was computed then: a
    // result that only adds the fallback basis and unverified providers is the same.
    let same = |(_, stored, policy): &(i64, String, String)| -> Result<bool> {
        if policy != LEGACY_POLICY {
            return Ok(*stored == digest);
        }
        if !out.iter().all(Row::legacy) {
            return Ok(false);
        }
        // Without the A4 fields a row serializes exactly as it did before them.
        let rows: Vec<Row> = out.iter().map(|row| Row { usage_basis: None, provider_check: None, ..row.clone() }).collect();
        Ok(*stored == self::digest(serde_json::to_string(&(LEGACY_POLICY, &rows))?.as_bytes()))
    };
    let now = jiff::Timestamp::now().as_millisecond();
    let record = |tx: &rusqlite::Transaction, revision: i64| tx.execute("INSERT INTO valuation_inputs(singleton,digest,revision,recorded_unix_ms) VALUES(1,?1,?2,?3)
        ON CONFLICT(singleton) DO UPDATE SET digest=excluded.digest,revision=excluded.revision,recorded_unix_ms=excluded.recorded_unix_ms",
        params![fingerprint, revision, now]);
    if let Some(latest) = &latest
        && same(latest)?
    {
        record(&tx, latest.0)?;
        store_metrics(&tx)?;
        tx.commit()?;
        return Ok(json!({"revision": latest.0, "appended": false, "entries": out.len()}));
    }
    // Stored as a delta against the previous revision as it reads back (§12).
    let previous = match &latest { Some((revision, ..)) => stored_at(&tx, *revision)?, None => BTreeMap::new() };
    let current: BTreeMap<String, Stored> = out.iter().map(|row| row.stored().map(|s| (s.entry_id.clone(), s))).collect::<Result<_>>()?;
    let changed: Vec<&Stored> = current.values().filter(|s| previous.get(&s.entry_id) != Some(*s)).collect();
    let removed: Vec<&String> = previous.keys().filter(|id| !current.contains_key(*id)).collect();
    let revision = latest.map_or(1, |l| l.0 + 1);
    tx.execute("INSERT INTO valuation_revisions(revision,basis,policy,ledger_synced_unix_ms,digest,computed_unix_ms) VALUES(?1,?2,?3,?4,?5,?6)",
        params![revision, BASIS, POLICY, synced, digest, now])?;
    tx.execute("INSERT INTO valuation_delta_revisions(revision,changed,removed) VALUES(?1,?2,?3)", params![revision, changed.len(), removed.len()])?;
    for s in &changed {
        tx.execute("INSERT INTO valuation_deltas(revision,entry_id,removed,session_id,role,attempt_id,model,usage_from_unix_ms,usage_to_unix_ms,new_input_tokens,
            cache_read_tokens,cache_write_tokens,output_tokens,status,reason,card_id,card_version,currency,amount,components,usage_basis,provider_check)
            VALUES(?1,?2,0,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",
            params![revision, s.entry_id, s.session_id, s.role, s.attempt_id, s.model, s.from, s.to, s.q[0], s.q[1], s.q[2], s.q[3], s.status, s.reason,
                s.card_id, s.card_version, s.currency, s.amount, s.components, s.usage_basis, s.provider_check])?;
    }
    for id in &removed {
        tx.execute("INSERT INTO valuation_deltas(revision,entry_id,removed) VALUES(?1,?2,1)", params![revision, id])?;
    }
    record(&tx, revision)?;
    store_metrics(&tx)?;
    tx.commit()?;
    Ok(json!({"revision": revision, "appended": true, "entries": out.len(), "stored": {"changed": changed.len(), "removed": removed.len()}}))
}

/// One valuation as stored and read back: a `valuations` row (with its
/// `valuation_bases` row, if any) or a `valuation_deltas` row.
#[derive(Clone, PartialEq)]
pub(super) struct Stored {
    pub(super) entry_id: String,
    pub(super) session_id: String,
    role: String,
    pub(super) attempt_id: Option<String>,
    model: Option<String>,
    pub(super) from: Option<i64>,
    pub(super) to: Option<i64>,
    /// `[new_input, cache_read, cache_write, output]`.
    pub(super) q: [Option<i64>; 4],
    pub(super) status: String,
    pub(super) reason: Option<String>,
    pub(super) card_id: Option<String>,
    pub(super) card_version: Option<i64>,
    pub(super) currency: Option<String>,
    pub(super) amount: Option<String>,
    components: Option<String>,
    usage_basis: Option<String>,
    provider_check: Option<String>,
}

impl Stored {
    /// Codex `total_tokens` (input incl. cache reads + output incl. reasoning)
    /// of a counted, normalized entry; `None` when its usage is not counted.
    pub(super) fn tokens(&self) -> Option<i64> {
        if self.reason.as_deref() == Some("usage_not_counted") { return None; }
        let [new, cached, _, output] = self.q;
        Some(new? + cached? + output?)
    }
}

impl Row {
    /// The row as it is stored (the same columns a full copy wrote).
    fn stored(&self) -> Result<Stored> {
        Ok(Stored {
            entry_id: self.entry_id.clone(),
            session_id: self.session_id.clone(),
            role: self.role.clone(),
            attempt_id: self.attempt_id.clone(),
            model: self.model.clone(),
            from: self.usage.map(|u| u.0),
            to: self.usage.map(|u| u.1),
            q: self.quantities.map(|q| q.map(Some)).unwrap_or([None; 4]),
            status: if self.reason.is_none() { "priced" } else { "unavailable" }.to_owned(),
            reason: self.reason.clone(),
            card_id: self.card.as_ref().map(|c| c.0.clone()),
            card_version: self.card.as_ref().map(|c| c.1),
            currency: self.currency.clone(),
            amount: self.amount.clone(),
            components: self.components.as_ref().map(serde_json::to_string).transpose()?,
            usage_basis: self.usage_basis.map(str::to_owned),
            provider_check: self.provider_check.map(str::to_owned),
        })
    }
}

/// The stored columns from `entry_id` on, in `Stored` field order.
const STORED: &str = "entry_id,session_id,role,attempt_id,model,usage_from_unix_ms,usage_to_unix_ms,new_input_tokens,cache_read_tokens,cache_write_tokens,
    output_tokens,status,reason,card_id,card_version,currency,amount,components";

fn stored(r: &rusqlite::Row) -> rusqlite::Result<Stored> {
    Ok(Stored {
        entry_id: r.get(0)?,
        session_id: r.get(1)?,
        role: r.get(2)?,
        attempt_id: r.get(3)?,
        model: r.get(4)?,
        from: r.get(5)?,
        to: r.get(6)?,
        q: [r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?],
        status: r.get(11)?,
        reason: r.get(12)?,
        card_id: r.get(13)?,
        card_version: r.get(14)?,
        currency: r.get(15)?,
        amount: r.get(16)?,
        components: r.get(17)?,
        usage_basis: r.get(18)?,
        provider_check: r.get(19)?,
    })
}

/// Revision `revision` as stored: the latest full copy at or below it
/// (written before stream version 9), then every delta above that copy up
/// to it, in order. Keyed by entry.
pub(super) fn stored_at(db: &Connection, revision: i64) -> Result<BTreeMap<String, Stored>> {
    let deltas: Vec<i64> = if table(db, "valuation_delta_revisions")? {
        db.prepare("SELECT revision FROM valuation_delta_revisions WHERE revision<=?1 ORDER BY revision")?
            .query_map([revision], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    } else {
        Vec::new()
    };
    let not_delta = if deltas.is_empty() { String::new() } else {
        format!(" AND revision NOT IN ({})", deltas.iter().map(i64::to_string).collect::<Vec<_>>().join(","))
    };
    let full: Option<i64> = db.query_row(&format!("SELECT max(revision) FROM valuation_revisions WHERE revision<=?1{not_delta}"), [revision], |r| r.get(0))?;
    let mut state = BTreeMap::new();
    if let Some(full) = full {
        // Stream version 6 records the A4 bases; a sidecar read before its upgrade has none.
        let (extra, join) = if table(db, "valuation_bases")? {
            ("b.usage_basis,b.provider_check", "LEFT JOIN valuation_bases b ON b.revision=v.revision AND b.entry_id=v.entry_id")
        } else {
            ("NULL,NULL", "")
        };
        let columns = STORED.split(',').map(|c| format!("v.{}", c.trim())).collect::<Vec<_>>().join(",");
        let mut stmt = db.prepare(&format!("SELECT {columns},{extra} FROM valuations v {join} WHERE v.revision=?1"))?;
        for row in stmt.query_map([full], stored)? {
            let row = row?;
            state.insert(row.entry_id.clone(), row);
        }
    }
    let mut stmt = if deltas.is_empty() { None } else {
        Some(db.prepare(&format!("SELECT {STORED},usage_basis,provider_check,removed FROM valuation_deltas WHERE revision=?1"))?)
    };
    for delta in deltas.iter().filter(|d| full.is_none_or(|full| **d > full)) {
        let Some(stmt) = stmt.as_mut() else { break };
        let mut rows = stmt.query([delta])?;
        while let Some(r) = rows.next()? {
            if r.get::<_, bool>(20)? {
                state.remove(&r.get::<_, String>(0)?);
            } else {
                let row = stored(r)?;
                state.insert(row.entry_id.clone(), row);
            }
        }
    }
    Ok(state)
}

/// The estimate over some valuations: complete only when every entry is
/// priced in one currency; a priced subset is labeled partial; currencies
/// are never added together; nothing priced is unavailable, never 0.
pub(super) fn summarize(entries: &[&Value]) -> Result<(Value, Value)> {
    let (mut by_currency, mut unpriced) = (
        BTreeMap::<String, Dec>::new(),
        BTreeMap::<String, usize>::new(),
    );
    for entry in entries {
        let v = &entry["valuation"];
        match v["status"].as_str() {
            Some("priced") => {
                let amount = Dec::parse(
                    v["amount"]
                        .as_str()
                        .context("priced valuation without amount")?,
                )?;
                let sum = by_currency
                    .entry(
                        v["currency"]
                            .as_str()
                            .context("priced valuation without currency")?
                            .to_owned(),
                    )
                    .or_insert(Dec::ZERO);
                *sum = sum.add(amount)?;
            }
            _ => {
                *unpriced
                    .entry(v["reason"].as_str().unwrap_or_default().to_owned())
                    .or_default() += 1
            }
        }
    }
    let priced = entries.len() - unpriced.values().sum::<usize>();
    let coverage = json!({"entries": entries.len(), "priced": priced, "unpriced": unpriced});
    let estimate = match (by_currency.len(), priced == entries.len()) {
        (0, _) => super::unavailable("no_priced_entries"),
        (1, complete) => {
            let (currency, amount) = by_currency
                .iter()
                .next()
                .map(|(c, a)| (c.clone(), a.to_string()))
                .unwrap_or_default();
            if complete {
                json!({"status": "complete", "currency": currency, "amount": amount})
            } else {
                json!({"status": "partial", "reason": "unpriced_entries", "currency": currency, "priced_amount": amount})
            }
        }
        _ => {
            json!({"status": "unavailable", "reason": "mixed_currency", "priced_by_currency": by_currency.iter().map(|(c, a)| (c.clone(), a.to_string())).collect::<BTreeMap<_, _>>()})
        }
    };
    Ok((estimate, coverage))
}

/// A stored valuation as `cost` shows it.
pub(super) fn valuation(s: &Stored, basis: &str) -> Result<Value> {
    Ok(if s.status == "priced" {
        json!({"status": "priced", "basis": basis, "rate_card": {"card_id": s.card_id, "version": s.card_version},
            "currency": s.currency, "amount": s.amount,
            "components": serde_json::from_str::<Value>(s.components.as_deref().context("priced valuation without components")?)?})
    } else {
        super::unavailable(s.reason.as_deref().unwrap_or_default())
    })
}

/// The latest revision number and its header, or `None` before any reprice.
pub(super) type Header = (i64, String, String, i64, i64);

/// Revision `revision`, else the latest one computed at or before `as_of`
/// (`computed_unix_ms`, §13), else the latest; `None` before any reprice (or
/// before `as_of`).
pub(super) fn header(db: &Connection, revision: Option<i64>, as_of: Option<i64>) -> Result<Option<Header>> {
    if !table(db, "valuation_revisions")? {
        return Ok(None);
    }
    let latest: Option<i64> = match as_of {
        Some(as_of) => db.query_row("SELECT max(revision) FROM valuation_revisions WHERE computed_unix_ms<=?1", [as_of], |r| r.get(0))?,
        None => db.query_row("SELECT max(revision) FROM valuation_revisions", [], |r| r.get(0))?,
    };
    let Some(latest) = latest else { return Ok(None) };
    let revision = revision.unwrap_or(latest);
    let Some((basis, policy, synced, computed)) = db.query_row("SELECT basis,policy,ledger_synced_unix_ms,computed_unix_ms FROM valuation_revisions WHERE revision=?1",
        [revision], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))).optional()? else {
        bail!("valuation revision {revision} does not exist (latest is {latest})");
    };
    Ok(Some((revision, basis, policy, synced, computed)))
}

/// A stored revision (the latest by default) per attempt and session, as JSON.
/// A delta revision is replayed onto the full copy below it, so every revision
/// reads back byte-identical to when it was appended. Read-only.
///
/// `as_of` (§13) selects the latest revision computed at or before that
/// instant; the output is byte-identical to `--revision` of that revision.
pub fn cost(db: &Connection, revision: Option<i64>, as_of: Option<i64>) -> Result<Value> {
    let Some((revision, basis, policy, synced, computed)) = header(db, revision, as_of)? else {
        return Ok(super::unavailable(if as_of.is_some() && header(db, None, None)?.is_some() { "not_priced_as_of" } else { "not_priced" }));
    };
    let mut rows: Vec<Stored> = stored_at(db, revision)?.into_values().collect();
    rows.sort_by(|a, b| (&a.session_id, &a.entry_id).cmp(&(&b.session_id, &b.entry_id)));
    let mut sessions = BTreeMap::<String, (String, Option<String>, Vec<Value>)>::new();
    for s in &rows {
        let mut interval = s.from.map(|from| json!({"from_unix_ms": from, "to_unix_ms": s.to}));
        // A4 fields appear only on revisions that recorded them, so earlier ones read back unchanged.
        if let (Some(interval), Some(basis)) = (interval.as_mut(), &s.usage_basis) { interval["basis"] = json!(basis); }
        let mut entry = json!({"entry_id": s.entry_id, "model": s.model,
            "usage_interval": interval,
            "quantities": {"new_input_tokens": s.q[0], "cache_read_tokens": s.q[1],
                "cache_write_tokens": s.q[2], "output_tokens": s.q[3]},
            "valuation": valuation(s, &basis)?});
        if let Some(check) = &s.provider_check { entry["provider_check"] = json!(check); }
        sessions
            .entry(s.session_id.clone())
            .or_insert((s.role.clone(), s.attempt_id.clone(), Vec::new()))
            .2
            .push(entry);
    }
    let mut attempts = BTreeMap::<Option<String>, (Vec<&Value>, Vec<&Value>)>::new();
    let mut out = Vec::new();
    for (session, (role, attempt, entries)) in &sessions {
        let slot = attempts.entry(attempt.clone()).or_default();
        if role == "primary" {
            slot.0.extend(entries)
        } else {
            slot.1.extend(entries)
        }
        let (estimate, coverage) = summarize(&entries.iter().collect::<Vec<_>>())?;
        let cards: BTreeSet<String> = entries
            .iter()
            .filter_map(|e| e["valuation"]["rate_card"].as_object())
            .map(|c| {
                format!(
                    "{}@{}",
                    c["card_id"].as_str().unwrap_or_default(),
                    c["version"]
                )
            })
            .collect();
        out.push(json!({"session_id": session, "role": role, "attempt_id": attempt, "estimate": estimate, "coverage": coverage,
            "rate_cards": cards, "entries": entries}));
    }
    // Guardian and subagent sessions have no native parent evidence (§3):
    // their estimate stays apart from the attempt's primary sessions.
    let mut rollups = Vec::new();
    for (attempt, (primary, children)) in &attempts {
        let (estimate, coverage) = summarize(primary)?;
        let children = if children.is_empty() {
            Value::Null
        } else {
            let (estimate, coverage) = summarize(children)?;
            json!({"estimate": estimate, "coverage": coverage})
        };
        rollups.push(json!({"attempt_id": attempt, "estimate": estimate, "coverage": coverage, "unlinked_children": children}));
    }
    Ok(
        json!({"revision": revision, "basis": basis, "policy": policy, "ledger_synced_unix_ms": synced,
        "computed_unix_ms": computed, "attempts": rollups, "sessions": out}),
    )
}

const NAMES: [(&str, &str); 2] = [("M12", "repriced_estimated_spend"), ("M14", "cost_coverage")];
const FIXTURE_RATES: &str = "rate cards are fixture-only by owner decision: the only cards in the repository are invented synthetic test fixtures and no real \
    prices ship, so an estimate is only as real as the cards imported; never a provider charge and never added to one (M11)";

fn metric(id: &str, mut body: Value) -> Value {
    body["definition"] = json!(format!("{id}.cost-v1"));
    body["name"] = json!(NAMES.iter().find(|(n, _)| *n == id).map_or("", |(_, name)| *name));
    body
}

fn unavailable_metrics(reason: &str) -> BTreeMap<String, Value> {
    NAMES.iter().map(|(id, _)| (id.to_string(), metric(id, json!({"value": super::unavailable(reason), "basis": BASIS})))).collect()
}

/// M12 and M14 (§12) for `telemetry <slug> report`, from the latest stored
/// revision: every valued delta entry of sessions started in the window. M12
/// follows `cost`: complete only when every entry is priced in one currency,
/// else `partial` (never the total) or `unavailable`; M14 counts priced entries
/// of the entries valued. Unknown is never 0.
pub fn metrics(project: &Path, since: Option<i64>) -> Result<BTreeMap<String, Value>> {
    let Some(db) = crate::telemetry::sidecar::read(project)? else { return Ok(unavailable_metrics("collection_not_run")) };
    metrics_db(&db, since, true)
}

pub(crate) fn metrics_with(project: &Path, since: Option<i64>, aggregates: bool) -> Result<BTreeMap<String, Value>> {
    let Some(db) = crate::telemetry::sidecar::read(project)? else { return Ok(unavailable_metrics("collection_not_run")) };
    metrics_db(&db, since, aggregates)
}

fn metrics_db(db: &Connection, since: Option<i64>, aggregates: bool) -> Result<BTreeMap<String, Value>> {
    let Some((revision, basis, policy, synced, _)) = header(db, None, None)? else { return Ok(unavailable_metrics("not_priced")) };
    if aggregates && since.is_none() && table(db, "accounting_cost_aggregates")? {
        let body: Option<String> = db.query_row("SELECT body FROM accounting_cost_aggregates WHERE revision=?1", [revision], |r| r.get(0)).optional()?;
        if let Some(body) = body { return Ok(serde_json::from_str(&body)?); }
    }
    let starts: BTreeMap<String, Option<i64>> = db.prepare("SELECT session_id,min(session_unix_ms) FROM rollout_sources GROUP BY session_id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut entries = Vec::new();
    for s in stored_at(db, revision)?.values() {
        if since.is_some_and(|since| starts.get(&s.session_id).copied().flatten().is_none_or(|at| at < since)) { continue; }
        entries.push(json!({"valuation": valuation(s, &basis)?}));
    }
    let (estimate, coverage) = summarize(&entries.iter().collect::<Vec<_>>())?;
    let common = json!({"basis": basis, "revision": revision, "policy": policy, "ledger_synced_unix_ms": synced, "rate_cards": "fixture_only",
        "caveat": FIXTURE_RATES, "scope": "every valued delta entry of the latest revision (sessions started in the window)"});
    let mut m12 = common.clone();
    m12["value"] = match estimate["status"].as_str() {
        Some("complete") => { m12["currency"] = estimate["currency"].clone(); estimate["amount"].clone() }
        _ => estimate.clone(),
    };
    m12["estimate"] = estimate;
    m12["coverage"] = coverage.clone();
    m12["never_added_to"] = json!("M11");
    let (priced, total) = (coverage["priced"].as_u64().unwrap_or(0), coverage["entries"].as_u64().unwrap_or(0));
    let mut m14 = common;
    m14["numerator"] = json!(priced);
    m14["denominator"] = json!(total);
    if total == 0 { m14["value"] = Value::Null; m14["reason"] = json!("empty_denominator"); } else { m14["value"] = json!(format!("{priced}/{total}")); }
    m14["unpriced"] = coverage["unpriced"].clone();
    m14["unit"] = json!("entries");
    m14["detail"] = json!("count coverage of valued invocations (delta entries), not a share of monetary value: an unpriced entry may be the expensive one");
    Ok(BTreeMap::from([("M12".to_owned(), metric("M12", m12)), ("M14".to_owned(), metric("M14", m14))]))
}

pub(crate) fn store_metrics(db: &Connection) -> Result<()> {
    let Some((revision, ..)) = header(db, None, None)? else { return Ok(()); };
    let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM accounting_cost_aggregates WHERE revision=?1)", [revision], |r| r.get(0))?;
    if !exists {
        let body = metrics_db(db, None, false)?;
        db.execute("INSERT INTO accounting_cost_aggregates(revision,body) VALUES(?1,?2)", params![revision, serde_json::to_string(&body)?])?;
    }
    Ok(())
}

fn text_estimate(estimate: &Value, coverage: &Value) -> String {
    let amount = |key: &str| {
        Dec::parse(estimate[key].as_str().unwrap_or("0"))
            .map(|d| d.rounded(TEXT_PLACES))
            .unwrap_or_default()
    };
    let head = match estimate["status"].as_str() {
        Some("complete") => format!(
            "{} {}",
            estimate["currency"].as_str().unwrap_or_default(),
            amount("amount")
        ),
        Some("partial") => format!(
            "partial {} {} priced",
            estimate["currency"].as_str().unwrap_or_default(),
            amount("priced_amount")
        ),
        _ => match estimate["priced_by_currency"].as_object() {
            Some(by) => format!(
                "unavailable (mixed_currency: {})",
                by.iter()
                    .map(|(c, a)| format!(
                        "{c} {}",
                        Dec::parse(a.as_str().unwrap_or("0"))
                            .map(|d| d.rounded(TEXT_PLACES))
                            .unwrap_or_default()
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            None => format!(
                "unavailable ({})",
                estimate["reason"].as_str().unwrap_or_default()
            ),
        },
    };
    let unpriced = coverage["unpriced"]
        .as_object()
        .map(|u| {
            u.iter()
                .map(|(r, n)| format!("{r} {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let unpriced = if unpriced.is_empty() {
        String::new()
    } else {
        format!("; unpriced: {unpriced}")
    };
    format!(
        "{head} ({} of {} entries priced{unpriced})",
        coverage["priced"], coverage["entries"]
    )
}

/// The text view of `cost`: amounts rounded half-up to 6 places (the only rounding).
pub fn text(value: &Value) -> String {
    if value["status"] == "unavailable" {
        return format!(
            "cost: unavailable ({})\n",
            value["reason"].as_str().unwrap_or_default()
        );
    }
    let mut out = format!(
        "cost revision {}: {} (never a provider charge); {}\n",
        value["revision"],
        value["basis"].as_str().unwrap_or_default(),
        value["policy"].as_str().unwrap_or_default()
    );
    for attempt in value["attempts"].as_array().into_iter().flatten() {
        out += &format!(
            "attempt {}: {}\n",
            attempt["attempt_id"].as_str().unwrap_or("(unattributed)"),
            text_estimate(&attempt["estimate"], &attempt["coverage"])
        );
        if attempt["unlinked_children"].is_object() {
            out += &format!(
                "  unlinked children: {}\n",
                text_estimate(
                    &attempt["unlinked_children"]["estimate"],
                    &attempt["unlinked_children"]["coverage"]
                )
            );
        }
        for session in value["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s["attempt_id"] == attempt["attempt_id"])
        {
            let cards = session["rate_cards"]
                .as_array()
                .map(|c| {
                    c.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            out += &format!(
                "  session {} {}: {}{}\n",
                session["session_id"].as_str().unwrap_or_default(),
                session["role"].as_str().unwrap_or_default(),
                text_estimate(&session["estimate"], &session["coverage"]),
                if cards.is_empty() {
                    String::new()
                } else {
                    format!(" cards {cards}")
                }
            );
        }
    }
    out
}
