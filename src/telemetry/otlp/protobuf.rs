//! Bounded OTLP wire decoding into the JSON collector's normalization input.
use anyhow::{Context, Result, ensure};
use serde_json::{Map, Value, json};

#[derive(Clone, Copy)]
enum Kind {
    Message(&'static str),
    String,
    Int,
    Bool,
    Double,
    FixedInt,
    Enum,
    Unsupported,
}
use Kind::*;

fn field(message: &str, n: u64) -> Option<(&'static str, Kind, bool)> {
    Some(match (message, n) {
        ("metrics", 1) => ("resourceMetrics", Message("rm"), true),
        ("logs", 1) => ("resourceLogs", Message("rl"), true),
        ("rm" | "rl", 1) => ("resource", Message("resource"), false),
        ("rm", 2) => ("scopeMetrics", Message("sm"), true),
        ("rl", 2) => ("scopeLogs", Message("sl"), true),
        ("resource", 1) => ("attributes", Message("kv"), true),
        ("sm", 2) => ("metrics", Message("metric"), true),
        ("sl", 2) => ("logRecords", Message("log"), true),
        ("metric", 1) => ("name", String, false),
        ("metric", 3) => ("unit", String, false),
        ("metric", 5) => ("gauge", Message("gauge"), false),
        ("metric", 7) => ("sum", Message("sum"), false),
        ("metric", 9) => ("histogram", Message("histogram"), false),
        ("metric", 10) => ("exponentialHistogram", Unsupported, false),
        ("metric", 11) => ("summary", Unsupported, false),
        ("sum" | "gauge", 1) => ("dataPoints", Message("point"), true),
        ("histogram", 1) => ("dataPoints", Message("hp"), true),
        ("sum" | "histogram", 2) => ("aggregationTemporality", Enum, false),
        ("point", 7) | ("hp", 9) | ("log", 6) => ("attributes", Message("kv"), true),
        ("point" | "hp", 2) => ("startTimeUnixNano", FixedInt, false),
        ("point" | "hp", 3) | ("log", 1) => ("timeUnixNano", FixedInt, false),
        ("point", 4) => ("asDouble", Double, false),
        ("point", 6) => ("asInt", FixedInt, false),
        ("hp", 4) => ("count", FixedInt, false),
        ("hp", 5) => ("sum", Double, false),
        ("log", 2) => ("severityNumber", Enum, false),
        ("log", 3) => ("severityText", String, false),
        // Bodies are never inspected, even for event names. Use eventName or event.name.
        ("log", 5) => ("body", Message("any"), false),
        ("log", 11) => ("observedTimeUnixNano", FixedInt, false),
        ("log", 12) => ("eventName", String, false),
        ("kv", 1) => ("key", String, false),
        ("kv", 2) => ("value", Message("any"), false),
        ("any", 1) => ("stringValue", String, false),
        ("any", 2) => ("boolValue", Bool, false),
        ("any", 3) => ("intValue", Int, false),
        ("any", 4) => ("doubleValue", Double, false),
        ("any", 5) => ("arrayValue", Message("array"), false),
        ("any", 6) => ("kvlistValue", Message("kvlist"), false),
        ("any", 7) => ("bytesValue", Unsupported, false),
        ("array", 1) => ("values", Message("any"), true),
        ("kvlist", 1) => ("values", Message("kv"), true),
        _ => return None,
    })
}
fn varint(bytes: &[u8], pos: &mut usize) -> Result<u64> {
    let mut out = 0;
    for shift in (0..70).step_by(7) {
        let b = *bytes.get(*pos).context("truncated varint")?;
        *pos += 1;
        ensure!(shift != 63 || b <= 1, "varint overflow");
        out |= u64::from(b & 127) << shift;
        if b & 128 == 0 {
            return Ok(out);
        }
    }
    anyhow::bail!("varint overflow")
}
fn take<'a>(bytes: &'a [u8], pos: &mut usize, len: usize) -> Result<&'a [u8]> {
    let end = pos.checked_add(len).context("length overflow")?;
    let out = bytes.get(*pos..end).context("truncated field")?;
    *pos = end;
    Ok(out)
}
struct Budget {
    fields: usize,
    records: usize,
}
fn decode(
    bytes: &[u8],
    message: &str,
    depth: usize,
    budget: &mut Budget,
    discard: bool,
) -> Result<Value> {
    ensure!(depth <= 16, "protobuf nesting too deep");
    if matches!(message, "point" | "hp" | "log") {
        ensure!(budget.records < super::MAX_RECORDS, "too many records");
        budget.records += 1;
    }
    let mut out = Map::new();
    // Empty repeated fields have the same meaning as JSON empty arrays.
    for n in 1..=12 {
        if let Some((name, _, true)) = field(message, n) {
            out.insert(name.into(), json!([]));
        }
    }
    let mut pos = 0;
    while pos < bytes.len() {
        ensure!(budget.fields > 0, "too many protobuf fields");
        budget.fields -= 1;
        let tag = varint(bytes, &mut pos)?;
        ensure!(
            tag >> 3 > 0 && tag >> 3 <= 0x1fff_ffff,
            "invalid field number"
        );
        let wire = tag & 7;
        let (raw, numeric) = match wire {
            0 => (&[][..], varint(bytes, &mut pos)?),
            1 => (take(bytes, &mut pos, 8)?, 0),
            2 => {
                let len = usize::try_from(varint(bytes, &mut pos)?)?;
                (take(bytes, &mut pos, len)?, 0)
            }
            5 => (take(bytes, &mut pos, 4)?, 0),
            _ => anyhow::bail!("unsupported protobuf wire type"),
        };
        let Some((name, kind, repeated)) = field(message, tag >> 3) else {
            continue;
        };
        let expected = match kind {
            Message(_) | String | Unsupported => 2,
            Int | Bool | Enum => 0,
            Double | FixedInt => 1,
        };
        ensure!(wire == expected, "incorrect wire type");
        let value = match kind {
            Message(child) => decode(raw, child, depth + 1, budget, discard || name == "body")?,
            String => {
                let text = std::str::from_utf8(raw).context("invalid UTF-8")?;
                if discard { Value::Null } else { json!(text) }
            }
            Int => json!((numeric as i64).to_string()),
            Bool => {
                ensure!(numeric <= 1, "invalid boolean");
                json!(numeric == 1)
            }
            Enum => json!(numeric),
            FixedInt => {
                let n = u64::from_le_bytes(raw.try_into()?);
                if name == "asInt" {
                    json!((n as i64).to_string())
                } else {
                    json!(n.to_string())
                }
            }
            Double => {
                let n = f64::from_le_bytes(raw.try_into()?);
                ensure!(n.is_finite(), "nonfinite double");
                json!(n)
            }
            Unsupported => Value::Null,
        };
        if repeated {
            let values = out.get_mut(name).unwrap().as_array_mut().unwrap();
            let cap = if name == "attributes" || message == "kvlist" || message == "array" {
                128
            } else {
                super::MAX_RECORDS
            };
            ensure!(values.len() < cap, "protobuf count limit");
            values.push(value);
        } else {
            ensure!(
                out.insert(
                    name.into(),
                    if name == "body" { Value::Null } else { value }
                )
                .is_none(),
                "duplicate protobuf field"
            );
        }
    }
    if message == "any" {
        ensure!(out.len() == 1, "invalid AnyValue");
    }
    if message == "metric" {
        ensure!(
            [
                "sum",
                "gauge",
                "histogram",
                "exponentialHistogram",
                "summary"
            ]
            .iter()
            .filter(|k| out.contains_key(**k))
            .count()
                == 1,
            "invalid metric data"
        );
    }
    Ok(Value::Object(out))
}
pub(super) fn request(endpoint: &str, bytes: &[u8]) -> Result<Value> {
    ensure!(bytes.len() <= super::MAX_BODY, "body oversized");
    let message = match endpoint {
        "/v1/metrics" => "metrics",
        "/v1/logs" => "logs",
        _ => anyhow::bail!("unsupported endpoint"),
    };
    decode(
        bytes,
        message,
        0,
        &mut Budget {
            fields: 65536,
            records: 0,
        },
        false,
    )
}
