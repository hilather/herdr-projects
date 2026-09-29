//! Lane C metric registry `registry.v1` (contracts-quality.md §5): declared,
//! read-only minimum samples and the M42 interval estimator. A change here is
//! a new registry version, never an edit in place.
use serde_json::{Value, json};

pub const VERSION: &str = "registry.v1";

/// One declared minimum sample: below `value` units a cell is
/// `unavailable: insufficient_data` with its counts (plan doc 07 §6).
pub struct MinSample { pub metric: &'static str, pub value: u32, pub unit: &'static str }

pub const MIN_SAMPLES: [MinSample; 2] = [
    MinSample { metric: "M41", value: 10, unit: "closed_groups" },
    MinSample { metric: "M42", value: 10, unit: "closed_groups_containing_both" },
];

/// `(value in effect, {value, unit, source})`: the registry's minimum, or an
/// operator's `--min-groups` labelled `override` beside the registry value.
pub fn min_sample_json(metric: &str, override_value: Option<u32>) -> (u32, Value) {
    let entry = MIN_SAMPLES.iter().find(|m| m.metric == metric).unwrap_or_else(|| panic!("{metric} has no registry minimum"));
    match override_value {
        None => (entry.value, json!({"value": entry.value, "unit": entry.unit, "source": VERSION})),
        Some(value) => (value, json!({"value": value, "unit": entry.unit, "source": "override", "registry": {"value": entry.value, "source": VERSION}})),
    }
}

/// M42 interval: percentile bootstrap over whole tasks
/// (`percentile_bootstrap.v1`, contracts-quality.md §4).
pub struct Bootstrap { pub method: &'static str, pub iterations: u32, pub seed: u64, pub level_permille: u32 }

pub const M42_BOOTSTRAP: Bootstrap = Bootstrap { method: "percentile_bootstrap.v1", iterations: 1000, seed: 0x4d34_325f_626f_6f74, level_permille: 950 };
