//! TM4.4 estimators (docs/telemetry/contracts-evaluation.md §2): the declared
//! task-clustered percentile bootstrap (card C5's `percentile_bootstrap.v1`
//! generator and rank rule, extended to per-arm ratios and two-sample
//! differences), exact fraction arithmetic and fixed-point decimals. No float
//! enters an estimate.
use serde_json::{Value, json};
use std::cmp::Ordering;

pub fn unavailable(reason: &str) -> Value { json!({"status": "unavailable", "reason": reason}) }

/// SplitMix64, the declared bootstrap generator of `percentile_bootstrap.v1`
/// (contracts-quality.md §4): `state += 0x9e3779b97f4a7c15`, then the two
/// xor-shift multiplies, wrapping.
pub struct SplitMix64(pub u64);

impl SplitMix64 {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    /// Uniform in `0..k` by rejection (no modulo bias).
    pub fn below(&mut self, k: u64) -> u64 {
        let limit = u64::MAX - u64::MAX % k;
        loop { let x = self.next_u64(); if x < limit { return x % k; } }
    }
}

pub fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 { (a, b) = (b, a % b); }
    a
}

/// `n/d` reduced (`d > 0`), `"n"` when the denominator reduces to 1.
pub fn reduced(n: i128, d: i128) -> String {
    let g = gcd(n.unsigned_abs(), d.unsigned_abs()).max(1) as i128;
    let (n, d) = (n / g, d / g);
    if d == 1 { n.to_string() } else { format!("{n}/{d}") }
}

/// `n/d` (`d > 0`) to `places` decimals, rounded half away from zero, in integer arithmetic.
pub fn decimal(n: i128, d: i128, places: u32) -> String {
    let scale = 10u128.pow(places);
    let scaled = (2 * n.unsigned_abs() * scale + d.unsigned_abs()) / (2 * d.unsigned_abs());
    let sign = if n < 0 && scaled > 0 { "-" } else { "" };
    if places == 0 { return format!("{sign}{scaled}"); }
    format!("{sign}{}.{:0width$}", scaled / scale, scaled % scale, width = places as usize)
}

/// Order of two fractions with positive denominators; a zero denominator is
/// `+∞` (an undefined ratio sorts after every finite one). Ties by denominator.
pub fn compare(a: (i128, i128), b: (i128, i128)) -> Ordering {
    match (a.1 == 0, b.1 == 0) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => (a.0 * b.1).cmp(&(b.0 * a.1)).then(a.1.cmp(&b.1)),
    }
}

/// A declared percentile bootstrap: `B` iterations from `seed`, level in permille.
#[derive(Clone, Copy, Debug)]
pub struct Bootstrap { pub method: &'static str, pub iterations: u32, pub seed: u64, pub level_permille: u32 }

impl Bootstrap {
    /// Nearest-rank index (0-based) of `permille`: `ceil(B × permille / 1000) − 1`.
    fn rank(&self, permille: u64) -> usize { (u64::from(self.iterations) * permille).div_ceil(1000) as usize - 1 }
    fn tails(&self) -> (u64, u64) { let tail = u64::from(1000 - self.level_permille) / 2; (tail, 1000 - tail) }
    pub fn seed_hex(&self) -> String { format!("{:#018x}", self.seed) }
    pub fn level(&self) -> String { format!("0.{}", self.level_permille / 10) }
}

/// Exact bounds of an interval, `(numerator, denominator)`; a zero denominator is unbounded.
#[derive(Clone, Copy, Debug)]
pub struct Bounds { pub lower: (i128, i128), pub upper: (i128, i128) }

fn bound(b: (i128, i128), places: u32) -> (Value, Value) {
    if b.1 == 0 { (json!("unbounded"), Value::Null) } else { (json!(format!("{}/{}", b.0, b.1)), json!(decimal(b.0, b.1, places))) }
}

fn sort(draws: &mut [(i128, i128)]) { draws.sort_by(|a, b| compare(*a, *b)); }

/// Task-clustered ratio interval over `clusters`, one `(Σ numerator, Σ
/// denominator)` per task in task-ID order: each of `B` iterations draws `k`
/// tasks with replacement (SplitMix64 from the seed, restarted per call) and
/// yields `ΣN/ΣD`; the draws are sorted exactly (cross-multiplied, a zero
/// denominator last) and the nearest-rank percentiles are reported.
pub fn ratio_interval(clusters: &[(i64, i64)], spec: &Bootstrap, source: &Value) -> (Value, Option<Bounds>) {
    if clusters.len() < 2 { return (unavailable("single_task"), None); }
    let mut rng = SplitMix64(spec.seed);
    let k = clusters.len() as u64;
    let mut draws: Vec<(i128, i128)> = (0..spec.iterations).map(|_| (0..k).fold((0i128, 0i128), |(n, d), _| {
        let (x, y) = clusters[rng.below(k) as usize];
        (n + i128::from(x), d + i128::from(y))
    })).collect();
    sort(&mut draws);
    let (lo, hi) = spec.tails();
    let bounds = Bounds { lower: draws[spec.rank(lo)], upper: draws[spec.rank(hi)] };
    let ((lower, lower_decimal), (upper, upper_decimal)) = (bound(bounds.lower, 4), bound(bounds.upper, 4));
    (json!({"method": spec.method, "resample": "task", "clusters": k, "iterations": spec.iterations, "seed": spec.seed_hex(), "level": spec.level(),
        "lower": lower, "upper": upper, "lower_decimal": lower_decimal, "upper_decimal": upper_decimal, "source": source}), Some(bounds))
}

/// Two-sample difference interval `Y₁/N₁ − Y₀/N₀`, stratified by arm: each
/// iteration draws `k₀` reference clusters, then `k₁` treatment clusters
/// (one SplitMix64 stream from the seed), each cluster `(Σ outcome, units)`
/// of one task. The difference `(Y₁N₀ − Y₀N₁)/(N₁N₀)` is exact.
pub fn difference_interval(reference: &[(i64, i64)], treatment: &[(i64, i64)], spec: &Bootstrap, source: &Value) -> Value {
    if reference.len() < 2 || treatment.len() < 2 { return unavailable("single_task"); }
    let mut rng = SplitMix64(spec.seed);
    let mut draw = |clusters: &[(i64, i64)]| {
        let k = clusters.len() as u64;
        (0..k).fold((0i128, 0i128), |(y, n), _| { let (a, b) = clusters[rng.below(k) as usize]; (y + i128::from(a), n + i128::from(b)) })
    };
    let mut draws: Vec<(i128, i128)> = (0..spec.iterations).map(|_| {
        let (y0, n0) = draw(reference);
        let (y1, n1) = draw(treatment);
        (y1 * n0 - y0 * n1, n1 * n0)
    }).collect();
    sort(&mut draws);
    let (lo, hi) = spec.tails();
    let (lower, upper) = (draws[spec.rank(lo)], draws[spec.rank(hi)]);
    json!({"method": spec.method, "resample": "task_within_arm", "clusters": [reference.len(), treatment.len()], "iterations": spec.iterations,
        "seed": spec.seed_hex(), "level": spec.level(), "lower": reduced(lower.0, lower.1), "upper": reduced(upper.0, upper.1),
        "lower_decimal": decimal(lower.0, lower.1, 4), "upper_decimal": decimal(upper.0, upper.1, 4), "source": source})
}

/// Fixed-point scale of the planning arithmetic: values are integers in units of 10⁻⁹.
pub const SCALE: u128 = 1_000_000_000;

/// Parse a decimal in `[0, 1]`-style notation (`0.05`, `.8`, `1`) with at most
/// nine places into units of 10⁻⁹. `None` for anything else.
pub fn parse_fixed(text: &str) -> Option<u128> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if (whole.is_empty() && fraction.is_empty()) || fraction.len() > 9 || !whole.chars().chain(fraction.chars()).all(|c| c.is_ascii_digit()) { return None; }
    let whole: u128 = if whole.is_empty() { 0 } else { whole.parse().ok()? };
    let fraction: u128 = if fraction.is_empty() { 0 } else { fraction.parse::<u128>().ok()? * 10u128.pow(9 - fraction.len() as u32) };
    whole.checked_mul(SCALE)?.checked_add(fraction)
}

/// A fixed-point value (units of 10⁻⁹) as a decimal with nine places.
pub fn fixed(value: u128) -> String { format!("{}.{:09}", value / SCALE, value % SCALE) }

/// Integer square root `floor(√n)` by Newton's method on integers.
pub fn isqrt(n: u128) -> u128 {
    if n < 2 { return n; }
    let (mut x, mut y) = (n, n.div_ceil(2));
    while y < x { x = y; y = (x + n / x) / 2; }
    x
}
