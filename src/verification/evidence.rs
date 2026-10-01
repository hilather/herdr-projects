//! Verifier-owned metadata only. Output bodies are never included in receipts.
use serde_json::{Value, json};
use std::{
    collections::BTreeMap, fs, io::Read, os::fd::AsRawFd, os::unix::fs::OpenOptionsExt, path::Path,
};

const OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const TESTS: usize = 5_000;

fn read_small(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut text = String::new();
    file.take(4097).read_to_string(&mut text).ok()?;
    (text.len() <= 4096).then_some(text)
}
fn number(text: &str) -> Option<String> {
    text.parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|_| text.to_owned())
}
fn pressure(kind: &str) -> Value {
    let value = read_small(&Path::new("/proc/pressure").join(kind)).and_then(|text| {
        text.lines().find(|l| l.starts_with("some ")).and_then(|l| {
            l.split_whitespace()
                .find_map(|f| f.strip_prefix("avg10=").and_then(number))
        })
    });
    json!({"some_avg10": value, "reason": value.is_none().then_some("unreadable_or_invalid")})
}

/// OFD byte locks survive ownership narrowing and release on crash. The separate
/// probe descriptor counts our own slot too. No PID reuse or stale-run guesses.
pub(super) struct Execution {
    _slot: Option<fs::File>,
    load: Value,
}
impl Execution {
    pub(super) fn start(store: &Path) -> Self {
        let (slot, concurrent) = slots(store).unwrap_or((None, None));
        let average = read_small(Path::new("/proc/loadavg"))
            .and_then(|s| s.split_whitespace().next().and_then(number));
        Self {
            _slot: slot,
            load: json!({"sampled_unix_ms": jiff::Timestamp::now().as_millisecond(),
            "host_load_1m": average, "host_load_reason": average.is_none().then_some("unreadable_or_invalid"),
            "project_concurrent_runs": concurrent, "concurrency_reason": concurrent.is_none().then_some("execution_slots_unavailable"),
            "cpu": pressure("cpu"), "io": pressure("io")}),
        }
    }
    pub(super) fn completed(&mut self) {
        self._slot.take();
    }
    pub(super) fn metadata(&self, output: &str, truncated: bool) -> Value {
        json!({"version": "verification-metadata.v1", "load": self.load, "tests": if truncated { json!({"status": "unavailable", "reason": "output_limit", "results": []}) } else { test_results(output) }})
    }
}
fn slots(store: &Path) -> std::io::Result<(Option<fs::File>, Option<u32>)> {
    let path = store.with_file_name("verification-load.lock");
    let open = || {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
    };
    let slot = open()?;
    if !slot.metadata()?.is_file() {
        return Ok((None, None));
    }
    let probe = open()?;
    let mut lock = libc::flock {
        l_type: libc::F_WRLCK as _,
        l_whence: libc::SEEK_SET as _,
        l_start: 0,
        l_len: 1,
        l_pid: 0,
    };
    let mut claimed = false;
    for index in 0..1024 {
        lock.l_start = index;
        // SAFETY: the descriptor and flock pointer are valid for this call.
        if unsafe { libc::fcntl(slot.as_raw_fd(), libc::F_OFD_SETLK, &lock) } == 0 {
            claimed = true;
            break;
        }
        let error = std::io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::EAGAIN | libc::EACCES)) {
            return Err(error);
        }
    }
    if !claimed {
        return Ok((None, None));
    }
    let mut count = 0;
    for index in 0..1024 {
        lock.l_start = index;
        lock.l_type = libc::F_WRLCK as _;
        lock.l_pid = 0;
        // SAFETY: GETLK writes only the supplied valid flock struct.
        if unsafe { libc::fcntl(probe.as_raw_fd(), libc::F_OFD_GETLK, &mut lock) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if lock.l_type != libc::F_UNLCK as libc::c_short {
            count += 1;
        }
    }
    Ok((Some(slot), Some(count)))
}

fn name(raw: &str) -> Result<String, &'static str> {
    if raw.is_empty() || raw.len() > 256 || raw.chars().any(char::is_control) {
        return Err("invalid_test_name");
    }
    let sanitized = crate::telemetry::sanitize::excerpt(raw);
    if sanitized.is_empty() || sanitized.len() > 256 {
        return Err("invalid_test_name");
    }
    Ok(sanitized)
}
fn insert(
    rows: &mut BTreeMap<String, String>,
    raw: &str,
    outcome: &str,
) -> Result<(), &'static str> {
    let name = name(raw)?;
    if rows.len() >= TESTS || rows.insert(name, outcome.into()).is_some() {
        return Err("too_many_or_duplicate_tests");
    }
    Ok(())
}
fn test_results(output: &str) -> Value {
    let parsed = if output.len() > OUTPUT_BYTES {
        Err("output_limit")
    } else if output.trim_start().starts_with('<') {
        junit(output)
    } else {
        libtest(output)
    };
    let parsed = parsed.and_then(|rows| {
        if serde_json::to_vec(&rows)
            .map_err(|_| "metadata_limit")?
            .len()
            > 1_500_000
        {
            return Err("metadata_limit");
        }
        Ok(rows)
    });
    match parsed {
        Ok(rows) if !rows.is_empty() => {
            json!({"status": "available", "results": rows.into_iter().map(|(name, outcome)| json!({"name": name, "outcome": outcome})).collect::<Vec<_>>()})
        }
        Ok(_) => json!({"status": "unavailable", "reason": "unrecognized_output", "results": []}),
        Err(reason) => json!({"status": "unavailable", "reason": reason, "results": []}),
    }
}
fn libtest(output: &str) -> Result<BTreeMap<String, String>, &'static str> {
    let mut rows = BTreeMap::new();
    for line in output.lines() {
        if line.starts_with("test result:") {
            continue;
        }
        if let Some(test) = line.strip_prefix("test ") {
            let (name, outcome) = test.rsplit_once(" ... ").ok_or("malformed_libtest")?;
            let outcome = match outcome {
                "ok" => "pass",
                "FAILED" => "fail",
                "ignored" => "ignored",
                _ if outcome.starts_with("ignored, ") => "ignored",
                _ => return Err("malformed_libtest"),
            };
            insert(&mut rows, name, outcome)?;
        } else if line.starts_with('{') {
            let value: Value = serde_json::from_str(line).map_err(|_| "malformed_libtest_json")?;
            if value["type"] != "test" {
                continue;
            }
            let outcome = match value["event"].as_str() {
                Some("ok") => "pass",
                Some("failed") => "fail",
                Some("ignored") => "ignored",
                Some("started") => continue,
                _ => return Err("malformed_libtest_json"),
            };
            insert(
                &mut rows,
                value["name"].as_str().ok_or("malformed_libtest_json")?,
                outcome,
            )?;
        }
    }
    Ok(rows)
}

/// A deliberately restricted JUnit XML reader: matching elements, quoted
/// attributes, five predefined entities; DTD/entity declarations refused.
/// Text (failure bodies, system-out, system-err) is skipped without storage.
fn junit(output: &str) -> Result<BTreeMap<String, String>, &'static str> {
    let mut rows = BTreeMap::new();
    let mut stack = Vec::<String>::new();
    let mut current = None::<(String, String)>;
    let mut rest = output.trim();
    let mut root = false;
    if rest.starts_with("<?xml ") {
        let end = rest.find("?>").ok_or("malformed_junit")?;
        rest = &rest[end + 2..];
    }
    while !rest.is_empty() {
        let Some(start) = rest.find('<') else {
            if stack.is_empty() && !rest.trim().is_empty() {
                return Err("malformed_junit");
            }
            break;
        };
        if stack.is_empty() && !rest[..start].trim().is_empty() {
            return Err("malformed_junit");
        }
        rest = &rest[start + 1..];
        if rest.starts_with("!--") {
            let end = rest.find("-->").ok_or("malformed_junit")?;
            rest = &rest[end + 3..];
            continue;
        }
        if rest.starts_with('!') || rest.starts_with('?') {
            return Err("unsupported_junit_xml");
        }
        let mut quote = None;
        let end = rest
            .char_indices()
            .find_map(|(i, c)| {
                if let Some(q) = quote {
                    if c == q {
                        quote = None;
                    }
                } else if c == '\'' || c == '"' {
                    quote = Some(c);
                } else if c == '>' {
                    return Some(i);
                }
                None
            })
            .ok_or("malformed_junit")?;
        let tag = rest[..end].trim();
        rest = &rest[end + 1..];
        if let Some(close) = tag.strip_prefix('/') {
            if stack.pop().as_deref() != Some(close) {
                return Err("malformed_junit");
            }
            if close == "testcase" {
                let (name, outcome) = current.take().ok_or("malformed_junit")?;
                insert(&mut rows, &name, &outcome)?;
            }
            continue;
        }
        let empty = tag.ends_with('/');
        let tag = tag.strip_suffix('/').unwrap_or(tag).trim();
        let split = tag.find(char::is_whitespace).unwrap_or(tag.len());
        let element = &tag[..split];
        let attrs = attributes(&tag[split..])?;
        if stack.is_empty() {
            if root || !matches!(element, "testsuite" | "testsuites") {
                return Err("malformed_junit");
            }
            root = true;
        }
        if element == "testcase" {
            if current.is_some() || !matches!(stack.last().map(String::as_str), Some("testsuite")) {
                return Err("malformed_junit");
            }
            let raw = attrs.get("name").ok_or("malformed_junit")?;
            let full = attrs
                .get("classname")
                .filter(|c| !c.is_empty())
                .map_or_else(|| raw.clone(), |c| format!("{c}::{raw}"));
            current = Some((full, "pass".into()));
        } else if matches!(element, "failure" | "error" | "skipped") {
            let test = current.as_mut().ok_or("malformed_junit")?;
            if element != "skipped" || test.1 != "fail" {
                test.1 = if element == "skipped" {
                    "ignored"
                } else {
                    "fail"
                }
                .into();
            }
        }
        if !empty {
            if stack.len() >= 64 {
                return Err("malformed_junit");
            }
            stack.push(element.into());
        } else if element == "testcase" {
            let (name, outcome) = current.take().ok_or("malformed_junit")?;
            insert(&mut rows, &name, &outcome)?;
        }
    }
    if !stack.is_empty() || !root {
        return Err("malformed_junit");
    }
    Ok(rows)
}
fn attributes(mut text: &str) -> Result<BTreeMap<String, String>, &'static str> {
    let mut attrs = BTreeMap::new();
    while !text.trim().is_empty() {
        text = text.trim_start();
        let split = text
            .find(['=', ' ', '\t', '\n', '\r'])
            .ok_or("malformed_junit")?;
        let key = &text[..split];
        if key.is_empty()
            || !key
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b':' | b'-' | b'.'))
        {
            return Err("malformed_junit");
        }
        text = text[split..]
            .trim_start()
            .strip_prefix('=')
            .ok_or("malformed_junit")?
            .trim_start();
        let quote = text
            .chars()
            .next()
            .filter(|c| matches!(c, '\'' | '"'))
            .ok_or("malformed_junit")?;
        text = &text[1..];
        let end = text.find(quote).ok_or("malformed_junit")?;
        if attrs.len() >= 64 || attrs.contains_key(key) {
            return Err("malformed_junit");
        }
        // Only the identity attributes are retained; other values are never copied.
        let raw = &text[..end];
        if raw.contains('<') {
            return Err("malformed_junit");
        }
        let decoded = if matches!(key, "name" | "classname") {
            entities(raw)?
        } else {
            String::new()
        };
        attrs.insert(key.into(), decoded);
        text = &text[end + 1..];
        if !text.is_empty() && !text.starts_with(char::is_whitespace) {
            return Err("malformed_junit");
        }
    }
    Ok(attrs)
}
fn entities(mut text: &str) -> Result<String, &'static str> {
    if text.len() > 1536 {
        return Err("invalid_test_name");
    }
    let mut out = String::new();
    while let Some(at) = text.find('&') {
        out.push_str(&text[..at]);
        text = &text[at..];
        let (entity, remaining) = text.split_once(';').ok_or("malformed_junit")?;
        out.push(match entity {
            "&amp" => '&',
            "&lt" => '<',
            "&gt" => '>',
            "&quot" => '"',
            "&apos" => '\'',
            _ => return Err("unsupported_junit_xml"),
        });
        text = remaining;
    }
    out.push_str(text);
    Ok(out)
}
