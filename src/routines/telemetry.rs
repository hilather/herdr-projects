//! Typed command templates carried by the existing owner-signed script identity.
use std::{fs, path::Path};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use crate::{domain::RoutineDefinition, store::SqliteStore};

const PREFIX: &str = "# herdr-telemetry-routine.v1\n";
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Template {
    WeeklyReport,
    Replay { suite: String, configuration: String, subset: String, seed: String },
}

pub(super) fn parse(script: &[u8]) -> Result<Option<Template>> {
    let text = std::str::from_utf8(script)?;
    text.strip_prefix(PREFIX).map(serde_json::from_str).transpose().map_err(Into::into)
}

impl Template {
    pub(super) fn validate(&self, project: &Path, definition: &RoutineDefinition) -> Result<()> {
        if let Self::Replay { suite, configuration, subset, seed } = self {
            let bytes = crate::migration::read_plan_file(Path::new(&definition.config.path))?;
            let config = super::parse_config(&bytes, definition.config.digest.as_deref().context("routine config digest missing")?)?;
            ensure!(config.get("profiles").and_then(|p| p.get(configuration)).is_some(), "unknown replay configuration: {configuration}");
            let count: usize = subset.strip_prefix("stratified:").context("replay routine requires stratified:N")?.parse()?;
            ensure!((1..=16).contains(&count), "replay routine subset must be 1-16 cases");
            crate::replay::run(project, crate::replay::Command::Subset { suite: suite.clone(), subset: subset.clone(), seed: seed.clone() })?;
        }
        Ok(())
    }
    pub(super) fn run(&self, project: &Path, db: &mut SqliteStore, definition: &RoutineDefinition, control: &crate::store::controlled::ReadControl, locks: &[crate::runner::InheritedLock]) -> Result<Value> {
        self.validate(project, definition)?;
        control.check()?;
        match self {
            Self::WeeklyReport => weekly_report(project, Path::new(&definition.config.path).parent().context("config directory missing")?, control),
            Self::Replay { suite, configuration, subset, seed } => {
                let head = db.read_snapshot(None)?.head;
                crate::replay::run_suite_owned(project, db, suite, configuration, subset, seed, head, control, locks)
            }
        }
    }
}
fn digest(bytes: &[u8]) -> String { format!("sha256:{:x}", Sha256::digest(bytes)) }
fn display(value: &Value, reason: &Value) -> String {
    if value.is_null() { format!("n/a ({})", reason.as_str().unwrap_or("not_reported")) }
    else if let Some(s) = value.as_str() { s.to_owned() } else { value.to_string() }
}

fn weekly_report(project: &Path, config_dir: &Path, control: &crate::store::controlled::ReadControl) -> Result<Value> {
    use crate::telemetry::{export, workspace};
    let slug = project.file_name().and_then(|n| n.to_str()).context("project slug missing")?;
    ensure!(crate::telemetry::views::enabled(config_dir)?, "telemetry views disabled");
    let snapshot = workspace::snapshot(project, slug);
    ensure!(snapshot["status"] != "unavailable", "workspace unavailable: {}", snapshot["reason"]);
    let args = export::Args { metrics: ["M01", "M02", "M13", "M38", "M39", "M40", "M49"].map(String::from).to_vec(), cohort: None, from: None, to: None, as_of: None, as_of_seq: None, by: None, horizon_ms: None, drill: None, format: export::Format::Json, page_size: 100, cursor: None, out: None, max_bytes: None, external: false };
    control.check()?;
    let exported = export::run(project, config_dir, slug, &args)?;
    let doc: Value = serde_json::from_str(&exported)?;
    let week = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC).strftime("%G-W%V").to_string();
    let mut report = format!("# Fleet report: {slug}\n\nAdvisory; grants no launch, budget or selection. All-time evidence at export query time.\n\n");
    for metric in doc["metrics"].as_array().context("export metrics missing")? {
        report.push_str(&format!("- {}: {}; basis={}/{}; coverage={}; n={}\n", metric["metric_id"].as_str().unwrap_or("unknown"), display(&metric["value"], &metric["reason"]), metric["cohort"].as_str().unwrap_or("none"), metric["time_basis"].as_str().unwrap_or("none"), metric["coverage"]["state"].as_str().unwrap_or("unavailable"), display(&metric["denominator"], &metric["missing"]["denominator"])));
    }
    control.check()?;
    let library = project.join("library");
    if !library.exists() { fs::create_dir(&library)?; }
    ensure!(fs::symlink_metadata(&library)?.is_dir(), "library must be a real directory");
    // A new version for every successful read: query timestamps make successive
    // exports distinct. Never replace an operator's file or follow its symlink.
    for version in 1..=1000 {
        let stem = if version == 1 { format!("fleet-{week}") } else { format!("fleet-{week}-v{version}") };
        let path = library.join(format!("{stem}.md"));
        let manifest_path = library.join(format!("{stem}.manifest.json"));
        if path.try_exists()? || manifest_path.try_exists()? { continue; }
        let mut manifest = doc["manifest"].clone();
        manifest["report"] = json!({"file": format!("{stem}.md"), "digest": digest(report.as_bytes()), "bytes": report.len()});
        let bytes = serde_json::to_vec_pretty(&manifest)?;
        export::external::write_new(&manifest_path, &bytes)?;
        export::external::write_new(&path, report.as_bytes())?;
        return Ok(json!({"report": path, "manifest": manifest_path, "digest": digest(report.as_bytes())}));
    }
    anyhow::bail!("weekly report version bound exhausted")
}
