//! Propensity-logged assignment policies (TM4.7,
//! docs/telemetry/contracts-evaluation.md §9). Pure functions of the
//! operator-approved eligible arms, their quota headroom, their per-arm
//! assignment counts (cost constraints) and their recorded outcomes, plus a
//! seed: they return exact per-arm probabilities in ppm (summing to
//! 1_000_000) and, from one seeded draw, the chosen arm. A policy only ever
//! chooses among the arms it is given; it never builds launch inputs,
//! approvals, contracts or review/verification policy.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const PPM: u32 = 1_000_000;
pub const DETERMINISTIC: &str = "deterministic.v1";
pub const UNIFORM: &str = "uniform.v1";
pub const EPSILON: &str = "epsilon.v1";
pub const THOMPSON: &str = "thompson.v1";
pub const POLICIES: [&str; 4] = [DETERMINISTIC, UNIFORM, EPSILON, THOMPSON];
/// Thompson choice probabilities are the win shares of this many seeded posterior draws.
pub const THOMPSON_SIMULATIONS: u32 = 1000;
/// Recorded outcomes per arm beyond this are scaled down (successes rounded down) to bound a draw.
pub const THOMPSON_MAX_OUTCOMES: u64 = 200;
pub const THOMPSON_DEFAULT_PRIOR: [u32; 2] = [1, 1];
pub const THOMPSON_DEFAULT_FLOOR_PPM: u32 = 10_000;
pub const SEED_DOMAIN: &str = "assignment-seed.v1";
/// Constraint reasons: a capped or quota-exhausted arm keeps probability 0.
pub const ARM_CAP_REACHED: &str = "arm_cap_reached";
pub const QUOTA_EXHAUSTED: &str = "quota_exhausted";
pub const NO_ARM_WITHIN_CONSTRAINTS: &str = "no_arm_within_constraints";

/// One versioned policy with its parameters. Canonical bytes are the serde
/// JSON of a normalized spec (defaults filled); its digest is the identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicySpec {
    pub policy: String,
    /// `epsilon.v1`: the exploration share, spread uniformly over the allowed arms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epsilon_ppm: Option<u32>,
    /// `thompson.v1`: Beta prior `[alpha, beta]` (integers 1–100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior: Option<[u32; 2]>,
    /// `thompson.v1`: minimum probability of every allowed arm (positivity for weighted estimates).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor_ppm: Option<u32>,
    /// Per-arm budget caps: at most this many decisions may choose the arm
    /// under one settings revision; a capped arm has probability 0.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub arm_caps: BTreeMap<String, u32>,
    /// An arm whose known remaining quota (percent) is at or below this has probability 0 (default 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_headroom_percent: Option<u32>,
}

fn configuration_ref(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

impl PolicySpec {
    /// Parse and normalize one spec (JSON object, or a bare policy name).
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let spec: Self = if text.starts_with('{') {
            serde_json::from_str(text).map_err(|e| format!("invalid policy spec: {e}"))?
        } else {
            Self { policy: text.to_owned(), epsilon_ppm: None, prior: None, floor_ppm: None, arm_caps: BTreeMap::new(), min_headroom_percent: None }
        };
        spec.normalized()
    }

    /// Validate and fill defaults, so equal policies have equal bytes.
    pub fn normalized(mut self) -> Result<Self, String> {
        if !POLICIES.contains(&self.policy.as_str()) { return Err(format!("unknown policy {}; one of {}", self.policy, POLICIES.join(", "))); }
        match (self.policy.as_str(), self.epsilon_ppm) {
            (EPSILON, None) => return Err("epsilon.v1 requires epsilon_ppm".into()),
            (EPSILON, Some(e)) if e > PPM => return Err("epsilon_ppm is at most 1000000".into()),
            (EPSILON, _) => {}
            (_, Some(_)) => return Err("epsilon_ppm applies to epsilon.v1 only".into()),
            _ => {}
        }
        if self.policy == THOMPSON {
            let prior = *self.prior.get_or_insert(THOMPSON_DEFAULT_PRIOR);
            if prior.iter().any(|v| !(1..=100).contains(v)) { return Err("thompson prior values are integers 1 to 100".into()); }
            if *self.floor_ppm.get_or_insert(THOMPSON_DEFAULT_FLOOR_PPM) > 100_000 { return Err("floor_ppm is at most 100000".into()); }
        } else if self.prior.is_some() || self.floor_ppm.is_some() {
            return Err("prior and floor_ppm apply to thompson.v1 only".into());
        }
        if self.arm_caps.len() > 64 || !self.arm_caps.keys().all(|k| configuration_ref(k)) {
            return Err("arm_caps maps at most 64 configuration ids (sha256:<hex64>) to caps".into());
        }
        if self.min_headroom_percent.is_some_and(|p| p > 100) { return Err("min_headroom_percent is at most 100".into()); }
        Ok(self)
    }

    pub fn canonical_json(&self) -> String { serde_json::to_string(self).unwrap_or_default() }
    pub fn digest(&self) -> String { format!("sha256:{:x}", Sha256::digest(self.canonical_json().as_bytes())) }
    /// Stochastic policies choose by the seeded draw; `deterministic.v1` does not.
    pub fn stochastic(&self) -> bool { self.policy != DETERMINISTIC }

    /// The probabilities and choice over `arms` (in evaluation order) for `seed`.
    pub fn evaluate(&self, arms: &[ArmInput], seed: u64) -> PolicyEvaluation {
        let mut rng = SplitMix64(seed);
        // The first output is the choice draw, recorded whether or not it decides.
        let draw_ppm = u32::try_from(rng.next() % u64::from(PPM)).unwrap_or(0);
        let mut excluded = Vec::new();
        let mut allowed = Vec::new();
        for (index, arm) in arms.iter().enumerate() {
            if self.arm_caps.get(&arm.configuration_id).is_some_and(|cap| arm.assigned >= u64::from(*cap)) {
                excluded.push((index, ARM_CAP_REACHED));
            } else if arm.headroom_milli.is_some_and(|h| h <= i64::from(self.min_headroom_percent.unwrap_or(0)) * 1000) {
                excluded.push((index, QUOTA_EXHAUSTED));
            } else {
                allowed.push(index);
            }
        }
        let mut probabilities = vec![0u32; arms.len()];
        let mut greedy = None;
        let mut wins = None;
        if allowed.is_empty() {
            return PolicyEvaluation { policy: self.policy.clone(), policy_digest: self.digest(), seed, draw_ppm, probabilities, chosen: None, excluded,
                abstained: Some(NO_ARM_WITHIN_CONSTRAINTS), greedy, thompson_wins: wins };
        }
        let shares: Vec<u32> = match self.policy.as_str() {
            DETERMINISTIC => { let mut s = vec![0; allowed.len()]; s[0] = PPM; s }
            UNIFORM => split(PPM, allowed.len()),
            EPSILON => {
                let epsilon = self.epsilon_ppm.unwrap_or(0);
                let best = best_mean(arms, &allowed);
                greedy = Some(allowed[best]);
                let mut s = split(epsilon, allowed.len());
                s[best] += PPM - epsilon;
                s
            }
            _ => {
                let prior = self.prior.unwrap_or(THOMPSON_DEFAULT_PRIOR);
                let won = thompson_wins(arms, &allowed, prior, &mut rng);
                let floor = self.floor_ppm.unwrap_or(0).min(PPM / allowed.len() as u32);
                let rest = PPM - floor * allowed.len() as u32;
                let s = apportion(rest, &won).into_iter().map(|p| p + floor).collect();
                wins = Some(allowed.iter().zip(&won).map(|(i, w)| (*i, *w)).collect());
                s
            }
        };
        for (index, share) in allowed.iter().zip(shares) { probabilities[*index] = share; }
        let chosen = choose(&probabilities, draw_ppm);
        PolicyEvaluation { policy: self.policy.clone(), policy_digest: self.digest(), seed, draw_ppm, probabilities, chosen, excluded, abstained: None, greedy, thompson_wins: wins }
    }
}

/// One operator-approved arm as a policy sees it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArmInput {
    pub configuration_id: String,
    /// Known remaining quota in thousandths of a percent; `None` when unknown (never 0).
    pub headroom_milli: Option<i64>,
    /// Decisions that chose this arm under the current settings revision (per-arm caps).
    pub assigned: u64,
    /// Recorded terminal outcomes of tasks on this arm in the task's class.
    pub successes: u64,
    pub failures: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyEvaluation {
    pub policy: String,
    pub policy_digest: String,
    pub seed: u64,
    pub draw_ppm: u32,
    /// Per arm, in the arms' order; sums to 1_000_000 unless abstained (all 0).
    pub probabilities: Vec<u32>,
    pub chosen: Option<usize>,
    pub excluded: Vec<(usize, &'static str)>,
    pub abstained: Option<&'static str>,
    /// `epsilon.v1`: the greedy arm (highest posterior mean, earliest on ties).
    pub greedy: Option<usize>,
    /// `thompson.v1`: `(arm, wins)` of the posterior draws.
    pub thompson_wins: Option<Vec<(usize, u32)>>,
}

impl PolicyEvaluation {
    pub fn seed_hex(&self) -> String { format!("{:016x}", self.seed) }
}

/// The seed of one policy's decision for one task revision: the first eight
/// bytes (big-endian) of `sha256("assignment-seed.v1\0<policy digest>\0<task>\0<task revision>")`.
pub fn policy_seed(policy_digest: &str, task: &str, task_revision: u64) -> u64 {
    let digest = Sha256::digest(format!("{SEED_DOMAIN}\0{policy_digest}\0{task}\0{task_revision}").as_bytes());
    u64::from_be_bytes(digest[..8].try_into().unwrap_or([0; 8]))
}

/// The first arm whose cumulative probability exceeds the draw.
fn choose(probabilities: &[u32], draw: u32) -> Option<usize> {
    let mut cumulative = 0u32;
    for (index, p) in probabilities.iter().enumerate() {
        cumulative += p;
        if cumulative > draw { return Some(index); }
    }
    None
}

/// `total` over `n` parts: `total / n` each, the remainder one each to the first parts.
fn split(total: u32, n: usize) -> Vec<u32> {
    let n32 = n as u32;
    (0..n32).map(|i| total / n32 + u32::from(i < total % n32)).collect()
}

/// Largest-remainder apportionment of `total` by `weights` (earlier parts win ties); equal split when all weights are 0.
fn apportion(total: u32, weights: &[u32]) -> Vec<u32> {
    let sum: u64 = weights.iter().map(|w| u64::from(*w)).sum();
    if sum == 0 { return split(total, weights.len()); }
    let mut parts: Vec<u32> = weights.iter().map(|w| (u64::from(total) * u64::from(*w) / sum) as u32).collect();
    let mut order: Vec<usize> = (0..weights.len()).collect();
    order.sort_by_key(|i| (std::cmp::Reverse(u64::from(total) * u64::from(weights[*i]) % sum), *i));
    let left = total - parts.iter().sum::<u32>();
    for i in order.into_iter().take(left as usize) { parts[i] += 1; }
    parts
}

/// Position in `allowed` of the highest posterior mean `(s + 1) / (s + f + 2)`, earliest on ties.
fn best_mean(arms: &[ArmInput], allowed: &[usize]) -> usize {
    let mean = |i: usize| (u128::from(arms[i].successes) + 1, u128::from(arms[i].successes) + u128::from(arms[i].failures) + 2);
    let mut best = 0;
    for (position, index) in allowed.iter().enumerate().skip(1) {
        let (a, b) = (mean(*index), mean(allowed[best]));
        if a.0 * b.1 > b.0 * a.1 { best = position; }
    }
    best
}

/// Counts scaled to at most `THOMPSON_MAX_OUTCOMES` (successes rounded down).
fn scaled(successes: u64, failures: u64) -> (u64, u64) {
    let n = successes + failures;
    if n <= THOMPSON_MAX_OUTCOMES { return (successes, failures); }
    let s = u64::try_from(u128::from(successes) * u128::from(THOMPSON_MAX_OUTCOMES) / u128::from(n)).unwrap_or(0);
    (s, THOMPSON_MAX_OUTCOMES - s)
}

/// Win counts of each allowed arm over the seeded posterior draws. A
/// Beta(a, b) draw with integer parameters is the a-th smallest of
/// a + b − 1 uniform 64-bit integers (integer-only, platform-independent);
/// ties go to the earlier arm.
fn thompson_wins(arms: &[ArmInput], allowed: &[usize], prior: [u32; 2], rng: &mut SplitMix64) -> Vec<u32> {
    let params: Vec<(usize, usize)> = allowed.iter().map(|i| {
        let (s, f) = scaled(arms[*i].successes, arms[*i].failures);
        ((u64::from(prior[0]) + s) as usize, (u64::from(prior[1]) + f) as usize)
    }).collect();
    let mut wins = vec![0u32; allowed.len()];
    let mut buffer = Vec::new();
    for _ in 0..THOMPSON_SIMULATIONS {
        let mut best: Option<(usize, u64)> = None;
        for (position, (a, b)) in params.iter().enumerate() {
            buffer.clear();
            buffer.extend((0..a + b - 1).map(|_| rng.next()));
            let (_, value, _) = buffer.select_nth_unstable(a - 1);
            let value = *value;
            if best.is_none_or(|(_, v)| value > v) { best = Some((position, value)); }
        }
        if let Some((position, _)) = best { wins[position] += 1; }
    }
    wins
}

/// SplitMix64 (Steele, Lea, Flood 2014).
struct SplitMix64(u64);
impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// A policy decision written with the canonical dispatch decision it made
/// (`DispatchContext::Assigned`). Descriptive and checked again in the
/// reservation transaction: the settings revision must still be the current
/// `assign` revision under the same grant, the chosen arm within its cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyAssignment {
    pub settings_revision: u64,
    pub spec: PolicySpec,
    pub grant_id: String,
    pub seed: u64,
    pub draw_ppm: u32,
    /// Per eligible entry, in the eligible order.
    pub probabilities: Vec<u32>,
    /// `(configuration_id, reason)` of approved arms held at probability 0.
    pub constraints: Vec<(String, &'static str)>,
}
